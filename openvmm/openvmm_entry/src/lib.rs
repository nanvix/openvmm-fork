// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! This module implements the interactive control process and the entry point
//! for the worker process.

#![expect(missing_docs)]
#![forbid(unsafe_code)]

mod cli_args;
mod crash_dump;
mod kvp;
mod meshworker;
mod pidfile;
mod repl;
mod serial_io;
mod storage_builder;
mod tracing_init;
mod ttrpc;
mod vm_controller;

// `pub` so that the missing_docs warning fires for options without
// documentation.
pub use cli_args::Options;
use console_relay::ConsoleLaunchOptions;

use crate::cli_args::SecureBootTemplateCli;
use anyhow::Context;
use anyhow::bail;
use chipset_resources::battery::HostBatteryUpdate;
use chipset_resources::microvm::MicrovmPortbHandle;
use chipset_resources::microvm::MicrovmShutdownHandle;
use chipset_resources::microvm::MicrovmSnapshotRequestHandle;
use cli_args::DiskCliKind;
use cli_args::EfiDiagnosticsLogLevelCli;
use cli_args::EndpointConfigCli;
use cli_args::IgvmPersonalityCli;
use cli_args::MachineProfileCli;
use cli_args::NicConfigCli;
use cli_args::ProvisionVmgs;
use cli_args::SerialConfigCli;
use cli_args::TpmVersionCli;
use cli_args::UefiConsoleModeCli;
use cli_args::VirtioBusCli;
use cli_args::VmgsCli;
use crash_dump::spawn_dump_handler;
use cxl_spec::test::CxlTestDeviceHandle;
use disk_backend_resources::DelayDiskHandle;
use disk_backend_resources::DiskLayerDescription;
use disk_backend_resources::layer::DiskLayerHandle;
use disk_backend_resources::layer::RamDiskLayerHandle;
use disk_backend_resources::layer::SqliteAutoCacheDiskLayerHandle;
use disk_backend_resources::layer::SqliteDiskLayerHandle;
use floppy_resources::FloppyDiskConfig;
use framebuffer::FRAMEBUFFER_SIZE;
use framebuffer::FramebufferAccess;
use futures::AsyncReadExt;
use futures::AsyncWrite;
use futures::StreamExt;
use futures::executor::block_on;
use futures::io::AllowStdIo;
use gdma_resources::GdmaDeviceHandle;
use gdma_resources::VportDefinition;
use guid::Guid;
use input_core::MultiplexedInputHandle;
use inspect::InspectMut;
use mesh::CancelContext;
use mesh::CellUpdater;
use mesh::rpc::RpcSend;
use meshworker::VmmMesh;
use net_backend_resources::mac_address::MacAddress;
use nvme_resources::NvmeControllerRequest;
use openvmm_defs::config::Config;
use openvmm_defs::config::DEFAULT_PCAT_BOOT_ORDER;
use openvmm_defs::config::DeviceVtl;
use openvmm_defs::config::HypervisorConfig;
use openvmm_defs::config::LateMapVtl0MemoryPolicy;
use openvmm_defs::config::LoadMode;
use openvmm_defs::config::MachineProfile;
use openvmm_defs::config::MemoryConfig;
use openvmm_defs::config::NumaDistance;
use openvmm_defs::config::NumaNode;
use openvmm_defs::config::NumaTopology;
use openvmm_defs::config::PcieDeviceConfig;
use openvmm_defs::config::PcieMmioRangeConfig;
use openvmm_defs::config::PciePortConfig;
use openvmm_defs::config::PcieRootComplexConfig;
use openvmm_defs::config::PcieSwitchConfig;
use openvmm_defs::config::ProcessorTopologyConfig;
use openvmm_defs::config::RootComplexCxlConfig;
use openvmm_defs::config::SerialInformation;
use openvmm_defs::config::VirtioBus;
use openvmm_defs::config::VmbusConfig;
use openvmm_defs::config::VpAssignment;
use openvmm_defs::config::VpciDeviceConfig;
use openvmm_defs::config::Vtl2BaseAddressType;
use openvmm_defs::config::Vtl2Config;
use openvmm_defs::config::build_microvm_command_line;
use openvmm_defs::rpc::VmRpc;
use openvmm_defs::worker::VM_WORKER;
use openvmm_defs::worker::VmWorkerParameters;
use openvmm_helpers::disk::OpenDiskOptions;
use openvmm_helpers::disk::create_disk_type;
use openvmm_helpers::disk::open_disk_type;
use pal_async::DefaultDriver;
use pal_async::DefaultPool;
use pal_async::socket::PolledSocket;
use pal_async::task::Spawn;
use pal_async::task::Task;
use serial_16550_resources::ComPort;
use serial_core::resources::DisconnectedSerialBackendHandle;
use sparse_mmap::alloc_shared_memory;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io;
#[cfg(unix)]
use std::io::IsTerminal;
use std::io::Write;
use std::net::TcpListener;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use storvsp_resources::ScsiControllerRequest;
use tpm_resources::TpmDeviceHandle;
use tpm_resources::TpmRegisterLayout;
use tpm_resources::TpmVersion;
use uidevices_resources::SynthKeyboardHandle;
use uidevices_resources::SynthMouseHandle;
use uidevices_resources::SynthVideoHandle;
use video_core::SharedFramebufferHandle;
use virtio_resources::VirtioPciDeviceHandle;
use vm_manifest_builder::BaseChipsetType;
use vm_manifest_builder::MachineArch;
use vm_manifest_builder::VmChipsetResult;
use vm_manifest_builder::VmManifestBuilder;
use vm_resource::IntoResource;
use vm_resource::Resource;
use vm_resource::ResourceId;
use vm_resource::kind::DiskHandleKind;
use vm_resource::kind::DiskLayerHandleKind;
use vm_resource::kind::NetEndpointHandleKind;
use vm_resource::kind::VirtioDeviceHandle;
use vm_resource::kind::VmbusDeviceHandleKind;
use vmbus_serial_resources::VmbusSerialDeviceHandle;
use vmbus_serial_resources::VmbusSerialPort;
use vmcore::non_volatile_store::resources::EphemeralNonVolatileStoreHandle;
use vmgs_resources::GuestStateEncryptionPolicy;
use vmgs_resources::VmgsDisk;
use vmgs_resources::VmgsFileHandle;
use vmgs_resources::VmgsResource;
use vmotherboard::ChipsetDeviceHandle;
use vnc_worker_defs::VncParameters;

pub fn openvmm_main() {
    #[cfg(target_os = "linux")]
    if let Err(error) = pal::unix::expand_fd_table() {
        eprintln!("warning: failed to expand the file descriptor table: {error}");
    }

    // Save the current state of the terminal so we can restore it back to
    // normal before exiting.
    #[cfg(unix)]
    let orig_termios = io::stderr().is_terminal().then(term::get_termios);

    let mut pidfile_guard: Option<pidfile::Pidfile> = None;
    let exit_code = match do_main(&mut pidfile_guard) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("fatal error: {:?}", err);
            1
        }
    };

    // Restore the terminal to its initial state.
    #[cfg(unix)]
    if let Some(orig_termios) = orig_termios {
        term::set_termios(orig_termios);
    }

    // Clean up the pidfile before terminating, since
    // pal::process::terminate skips destructors.
    drop(pidfile_guard);

    // Terminate the process immediately without graceful shutdown of DLLs or
    // C++ destructors or anything like that. This is all unnecessary and saves
    // time on Windows.
    //
    // Do flush stdout, though, since there may be buffered data.
    let _ = io::stdout().flush();
    pal::process::terminate(exit_code);
}

#[derive(Default)]
struct VmResources {
    console_in: Option<Box<dyn AsyncWrite + Send + Unpin>>,
    /// Keeps the dedicated serial reactor alive while serial I/O objects exist.
    serial_driver: Option<DefaultDriver>,
    framebuffer_access: Option<FramebufferAccess>,
    shutdown_ic: Option<mesh::Sender<hyperv_ic_resources::shutdown::ShutdownRpc>>,
    kvp_ic: Option<mesh::Sender<hyperv_ic_resources::kvp::KvpConnectRpc>>,
    scsi_rpc: Option<mesh::Sender<ScsiControllerRequest>>,
    nvme_vtl2_rpc: Option<mesh::Sender<NvmeControllerRequest>>,
    consomme_rpc: Option<mesh::Sender<net_backend_resources::consomme::ConsommeRequest>>,
    ged_rpc: Option<mesh::Sender<get_resources::ged::GuestEmulationRequest>>,
    vtl2_settings: Option<vtl2_settings_proto::Vtl2Settings>,
    microvm_snapshot_requests:
        Option<mesh::Receiver<chipset_resources::microvm::MicrovmSnapshotBoundaryRequest>>,
    microvm_console_attachment: Option<openvmm_helpers::snapshot::SnapshotAttachment>,
    microvm_console_socket_cleanup: Option<MicrovmConsoleSocketCleanup>,
    microvm_network_attachment: Option<openvmm_helpers::snapshot::SnapshotAttachment>,
    microvm_egress_policy: Option<net_backend_resources::egress::EgressPolicy>,
    microvm_filesystem_attachment: Option<openvmm_helpers::snapshot::SnapshotAttachment>,
    microvm_filesystem_root_path: Option<PathBuf>,
    microvm_sandbox_block_sources: Vec<storage_builder::MicrovmSandboxBlockSource>,
    /// Receives dirty rectangles from the synthetic video device for the VNC worker.
    dirty_rect_recv: Option<mesh::Receiver<Vec<video_core::DirtyRect>>>,
    #[cfg(windows)]
    switch_ports: Vec<vmswitch::kernel::SwitchPort>,
}

struct ConsoleState<'a> {
    device: &'a str,
    input: Box<dyn AsyncWrite + Unpin + Send>,
}

const MICROVM_CONSOLE_STABLE_ID: &str = "console:microvm-virtio0";
const MICROVM_NETWORK_STABLE_ID: &str = "net:microvm0";
const MICROVM_FILESYSTEM_STABLE_ID: &str = "fs:microvm0";

#[derive(Clone)]
struct EffectiveMicrovmNetwork {
    config: openvmm_defs::config::MicrovmNetworkConfig,
    policy: net_backend_resources::egress::EgressPolicy,
    attachment: openvmm_helpers::snapshot::SnapshotAttachment,
}

#[derive(Clone)]
struct EffectiveMicrovmFilesystem {
    config: openvmm_defs::config::MicrovmFilesystemConfig,
    root_path: String,
    attachment: openvmm_helpers::snapshot::SnapshotAttachment,
}

pub(crate) struct MicrovmConsoleSocketCleanup {
    #[cfg(unix)]
    path: PathBuf,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

impl MicrovmConsoleSocketCleanup {
    #[cfg(unix)]
    fn new(path: PathBuf) -> anyhow::Result<Self> {
        use std::os::unix::fs::FileTypeExt as _;
        use std::os::unix::fs::MetadataExt as _;

        let metadata = fs_err::symlink_metadata(&path)
            .with_context(|| format!("failed to inspect console socket {}", path.display()))?;
        anyhow::ensure!(
            metadata.file_type().is_socket(),
            "microVM console path is not a Unix socket: {}",
            path.display()
        );
        Ok(Self {
            path,
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }

    fn remove_if_owned(&self) -> anyhow::Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileTypeExt as _;
            use std::os::unix::fs::MetadataExt as _;

            let metadata = match fs_err::symlink_metadata(&self.path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
                Err(error) => return Err(error.into()),
            };
            anyhow::ensure!(
                metadata.file_type().is_socket()
                    && metadata.dev() == self.device
                    && metadata.ino() == self.inode,
                "microVM console socket ownership changed: {}",
                self.path.display()
            );
            fs_err::remove_file(&self.path).with_context(|| {
                format!("failed to remove console socket {}", self.path.display())
            })?;
        }
        Ok(())
    }
}

impl Drop for MicrovmConsoleSocketCleanup {
    fn drop(&mut self) {
        let _ = self.remove_if_owned();
    }
}

pub(crate) fn microvm_console_socket_cleanup(
    path: PathBuf,
) -> anyhow::Result<Option<MicrovmConsoleSocketCleanup>> {
    #[cfg(unix)]
    {
        Ok(Some(MicrovmConsoleSocketCleanup::new(path)?))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(None)
    }
}

fn canonical_microvm_console_path(path: &Path) -> anyhow::Result<PathBuf> {
    #[cfg(windows)]
    if path.starts_with("//./pipe") {
        return Ok(path.to_owned());
    }

    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()
            .context("failed to resolve current directory for console endpoint")?
            .join(path)
    };
    let file_name = absolute
        .file_name()
        .context("microVM console endpoint must name a socket or pipe")?;
    let parent = absolute
        .parent()
        .context("microVM console endpoint has no parent directory")?;
    let parent = fs_err::canonicalize(parent).with_context(|| {
        format!(
            "failed to canonicalize microVM console endpoint parent {}",
            parent.display()
        )
    })?;
    let canonical = parent.join(file_name);
    if let Ok(metadata) = fs_err::symlink_metadata(&canonical) {
        anyhow::ensure!(
            !metadata.file_type().is_symlink(),
            "microVM console endpoint must not be a symbolic link: {}",
            canonical.display()
        );
    }
    Ok(canonical)
}

fn microvm_console_attachment_from_cli(
    config: &SerialConfigCli,
) -> anyhow::Result<(
    SerialConfigCli,
    virtio_resources::console::VirtioConsoleAttachment,
    openvmm_helpers::snapshot::SnapshotAttachment,
)> {
    use virtio_resources::console::VirtioConsoleAttachment;
    use virtio_resources::console::VirtioConsoleAttachmentMode;
    use virtio_resources::console::VirtioConsoleBackendKind;
    use virtio_resources::console::VirtioConsoleReconnectPolicy;

    let (
        effective,
        backend_kind,
        identity_kind,
        endpoint_identity,
        mode,
        reconnect_policy,
        reconnect_policy_name,
        required,
        reconnect_timeout_ms,
    ) = match config {
        SerialConfigCli::Pipe(path) | SerialConfigCli::ConnectPipe(path) => {
            let path = canonical_microvm_console_path(path)?;
            let identity = path
                .to_str()
                .context("microVM console endpoint path is not valid UTF-8")?
                .to_owned();
            #[cfg(windows)]
            let (backend_kind, identity_kind) = {
                anyhow::ensure!(
                    path.starts_with("//./pipe/openvmm-microvm-"),
                    "Windows microVM console pipes must use //./pipe/openvmm-microvm-<NAME>"
                );
                (VirtioConsoleBackendKind::NamedPipe, "named-pipe")
            };
            #[cfg(not(windows))]
            let (backend_kind, identity_kind) =
                (VirtioConsoleBackendKind::UnixSocket, "unix-socket");
            let is_client = matches!(config, SerialConfigCli::ConnectPipe(_));
            (
                if is_client {
                    SerialConfigCli::ConnectPipe(path)
                } else {
                    SerialConfigCli::Pipe(path)
                },
                backend_kind,
                identity_kind,
                identity,
                if is_client {
                    VirtioConsoleAttachmentMode::Connect
                } else {
                    VirtioConsoleAttachmentMode::Listen
                },
                if is_client {
                    VirtioConsoleReconnectPolicy::ReconnectClient
                } else {
                    VirtioConsoleReconnectPolicy::RecreateListener
                },
                if is_client {
                    "reconnect-client"
                } else {
                    "recreate-listener"
                },
                is_client,
                if is_client {
                    openvmm_defs::config::MICROVM_CONSOLE_RECONNECT_TIMEOUT_MS
                } else {
                    0
                },
            )
        }
        SerialConfigCli::Tcp(address) | SerialConfigCli::ConnectTcp(address) => {
            anyhow::ensure!(
                address.port() != 0,
                "microVM console TCP endpoint requires a nonzero stable port"
            );
            anyhow::ensure!(
                address.ip().is_loopback(),
                "microVM console TCP endpoints must use a loopback address"
            );
            let is_client = matches!(config, SerialConfigCli::ConnectTcp(_));
            (
                if is_client {
                    SerialConfigCli::ConnectTcp(*address)
                } else {
                    SerialConfigCli::Tcp(*address)
                },
                VirtioConsoleBackendKind::Tcp,
                "tcp",
                address.to_string(),
                if is_client {
                    VirtioConsoleAttachmentMode::Connect
                } else {
                    VirtioConsoleAttachmentMode::Listen
                },
                if is_client {
                    VirtioConsoleReconnectPolicy::ReconnectClient
                } else {
                    VirtioConsoleReconnectPolicy::RecreateListener
                },
                if is_client {
                    "reconnect-client"
                } else {
                    "recreate-listener"
                },
                is_client,
                if is_client {
                    openvmm_defs::config::MICROVM_CONSOLE_RECONNECT_TIMEOUT_MS
                } else {
                    0
                },
            )
        }
        SerialConfigCli::Console => (
            SerialConfigCli::Console,
            VirtioConsoleBackendKind::Inherited,
            "provider",
            "console".to_owned(),
            VirtioConsoleAttachmentMode::Inherited,
            VirtioConsoleReconnectPolicy::RequireInheritedAttachment,
            "require-inherited-attachment",
            true,
            0,
        ),
        SerialConfigCli::None => (
            SerialConfigCli::None,
            VirtioConsoleBackendKind::Disconnected,
            "disconnected",
            "discard".to_owned(),
            VirtioConsoleAttachmentMode::Inherited,
            VirtioConsoleReconnectPolicy::DiscardWhileDisconnected,
            "discard-while-disconnected",
            false,
            0,
        ),
        _ => anyhow::bail!(
            "microVM virtio-console requires listen=..., connect=..., console, or none"
        ),
    };
    anyhow::ensure!(
        !endpoint_identity.is_empty() && endpoint_identity.len() <= 4096,
        "microVM console endpoint identity is empty or exceeds 4096 bytes"
    );

    let attachment = VirtioConsoleAttachment {
        stable_id: MICROVM_CONSOLE_STABLE_ID.to_owned(),
        backend_kind,
        mode,
        endpoint_identity: endpoint_identity.clone(),
        reconnect_policy,
        required,
        reconnect_timeout_ms,
    };
    let snapshot_attachment = openvmm_helpers::snapshot::SnapshotAttachment {
        stable_id: MICROVM_CONSOLE_STABLE_ID.to_owned(),
        kind: "virtio-console".to_owned(),
        required,
        reconnect_policy: reconnect_policy_name.to_owned(),
        identity_kind: identity_kind.to_owned(),
        identity: endpoint_identity.into_bytes(),
        length: 0,
        reconnect_timeout_ms,
    };
    Ok((effective, attachment, snapshot_attachment))
}

pub(crate) fn validate_microvm_console_attachment_namespace(
    attachment: &openvmm_helpers::snapshot::SnapshotAttachment,
    snapshot_dir: &Path,
) -> anyhow::Result<()> {
    match attachment.identity_kind.as_str() {
        "unix-socket" => {
            let identity = std::str::from_utf8(&attachment.identity)
                .context("microVM console socket identity is not valid UTF-8")?;
            let endpoint = Path::new(identity);
            let endpoint_parent = endpoint
                .parent()
                .context("microVM console socket identity has no parent")?;
            let snapshot_parent = snapshot_dir
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            let snapshot_parent = fs_err::canonicalize(snapshot_parent).with_context(|| {
                format!(
                    "failed to canonicalize snapshot parent {}",
                    snapshot_parent.display()
                )
            })?;
            anyhow::ensure!(
                endpoint_parent == snapshot_parent,
                "microVM console socket must be inside the snapshot parent namespace {}",
                snapshot_parent.display()
            );
        }
        "named-pipe" => {
            let identity = std::str::from_utf8(&attachment.identity)
                .context("microVM console pipe identity is not valid UTF-8")?;
            let name = identity
                .strip_prefix("//./pipe/openvmm-microvm-")
                .filter(|name| !name.is_empty() && !name.contains(['/', '\\']));
            anyhow::ensure!(
                name.is_some(),
                "microVM console pipe is outside //./pipe/openvmm-microvm-<NAME>"
            );
        }
        "tcp" => {}
        "provider" => {}
        "disconnected" => {}
        kind => anyhow::bail!("unsupported microVM console identity kind '{kind}'"),
    }
    Ok(())
}

fn microvm_console_attachment_from_snapshot(
    attachment: &openvmm_helpers::snapshot::SnapshotAttachment,
    requested: Option<&SerialConfigCli>,
) -> anyhow::Result<(
    SerialConfigCli,
    virtio_resources::console::VirtioConsoleAttachment,
    openvmm_helpers::snapshot::SnapshotAttachment,
)> {
    anyhow::ensure!(
        attachment.stable_id == MICROVM_CONSOLE_STABLE_ID
            && attachment.kind == "virtio-console"
            && attachment.length == 0,
        "snapshot has an invalid microVM console attachment"
    );
    let identity = std::str::from_utf8(&attachment.identity)
        .context("snapshot microVM console identity is not valid UTF-8")?;
    if attachment.reconnect_policy == "reconnect-client" {
        anyhow::ensure!(
            requested.is_some(),
            "snapshot requires an explicitly approved restore-time client attachment"
        );
    }
    let config = match (
        attachment.reconnect_policy.as_str(),
        attachment.identity_kind.as_str(),
    ) {
        ("recreate-listener", "unix-socket" | "named-pipe") => {
            SerialConfigCli::Pipe(PathBuf::from(identity))
        }
        ("recreate-listener", "tcp") => SerialConfigCli::Tcp(
            identity
                .parse()
                .context("snapshot microVM console TCP identity is invalid")?,
        ),
        ("reconnect-client", "unix-socket" | "named-pipe") => {
            SerialConfigCli::ConnectPipe(PathBuf::from(identity))
        }
        ("reconnect-client", "tcp") => SerialConfigCli::ConnectTcp(
            identity
                .parse()
                .context("snapshot microVM console TCP identity is invalid")?,
        ),
        ("require-inherited-attachment", "provider") => requested
            .cloned()
            .context("snapshot requires --virtio-console console as a replacement attachment")?,
        ("discard-while-disconnected", "disconnected") => SerialConfigCli::None,
        (policy, kind) => anyhow::bail!(
            "snapshot microVM console policy '{policy}' and backend kind '{kind}' are unsupported"
        ),
    };
    let reconstructed = microvm_console_attachment_from_cli(&config)?;
    anyhow::ensure!(
        reconstructed.2 == *attachment,
        "snapshot microVM console identity is not canonical"
    );
    if let Some(requested) = requested {
        anyhow::ensure!(
            microvm_console_attachment_from_cli(requested)?.2 == *attachment,
            "restore-time virtio-console does not match the snapshot attachment"
        );
    }
    Ok(reconstructed)
}

fn effective_microvm_console(
    requested: Option<&SerialConfigCli>,
    restore: Option<&openvmm_helpers::snapshot::SnapshotMachineContract>,
) -> anyhow::Result<
    Option<(
        SerialConfigCli,
        virtio_resources::console::VirtioConsoleAttachment,
        openvmm_helpers::snapshot::SnapshotAttachment,
    )>,
> {
    let Some(restore) = restore else {
        return requested
            .map(microvm_console_attachment_from_cli)
            .transpose();
    };
    let has_console = restore
        .devices
        .iter()
        .any(|device| device.stable_id == MICROVM_CONSOLE_STABLE_ID);
    let saved_attachment = restore
        .attachments
        .iter()
        .find(|attachment| attachment.stable_id == MICROVM_CONSOLE_STABLE_ID);
    anyhow::ensure!(
        has_console == saved_attachment.is_some(),
        "snapshot microVM console device and attachment inventories disagree"
    );
    let Some(saved_attachment) = saved_attachment else {
        anyhow::ensure!(
            requested.is_none(),
            "a restore-time virtio-console cannot be added to a snapshot without one"
        );
        return Ok(None);
    };
    Ok(Some(microvm_console_attachment_from_snapshot(
        saved_attachment,
        requested,
    )?))
}

fn microvm_network_attachment() -> openvmm_helpers::snapshot::SnapshotAttachment {
    openvmm_helpers::snapshot::SnapshotAttachment {
        stable_id: MICROVM_NETWORK_STABLE_ID.to_owned(),
        kind: "virtio-net".to_owned(),
        required: false,
        reconnect_policy: "recreate-endpoint".to_owned(),
        identity_kind: "user-mode-nat".to_owned(),
        identity: b"consomme".to_vec(),
        length: 0,
        reconnect_timeout_ms: 0,
    }
}

fn microvm_network_from_snapshot(
    saved: &openvmm_helpers::snapshot::SnapshotMicrovmNetwork,
) -> anyhow::Result<openvmm_defs::config::MicrovmNetworkConfig> {
    let prefix_length =
        u8::try_from(saved.prefix_length).context("snapshot network prefix does not fit in u8")?;
    let config = format!(
        "{}/{}",
        std::net::Ipv4Addr::from(saved.guest_ipv4),
        prefix_length
    )
    .parse::<openvmm_defs::config::MicrovmNetworkConfig>()
    .context("snapshot static network identity is invalid")?;
    anyhow::ensure!(
        saved.gateway_ipv4 == u32::from(config.derived_gateway_ipv4)
            && saved.guest_mac == config.guest_mac.to_bytes()
            && saved.gateway_mac == config.gateway_mac.to_bytes(),
        "snapshot static network identity is not canonical"
    );
    Ok(config)
}

fn effective_microvm_network(
    opt: &Options,
    restore: Option<&openvmm_helpers::snapshot::SnapshotMachineContract>,
) -> anyhow::Result<Option<EffectiveMicrovmNetwork>> {
    let requested = match opt.net.as_slice() {
        [] => None,
        [network] => match &network.endpoint {
            EndpointConfigCli::Microvm(config) => Some(config.clone()),
            _ => anyhow::bail!("microVM --net was not validated as a static IPv4 identity"),
        },
        _ => anyhow::bail!("microVM permits at most one virtio-net device"),
    };
    let Some(restore) = restore else {
        return requested
            .map(|config| {
                let policy = opt.microvm_egress_policy(&config)?;
                Ok(EffectiveMicrovmNetwork {
                    config,
                    policy,
                    attachment: microvm_network_attachment(),
                })
            })
            .transpose();
    };
    anyhow::ensure!(
        requested.is_none(),
        "restore takes microVM network addressing from saved state"
    );

    let has_device = restore
        .devices
        .iter()
        .any(|device| device.stable_id == MICROVM_NETWORK_STABLE_ID);
    let saved_attachment = restore
        .attachments
        .iter()
        .find(|attachment| attachment.stable_id == MICROVM_NETWORK_STABLE_ID);
    anyhow::ensure!(
        has_device == restore.microvm_network.is_some() && has_device == saved_attachment.is_some(),
        "snapshot microVM network device, identity, and attachment inventories disagree"
    );
    let Some(saved) = restore.microvm_network.as_ref() else {
        anyhow::ensure!(
            opt.network_profile.is_none()
                && opt.net_tap.is_none()
                && opt.allow_host.is_empty()
                && opt.block_host.is_empty()
                && opt.allow_endpoint.is_empty(),
            "restore-time network resources cannot be added to a snapshot without a NIC"
        );
        return Ok(None);
    };
    anyhow::ensure!(
        saved.profile == openvmm_defs::config::MicrovmNetworkProfile::Portable.as_str(),
        "snapshot microVM network profile is unsupported"
    );
    anyhow::ensure!(
        opt.network_profile == Some(cli_args::MicrovmNetworkProfileCli::Portable),
        "networked microVM snapshot restore requires --network-profile portable"
    );
    let config = microvm_network_from_snapshot(saved)?;
    let policy = opt.microvm_egress_policy(&config)?;
    openvmm_helpers::snapshot::validate_microvm_network_policy(saved, &policy)?;
    let attachment = microvm_network_attachment();
    anyhow::ensure!(
        Some(&attachment) == saved_attachment,
        "restore-time network endpoint does not match the snapshot attachment"
    );
    Ok(Some(EffectiveMicrovmNetwork {
        config,
        policy,
        attachment,
    }))
}

fn canonical_microvm_filesystem_root(
    path: &Path,
) -> anyhow::Result<(PathBuf, &'static str, Vec<u8>)> {
    anyhow::ensure!(
        !path.as_os_str().is_empty(),
        "microVM filesystem host path is empty"
    );
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()
            .context("failed to resolve current directory for microVM filesystem")?
            .join(path)
    };

    let mut current = PathBuf::new();
    for component in absolute.components() {
        use std::path::Component;
        match component {
            Component::Prefix(_) | Component::RootDir => {
                current.push(component.as_os_str());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                anyhow::bail!("microVM filesystem host path contains a parent component")
            }
            Component::Normal(_) => {
                current.push(component.as_os_str());
                let metadata = fs_err::symlink_metadata(&current).with_context(|| {
                    format!(
                        "failed to inspect microVM filesystem path component {}",
                        current.display()
                    )
                })?;
                anyhow::ensure!(
                    !metadata.file_type().is_symlink(),
                    "microVM filesystem path component is a symbolic link: {}",
                    current.display()
                );
                #[cfg(windows)]
                anyhow::ensure!(
                    std::os::windows::fs::MetadataExt::file_attributes(&metadata) & 0x400 == 0,
                    "microVM filesystem path component is a reparse point: {}",
                    current.display()
                );
            }
        }
    }

    let canonical = fs_err::canonicalize(&absolute).with_context(|| {
        format!(
            "failed to canonicalize microVM filesystem root {}",
            absolute.display()
        )
    })?;
    let metadata = fs_err::symlink_metadata(&canonical).with_context(|| {
        format!(
            "failed to inspect microVM filesystem root {}",
            canonical.display()
        )
    })?;
    anyhow::ensure!(
        metadata.file_type().is_dir() && !metadata.file_type().is_symlink(),
        "microVM filesystem root is not a plain directory: {}",
        canonical.display()
    );

    #[cfg(unix)]
    let (identity_kind, identity) = {
        use std::os::unix::fs::MetadataExt as _;
        let mut identity = b"openvmm-microvm-fs-unix-v1\0".to_vec();
        identity.extend_from_slice(&metadata.dev().to_le_bytes());
        identity.extend_from_slice(&metadata.ino().to_le_bytes());
        ("unix-device-inode-v1", identity)
    };
    #[cfg(windows)]
    let (identity_kind, identity) = {
        use std::os::windows::ffi::OsStrExt as _;
        let stat = pal::windows::fs::query_stat_lx_by_name(&canonical)
            .context("failed to query the microVM filesystem root identity")?;
        anyhow::ensure!(
            stat.FileId != 0,
            "microVM filesystem root has no stable file identity"
        );
        let volume = canonical
            .components()
            .next()
            .and_then(|component| match component {
                std::path::Component::Prefix(prefix) => Some(prefix.as_os_str()),
                _ => None,
            })
            .context("microVM filesystem root has no volume prefix")?;
        let volume = volume.encode_wide().collect::<Vec<_>>();
        let volume_bytes = u32::try_from(volume.len())
            .context("microVM filesystem volume identity is too long")?
            .to_le_bytes();
        let mut identity = b"openvmm-microvm-fs-windows-v1\0".to_vec();
        identity.extend_from_slice(&volume_bytes);
        identity.extend(volume.into_iter().flat_map(u16::to_le_bytes));
        identity.extend_from_slice(&stat.FileId.to_le_bytes());
        ("windows-volume-file-id-v1", identity)
    };
    #[cfg(not(any(unix, windows)))]
    let (identity_kind, identity) =
        { anyhow::bail!("microVM virtio-fs requires Linux KVM/MSHV or Windows WHP") };
    anyhow::ensure!(
        !identity.is_empty() && identity.len() <= 4096,
        "microVM filesystem root identity is empty or exceeds 4096 bytes"
    );

    Ok((canonical, identity_kind, identity))
}

pub(crate) fn microvm_filesystem_attachment(
    host_path: &Path,
) -> anyhow::Result<(String, openvmm_helpers::snapshot::SnapshotAttachment)> {
    let (canonical, identity_kind, identity) = canonical_microvm_filesystem_root(host_path)?;
    let root_path = canonical
        .to_str()
        .context("microVM filesystem host path is not valid UTF-8")?
        .to_owned();
    Ok((
        root_path,
        openvmm_helpers::snapshot::SnapshotAttachment {
            stable_id: MICROVM_FILESYSTEM_STABLE_ID.to_owned(),
            kind: "virtio-fs".to_owned(),
            required: true,
            reconnect_policy: "live-revalidate".to_owned(),
            identity_kind: identity_kind.to_owned(),
            identity,
            length: 0,
            reconnect_timeout_ms: 0,
        },
    ))
}

pub(crate) fn validate_microvm_filesystem_private_storage(
    root_path: &Path,
    snapshot_destination: Option<&Path>,
    restore_snapshot: Option<&Path>,
    memory_backing_file: Option<&Path>,
) -> anyhow::Result<()> {
    let root_path = fs_err::canonicalize(root_path).with_context(|| {
        format!(
            "failed to canonicalize microVM filesystem export root {}",
            root_path.display()
        )
    })?;
    let ensure_outside = |path: &Path, description: &str| -> anyhow::Result<()> {
        anyhow::ensure!(
            !path.starts_with(&root_path),
            "{description} must be outside the microVM filesystem export root {}",
            root_path.display()
        );
        Ok(())
    };

    if let Some(destination) = snapshot_destination {
        let parent = destination
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let parent = fs_err::canonicalize(parent).with_context(|| {
            format!(
                "failed to canonicalize snapshot destination parent {}",
                parent.display()
            )
        })?;
        ensure_outside(&parent, "snapshot destination")?;
    }
    if let Some(snapshot) = restore_snapshot {
        let snapshot = fs_err::canonicalize(snapshot).with_context(|| {
            format!(
                "failed to canonicalize restore snapshot {}",
                snapshot.display()
            )
        })?;
        ensure_outside(&snapshot, "restore snapshot")?;
    }
    if let Some(memory) = memory_backing_file {
        let memory = fs_err::canonicalize(memory).with_context(|| {
            format!(
                "failed to canonicalize guest memory backing file {}",
                memory.display()
            )
        })?;
        ensure_outside(&memory, "guest memory backing file")?;
    }
    Ok(())
}

pub(crate) fn microvm_filesystem_from_snapshot(
    saved: &openvmm_helpers::snapshot::SnapshotMicrovmFilesystem,
) -> anyhow::Result<openvmm_defs::config::MicrovmFilesystemConfig> {
    let access = match saved.access_mode.as_str() {
        "ro" => openvmm_defs::config::MicrovmFilesystemAccess::ReadOnly,
        "rw" => openvmm_defs::config::MicrovmFilesystemAccess::ReadWrite,
        mode => anyhow::bail!("snapshot microVM filesystem access mode '{mode}' is unsupported"),
    };
    openvmm_defs::config::MicrovmFilesystemConfig::new(saved.guest_mount_target.clone(), access)
        .context("snapshot microVM filesystem policy is invalid")
}

fn microvm_filesystem_slot_from_snapshot(
    contract: &openvmm_helpers::snapshot::SnapshotMachineContract,
) -> anyhow::Result<bool> {
    let has_device = contract
        .devices
        .iter()
        .any(|device| device.stable_id == MICROVM_FILESYSTEM_STABLE_ID);
    match contract.microvm_filesystem_slot_version {
        0 => Ok(has_device),
        openvmm_helpers::snapshot::MICROVM_FILESYSTEM_SLOT_VERSION => {
            anyhow::ensure!(
                has_device,
                "snapshot advertises a restore-attachable microVM filesystem slot but omits its fixed device"
            );
            Ok(true)
        }
        version => anyhow::bail!(
            "snapshot microVM filesystem slot capability version {version} is unsupported"
        ),
    }
}

fn effective_microvm_filesystem(
    requested: Option<&cli_args::MicrovmMountCli>,
    restore: Option<&openvmm_helpers::snapshot::SnapshotMachineContract>,
) -> anyhow::Result<Option<EffectiveMicrovmFilesystem>> {
    let Some(restore) = restore else {
        return requested
            .map(|requested| {
                let config = openvmm_defs::config::MicrovmFilesystemConfig::new(
                    requested.guest_target.clone(),
                    requested.access,
                )?;
                let (root_path, attachment) = microvm_filesystem_attachment(&requested.host_path)?;
                Ok(EffectiveMicrovmFilesystem {
                    config,
                    root_path,
                    attachment,
                })
            })
            .transpose();
    };

    let has_device = microvm_filesystem_slot_from_snapshot(restore)?;
    let saved_attachment = restore
        .attachments
        .iter()
        .find(|attachment| attachment.stable_id == MICROVM_FILESYSTEM_STABLE_ID);
    anyhow::ensure!(
        saved_attachment.is_some() == restore.microvm_filesystem.is_some()
            && (restore.microvm_filesystem_slot_version
                == openvmm_helpers::snapshot::MICROVM_FILESYSTEM_SLOT_VERSION
                || has_device == restore.microvm_filesystem.is_some()),
        "snapshot microVM filesystem slot, policy, and attachment inventories disagree"
    );
    let Some(saved) = restore.microvm_filesystem.as_ref() else {
        let Some(requested) = requested else {
            return Ok(None);
        };
        anyhow::ensure!(
            restore.microvm_filesystem_slot_version
                == openvmm_helpers::snapshot::MICROVM_FILESYSTEM_SLOT_VERSION,
            "snapshot does not support restore-time microVM filesystem attachment"
        );
        let config = openvmm_defs::config::MicrovmFilesystemConfig::new(
            requested.guest_target.clone(),
            requested.access,
        )?;
        let (root_path, attachment) = microvm_filesystem_attachment(&requested.host_path)?;
        return Ok(Some(EffectiveMicrovmFilesystem {
            config,
            root_path,
            attachment,
        }));
    };
    let requested = requested
        .context("snapshot restore requires a fresh --mount attachment for fs:microvm0")?;
    let config = microvm_filesystem_from_snapshot(saved)?;
    anyhow::ensure!(
        requested.guest_target == config.guest_mount_target && requested.access == config.access,
        "restore-time mount target or access mode does not match the snapshot contract"
    );
    let (root_path, attachment) = microvm_filesystem_attachment(&requested.host_path)?;
    anyhow::ensure!(
        !saved.canonical_host_path.is_empty(),
        "snapshot filesystem canonical host path is missing; this snapshot predates path-bound filesystem restore"
    );
    anyhow::ensure!(
        root_path == saved.canonical_host_path,
        "restore-time filesystem canonical host path does not match the snapshot contract"
    );
    anyhow::ensure!(
        Some(&attachment) == saved_attachment,
        "restore-time filesystem root identity does not match the snapshot attachment"
    );
    Ok(Some(EffectiveMicrovmFilesystem {
        config,
        root_path,
        attachment,
    }))
}

/// Build a flat list of switches with their parent port assignments.
///
/// This function converts hierarchical CLI switch definitions into a flat list
/// where each switch specifies its parent port directly.
fn build_switch_list(all_switches: &[cli_args::GenericPcieSwitchCli]) -> Vec<PcieSwitchConfig> {
    all_switches
        .iter()
        .map(|switch_cli| PcieSwitchConfig {
            name: switch_cli.name.clone(),
            parent_port: switch_cli.port_name.clone(),
            ports: (0..switch_cli.num_downstream_ports)
                .map(|i| PciePortConfig {
                    name: format!("{}-downstream-{}", switch_cli.name, i),
                    devfn: None,
                    hotplug: switch_cli.hotplug,
                    acs_capabilities_supported: switch_cli.acs_capabilities_supported,
                    cxl: false,
                    pasid: switch_cli.pasid,
                })
                .collect(),
        })
        .collect()
}

fn base_chipset_type(opt: &Options) -> BaseChipsetType {
    if opt.machine == MachineProfileCli::Microvm {
        BaseChipsetType::Microvm
    } else if opt.igvm.is_some() {
        match opt.igvm_personality {
            None => BaseChipsetType::HclHost,
            Some(IgvmPersonalityCli::Uefi) => BaseChipsetType::HypervGen2Uefi,
            Some(IgvmPersonalityCli::LinuxDirect)
                if matches!(opt.isolation, Some(cli_args::IsolationCli::Snp)) =>
            {
                BaseChipsetType::EnlightenedLinuxDirect
            }
            Some(IgvmPersonalityCli::LinuxDirect) if opt.hv => {
                BaseChipsetType::HyperVGen2LinuxDirect
            }
            Some(IgvmPersonalityCli::LinuxDirect) => BaseChipsetType::UnenlightenedLinuxDirect,
        }
    } else if matches!(opt.isolation, Some(cli_args::IsolationCli::Snp)) {
        BaseChipsetType::EnlightenedLinuxDirect
    } else if opt.pcat {
        BaseChipsetType::HypervGen1
    } else if opt.uefi {
        BaseChipsetType::HypervGen2Uefi
    } else if opt.hv {
        BaseChipsetType::HyperVGen2LinuxDirect
    } else {
        BaseChipsetType::UnenlightenedLinuxDirect
    }
}

/// Build the loader's [`SmbiosConfig`](openvmm_defs::config::SmbiosConfig) from
/// the parsed `--smbios` arguments.
///
/// Multiple `--smbios` arguments are merged (erroring on a field set twice).
/// String overrides left unset fall through to the loader's default identity.
/// The system UUID defaults to the all-zero GUID unless overridden with
/// `uuid=GUID`; `uuid=random` requests a freshly generated per-VM GUID.
fn smbios_config_from_cli(
    args: &[cli_args::SmbiosCli],
) -> anyhow::Result<openvmm_defs::config::SmbiosConfig> {
    let mut merged = cli_args::SmbiosCli::default();
    for arg in args {
        merged.merge(arg.clone())?;
    }
    let cli_args::SmbiosCli {
        bios:
            cli_args::SmbiosBiosCli {
                vendor: bios_vendor,
                version: bios_version,
                release_date: bios_release_date,
                release: bios_release,
            },
        system:
            cli_args::SmbiosSystemCli {
                manufacturer: system_manufacturer,
                product_name: system_product,
                version: system_version,
                serial_number: system_serial,
                sku_number: system_sku,
                family: system_family,
                uuid: system_uuid,
            },
    } = merged;
    Ok(openvmm_defs::config::SmbiosConfig {
        bios: openvmm_defs::config::SmbiosBiosOverrides {
            vendor: bios_vendor,
            version: bios_version,
            release_date: bios_release_date,
            release: bios_release.map(|r| (r.0, r.1)),
        },
        system: openvmm_defs::config::SmbiosSystemOverrides {
            manufacturer: system_manufacturer,
            product_name: system_product,
            version: system_version,
            serial_number: system_serial,
            sku_number: system_sku,
            family: system_family,
            uuid: match system_uuid {
                None => Guid::ZERO,
                Some(cli_args::SmbiosUuid::Random) => Guid::new_random(),
                Some(cli_args::SmbiosUuid::Fixed(guid)) => guid,
            },
        },
    })
}

#[cfg(test)]
mod microvm_console_attachment_tests {
    use super::*;
    use clap::Parser as _;
    use test_with_tracing::test;

    #[test]
    fn maps_igvm_personalities_to_chipsets() {
        for (args, expected) in [
            (
                vec![
                    "openvmm",
                    "--igvm",
                    "guest.igvm",
                    "--igvm-personality",
                    "uefi",
                ],
                BaseChipsetType::HypervGen2Uefi,
            ),
            (
                vec![
                    "openvmm",
                    "--igvm",
                    "guest.igvm",
                    "--igvm-personality",
                    "linux-direct",
                ],
                BaseChipsetType::UnenlightenedLinuxDirect,
            ),
            (
                vec![
                    "openvmm",
                    "--igvm",
                    "guest.igvm",
                    "--igvm-personality",
                    "linux-direct",
                    "--hv",
                ],
                BaseChipsetType::HyperVGen2LinuxDirect,
            ),
            (
                vec![
                    "openvmm",
                    "--igvm",
                    "guest.igvm",
                    "--igvm-personality",
                    "linux-direct",
                    "--isolation",
                    "snp",
                ],
                BaseChipsetType::EnlightenedLinuxDirect,
            ),
            (
                vec!["openvmm", "--igvm", "guest.igvm", "--hv", "--vtl2"],
                BaseChipsetType::HclHost,
            ),
        ] {
            let opt = Options::try_parse_from(args).unwrap();
            assert_eq!(
                std::mem::discriminant(&base_chipset_type(&opt)),
                std::mem::discriminant(&expected)
            );
        }
    }

    fn socket_attachment(path: &Path) -> openvmm_helpers::snapshot::SnapshotAttachment {
        openvmm_helpers::snapshot::SnapshotAttachment {
            stable_id: MICROVM_CONSOLE_STABLE_ID.to_owned(),
            kind: "virtio-console".to_owned(),
            required: false,
            reconnect_policy: "recreate-listener".to_owned(),
            identity_kind: "unix-socket".to_owned(),
            identity: path.to_string_lossy().into_owned().into_bytes(),
            length: 0,
            reconnect_timeout_ms: 0,
        }
    }

    #[test]
    fn socket_attachment_must_stay_in_snapshot_parent() {
        let allowed = tempfile::tempdir().unwrap();
        let allowed = fs_err::canonicalize(allowed.path()).unwrap();
        let snapshot = allowed.join("snapshot");
        let attachment = socket_attachment(&allowed.join("console.sock"));
        validate_microvm_console_attachment_namespace(&attachment, &snapshot).unwrap();

        let outside = tempfile::tempdir().unwrap();
        let outside = fs_err::canonicalize(outside.path()).unwrap();
        let attachment = socket_attachment(&outside.join("console.sock"));
        assert!(validate_microvm_console_attachment_namespace(&attachment, &snapshot).is_err());
    }

    #[test]
    fn named_pipe_attachment_uses_dedicated_namespace() {
        let mut attachment = socket_attachment(Path::new("unused"));
        attachment.identity_kind = "named-pipe".to_owned();
        attachment.identity = b"//./pipe/openvmm-microvm-console0".to_vec();
        validate_microvm_console_attachment_namespace(&attachment, Path::new("snapshot")).unwrap();

        attachment.identity = b"//./pipe/unrelated".to_vec();
        assert!(
            validate_microvm_console_attachment_namespace(&attachment, Path::new("snapshot"))
                .is_err()
        );
    }

    #[test]
    fn client_attachment_is_required_and_bounded() {
        let requested = SerialConfigCli::ConnectTcp("127.0.0.1:5555".parse().unwrap());
        let (_, resource, snapshot) = microvm_console_attachment_from_cli(&requested).unwrap();
        assert!(snapshot.required);
        assert_eq!(snapshot.reconnect_policy, "reconnect-client");
        assert_eq!(
            snapshot.reconnect_timeout_ms,
            openvmm_defs::config::MICROVM_CONSOLE_RECONNECT_TIMEOUT_MS
        );
        assert_eq!(
            resource.reconnect_policy,
            virtio_resources::console::VirtioConsoleReconnectPolicy::ReconnectClient
        );

        assert!(microvm_console_attachment_from_snapshot(&snapshot, None).is_err());
        let (restored, _, restored_snapshot) =
            microvm_console_attachment_from_snapshot(&snapshot, Some(&requested)).unwrap();
        assert!(matches!(restored, SerialConfigCli::ConnectTcp(_)));
        assert_eq!(restored_snapshot, snapshot);
    }

    #[test]
    fn tcp_attachment_rejects_non_loopback_addresses() {
        assert!(
            microvm_console_attachment_from_cli(&SerialConfigCli::Tcp(
                "0.0.0.0:5555".parse().unwrap()
            ))
            .is_err()
        );
        assert!(
            microvm_console_attachment_from_cli(&SerialConfigCli::ConnectTcp(
                "192.0.2.1:5555".parse().unwrap()
            ))
            .is_err()
        );
    }

    #[test]
    fn inherited_attachment_requires_matching_replacement() {
        let (_, _, snapshot) =
            microvm_console_attachment_from_cli(&SerialConfigCli::Console).unwrap();
        assert!(snapshot.required);
        assert_eq!(snapshot.reconnect_policy, "require-inherited-attachment");
        assert!(microvm_console_attachment_from_snapshot(&snapshot, None).is_err());
        assert!(
            microvm_console_attachment_from_snapshot(&snapshot, Some(&SerialConfigCli::Console))
                .is_ok()
        );
        assert!(
            microvm_console_attachment_from_snapshot(
                &snapshot,
                Some(&SerialConfigCli::ConnectTcp(
                    "127.0.0.1:5555".parse().unwrap()
                ))
            )
            .is_err()
        );
    }

    #[test]
    fn disconnected_attachment_preserves_discard_policy() {
        let (config, resource, snapshot) =
            microvm_console_attachment_from_cli(&SerialConfigCli::None).unwrap();
        assert!(matches!(config, SerialConfigCli::None));
        assert!(!snapshot.required);
        assert_eq!(snapshot.reconnect_policy, "discard-while-disconnected");
        assert_eq!(
            resource.reconnect_policy,
            virtio_resources::console::VirtioConsoleReconnectPolicy::DiscardWhileDisconnected
        );

        let (restored, _, restored_snapshot) =
            microvm_console_attachment_from_snapshot(&snapshot, None).unwrap();
        assert!(matches!(restored, SerialConfigCli::None));
        assert_eq!(restored_snapshot, snapshot);
    }

    #[test]
    fn snapshot_downtime_accepts_supported_elapsed_time() {
        let capture = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1);

        assert_eq!(
            calculate_snapshot_downtime(capture, capture).unwrap(),
            Duration::ZERO
        );
        assert_eq!(
            calculate_snapshot_downtime(capture, capture + MAX_SNAPSHOT_DOWNTIME).unwrap(),
            MAX_SNAPSHOT_DOWNTIME
        );
    }

    #[test]
    fn snapshot_downtime_rejects_rollback_and_excessive_elapsed_time() {
        let capture = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1);

        let rollback =
            calculate_snapshot_downtime(capture, std::time::SystemTime::UNIX_EPOCH).unwrap_err();
        assert!(
            rollback
                .to_string()
                .contains("before snapshot capture time")
        );

        let excessive = calculate_snapshot_downtime(
            capture,
            capture + MAX_SNAPSHOT_DOWNTIME + Duration::from_secs(1),
        )
        .unwrap_err();
        assert!(excessive.to_string().contains("exceeds the supported"));
    }

    fn network_contract() -> openvmm_helpers::snapshot::SnapshotMachineContract {
        let network: openvmm_defs::config::MicrovmNetworkConfig = "10.0.0.2/24".parse().unwrap();
        let policy = net_backend_resources::egress::EgressPolicy::bind(
            network.guest_ipv4,
            network.prefix_length,
            network.guest_mac,
            network.derived_gateway_ipv4,
            net_backend_resources::egress::EgressPolicyMode::TcpEndpoints(vec![
                "10.0.0.9:8443".parse().unwrap(),
                "192.0.2.7:443".parse().unwrap(),
                "10.0.0.9:443".parse().unwrap(),
            ]),
        )
        .unwrap();
        let source_hypervisor = if cfg!(windows) { "whp" } else { "kvm" };
        let irq = openvmm_defs::config::microvm_virtio_net_irq(Some(source_hypervisor)).unwrap();
        let mut command_line = build_microvm_command_line(&[], false).unwrap();
        openvmm_defs::config::append_microvm_virtio_discovery(
            &mut command_line,
            Some((&network, irq, policy.allows_gateway_dns())),
            false,
            None,
            false,
            &[],
        )
        .unwrap();
        openvmm_helpers::snapshot::microvm_machine_contract(
            source_hypervisor,
            command_line,
            Some((&network, &policy, microvm_network_attachment())),
            false,
            None,
            None,
            Vec::new(),
            1,
            1024,
            None,
            [
                "partition",
                "vmtime",
                "pic",
                "ioapic",
                "pit",
                "rtc",
                "microvm-portb",
                "microvm-shutdown",
                "microvm-snapshot-request",
                "virtio-net-3489660928",
            ]
            .map(str::to_owned)
            .to_vec(),
            std::time::SystemTime::now().into(),
            1_000_000_000,
            Some(1_000_000_000),
            vec![1, 2, 3],
        )
        .unwrap()
    }

    #[test]
    fn network_restore_rebinds_endpoint_next_hops_and_requires_policy() {
        let contract = network_contract();
        let options = Options::try_parse_from([
            "openvmm",
            "--machine",
            "microvm",
            "--restore-snapshot",
            "snapshot",
            "--network-profile",
            "portable",
            "--allow-endpoint",
            "192.0.2.7:443",
            "--allow-endpoint",
            "10.0.0.9:443",
            "--allow-endpoint",
            "10.0.0.9:8443",
        ])
        .unwrap();
        let restored = effective_microvm_network(&options, Some(&contract))
            .unwrap()
            .unwrap();
        assert_eq!(
            restored.config.guest_ipv4,
            std::net::Ipv4Addr::new(10, 0, 0, 2)
        );
        assert_eq!(
            restored.policy.next_hops(),
            &[
                std::net::Ipv4Addr::new(10, 0, 0, 1),
                std::net::Ipv4Addr::new(10, 0, 0, 9),
            ]
        );
        let restored_again = effective_microvm_network(&options, Some(&contract))
            .unwrap()
            .unwrap();
        assert_eq!(restored_again.policy, restored.policy);

        let missing_policy = Options::try_parse_from([
            "openvmm",
            "--machine",
            "microvm",
            "--restore-snapshot",
            "snapshot",
            "--network-profile",
            "portable",
        ])
        .unwrap();
        assert!(effective_microvm_network(&missing_policy, Some(&contract)).is_err());
    }

    #[test]
    fn legacy_network_policy_contract_alignment_preserves_validated_digest() {
        let mut saved = network_contract();
        let saved_network = saved.microvm_network.as_mut().unwrap();
        saved_network.egress_policy_encoding_version = 0;
        saved_network.egress_policy_sha256 = vec![0x5a; 32];
        let mut expected = network_contract();

        align_legacy_network_policy_contract(&saved, &mut expected);
        assert_eq!(saved.microvm_network, expected.microvm_network);
    }

    #[test]
    fn network_restore_rejects_attachment_identity_change() {
        let mut contract = network_contract();
        let attachment = contract
            .attachments
            .iter_mut()
            .find(|attachment| attachment.stable_id == MICROVM_NETWORK_STABLE_ID)
            .unwrap();
        attachment.identity.push(b'x');
        let options = Options::try_parse_from([
            "openvmm",
            "--machine",
            "microvm",
            "--restore-snapshot",
            "snapshot",
            "--network-profile",
            "portable",
            "--allow-endpoint",
            "192.0.2.7:443",
            "--allow-endpoint",
            "10.0.0.9:443",
            "--allow-endpoint",
            "10.0.0.9:8443",
        ])
        .unwrap();
        assert!(effective_microvm_network(&options, Some(&contract)).is_err());
    }

    fn filesystem_contract(root: &Path) -> openvmm_helpers::snapshot::SnapshotMachineContract {
        let filesystem = openvmm_defs::config::MicrovmFilesystemConfig::new(
            "/mnt/share".to_owned(),
            openvmm_defs::config::MicrovmFilesystemAccess::ReadOnly,
        )
        .unwrap();
        let (root_path, attachment) = microvm_filesystem_attachment(root).unwrap();
        let mut command_line = build_microvm_command_line(&[], false).unwrap();
        openvmm_defs::config::append_microvm_virtio_discovery(
            &mut command_line,
            None,
            true,
            Some(&filesystem),
            false,
            &[],
        )
        .unwrap();
        openvmm_helpers::snapshot::microvm_machine_contract(
            if cfg!(windows) { "whp" } else { "kvm" },
            command_line,
            None,
            true,
            Some((&filesystem, Path::new(&root_path), attachment)),
            None,
            Vec::new(),
            1,
            1024,
            None,
            [
                "partition",
                "vmtime",
                "pic",
                "ioapic",
                "pit",
                "rtc",
                "microvm-portb",
                "microvm-shutdown",
                "microvm-snapshot-request",
                "virtiofs-3489665024",
            ]
            .map(str::to_owned)
            .to_vec(),
            std::time::SystemTime::now().into(),
            1_000_000_000,
            Some(1_000_000_000),
            vec![1, 2, 3],
        )
        .unwrap()
    }

    fn dormant_filesystem_contract() -> openvmm_helpers::snapshot::SnapshotMachineContract {
        let mut command_line = build_microvm_command_line(&[], false).unwrap();
        openvmm_defs::config::append_microvm_virtio_discovery(
            &mut command_line,
            None,
            true,
            None,
            false,
            &[],
        )
        .unwrap();
        openvmm_helpers::snapshot::microvm_machine_contract(
            if cfg!(windows) { "whp" } else { "kvm" },
            command_line,
            None,
            true,
            None,
            None,
            Vec::new(),
            1,
            1024,
            None,
            [
                "partition",
                "vmtime",
                "pic",
                "ioapic",
                "pit",
                "rtc",
                "microvm-portb",
                "microvm-shutdown",
                "microvm-snapshot-request",
                "virtiofs-3489665024",
            ]
            .map(str::to_owned)
            .to_vec(),
            std::time::SystemTime::now().into(),
            1_000_000_000,
            Some(1_000_000_000),
            vec![1, 2, 3],
        )
        .unwrap()
    }

    fn restore_mount_options(root: &Path, mode: &str) -> Options {
        Options::try_parse_from([
            "openvmm",
            "--machine",
            "microvm",
            "--restore-snapshot",
            "snapshot",
            "--mount",
            &format!("/mnt/share,{},{}", root.display(), mode),
        ])
        .unwrap()
    }

    #[test]
    fn filesystem_restore_requires_same_live_root_identity() {
        let root = tempfile::tempdir().unwrap();
        let contract = filesystem_contract(root.path());
        let options = restore_mount_options(root.path(), "ro");
        let restored =
            effective_microvm_filesystem(options.microvm_mount.as_ref(), Some(&contract))
                .unwrap()
                .unwrap();
        assert_eq!(restored.config.guest_mount_target, "/mnt/share");
        assert_eq!(restored.attachment, contract.attachments[0]);

        let replacement = tempfile::tempdir().unwrap();
        let replacement_options = restore_mount_options(replacement.path(), "ro");
        assert!(
            effective_microvm_filesystem(
                replacement_options.microvm_mount.as_ref(),
                Some(&contract)
            )
            .is_err()
        );
    }

    #[test]
    fn filesystem_restore_rejects_same_root_at_a_new_path() {
        let parent = tempfile::tempdir().unwrap();
        let original = parent.path().join("original");
        let moved = parent.path().join("moved");
        fs_err::create_dir(&original).unwrap();
        let contract = filesystem_contract(&original);
        fs_err::rename(&original, &moved).unwrap();

        let options = restore_mount_options(&moved, "ro");
        assert!(
            effective_microvm_filesystem(options.microvm_mount.as_ref(), Some(&contract)).is_err()
        );
    }

    #[test]
    fn filesystem_restore_rejects_missing_or_changed_policy() {
        let root = tempfile::tempdir().unwrap();
        let contract = filesystem_contract(root.path());
        assert!(effective_microvm_filesystem(None, Some(&contract)).is_err());

        let changed_mode = restore_mount_options(root.path(), "rw");
        assert!(
            effective_microvm_filesystem(changed_mode.microvm_mount.as_ref(), Some(&contract))
                .is_err()
        );
    }

    #[test]
    fn filesystem_restore_without_mount_preserves_dormant_slot() {
        let contract = dormant_filesystem_contract();
        assert!(
            effective_microvm_filesystem(None, Some(&contract))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn filesystem_restore_attaches_mount_to_dormant_slot() {
        let root = tempfile::tempdir().unwrap();
        let contract = dormant_filesystem_contract();
        let options = restore_mount_options(root.path(), "rw");
        let filesystem =
            effective_microvm_filesystem(options.microvm_mount.as_ref(), Some(&contract))
                .unwrap()
                .unwrap();
        assert_eq!(filesystem.config.guest_mount_target, "/mnt/share");
        assert_eq!(
            filesystem.config.access,
            openvmm_defs::config::MicrovmFilesystemAccess::ReadWrite
        );
        assert_eq!(
            filesystem.root_path,
            fs_err::canonicalize(root.path()).unwrap().to_str().unwrap()
        );
    }

    #[test]
    fn filesystem_restore_rejects_mount_for_legacy_snapshot_without_slot() {
        let root = tempfile::tempdir().unwrap();
        let contract = network_contract();
        let options = restore_mount_options(root.path(), "ro");
        let error =
            match effective_microvm_filesystem(options.microvm_mount.as_ref(), Some(&contract)) {
                Err(error) => error,
                Ok(_) => panic!("legacy snapshot unexpectedly accepted a restore-time mount"),
            };
        assert!(
            error
                .to_string()
                .contains("does not support restore-time microVM filesystem attachment")
        );
    }

    #[test]
    fn filesystem_root_rejects_parent_components() {
        let root = tempfile::tempdir().unwrap();
        assert!(canonical_microvm_filesystem_root(&root.path().join("child").join("..")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn filesystem_root_rejects_symbolic_link_components() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target");
        fs_err::create_dir(&target).unwrap();
        let link = root.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(canonical_microvm_filesystem_root(&link).is_err());
    }

    #[test]
    fn filesystem_export_rejects_snapshot_and_memory_storage() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("share");
        fs_err::create_dir(&root).unwrap();
        let memory = root.join("memory.bin");
        fs_err::write(&memory, b"memory").unwrap();
        let restore = root.join("restore");
        fs_err::create_dir(&restore).unwrap();

        assert!(
            validate_microvm_filesystem_private_storage(
                &root,
                Some(&root.join("snapshot")),
                None,
                None,
            )
            .is_err()
        );
        assert!(
            validate_microvm_filesystem_private_storage(&root, None, Some(&restore), None,)
                .is_err()
        );
        assert!(
            validate_microvm_filesystem_private_storage(&root, None, None, Some(&memory),).is_err()
        );
        validate_microvm_filesystem_private_storage(
            &root,
            Some(&parent.path().join("snapshot")),
            None,
            None,
        )
        .unwrap();
    }

    #[test]
    fn network_restore_requires_portable_profile() {
        let contract = network_contract();
        let options = Options::try_parse_from([
            "openvmm",
            "--machine",
            "microvm",
            "--restore-snapshot",
            "snapshot",
            "--allow-host",
            "192.0.2.0/24",
        ])
        .unwrap();
        assert!(effective_microvm_network(&options, Some(&contract)).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn restore_bind_does_not_unlink_existing_socket() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("console.sock");
        let _existing = unix_socket::UnixListener::bind(&path).unwrap();
        assert!(serial_io::bind_serial_without_cleanup(&path).is_err());
        assert!(fs_err::symlink_metadata(&path).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_guard_does_not_remove_replaced_socket() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("console.sock");
        let listener = unix_socket::UnixListener::bind(&path).unwrap();
        let cleanup = MicrovmConsoleSocketCleanup::new(path.clone()).unwrap();
        fs_err::remove_file(&path).unwrap();
        let replacement = unix_socket::UnixListener::bind(&path).unwrap();

        assert!(cleanup.remove_if_owned().is_err());
        assert!(fs_err::symlink_metadata(&path).is_ok());
        drop((cleanup, replacement, listener));
    }
}

async fn vm_config_from_command_line(
    spawner: impl Spawn,
    mesh: &VmmMesh,
    opt: &Options,
    restore_machine_contract: Option<&openvmm_helpers::snapshot::SnapshotMachineContract>,
    restore_memory_target_requested: bool,
    restore_memory_ranges: &[openvmm_helpers::snapshot::SnapshotMemoryExpansionRange],
) -> anyhow::Result<(Config, VmResources)> {
    let is_microvm = opt.machine == MachineProfileCli::Microvm;
    opt.validate_isolation_options()?;
    opt.validate_igvm_options()?;
    if let Some(contract) = restore_machine_contract {
        openvmm_helpers::snapshot::validate_supported_microvm_contract(contract)?;
    }
    opt.validate_microvm_options()?;
    let effective_microvm_network = if is_microvm {
        effective_microvm_network(opt, restore_machine_contract)?
    } else {
        None
    };
    let microvm_network = effective_microvm_network
        .as_ref()
        .map(|network| network.config.clone());
    let microvm_egress_policy = effective_microvm_network
        .as_ref()
        .map(|network| network.policy.clone());
    let microvm_network_attachment = effective_microvm_network
        .as_ref()
        .map(|network| network.attachment.clone());
    let microvm_filesystem_slot = if is_microvm {
        restore_machine_contract
            .map(microvm_filesystem_slot_from_snapshot)
            .transpose()?
            .unwrap_or(true)
    } else {
        false
    };
    let effective_microvm_filesystem = if is_microvm {
        effective_microvm_filesystem(opt.microvm_mount.as_ref(), restore_machine_contract)?
    } else {
        None
    };
    let microvm_filesystem = effective_microvm_filesystem
        .as_ref()
        .map(|filesystem| filesystem.config.clone());
    let microvm_filesystem_attachment = effective_microvm_filesystem
        .as_ref()
        .map(|filesystem| filesystem.attachment.clone());
    if let Some(filesystem) = &effective_microvm_filesystem
        && opt.snapshot_destination.is_some()
    {
        tracing::warn!(
            stable_id = MICROVM_FILESYSTEM_STABLE_ID,
            access_mode = filesystem.config.access.as_str(),
            "microVM snapshot excludes live host filesystem contents; restore revalidates the external directory and may fail after host changes"
        );
    }
    let microvm_gateway_dns = microvm_egress_policy
        .as_ref()
        .is_some_and(|policy| policy.allows_gateway_dns());

    let (_, serial_driver) = DefaultPool::spawn_on_thread("serial");

    let openhcl_vtl = if opt.vtl2 {
        DeviceVtl::Vtl2
    } else {
        DeviceVtl::Vtl0
    };

    let console_state: RefCell<Option<ConsoleState<'_>>> = RefCell::new(None);
    let setup_serial = |name: &str, cli_cfg, device| -> anyhow::Result<_> {
        Ok(match cli_cfg {
            SerialConfigCli::Console => {
                if let Some(console_state) = console_state.borrow().as_ref() {
                    bail!("console already set by {}", console_state.device);
                }
                let (config, serial) = serial_io::anonymous_serial_pair(&serial_driver)?;
                let (serial_read, serial_write) = AsyncReadExt::split(serial);
                *console_state.borrow_mut() = Some(ConsoleState {
                    device,
                    input: Box::new(serial_write),
                });
                thread::Builder::new()
                    .name(name.to_owned())
                    .spawn(move || {
                        let _ = block_on(futures::io::copy(
                            serial_read,
                            &mut AllowStdIo::new(term::raw_stdout()),
                        ));
                    })
                    .unwrap();
                Some(config)
            }
            SerialConfigCli::Stderr => {
                let (config, serial) = serial_io::anonymous_serial_pair(&serial_driver)?;
                thread::Builder::new()
                    .name(name.to_owned())
                    .spawn(move || {
                        let _ = block_on(futures::io::copy(
                            serial,
                            &mut AllowStdIo::new(term::raw_stderr()),
                        ));
                    })
                    .unwrap();
                Some(config)
            }
            SerialConfigCli::File(path) => {
                let (config, serial) = serial_io::anonymous_serial_pair(&serial_driver)?;
                let file = fs_err::File::create(path).context("failed to create file")?;

                thread::Builder::new()
                    .name(name.to_owned())
                    .spawn(move || {
                        let _ = block_on(futures::io::copy(serial, &mut AllowStdIo::new(file)));
                    })
                    .unwrap();
                Some(config)
            }
            SerialConfigCli::None => None,
            SerialConfigCli::Pipe(path) => {
                Some(serial_io::bind_serial(&path).context("failed to bind serial")?)
            }
            SerialConfigCli::Tcp(addr) => {
                Some(serial_io::bind_tcp_serial(&addr).context("failed to bind serial")?)
            }
            SerialConfigCli::ConnectPipe(path) => Some(
                serial_io::connect_serial_with_timeout(
                    &path,
                    Duration::from_millis(
                        openvmm_defs::config::MICROVM_CONSOLE_RECONNECT_TIMEOUT_MS,
                    ),
                )
                .context("failed to connect serial")?,
            ),
            SerialConfigCli::ConnectTcp(addr) => Some(serial_io::connect_tcp_serial(
                &addr,
                Duration::from_millis(openvmm_defs::config::MICROVM_CONSOLE_RECONNECT_TIMEOUT_MS),
            )?),
            SerialConfigCli::NewConsole(app, window_title) => {
                let path = console_relay::random_console_path();
                let config =
                    serial_io::bind_serial(&path).context("failed to bind console serial")?;
                let window_title =
                    window_title.unwrap_or_else(|| name.to_uppercase() + " [OpenVMM]");

                console_relay::launch_console(
                    app.or_else(openvmm_terminal_app).as_deref(),
                    &path,
                    ConsoleLaunchOptions {
                        window_title: Some(window_title),
                    },
                )
                .context("failed to launch console")?;

                Some(config)
            }
        })
    };

    let mut vmbus_devices = Vec::new();

    let com_debugger_mode = [
        opt.com1.as_ref().is_some_and(|c| c.debugger_mode),
        opt.com2.as_ref().is_some_and(|c| c.debugger_mode),
        opt.com3.as_ref().is_some_and(|c| c.debugger_mode),
        opt.com4.as_ref().is_some_and(|c| c.debugger_mode),
    ];

    if is_microvm
        && (opt.com1.is_some()
            || opt.com2.is_some()
            || opt.com3.is_some()
            || opt.com4.is_some()
            || opt.vmbus_com1_serial.is_some()
            || opt.vmbus_com2_serial.is_some()
            || opt.debugcon.is_some())
    {
        bail!("microVM does not expose UART, debugcon, or VMBus serial");
    }

    let microvm_console = if is_microvm {
        effective_microvm_console(opt.virtio_console.as_ref(), restore_machine_contract)?
    } else {
        None
    };
    if let Some((_, _, attachment)) = &microvm_console
        && let Some(snapshot_dir) = opt
            .restore_snapshot
            .as_deref()
            .or(opt.snapshot_destination.as_deref())
    {
        validate_microvm_console_attachment_namespace(attachment, snapshot_dir)?;
    }

    let microvm_portb_cfg = if is_microvm {
        let backend = if microvm_console
            .as_ref()
            .is_some_and(|(config, _, _)| matches!(config, SerialConfigCli::Console))
        {
            SerialConfigCli::Stderr
        } else {
            SerialConfigCli::Console
        };
        setup_serial("microvm-portb", backend, "hvc0")?
    } else {
        None
    };

    let serial0_cfg = if is_microvm {
        None
    } else {
        setup_serial(
            "com1",
            opt.com1
                .clone()
                .map_or(SerialConfigCli::Console, |c| c.backend),
            if cfg!(guest_arch = "x86_64") {
                "ttyS0"
            } else {
                "ttyAMA0"
            },
        )?
    };
    let serial1_cfg = setup_serial(
        "com2",
        opt.com2
            .clone()
            .map_or(SerialConfigCli::None, |c| c.backend),
        if cfg!(guest_arch = "x86_64") {
            "ttyS1"
        } else {
            "ttyAMA1"
        },
    )?;
    let serial2_cfg = setup_serial(
        "com3",
        opt.com3
            .clone()
            .map_or(SerialConfigCli::None, |c| c.backend),
        if cfg!(guest_arch = "x86_64") {
            "ttyS2"
        } else {
            "ttyAMA2"
        },
    )?;
    let serial3_cfg = setup_serial(
        "com4",
        opt.com4
            .clone()
            .map_or(SerialConfigCli::None, |c| c.backend),
        if cfg!(guest_arch = "x86_64") {
            "ttyS3"
        } else {
            "ttyAMA3"
        },
    )?;
    let with_vmbus_com1_serial = if let Some(vmbus_com1_cfg) = setup_serial(
        "vmbus_com1",
        opt.vmbus_com1_serial
            .clone()
            .unwrap_or(SerialConfigCli::None),
        "vmbus_com1",
    )? {
        vmbus_devices.push((
            openhcl_vtl,
            VmbusSerialDeviceHandle {
                port: VmbusSerialPort::Com1,
                backend: vmbus_com1_cfg,
            }
            .into_resource(),
        ));
        true
    } else {
        false
    };
    let with_vmbus_com2_serial = if let Some(vmbus_com2_cfg) = setup_serial(
        "vmbus_com2",
        opt.vmbus_com2_serial
            .clone()
            .unwrap_or(SerialConfigCli::None),
        "vmbus_com2",
    )? {
        vmbus_devices.push((
            openhcl_vtl,
            VmbusSerialDeviceHandle {
                port: VmbusSerialPort::Com2,
                backend: vmbus_com2_cfg,
            }
            .into_resource(),
        ));
        true
    } else {
        false
    };
    let debugcon_cfg = setup_serial(
        "debugcon",
        opt.debugcon
            .clone()
            .map(|cfg| cfg.serial)
            .unwrap_or(SerialConfigCli::None),
        "debugcon",
    )?;

    let virtio_console_config = if is_microvm {
        microvm_console
            .as_ref()
            .map(|(config, _, _)| config.clone())
    } else {
        opt.virtio_console.clone()
    };
    let (virtio_console_backend, microvm_console_socket_cleanup) =
        if let Some(serial_cfg) = virtio_console_config {
            if is_microvm {
                match serial_cfg {
                    SerialConfigCli::Pipe(path) => {
                        let backend =
                            serial_io::bind_serial_without_cleanup(&path).with_context(|| {
                                format!(
                                    "failed to bind microVM virtio console listener {}",
                                    path.display()
                                )
                            })?;
                        let cleanup = microvm_console_socket_cleanup(path)?;
                        (Some(backend), cleanup)
                    }
                    SerialConfigCli::Tcp(address) => {
                        (Some(serial_io::bind_tcp_serial(&address)?), None)
                    }
                    SerialConfigCli::ConnectPipe(path) => (
                        Some(
                            serial_io::connect_serial_with_timeout(
                                &path,
                                Duration::from_millis(
                                    openvmm_defs::config::MICROVM_CONSOLE_RECONNECT_TIMEOUT_MS,
                                ),
                            )
                            .with_context(|| {
                                format!(
                                    "failed to reconnect microVM virtio console client {}",
                                    path.display()
                                )
                            })?,
                        ),
                        None,
                    ),
                    SerialConfigCli::ConnectTcp(address) => (
                        Some(serial_io::connect_tcp_serial(
                            &address,
                            Duration::from_millis(
                                openvmm_defs::config::MICROVM_CONSOLE_RECONNECT_TIMEOUT_MS,
                            ),
                        )?),
                        None,
                    ),
                    SerialConfigCli::Console => (
                        setup_serial("virtio-console", SerialConfigCli::Console, "hvc1")?,
                        None,
                    ),
                    SerialConfigCli::None => {
                        (Some(DisconnectedSerialBackendHandle.into_resource()), None)
                    }
                    _ => unreachable!("microVM console backend was validated as an attachment"),
                }
            } else {
                (setup_serial("virtio-console", serial_cfg, "hvc0")?, None)
            }
        } else {
            (None, None)
        };

    let mut resources = VmResources {
        microvm_console_attachment: microvm_console
            .as_ref()
            .map(|(_, _, attachment)| attachment.clone()),
        microvm_console_socket_cleanup,
        microvm_network_attachment,
        microvm_egress_policy: microvm_egress_policy.clone(),
        microvm_filesystem_attachment,
        microvm_filesystem_root_path: effective_microvm_filesystem
            .as_ref()
            .map(|filesystem| PathBuf::from(&filesystem.root_path)),
        ..Default::default()
    };
    let mut console_str = "";
    if let Some(ConsoleState { device, input }) = console_state.into_inner() {
        resources.console_in = Some(input);
        console_str = device;
    }

    if opt.shared_memory {
        tracing::warn!("--shared-memory/-M flag has no effect and will be removed");
    }
    if opt.deprecated_prefetch {
        tracing::warn!("--prefetch is deprecated; use --memory prefetch=on");
    }
    if opt.deprecated_private_memory {
        tracing::warn!("--private-memory is deprecated; use --memory shared=off");
    }
    if opt.deprecated_thp {
        tracing::warn!("--thp is deprecated; use --memory shared=off,thp=on");
    }
    if opt.deprecated_memory_backing_file.is_some() {
        tracing::warn!("--memory-backing-file is deprecated; use --memory file=<path>");
    }

    opt.validate_memory_options()?;

    const MAX_PROCESSOR_COUNT: u32 = 1024;

    if opt.processors == 0 || opt.processors > MAX_PROCESSOR_COUNT {
        bail!("invalid proc count: {}", opt.processors);
    }

    // Total SCSI channel count should not exceed the processor count
    // (at most, one channel per VP).
    if opt.scsi_sub_channels > (MAX_PROCESSOR_COUNT - 1) as u16 {
        bail!(
            "invalid SCSI sub-channel count: requested {}, max {}",
            opt.scsi_sub_channels,
            MAX_PROCESSOR_COUNT - 1
        );
    }

    let with_get = opt.get || (opt.vtl2 && !opt.no_get);

    let mut storage = storage_builder::StorageBuilder::new(with_get.then_some(openhcl_vtl));

    // Register named controllers first, so that --disk on=<name>
    // references can be resolved.
    for ctrl in &opt.nvme_pci {
        let transport = match &ctrl.transport {
            cli_args::NvmeControllerTransport::Pcie(port) => {
                storage_builder::NvmeControllerTransport::Pcie(port.clone())
            }
            cli_args::NvmeControllerTransport::Vpci(guid) => {
                let guid = guid.unwrap_or_else(|| storage_builder::deterministic_guid(&ctrl.id));
                storage_builder::NvmeControllerTransport::Vpci(guid)
            }
        };
        storage.add_nvme_controller(ctrl.id.clone(), ctrl.vtl, transport, None)?;
    }

    for ctrl in &opt.vmbus_scsi {
        let instance_id = storage_builder::deterministic_guid(&ctrl.id);
        storage.add_scsi_controller(ctrl.id.clone(), ctrl.vtl, instance_id, ctrl.sub_channels)?;
    }

    for ctrl in &opt.openhcl_controller {
        let controller_type = match ctrl.controller_type {
            cli_args::OpenhclControllerType::Scsi => storage_builder::OpenhclControllerType::Scsi,
            cli_args::OpenhclControllerType::Nvme => storage_builder::OpenhclControllerType::Nvme,
        };
        let instance_id = ctrl
            .guid
            .unwrap_or_else(|| storage_builder::deterministic_guid(&ctrl.id));
        storage.add_openhcl_controller(ctrl.id.clone(), controller_type, instance_id)?;
    }

    for &cli_args::DiskCli {
        vtl,
        ref kind,
        read_only,
        is_dvd,
        underhill,
        ref pcie_port,
        ref controller,
        nsid,
        lun,
        ref relay,
    } in &opt.disk
    {
        if controller.is_none() && underhill.is_none() && relay.is_none() {
            tracing::warn!(
                "--disk without `on` is deprecated; \
                 use --vmbus-scsi and --disk on=<name> instead"
            );
        }

        let relay_target = relay
            .as_ref()
            .map(|(name, loc)| storage_builder::RelayTarget {
                controller: name.clone(),
                location: *loc,
            });

        let target = if let Some(name) = controller {
            if pcie_port.is_some() {
                anyhow::bail!("`on` is incompatible with `pcie_port` on `--disk`");
            }
            storage_builder::DiskLocation::Named {
                controller: name.clone(),
                nsid,
                lun,
            }
        } else if pcie_port.is_some() {
            anyhow::bail!("`--disk` is incompatible with `pcie_port` without `controller`");
        } else {
            if opt.no_vmbus {
                anyhow::bail!(
                    "`--disk` without `on=` attaches to the default VMBus SCSI controller and \
                     cannot be used with `--no-vmbus`; use `on=<name>` to attach to a named controller"
                );
            }
            storage_builder::DiskLocation::Scsi(None)
        };

        storage
            .add(
                vtl,
                underhill,
                relay_target,
                target,
                kind,
                is_dvd,
                read_only,
            )
            .await?;
    }

    for block in &opt.microvm_sandbox_block {
        let disk = &block.disk;
        anyhow::ensure!(
            disk.vtl == DeviceVtl::Vtl0
                && !disk.is_dvd
                && disk.underhill.is_none()
                && disk.pcie_port.is_none()
                && disk.controller.is_none()
                && disk.nsid.is_none()
                && disk.lun.is_none()
                && disk.relay.is_none(),
            "--microvm-sandbox-block accepts only a plain VTL0 disk backend"
        );
        storage
            .add_microvm_sandbox_block(
                block.role,
                &disk.kind,
                disk.read_only,
                opt.snapshot_destination.is_some() || opt.restore_snapshot.is_some(),
            )
            .await?;
    }

    for &cli_args::IdeDiskCli {
        ref kind,
        read_only,
        channel,
        device,
        is_dvd,
    } in &opt.ide
    {
        storage
            .add(
                DeviceVtl::Vtl0,
                None,
                None,
                storage_builder::DiskLocation::Ide(channel, device),
                kind,
                is_dvd,
                read_only,
            )
            .await?;
    }

    if !opt.nvme.is_empty() {
        tracing::warn!("--nvme is deprecated; use --nvme-pci and --disk on=<name> instead");

        // Pre-register implicit PCIe controllers for unique port names.
        let mut registered_ports = std::collections::BTreeSet::new();
        for disk in &opt.nvme {
            if let Some(port) = &disk.pcie_port {
                if registered_ports.insert(port.clone()) {
                    storage.add_nvme_controller(
                        port.clone(),
                        DeviceVtl::Vtl0,
                        storage_builder::NvmeControllerTransport::Pcie(port.clone()),
                        None,
                    ).with_context(|| format!(
                        "legacy --nvme flag conflicts with an explicit controller named '{port}'; \
                         use --nvme-pci and --disk on=<name> instead"
                    ))?;
                }
            }
        }
    }

    for &cli_args::DiskCli {
        vtl,
        ref kind,
        read_only,
        is_dvd,
        underhill,
        ref pcie_port,
        controller: _,
        nsid: _,
        lun: _,
        relay: _,
    } in &opt.nvme
    {
        let target = if let Some(port) = pcie_port {
            storage_builder::DiskLocation::Named {
                controller: port.clone(),
                nsid: None,
                lun: None,
            }
        } else {
            storage_builder::DiskLocation::Nvme(None)
        };
        storage
            .add(vtl, underhill, None, target, kind, is_dvd, read_only)
            .await?;
    }

    for &cli_args::DiskCli {
        vtl,
        ref kind,
        read_only,
        is_dvd,
        ref underhill,
        ref pcie_port,
        controller: _,
        nsid: _,
        lun: _,
        relay: _,
    } in &opt.virtio_blk
    {
        if underhill.is_some() {
            anyhow::bail!("underhill not supported with virtio-blk");
        }
        storage
            .add(
                vtl,
                None,
                None,
                storage_builder::DiskLocation::VirtioBlk(pcie_port.clone()),
                kind,
                is_dvd,
                read_only,
            )
            .await?;
    }

    let mut floppy_disks = Vec::new();
    for disk in &opt.floppy {
        let &cli_args::FloppyDiskCli {
            ref kind,
            read_only,
        } = disk;
        floppy_disks.push(FloppyDiskConfig {
            disk_type: disk_open(kind, read_only).await?,
            read_only,
        });
    }

    let mut vpci_mana_nics = [(); 3].map(|()| None);
    let mut pcie_mana_nics = BTreeMap::<String, GdmaDeviceHandle>::new();
    let mut underhill_nics = Vec::new();
    let mut vpci_devices = Vec::new();

    let mut nic_index = 0;
    for cli_cfg in &opt.net {
        if is_microvm {
            continue;
        }
        if cli_cfg.pcie_port.is_some() {
            anyhow::bail!("`--net` does not support PCIe");
        }
        let vport = parse_endpoint(cli_cfg, &mut nic_index, &mut resources)?;
        if cli_cfg.underhill {
            if !opt.no_alias_map {
                anyhow::bail!("must specify --no-alias-map to offer NICs to VTL2");
            }
            let mana = vpci_mana_nics[openhcl_vtl as usize].get_or_insert_with(|| {
                let vpci_instance_id = Guid::new_random();
                underhill_nics.push(vtl2_settings_proto::NicDeviceLegacy {
                    instance_id: vpci_instance_id.to_string(),
                    subordinate_instance_id: None,
                    max_sub_channels: None,
                });
                (vpci_instance_id, GdmaDeviceHandle { vports: Vec::new() })
            });
            mana.1.vports.push(VportDefinition {
                mac_address: vport.mac_address,
                endpoint: vport.endpoint,
            });
        } else {
            vmbus_devices.push(vport.into_netvsp_handle());
        }
    }

    if opt.nic {
        let nic_config = parse_endpoint(
            &NicConfigCli {
                vtl: DeviceVtl::Vtl0,
                endpoint: EndpointConfigCli::Consomme {
                    cidr: None,
                    host_fwd: Vec::new(),
                },
                max_queues: None,
                underhill: false,
                pcie_port: None,
            },
            &mut nic_index,
            &mut resources,
        )?;
        vmbus_devices.push(nic_config.into_netvsp_handle());
    }

    // Build initial PCIe devices list from CLI options. Storage devices
    // (e.g., NVMe controllers on PCIe ports) are added later by storage_builder.
    let mut pcie_devices = Vec::new();
    for (index, cli_cfg) in opt.pcie_remote.iter().enumerate() {
        tracing::info!(
            port_name = %cli_cfg.port_name,
            socket_addr = ?cli_cfg.socket_addr,
            "instantiating PCIe remote device"
        );

        // Generate a deterministic instance ID based on index
        const PCIE_REMOTE_BASE_INSTANCE_ID: Guid =
            guid::guid!("28ed784d-c059-429f-9d9a-46bea02562c0");
        let instance_id = Guid {
            data1: index as u32,
            ..PCIE_REMOTE_BASE_INSTANCE_ID
        };

        pcie_devices.push(PcieDeviceConfig {
            port_name: cli_cfg.port_name.clone(),
            resource: pcie_remote_resources::PcieRemoteHandle {
                instance_id,
                socket_addr: cli_cfg.socket_addr.clone(),
                hu: cli_cfg.hu,
                controller: cli_cfg.controller,
            }
            .into_resource(),
        });
    }

    #[cfg(windows)]
    let mut kernel_vmnics = Vec::new();
    #[cfg(windows)]
    for (index, switch_id) in opt.kernel_vmnic.iter().enumerate() {
        // Pick a random MAC address.
        let mut mac_address = [0x00, 0x15, 0x5D, 0, 0, 0];
        getrandom::fill(&mut mac_address[3..]).expect("rng failure");

        // Pick a fixed instance ID based on the index.
        const BASE_INSTANCE_ID: Guid = guid::guid!("00000000-435d-11ee-9f59-00155d5016fc");
        let instance_id = Guid {
            data1: index as u32,
            ..BASE_INSTANCE_ID
        };

        let switch_id = if switch_id == "default" {
            None
        } else {
            Some(switch_id.as_str())
        };
        let (port_id, port) = new_switch_port(switch_id)?;
        resources.switch_ports.push(port);

        kernel_vmnics.push(openvmm_defs::config::KernelVmNicConfig {
            instance_id,
            mac_address: mac_address.into(),
            switch_port_id: port_id,
        });
    }

    for vport in &opt.mana {
        let vport = parse_endpoint(vport, &mut nic_index, &mut resources)?;
        let vport_array = match (vport.vtl as usize, vport.pcie_port) {
            (vtl, None) => {
                &mut vpci_mana_nics[vtl]
                    .get_or_insert_with(|| {
                        (Guid::new_random(), GdmaDeviceHandle { vports: Vec::new() })
                    })
                    .1
                    .vports
            }
            (0, Some(pcie_port)) => {
                &mut pcie_mana_nics
                    .entry(pcie_port)
                    .or_insert(GdmaDeviceHandle { vports: Vec::new() })
                    .vports
            }
            _ => anyhow::bail!("PCIe NICs only supported to VTL0"),
        };
        vport_array.push(VportDefinition {
            mac_address: vport.mac_address,
            endpoint: vport.endpoint,
        });
    }

    vpci_devices.extend(
        vpci_mana_nics
            .into_iter()
            .enumerate()
            .filter_map(|(vtl, nic)| {
                nic.map(|(instance_id, handle)| VpciDeviceConfig {
                    vtl: match vtl {
                        0 => DeviceVtl::Vtl0,
                        1 => DeviceVtl::Vtl1,
                        2 => DeviceVtl::Vtl2,
                        _ => unreachable!(),
                    },
                    instance_id,
                    resource: handle.into_resource(),
                    vnode: None,
                })
            }),
    );

    pcie_devices.extend(
        pcie_mana_nics
            .into_iter()
            .map(|(pcie_port, handle)| PcieDeviceConfig {
                port_name: pcie_port,
                resource: handle.into_resource(),
            }),
    );

    for cxl_test in &opt.cxl_test {
        pcie_devices.push(PcieDeviceConfig {
            port_name: cxl_test.pcie_port.clone(),
            resource: CxlTestDeviceHandle {
                hdm_size_bytes: cxl_test.hdm_size,
            }
            .into_resource(),
        });
    }

    #[cfg(guest_arch = "aarch64")]
    let arch = MachineArch::Aarch64;
    #[cfg(guest_arch = "x86_64")]
    let arch = MachineArch::X86_64;

    #[cfg(guest_arch = "x86_64")]
    anyhow::ensure!(
        opt.amd_iommu.is_empty() || opt.intel_vtd.is_empty(),
        "--amd-iommu and --intel-vtd cannot both be used in the same VM"
    );

    #[cfg(guest_arch = "x86_64")]
    let mut amd_iommu_names: std::collections::HashSet<&str> =
        opt.amd_iommu.iter().map(|s| s.as_str()).collect();
    #[cfg(guest_arch = "x86_64")]
    let mut vtd_names: std::collections::HashSet<&str> =
        opt.intel_vtd.iter().map(|s| s.as_str()).collect();

    // Map each `--smmu` entry to its root complex, rejecting duplicate `rc=`
    // entries up front. Entries are removed as they are matched to a root
    // complex below; any left over refer to unknown root complexes.
    #[cfg(guest_arch = "aarch64")]
    let mut smmu_names: std::collections::HashMap<&str, &cli_args::SmmuCli> = {
        let mut map = std::collections::HashMap::new();
        for s in &opt.smmu {
            if map.insert(s.rc_name.as_str(), s).is_some() {
                anyhow::bail!(
                    "--smmu specified multiple times for root complex '{}'",
                    s.rc_name
                );
            }
        }
        map
    };

    let mut pcie_root_complexes = Vec::new();
    for (i, rc_cli) in opt.pcie_root_complex.iter().enumerate() {
        let ports: Vec<PciePortConfig> = opt
            .pcie_root_port
            .iter()
            .filter(|port_cli| port_cli.root_complex_name == rc_cli.name)
            .map(|port_cli| PciePortConfig {
                name: port_cli.name.clone(),
                devfn: port_cli.devfn,
                hotplug: port_cli.hotplug,
                acs_capabilities_supported: port_cli.acs_capabilities_supported,
                cxl: port_cli.cxl,
                pasid: port_cli.pasid,
            })
            .collect();

        const ONE_MB: u64 = 1024 * 1024;
        // Keep all PCI windows 1MB-granular to match layout and downstream placement rules.
        let low_mmio_size = (rc_cli.low_mmio as u64).next_multiple_of(ONE_MB);
        let high_mmio_size = rc_cli
            .high_mmio
            .checked_next_multiple_of(ONE_MB)
            .context("high mmio rounding error")?;

        // Count CXL-capable ports under the root bus. If the root bus has CXL root ports, it needs CHBCR.
        let cxl_port_count = ports.iter().filter(|port| port.cxl).count() as u64;

        let cxl = if cxl_port_count != 0 {
            Some(RootComplexCxlConfig {
                hdm_size: rc_cli.hdm,
                hdm_window_restrictions: rc_cli.hdm_window_restrictions.bits(),
            })
        } else {
            None
        };
        pcie_root_complexes.push(PcieRootComplexConfig {
            index: i as u32,
            name: rc_cli.name.clone(),
            segment: rc_cli.segment,
            start_bus: rc_cli.start_bus,
            end_bus: rc_cli.end_bus,
            low_mmio: if let Some(base) = rc_cli.low_mmio_base {
                PcieMmioRangeConfig::Fixed(
                    memory_range::MemoryRange::try_new(base..base.wrapping_add(low_mmio_size))
                        .context("invalid low MMIO range")?,
                )
            } else {
                PcieMmioRangeConfig::Dynamic {
                    size: low_mmio_size,
                }
            },
            high_mmio: if let Some(base) = rc_cli.high_mmio_base {
                PcieMmioRangeConfig::Fixed(
                    memory_range::MemoryRange::try_new(base..base.wrapping_add(high_mmio_size))
                        .context("invalid high MMIO range")?,
                )
            } else {
                PcieMmioRangeConfig::Dynamic {
                    size: high_mmio_size,
                }
            },
            cxl,
            ports,
            #[cfg(guest_arch = "aarch64")]
            iommu: smmu_names.remove(rc_cli.name.as_str()).map(|s| {
                openvmm_defs::config::PcieIommuConfig::Smmu {
                    accel: s.accel,
                    oas: match s.oas {
                        cli_args::SmmuOasCli::Auto => openvmm_defs::config::SmmuOas::Auto,
                        cli_args::SmmuOasCli::Fixed(bits) => {
                            openvmm_defs::config::SmmuOas::Fixed(bits)
                        }
                    },
                }
            }),
            #[cfg(guest_arch = "x86_64")]
            iommu: if amd_iommu_names.remove(rc_cli.name.as_str()) {
                Some(openvmm_defs::config::PcieIommuConfig::AmdVi)
            } else if vtd_names.remove(rc_cli.name.as_str()) {
                Some(openvmm_defs::config::PcieIommuConfig::IntelVtd)
            } else {
                None
            },
            vnode: rc_cli.vnode,
            preserve_bars: rc_cli.preserve_bars,
        });
    }

    #[cfg(guest_arch = "aarch64")]
    if let Some(name) = smmu_names.into_keys().next() {
        anyhow::bail!("--smmu refers to unknown root complex '{name}'");
    }
    #[cfg(guest_arch = "x86_64")]
    if let Some(name) = amd_iommu_names.into_iter().next() {
        anyhow::bail!("--amd-iommu refers to unknown root complex '{name}'");
    }
    #[cfg(guest_arch = "x86_64")]
    if let Some(name) = vtd_names.into_iter().next() {
        anyhow::bail!("--intel-vtd refers to unknown root complex '{name}'");
    }

    let pcie_switches = build_switch_list(&opt.pcie_switch);
    let pcie_generic_initiators = opt
        .pcie_generic_initiator
        .iter()
        .map(|gi| openvmm_defs::config::PcieGenericInitiatorConfig {
            port_name: gi.port_name.clone(),
            node: gi.node,
        })
        .collect();
    #[cfg(target_os = "linux")]
    let vfio_pcie_devices: Vec<PcieDeviceConfig> = {
        use std::collections::HashMap;
        use vm_resource::IntoResource;

        // Process --iommu flags: open /dev/iommu for each declared context.
        let mut iommu_map: HashMap<String, std::fs::File> = HashMap::new();
        for iommu_cli in &opt.iommu {
            anyhow::ensure!(
                !iommu_map.contains_key(&iommu_cli.id),
                "duplicate --iommu id={}",
                iommu_cli.id
            );
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open("/dev/iommu")
                .context("failed to open /dev/iommu (is iommufd available?)")?;
            iommu_map.insert(iommu_cli.id.clone(), file);
        }

        opt.vfio
            .iter()
            .map(|cli_cfg| {
                let sysfs_path = Path::new("/sys/bus/pci/devices").join(&cli_cfg.pci_id);

                if let Some(iommu_id) = &cli_cfg.iommu {
                    // cdev + iommufd path
                    let iommufd = iommu_map.get(iommu_id).with_context(|| {
                        format!(
                            "--vfio device {} references iommu={iommu_id}, \
                             but no --iommu id={iommu_id} was specified",
                            cli_cfg.pci_id
                        )
                    })?;
                    // Clone the iommufd fd so the per-iommu manager can own it.
                    // The first device for a given iommu ID uses the cloned fd
                    // to create the IoasManager; subsequent devices reuse the
                    // existing manager and the cloned fd is dropped.
                    let iommufd = iommufd.try_clone().with_context(|| {
                        format!("failed to dup iommufd fd for iommu={iommu_id}")
                    })?;

                    // Open the cdev device node.
                    let vfio_dev_dir = sysfs_path.join("vfio-dev");
                    let entry = std::fs::read_dir(&vfio_dev_dir)
                        .with_context(|| {
                            format!(
                                "failed to read {}: is {} bound to vfio-pci?",
                                vfio_dev_dir.display(),
                                cli_cfg.pci_id
                            )
                        })?
                        .next()
                        .context("no vfio-dev entry found")?
                        .context("failed to read vfio-dev entry")?;
                    let dev_path = Path::new("/dev/vfio/devices").join(entry.file_name());
                    let cdev = std::fs::OpenOptions::new()
                        .read(true)
                        .write(true)
                        .open(&dev_path)
                        .with_context(|| format!("failed to open {}", dev_path.display()))?;

                    Ok(PcieDeviceConfig {
                        port_name: cli_cfg.port_name.clone(),
                        resource: vfio_assigned_device_resources::VfioCdevDeviceHandle {
                            pci_id: cli_cfg.pci_id.clone(),
                            cdev,
                            iommufd,
                            iommu_id: iommu_id.clone(),
                            bar_addresses: cli_cfg.bar_addresses,
                        }
                        .into_resource(),
                    })
                } else {
                    // Legacy group/container path
                    let iommu_group_link = std::fs::read_link(sysfs_path.join("iommu_group"))
                        .with_context(|| {
                            format!("failed to read IOMMU group for {}", cli_cfg.pci_id)
                        })?;
                    let group_id: u64 = iommu_group_link
                        .file_name()
                        .and_then(|s| s.to_str())
                        .context("invalid iommu_group symlink")?
                        .parse()
                        .context("failed to parse IOMMU group ID")?;
                    let group = std::fs::OpenOptions::new()
                        .read(true)
                        .write(true)
                        .open(format!("/dev/vfio/{group_id}"))
                        .with_context(|| format!("failed to open /dev/vfio/{group_id}"))?;

                    Ok(PcieDeviceConfig {
                        port_name: cli_cfg.port_name.clone(),
                        resource: vfio_assigned_device_resources::VfioDeviceHandle {
                            pci_id: cli_cfg.pci_id.clone(),
                            group,
                            bar_addresses: cli_cfg.bar_addresses,
                        }
                        .into_resource(),
                    })
                }
            })
            .collect::<anyhow::Result<Vec<_>>>()?
    };

    #[cfg(windows)]
    let vpci_resources: Vec<_> = opt
        .device
        .iter()
        .map(|path| -> anyhow::Result<_> {
            Ok(virt_whp::device::DeviceHandle(
                whp::VpciResource::new(
                    None,
                    Default::default(),
                    &whp::VpciResourceDescriptor::Sriov(path, 0, 0),
                )
                .with_context(|| format!("opening PCI device {}", path))?,
            ))
        })
        .collect::<Result<_, _>>()?;

    // Create a vmbusproxy handle if needed by any devices.
    #[cfg(windows)]
    let vmbusproxy_handle = if !kernel_vmnics.is_empty() {
        Some(vmbus_proxy::ProxyHandle::new().context("failed to open vmbusproxy handle")?)
    } else {
        None
    };

    let framebuffer = if opt.gfx || opt.vtl2_gfx || opt.vnc.vnc || opt.pcat {
        let vram = alloc_shared_memory(FRAMEBUFFER_SIZE, "vram")?;
        let (fb, fba) =
            framebuffer::framebuffer(vram, FRAMEBUFFER_SIZE, 0).context("creating framebuffer")?;
        resources.framebuffer_access = Some(fba);
        Some(fb)
    } else {
        None
    };

    let load_mode;
    let with_hv;

    let any_serial_configured = serial0_cfg.is_some()
        || serial1_cfg.is_some()
        || serial2_cfg.is_some()
        || serial3_cfg.is_some();

    let has_com3 = serial2_cfg.is_some();

    let mut chipset = VmManifestBuilder::new(base_chipset_type(opt), arch);

    if framebuffer.is_some() {
        chipset = chipset.with_framebuffer();
    }
    if opt.guest_watchdog {
        chipset = chipset.with_guest_watchdog();
    }
    if any_serial_configured {
        chipset = chipset.with_serial([serial0_cfg, serial1_cfg, serial2_cfg, serial3_cfg]);
    }
    chipset = chipset.with_serial_debugger_mode(com_debugger_mode);
    if opt.battery {
        let (tx, rx) = mesh::channel();
        tx.send(HostBatteryUpdate::default_present());
        chipset = chipset.with_battery(rx);
    }
    if opt.no_vmbus {
        chipset = chipset.without_vmbus();
    }
    if let Some(cfg) = &opt.debugcon {
        chipset = chipset.with_debugcon(
            debugcon_cfg.unwrap_or_else(|| DisconnectedSerialBackendHandle.into_resource()),
            cfg.port,
        );
    }

    let (base_template, custom_uefi_json) = {
        #[cfg(guest_arch = "aarch64")]
        use firmware_uefi_resources::aarch64_secure_boot_templates as secure_boot_templates;
        #[cfg(guest_arch = "x86_64")]
        use firmware_uefi_resources::x64_secure_boot_templates as secure_boot_templates;
        let base_template = opt.secure_boot_template.map(|template| match template {
            SecureBootTemplateCli::Windows => secure_boot_templates::microsoft_windows(),
            SecureBootTemplateCli::UefiCa => secure_boot_templates::microsoft_uefi_ca(),
        });

        // TODO: fallback to VMGS read if no command line flag was given

        let custom_uefi_json = match &opt.custom_uefi_json {
            Some(file) => Some(
                fs_err::read(file)
                    .context("opening custom uefi json file")?
                    .into(),
            ),
            None => None,
        };

        (base_template, custom_uefi_json)
    };

    if (opt.uefi && opt.igvm.is_none() && !opt.pcat)
        || matches!(opt.igvm_personality, Some(IgvmPersonalityCli::Uefi))
    {
        let log_level = match opt.efi_diagnostics_log_level.unwrap_or_default() {
            EfiDiagnosticsLogLevelCli::Default => firmware_uefi_resources::LogLevel::make_default(),
            EfiDiagnosticsLogLevelCli::Info => firmware_uefi_resources::LogLevel::make_info(),
            EfiDiagnosticsLogLevelCli::Full => firmware_uefi_resources::LogLevel::make_full(),
        };
        let nvram_storage = if opt.vmgs.is_some() {
            VmgsFileHandle::new(vmgs_format::FileId::BIOS_NVRAM, true).into_resource()
        } else {
            EphemeralNonVolatileStoreHandle.into_resource()
        };
        chipset = chipset.with_uefi(vm_manifest_builder::UefiManifest::new(
            arch,
            base_template,
            custom_uefi_json,
            opt.secure_boot,
            log_level,
            None,
            nvram_storage,
            None,
        ));
    }

    // Build the SMBIOS config once, up front, so that UEFI and Linux direct
    // boot share a single source for the VM's BIOS GUID / system UUID. The TPM
    // also keys off this GUID.
    let smbios = Box::new(smbios_config_from_cli(&opt.smbios)?);
    let bios_guid = smbios.system.uuid;

    // Capture the SMBIOS config for the OpenHCL/GED path before `smbios` is
    // potentially moved into a non-VTL2 LoadMode below. The GED forwards only
    // the system identity to the paravisor and fails closed on BIOS overrides
    // it cannot honor, so it is delivered as the shared `SmbiosConfig`.
    let ged_smbios = (*smbios).clone();

    let layout_config = chipset.layout_config();
    let VmChipsetResult {
        chipset,
        mut chipset_devices,
        pci_chipset_devices,
        isa_dma_controller,
        capabilities,
    } = chipset
        .build()
        .context("failed to build chipset configuration")?;

    if let Some(io) = microvm_portb_cfg {
        let (generation_id, restore_entropy) =
            if opt.restore_entropy || restore_memory_target_requested {
                fresh_microvm_restore_packet(
                    opt.restore_processors,
                    restore_memory_target_requested,
                    restore_memory_ranges,
                )?
            } else {
                (fresh_microvm_generation_id()?, Vec::new())
            };
        chipset_devices.push(ChipsetDeviceHandle {
            name: MicrovmPortbHandle::ID.to_owned(),
            resource: MicrovmPortbHandle {
                io,
                generation_id,
                restore_entropy,
            }
            .into_resource(),
        });
        chipset_devices.push(ChipsetDeviceHandle {
            name: MicrovmShutdownHandle::ID.to_owned(),
            resource: MicrovmShutdownHandle.into_resource(),
        });
        chipset_devices.push(ChipsetDeviceHandle {
            name: MicrovmSnapshotRequestHandle::ID.to_owned(),
            resource: {
                let (notify, requests) = mesh::channel();
                resources.microvm_snapshot_requests = Some(requests);
                MicrovmSnapshotRequestHandle {
                    notify: Some(notify),
                    input_gate_timeout: Duration::from_millis(opt.snapshot_quiesce_timeout_ms),
                }
                .into_resource()
            },
        });
    }

    if is_microvm {
        if arch != MachineArch::X86_64 {
            bail!("the microVM profile requires an x86-64 guest");
        }
        if opt.igvm.is_some() || opt.pcat || opt.uefi {
            bail!("the microVM profile requires Xen PVH direct boot");
        }

        let (kernel, initrd, cmdline) = if let Some(contract) = restore_machine_contract {
            (
                tempfile::tempfile().context("failed to create inert restore kernel handle")?,
                None,
                contract.effective_command_line.clone(),
            )
        } else {
            let kernel = fs_err::File::open(
                (opt.kernel.0)
                    .as_ref()
                    .context("must provide a PVH kernel when using --machine microvm")?,
            )
            .context("failed to open PVH kernel")?;
            let initrd = (opt.initrd.0)
                .as_ref()
                .map(fs_err::File::open)
                .transpose()
                .context("failed to open PVH initrd")?;
            (
                kernel.into(),
                initrd.map(Into::into),
                build_microvm_command_line(&opt.cmdline, microvm_console.is_some())?,
            )
        };

        load_mode = LoadMode::Pvh {
            kernel,
            initrd,
            cmdline,
        };
        with_hv = false;
    } else if opt.restore_snapshot.is_some() {
        // Snapshot restore: skip firmware loading entirely. Device state and
        // memory come from the snapshot directory.
        load_mode = LoadMode::None;
        with_hv = true;
    } else if let Some(path) = &opt.igvm {
        let file = fs_err::File::open(path)
            .context("failed to open igvm file")?
            .into();
        let cmdline = opt.cmdline.join(" ");
        with_hv = match opt.igvm_personality {
            None | Some(IgvmPersonalityCli::Uefi) => true,
            Some(IgvmPersonalityCli::LinuxDirect) => opt.hv,
        };

        load_mode = LoadMode::Igvm {
            file,
            cmdline,
            vtl2_base_address: if opt.vtl2 {
                opt.igvm_vtl2_relocation_type
            } else {
                Vtl2BaseAddressType::File
            },
            com_serial: has_com3.then(|| SerialInformation {
                io_port: ComPort::Com3.io_port(),
                irq: ComPort::Com3.irq().into(),
            }),
        };

        // An IGVM launch carries no SMBIOS field of its own; the identity is
        // only delivered over the GET/GED channel, which is absent here. Reject
        // overrides that would otherwise be silently dropped.
        let smbios_requested = !opt.smbios.is_empty();
        let smbios_delivered_via_get = with_get && with_hv;
        if smbios_requested && !smbios_delivered_via_get {
            anyhow::bail!(
                "--smbios is not supported for IGVM launches without an OpenHCL GET channel"
            );
        }
    } else if opt.pcat {
        // Emit a nice error early instead of complaining about missing firmware.
        if arch != MachineArch::X86_64 {
            anyhow::bail!("pcat not supported on this architecture");
        }
        with_hv = true;

        let firmware = openvmm_pcat_locator::find_pcat_bios(opt.pcat_firmware.as_deref())?;
        load_mode = LoadMode::Pcat {
            firmware,
            boot_order: opt
                .pcat_boot_order
                .map(|x| x.0)
                .unwrap_or(DEFAULT_PCAT_BOOT_ORDER),
            hibernation_enabled: opt.hibernation,
            smbios,
        };
    } else if opt.uefi {
        use openvmm_defs::config::UefiConsoleMode;

        if opt.no_hv && cfg!(guest_arch = "x86_64") {
            anyhow::bail!("--no-hv is not supported on x86_64");
        }

        with_hv = !opt.no_hv;

        let firmware = fs_err::File::open(
            (opt.uefi_firmware.0)
                .as_ref()
                .context("must provide uefi firmware when booting with uefi")?,
        )
        .context("failed to open uefi firmware")?;

        // TODO: It would be better to default memory protections to on, but currently Linux does not boot via UEFI due to what
        //       appears to be a GRUB memory protection fault. Memory protections are therefore only enabled if configured.
        load_mode = LoadMode::Uefi {
            firmware: firmware.into(),
            enable_debugging: opt.uefi_debug,
            enable_memory_protections: opt.uefi_enable_memory_protections,
            disable_frontpage: opt.disable_frontpage,
            enable_tpm: opt.tpm.is_some(),
            enable_battery: opt.battery,
            enable_serial: any_serial_configured,
            enable_vpci_boot: false,
            uefi_console_mode: opt.uefi_console_mode.map(|m| match m {
                UefiConsoleModeCli::Default => UefiConsoleMode::Default,
                UefiConsoleModeCli::Com1 => UefiConsoleMode::Com1,
                UefiConsoleModeCli::Com2 => UefiConsoleMode::Com2,
                UefiConsoleModeCli::None => UefiConsoleMode::None,
            }),
            default_boot_always_attempt: opt.default_boot_always_attempt,
            smbios,
            enable_vmbus: !opt.no_vmbus,
            force_dma_bounce: opt.uefi_force_dma_bounce,
            enable_hv: !opt.no_hv,
            hibernation_enabled: opt.hibernation,
        };
    } else {
        // Linux Direct
        let mut cmdline = "panic=-1 debug".to_string();

        with_hv = opt.hv;
        if with_hv && opt.pcie_root_complex.is_empty() {
            cmdline += " pci=off";
        }

        if !console_str.is_empty() {
            let _ = write!(&mut cmdline, " console={}", console_str);
        }

        if opt.gfx {
            cmdline += " console=tty";
        }
        for extra in &opt.cmdline {
            let _ = write!(&mut cmdline, " {}", extra);
        }

        let kernel = fs_err::File::open(
            (opt.kernel.0)
                .as_ref()
                .context("must provide kernel when booting with linux direct")?,
        )
        .context("failed to open kernel")?;
        let initrd = (opt.initrd.0)
            .as_ref()
            .map(fs_err::File::open)
            .transpose()
            .context("failed to open initrd")?;

        load_mode = LoadMode::Linux {
            kernel: kernel.into(),
            initrd: initrd.map(Into::into),
            cmdline,
            enable_serial: any_serial_configured,
            isolation: if matches!(opt.isolation, Some(cli_args::IsolationCli::Snp)) {
                openvmm_defs::config::LinuxIsolationConfig::Snp {
                    restricted_injection: opt.snp_restricted_injection,
                }
            } else {
                openvmm_defs::config::LinuxIsolationConfig::None
            },
            boot_mode: if opt.device_tree {
                openvmm_defs::config::LinuxDirectBootMode::DeviceTree
            } else {
                openvmm_defs::config::LinuxDirectBootMode::Acpi
            },
            smbios,
        };
    }

    let mut vmgs = if is_microvm {
        if opt.vmgs.is_some() {
            bail!("microVM does not support VMGS");
        }
        None
    } else {
        Some(if let Some(VmgsCli { kind, provision }) = &opt.vmgs {
            let disk = VmgsDisk {
                disk: disk_open(kind, false)
                    .await
                    .context("failed to open vmgs disk")?,
                encryption_policy: if opt.test_gsp_by_id {
                    GuestStateEncryptionPolicy::GspById(true)
                } else {
                    GuestStateEncryptionPolicy::None(true)
                },
            };
            match provision {
                ProvisionVmgs::OnEmpty => VmgsResource::Disk(disk),
                ProvisionVmgs::OnFailure => VmgsResource::ReprovisionOnFailure(disk),
                ProvisionVmgs::True => VmgsResource::Reprovision(disk),
            }
        } else {
            VmgsResource::Ephemeral
        })
    };

    if with_get && with_hv {
        let has_vtl0_nvme = storage.has_vtl0_nvme();
        let vtl2_settings = vtl2_settings_proto::Vtl2Settings {
            version: vtl2_settings_proto::vtl2_settings_base::Version::V1.into(),
            fixed: Some(Default::default()),
            dynamic: Some(vtl2_settings_proto::Vtl2SettingsDynamic {
                storage_controllers: storage.build_openhcl_settings(opt.vmbus_redirect),
                nic_devices: underhill_nics,
            }),
            namespace_settings: Vec::default(),
        };

        // Cache the VTL2 settings for later modification via the interactive console.
        resources.vtl2_settings = Some(vtl2_settings.clone());

        let (send, guest_request_recv) = mesh::channel();
        resources.ged_rpc = Some(send);

        let vmgs = vmgs.take().unwrap();

        vmbus_devices.extend([
            (
                openhcl_vtl,
                get_resources::gel::GuestEmulationLogHandle.into_resource(),
            ),
            (
                openhcl_vtl,
                get_resources::ged::GuestEmulationDeviceHandle {
                    firmware: if opt.pcat {
                        get_resources::ged::GuestFirmwareConfig::Pcat {
                            boot_order: opt
                                .pcat_boot_order
                                .map_or(DEFAULT_PCAT_BOOT_ORDER, |x| x.0)
                                .map(|x| match x {
                                    openvmm_defs::config::PcatBootDevice::Floppy => {
                                        get_resources::ged::PcatBootDevice::Floppy
                                    }
                                    openvmm_defs::config::PcatBootDevice::HardDrive => {
                                        get_resources::ged::PcatBootDevice::HardDrive
                                    }
                                    openvmm_defs::config::PcatBootDevice::Optical => {
                                        get_resources::ged::PcatBootDevice::Optical
                                    }
                                    openvmm_defs::config::PcatBootDevice::Network => {
                                        get_resources::ged::PcatBootDevice::Network
                                    }
                                }),
                        }
                    } else {
                        use get_resources::ged::UefiConsoleMode;

                        get_resources::ged::GuestFirmwareConfig::Uefi {
                            enable_vpci_boot: has_vtl0_nvme,
                            firmware_debug: opt.uefi_debug,
                            disable_frontpage: opt.disable_frontpage,
                            console_mode: match opt.uefi_console_mode.unwrap_or(UefiConsoleModeCli::Default) {
                                UefiConsoleModeCli::Default => UefiConsoleMode::Default,
                                UefiConsoleModeCli::Com1 => UefiConsoleMode::COM1,
                                UefiConsoleModeCli::Com2 => UefiConsoleMode::COM2,
                                UefiConsoleModeCli::None => UefiConsoleMode::None,
                            },
                            default_boot_always_attempt: opt.default_boot_always_attempt,
                        }
                    },
                    com1: with_vmbus_com1_serial,
                    com2: with_vmbus_com2_serial,
                    serial_tx_only: opt.serial_tx_only,
                    vtl2_settings: Some(prost::Message::encode_to_vec(&vtl2_settings)),
                    vmbus_redirection: opt.vmbus_redirect,
                    vmgs,
                    framebuffer: opt
                        .vtl2_gfx
                        .then(|| SharedFramebufferHandle.into_resource()),
                    guest_request_recv,
                    tpm_version: opt.tpm.map(|v| match v {
                        TpmVersionCli::V138 => get_resources::ged::GedTpmVersion::V138,
                        TpmVersionCli::V185 => get_resources::ged::GedTpmVersion::V185,
                    }),
                    firmware_event_send: None,
                    secure_boot_enabled: opt.secure_boot,
                    secure_boot_template: match opt.secure_boot_template {
                        Some(SecureBootTemplateCli::Windows) => {
                            get_resources::ged::GuestSecureBootTemplateType::MicrosoftWindows
                        },
                        Some(SecureBootTemplateCli::UefiCa) => {
                            get_resources::ged::GuestSecureBootTemplateType::MicrosoftUefiCertificateAuthority
                        }
                        None => {
                            get_resources::ged::GuestSecureBootTemplateType::None
                        },
                    },
                    enable_battery: opt.battery,
                    enable_hibernation: opt.hibernation,
                    no_persistent_secrets: true,
                    igvm_attest_test_config: None,
                    test_gsp_by_id: opt.test_gsp_by_id,
                    efi_diagnostics_log_level: {
                        match opt.efi_diagnostics_log_level.unwrap_or_default() {
                            EfiDiagnosticsLogLevelCli::Default => get_resources::ged::EfiDiagnosticsLogLevelType::Default,
                            EfiDiagnosticsLogLevelCli::Info => get_resources::ged::EfiDiagnosticsLogLevelType::Info,
                            EfiDiagnosticsLogLevelCli::Full => get_resources::ged::EfiDiagnosticsLogLevelType::Full,
                        }
                    },
                    force_dma_bounce_enabled: opt.uefi_force_dma_bounce,
                    smbios: ged_smbios,
                }
                .into_resource(),
            ),
        ]);
    }

    if let Some(tpm_version) = opt.tpm
        && !opt.vtl2
    {
        let register_layout = if cfg!(guest_arch = "x86_64") {
            TpmRegisterLayout::IoPort
        } else {
            TpmRegisterLayout::Mmio
        };

        let tpm_version = match tpm_version {
            TpmVersionCli::V138 => TpmVersion::V138,
            TpmVersionCli::V185 => TpmVersion::V185,
        };

        let (ppi_store, nvram_store) = if opt.vmgs.is_some() {
            (
                VmgsFileHandle::new(vmgs_format::FileId::TPM_PPI, true).into_resource(),
                VmgsFileHandle::new(tpm_version.to_nvram_vmgs_file_id(), true).into_resource(),
            )
        } else {
            (
                EphemeralNonVolatileStoreHandle.into_resource(),
                EphemeralNonVolatileStoreHandle.into_resource(),
            )
        };

        chipset_devices.push(ChipsetDeviceHandle {
            name: "tpm".to_string(),
            resource: chipset_device_worker_defs::RemoteChipsetDeviceHandle {
                device: TpmDeviceHandle {
                    version: tpm_version,
                    ppi_store,
                    nvram_store,
                    nvram_size: None,
                    refresh_tpm_seeds: false,
                    ak_cert_type: tpm_resources::TpmAkCertTypeResource::None,
                    register_layout,
                    guest_secret_key: None,
                    logger: None,
                    is_confidential_vm: false,
                    bios_guid,
                }
                .into_resource(),
                worker_host: mesh.make_host("tpm", None).await?,
            }
            .into_resource(),
        });
    }

    let vga_firmware = if opt.pcat {
        Some(openvmm_pcat_locator::find_svga_bios(
            opt.vga_firmware.as_deref(),
        )?)
    } else {
        None
    };

    if opt.gfx {
        // Channel for the video device to report dirty rectangles to the VNC worker.
        let (dirt_send, dirt_recv) = mesh::channel();
        resources.dirty_rect_recv = Some(dirt_recv);

        vmbus_devices.extend([
            (
                DeviceVtl::Vtl0,
                SynthVideoHandle {
                    framebuffer: SharedFramebufferHandle.into_resource(),
                    dirt_send: Some(dirt_send),
                }
                .into_resource(),
            ),
            (
                DeviceVtl::Vtl0,
                SynthKeyboardHandle {
                    source: MultiplexedInputHandle {
                        // Save 0 for PS/2
                        elevation: 1,
                    }
                    .into_resource(),
                }
                .into_resource(),
            ),
            (
                DeviceVtl::Vtl0,
                SynthMouseHandle {
                    source: MultiplexedInputHandle {
                        // Save 0 for PS/2
                        elevation: 1,
                    }
                    .into_resource(),
                }
                .into_resource(),
            ),
        ]);
    }

    let vsock_listener = |path: Option<&str>| -> anyhow::Result<_> {
        if let Some(path) = path {
            cleanup_socket(path.as_ref());
            let listener = unix_socket::UnixListener::bind(path)
                .with_context(|| format!("failed to bind to hybrid vsock path: {}", path))?;
            Ok(Some(listener))
        } else {
            Ok(None)
        }
    };

    let vtl0_vsock_listener = vsock_listener(opt.vmbus_vsock_path.as_deref())?;
    let vtl2_vsock_listener = vsock_listener(opt.vmbus_vtl2_vsock_path.as_deref())?;

    if let Some(path) = &opt.openhcl_dump_path {
        let (resource, task) = spawn_dump_handler(&spawner, path.clone(), None);
        task.detach();
        vmbus_devices.push((openhcl_vtl, resource));
    }

    #[cfg(guest_arch = "aarch64")]
    let topology_arch = openvmm_defs::config::ArchTopologyConfig::Aarch64(
        openvmm_defs::config::Aarch64TopologyConfig {
            // TODO: allow this to be configured from the command line
            gic_config: None,
            pmu_gsiv: openvmm_defs::config::PmuGsivConfig::Platform,
            gic_msi: match opt.gic_msi {
                cli_args::GicMsiCli::Auto => openvmm_defs::config::GicMsiConfig::Auto,
                cli_args::GicMsiCli::Its => openvmm_defs::config::GicMsiConfig::Its,
                cli_args::GicMsiCli::V2m => {
                    openvmm_defs::config::GicMsiConfig::V2m { spi_count: None }
                }
            },
        },
    );
    #[cfg(guest_arch = "x86_64")]
    let topology_arch =
        openvmm_defs::config::ArchTopologyConfig::X86(openvmm_defs::config::X86TopologyConfig {
            apic_id_offset: if is_microvm { 0 } else { opt.apic_id_offset },
            x2apic: if is_microvm {
                openvmm_defs::config::X2ApicConfig::Unsupported
            } else {
                opt.x2apic
            },
        });

    let with_isolation = if let Some(isolation) = &opt.isolation {
        match isolation {
            cli_args::IsolationCli::Vbs => {
                // TODO: For now, VBS isolation is only supported with VTL2.
                if !opt.vtl2 {
                    anyhow::bail!("VBS isolation is only currently supported with vtl2");
                }

                // TODO: Alias map support is not yet implemented with isolation.
                if !opt.no_alias_map {
                    anyhow::bail!("alias map not supported with isolation");
                }

                Some(openvmm_defs::config::IsolationType::Vbs)
            }
            cli_args::IsolationCli::Snp => Some(openvmm_defs::config::IsolationType::Snp),
        }
    } else {
        None
    };

    if with_hv && !opt.no_vmbus {
        let (shutdown_send, shutdown_recv) = mesh::channel();
        resources.shutdown_ic = Some(shutdown_send);
        let (kvp_send, kvp_recv) = mesh::channel();
        resources.kvp_ic = Some(kvp_send);
        vmbus_devices.extend(
            [
                hyperv_ic_resources::shutdown::ShutdownIcHandle {
                    recv: shutdown_recv,
                }
                .into_resource(),
                hyperv_ic_resources::kvp::KvpIcHandle { recv: kvp_recv }.into_resource(),
                hyperv_ic_resources::timesync::TimesyncIcHandle.into_resource(),
            ]
            .map(|r| (DeviceVtl::Vtl0, r)),
        );
    }

    if let Some(hive_path) = &opt.imc {
        let file = fs_err::File::open(hive_path).context("failed to open imc hive")?;
        vmbus_devices.push((
            DeviceVtl::Vtl0,
            vmbfs_resources::VmbfsImcDeviceHandle { file: file.into() }.into_resource(),
        ));
    }

    let mut virtio_devices = Vec::new();
    let mut add_virtio_device = |bus, resource: Resource<VirtioDeviceHandle>| {
        let bus = match bus {
            VirtioBusCli::Auto => {
                // Use VPCI when possible (currently only on Windows and macOS due
                // to KVM backend limitations).
                if with_hv && (cfg!(windows) || cfg!(target_os = "macos")) {
                    None
                } else {
                    Some(VirtioBus::Pci)
                }
            }
            VirtioBusCli::Mmio => Some(VirtioBus::Mmio),
            VirtioBusCli::Pci => Some(VirtioBus::Pci),
            VirtioBusCli::Vpci => None,
        };
        if let Some(bus) = bus {
            virtio_devices.push((bus, resource));
        } else {
            vpci_devices.push(VpciDeviceConfig {
                vtl: DeviceVtl::Vtl0,
                instance_id: Guid::new_random(),
                resource: VirtioPciDeviceHandle(resource).into_resource(),
                vnode: None,
            });
        }
    };

    if let Some(network) = &microvm_network {
        let endpoint = microvm_network_endpoint(network, &mut resources)?;
        add_virtio_device(
            VirtioBusCli::Mmio,
            virtio_resources::net::VirtioNetHandle {
                max_queues: Some(1),
                mac_address: network.guest_mac,
                endpoint,
                egress_policy: microvm_egress_policy.clone(),
                save_restore: true,
                static_ipv4: Some(net_backend_resources::consomme::StaticIpv4Config {
                    guest_ipv4: network.guest_ipv4,
                    prefix_length: network.prefix_length,
                    gateway_ipv4: network.derived_gateway_ipv4,
                    gateway_mac: network.gateway_mac,
                }),
                effective_features: Some(openvmm_defs::config::MICROVM_VIRTIO_NET_FEATURES),
            }
            .into_resource(),
        );
    }

    if microvm_filesystem_slot {
        let (fs, profile) = if let Some(filesystem) = &effective_microvm_filesystem {
            (
                virtio_resources::fs::VirtioFsBackend::HostFs {
                    root_path: filesystem.root_path.clone(),
                    mount_options: String::new(),
                },
                virtio_resources::fs::VirtioFsProfile::Microvm {
                    stable_id: MICROVM_FILESYSTEM_STABLE_ID.to_owned(),
                    root_identity: filesystem.attachment.identity.clone(),
                    read_only: filesystem.config.access.is_read_only(),
                },
            )
        } else {
            (
                virtio_resources::fs::VirtioFsBackend::Dormant,
                virtio_resources::fs::VirtioFsProfile::MicrovmDormant {
                    stable_id: MICROVM_FILESYSTEM_STABLE_ID.to_owned(),
                },
            )
        };
        add_virtio_device(
            VirtioBusCli::Mmio,
            virtio_resources::fs::VirtioFsHandle {
                tag: "microvm".to_owned(),
                fs,
                profile,
            }
            .into_resource(),
        );
    }

    for cli_cfg in &opt.virtio_net {
        if cli_cfg.underhill {
            anyhow::bail!("use --net uh:[...] to add underhill NICs")
        }
        let vport = parse_endpoint(cli_cfg, &mut nic_index, &mut resources)?;
        let resource = virtio_resources::net::VirtioNetHandle {
            max_queues: vport.max_queues,
            mac_address: vport.mac_address,
            endpoint: vport.endpoint,
            egress_policy: None,
            save_restore: false,
            static_ipv4: None,
            effective_features: None,
        }
        .into_resource();
        if let Some(pcie_port) = &cli_cfg.pcie_port {
            pcie_devices.push(PcieDeviceConfig {
                port_name: pcie_port.clone(),
                resource: VirtioPciDeviceHandle(resource).into_resource(),
            });
        } else {
            add_virtio_device(VirtioBusCli::Auto, resource);
        }
    }

    for args in &opt.virtio_fs {
        let resource: Resource<VirtioDeviceHandle> = virtio_resources::fs::VirtioFsHandle {
            tag: args.tag.clone(),
            fs: virtio_resources::fs::VirtioFsBackend::HostFs {
                root_path: args.path.clone(),
                mount_options: args.options.clone(),
            },
            profile: virtio_resources::fs::VirtioFsProfile::Standard,
        }
        .into_resource();
        if let Some(pcie_port) = &args.pcie_port {
            pcie_devices.push(PcieDeviceConfig {
                port_name: pcie_port.clone(),
                resource: VirtioPciDeviceHandle(resource).into_resource(),
            });
        } else {
            add_virtio_device(opt.virtio_fs_bus, resource);
        }
    }

    for args in &opt.virtio_fs_shmem {
        let resource: Resource<VirtioDeviceHandle> = virtio_resources::fs::VirtioFsHandle {
            tag: args.tag.clone(),
            fs: virtio_resources::fs::VirtioFsBackend::SectionFs {
                root_path: args.path.clone(),
            },
            profile: virtio_resources::fs::VirtioFsProfile::Standard,
        }
        .into_resource();
        if let Some(pcie_port) = &args.pcie_port {
            pcie_devices.push(PcieDeviceConfig {
                port_name: pcie_port.clone(),
                resource: VirtioPciDeviceHandle(resource).into_resource(),
            });
        } else {
            add_virtio_device(opt.virtio_fs_bus, resource);
        }
    }

    for args in &opt.virtio_9p {
        let resource: Resource<VirtioDeviceHandle> = virtio_resources::p9::VirtioPlan9Handle {
            tag: args.tag.clone(),
            root_path: args.path.clone(),
            debug: opt.virtio_9p_debug,
        }
        .into_resource();
        if let Some(pcie_port) = &args.pcie_port {
            pcie_devices.push(PcieDeviceConfig {
                port_name: pcie_port.clone(),
                resource: VirtioPciDeviceHandle(resource).into_resource(),
            });
        } else {
            add_virtio_device(VirtioBusCli::Auto, resource);
        }
    }

    if let Some(pmem_args) = &opt.virtio_pmem {
        let resource: Resource<VirtioDeviceHandle> = virtio_resources::pmem::VirtioPmemHandle {
            path: pmem_args.path.clone(),
        }
        .into_resource();
        if let Some(pcie_port) = &pmem_args.pcie_port {
            pcie_devices.push(PcieDeviceConfig {
                port_name: pcie_port.clone(),
                resource: VirtioPciDeviceHandle(resource).into_resource(),
            });
        } else {
            add_virtio_device(VirtioBusCli::Auto, resource);
        }
    }

    if opt.virtio_rng {
        let resource: Resource<VirtioDeviceHandle> =
            virtio_resources::rng::VirtioRngHandle.into_resource();
        if let Some(pcie_port) = &opt.virtio_rng_pcie_port {
            pcie_devices.push(PcieDeviceConfig {
                port_name: pcie_port.clone(),
                resource: VirtioPciDeviceHandle(resource).into_resource(),
            });
        } else {
            add_virtio_device(opt.virtio_rng_bus, resource);
        }
    }

    if let Some(backend) = virtio_console_backend {
        let resource: Resource<VirtioDeviceHandle> =
            virtio_resources::console::VirtioConsoleHandle {
                backend,
                disconnect_policy: if is_microvm
                    && !microvm_console.as_ref().is_some_and(|(_, attachment, _)| {
                        attachment.reconnect_policy
                            == virtio_resources::console::VirtioConsoleReconnectPolicy::DiscardWhileDisconnected
                    })
                {
                    virtio_resources::console::VirtioConsoleDisconnectPolicy::Retain
                } else {
                    virtio_resources::console::VirtioConsoleDisconnectPolicy::Discard
                },
                attachment: microvm_console
                    .as_ref()
                    .map(|(_, attachment, _)| attachment.clone()),
            }
            .into_resource();
        if is_microvm {
            add_virtio_device(VirtioBusCli::Mmio, resource);
        } else if let Some(pcie_port) = &opt.virtio_console_pcie_port {
            pcie_devices.push(PcieDeviceConfig {
                port_name: pcie_port.clone(),
                resource: VirtioPciDeviceHandle(resource).into_resource(),
            });
        } else {
            add_virtio_device(VirtioBusCli::Auto, resource);
        }
    }

    // Handle --vhost-user arguments.
    #[cfg(target_os = "linux")]
    for vhost_cli in &opt.vhost_user {
        let stream =
            unix_socket::UnixStream::connect(&vhost_cli.socket_path).with_context(|| {
                format!(
                    "failed to connect to vhost-user socket: {}",
                    vhost_cli.socket_path
                )
            })?;

        use crate::cli_args::VhostUserDeviceTypeCli;
        let resource: Resource<VirtioDeviceHandle> = match vhost_cli.device_type {
            VhostUserDeviceTypeCli::Fs {
                ref tag,
                num_queues,
                queue_size,
            } => virtio_resources::vhost_user::VhostUserFsHandle {
                socket: stream.into(),
                tag: tag.clone(),
                num_queues,
                queue_size,
            }
            .into_resource(),
            VhostUserDeviceTypeCli::Blk {
                num_queues,
                queue_size,
            } => virtio_resources::vhost_user::VhostUserBlkHandle {
                socket: stream.into(),
                num_queues,
                queue_size,
            }
            .into_resource(),
            VhostUserDeviceTypeCli::Other {
                device_id,
                ref queue_sizes,
            } => virtio_resources::vhost_user::VhostUserGenericHandle {
                socket: stream.into(),
                device_id,
                queue_sizes: queue_sizes.clone(),
            }
            .into_resource(),
        };
        if let Some(pcie_port) = &vhost_cli.pcie_port {
            pcie_devices.push(PcieDeviceConfig {
                port_name: pcie_port.clone(),
                resource: VirtioPciDeviceHandle(resource).into_resource(),
            });
        } else {
            add_virtio_device(VirtioBusCli::Auto, resource);
        }
    }

    let virtio_vsock_bus = opt.virtio_vsock_bus.unwrap_or(VirtioBusCli::Auto);

    if let Some(vsock_path) = &opt.virtio_vsock_path {
        let listener = vsock_listener(Some(vsock_path))?.unwrap();
        add_virtio_device(
            virtio_vsock_bus,
            virtio_resources::vsock::VirtioVsockHandle {
                // The guest CID does not matter since the UDS relay does not use it. It just needs
                // to be some non-reserved value for the guest to use.
                guest_cid: 0x3,
                base_path: vsock_path.clone(),
                listener,
            }
            .into_resource(),
        );
    }

    #[cfg(target_os = "linux")]
    if let Some(guest_cid) = opt.virtio_vsock_vhost_cid {
        let vhost = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/vhost-vsock")
            .context("failed to open /dev/vhost-vsock")?
            .into();
        add_virtio_device(
            virtio_vsock_bus,
            virtio_resources::vsock::VirtioVsockVhostHandle { vhost, guest_cid }.into_resource(),
        );
    }

    let mut cfg = Config {
        machine_profile: opt.machine.into(),
        chipset,
        load_mode,
        floppy_disks,
        pcie_root_complexes,
        pcie_ecam_below_4gb: opt.pcie_ecam_below_4gb,
        #[cfg(target_os = "linux")]
        pcie_devices: {
            let mut devs = pcie_devices;
            devs.extend(vfio_pcie_devices);
            devs
        },
        #[cfg(not(target_os = "linux"))]
        pcie_devices,
        pcie_switches,
        pcie_generic_initiators,
        vpci_devices,
        ide_disks: Vec::new(),
        numa: {
            if let Some(ref nodes) = opt.numa {
                // --numa mode: each --numa flag defines a node.
                NumaTopology {
                    nodes: nodes
                        .iter()
                        .map(|n| {
                            let vps = match &n.vps {
                                Some(vps) if vps.0.is_empty() => VpAssignment::Empty,
                                Some(vps) => {
                                    VpAssignment::Explicit(vps.expand_below(opt.processors)?)
                                }
                                None => VpAssignment::FromTopology,
                            };
                            Ok(NumaNode {
                                mem: Some(MemoryConfig {
                                    mem_size: n
                                        .memory
                                        .size
                                        .expect("NUMA memory size was validated")
                                        .0,
                                    prefetch_memory: n.memory.prefetch,
                                    private_memory: n.memory.shared == Some(false),
                                    transparent_hugepages: n
                                        .memory
                                        .transparent_hugepages
                                        .unwrap_or(!n.memory.hugepages),
                                    hugepages: n.memory.hugepages,
                                    hugepage_size: n.memory.hugepage_size.map(|m| m.0),
                                    host_numa_node: n.host_numa_node,
                                }),
                                vps,
                            })
                        })
                        .collect::<anyhow::Result<Vec<_>>>()?,
                    distances: opt
                        .numa_distance
                        .as_deref()
                        .unwrap_or(&[])
                        .iter()
                        .map(|d| NumaDistance {
                            src: d.src,
                            dst: d.dst,
                            distance: d.distance,
                        })
                        .collect(),
                }
            } else {
                // Single-node default from --memory.
                NumaTopology {
                    nodes: vec![NumaNode {
                        mem: Some(MemoryConfig {
                            mem_size: opt.memory_size(),
                            prefetch_memory: opt.prefetch_memory(),
                            private_memory: opt.private_memory(),
                            transparent_hugepages: opt.transparent_hugepages(),
                            hugepages: opt.memory.hugepages,
                            hugepage_size: opt.memory.hugepage_size.map(|m| m.0),
                            host_numa_node: None,
                        }),
                        vps: VpAssignment::FromTopology,
                    }],
                    distances: vec![],
                }
            }
        },
        processor_topology: ProcessorTopologyConfig {
            proc_count: opt.processors,
            vps_per_socket: if is_microvm {
                Some(opt.processors)
            } else {
                opt.vps_per_socket
            },
            enable_smt: if is_microvm {
                Some(false)
            } else {
                match opt.smt {
                    cli_args::SmtConfigCli::Auto => None,
                    cli_args::SmtConfigCli::Force => Some(true),
                    cli_args::SmtConfigCli::Off => Some(false),
                }
            },
            arch: Some(topology_arch),
        },
        hypervisor: HypervisorConfig {
            with_hv,
            with_vtl2: opt.vtl2.then_some(Vtl2Config {
                vtl0_alias_map: !opt.no_alias_map,
                late_map_vtl0_memory: match opt.late_map_vtl0_policy {
                    cli_args::Vtl0LateMapPolicyCli::Off => None,
                    cli_args::Vtl0LateMapPolicyCli::Log => Some(LateMapVtl0MemoryPolicy::Log),
                    cli_args::Vtl0LateMapPolicyCli::Halt => Some(LateMapVtl0MemoryPolicy::Halt),
                    cli_args::Vtl0LateMapPolicyCli::Exception => {
                        Some(LateMapVtl0MemoryPolicy::InjectException)
                    }
                },
            }),
            with_isolation,
            nested_virt: opt.nested_virt,
        },
        #[cfg(windows)]
        kernel_vmnics,
        input: mesh::Receiver::new(),
        framebuffer,
        vga_firmware,
        vtl2_gfx: opt.vtl2_gfx,
        virtio_devices,
        vmbus: (with_hv && !opt.no_vmbus).then_some(VmbusConfig {
            vsock_listener: vtl0_vsock_listener,
            vsock_path: opt.vmbus_vsock_path.clone(),
            vtl2_redirect: opt.vmbus_redirect,
            vmbus_max_version: opt.vmbus_max_version,
            #[cfg(windows)]
            vmbusproxy_handle,
        }),
        vtl2_vmbus: (with_hv && opt.vtl2).then_some(VmbusConfig {
            vsock_listener: vtl2_vsock_listener,
            vsock_path: opt.vmbus_vtl2_vsock_path.clone(),
            ..Default::default()
        }),
        vmbus_devices,
        chipset_devices,
        pci_chipset_devices,
        isa_dma_controller,
        chipset_capabilities: capabilities,
        layout: layout_config,
        #[cfg(windows)]
        vpci_resources,
        vmgs,
        firmware_event_send: None,
        debugger_rpc: None,
        rtc_delta_milliseconds: 0,
        microvm_network,
        microvm_filesystem_bootstrap: restore_machine_contract
            .map(|contract| contract.microvm_filesystem.is_some())
            .unwrap_or_else(|| microvm_filesystem.is_some()),
        microvm_filesystem,
        microvm_sandbox_blocks: Vec::new(),
        microvm_memory_capacity: restore_machine_contract
            .and_then(|contract| {
                (contract.memory_expansion_version != 0).then_some(contract.memory_capacity_bytes)
            })
            .or(opt.memory_capacity.map(|capacity| capacity.0)),
        microvm_snapshot_memory_ranges: restore_machine_contract
            .filter(|contract| contract.memory_expansion_version != 0)
            .map(|contract| {
                contract
                    .memory_ranges
                    .iter()
                    .map(|range| {
                        memory_range::MemoryRange::new(
                            range.gpa_start..range.gpa_start + range.length,
                        )
                    })
                    .collect()
            })
            .unwrap_or_default(),
        microvm_restore_memory_ranges: restore_memory_ranges
            .iter()
            .map(|range| {
                memory_range::MemoryRange::new(range.gpa_start..range.gpa_start + range.length)
            })
            .collect(),
    };

    storage.build_config(&mut cfg, &mut resources, opt.scsi_sub_channels)?;
    let requested_hypervisor = opt
        .hypervisor
        .as_deref()
        .and_then(|spec| spec.split(':').next());
    if cfg.machine_profile == MachineProfile::Microvm && restore_machine_contract.is_none() {
        let LoadMode::Pvh { cmdline, .. } = &mut cfg.load_mode else {
            unreachable!("microVM configuration was constructed with PVH load mode");
        };
        let has_console = cfg
            .virtio_devices
            .iter()
            .any(|(_, device)| device.id() == "virtio-console");
        let network_irq = cfg
            .microvm_network
            .as_ref()
            .map(|_| openvmm_defs::config::microvm_virtio_net_irq(requested_hypervisor))
            .transpose()?;
        let network = cfg
            .microvm_network
            .as_ref()
            .zip(network_irq)
            .map(|(network, irq)| (network, irq, microvm_gateway_dns));
        if let Some(snapshot_tier) = opt.snapshot_tier {
            anyhow::ensure!(
                !cmdline
                    .split_ascii_whitespace()
                    .any(|token| token.starts_with("nvx_snapshot_tier=")),
                "nvx_snapshot_tier is reserved for the host snapshot policy"
            );
            if !cmdline.is_empty() {
                cmdline.push(' ');
            }
            cmdline.push_str("nvx_snapshot_tier=");
            cmdline.push_str(snapshot_tier.manifest_name());
        }
        openvmm_defs::config::append_microvm_virtio_discovery(
            cmdline,
            network,
            microvm_filesystem_slot,
            cfg.microvm_filesystem.as_ref(),
            has_console,
            &cfg.microvm_sandbox_blocks,
        )?;
    }
    openvmm_defs::config::validate_machine_config(&cfg, requested_hypervisor)?;
    resources.serial_driver = Some(serial_driver);
    validate_snp_config(&cfg)?;
    Ok((cfg, resources))
}

fn validate_snp_config(cfg: &Config) -> anyhow::Result<()> {
    if cfg.hypervisor.with_isolation != Some(openvmm_defs::config::IsolationType::Snp) {
        return Ok(());
    }

    if !matches!(
        cfg.load_mode,
        LoadMode::Linux { .. } | LoadMode::Igvm { .. }
    ) {
        anyhow::bail!("SNP isolation currently only supports Linux direct or IGVM boot");
    }
    if cfg.hypervisor.with_hv {
        anyhow::bail!("SNP isolation currently does not support Hyper-V enlightenments");
    }
    if cfg.hypervisor.with_vtl2.is_some() {
        anyhow::bail!("SNP isolation currently does not support VTL2");
    }
    if cfg.vmbus.is_some() || cfg.vtl2_vmbus.is_some() || !cfg.vmbus_devices.is_empty() {
        anyhow::bail!("SNP isolation currently does not support VMBus devices");
    }

    let only_supported_chipset_devices = cfg.chipset_devices.iter().all(|device| {
        matches!(
            device.resource.id(),
            "serial_16550"
                | "pic"
                | "pit"
                | "generic-ioapic"
                | "hyperv_power_management"
                | "missing-dev"
        )
    });
    let only_virtio_pcie_devices = cfg
        .pcie_devices
        .iter()
        .all(|device| device.resource.id() == "virtio");
    if !cfg.floppy_disks.is_empty()
        || !cfg.ide_disks.is_empty()
        || !cfg.virtio_devices.is_empty()
        || !only_virtio_pcie_devices
        || !cfg.vpci_devices.is_empty()
        || !only_supported_chipset_devices
        || !cfg.pci_chipset_devices.is_empty()
    {
        anyhow::bail!("SNP isolation currently only supports virtio devices");
    }
    if cfg.framebuffer.is_some() || cfg.vga_firmware.is_some() || cfg.debugger_rpc.is_some() {
        anyhow::bail!("SNP isolation currently does not support this VM configuration");
    }

    Ok(())
}

/// Gets the terminal to use for externally launched console windows.
pub(crate) fn openvmm_terminal_app() -> Option<PathBuf> {
    std::env::var_os("OPENVMM_TERM")
        .or_else(|| std::env::var_os("HVLITE_TERM"))
        .map(Into::into)
}

// Tries to remove `path` if it is confirmed to be a Unix socket.
fn cleanup_socket(path: &Path) {
    #[cfg(windows)]
    let is_socket = pal::windows::fs::is_unix_socket(path).unwrap_or(false);
    #[cfg(not(windows))]
    let is_socket = path
        .metadata()
        .is_ok_and(|meta| std::os::unix::fs::FileTypeExt::is_socket(&meta.file_type()));

    if is_socket {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(windows)]
fn new_switch_port(
    switch_id: Option<&str>,
) -> anyhow::Result<(
    openvmm_defs::config::SwitchPortId,
    vmswitch::kernel::SwitchPort,
)> {
    let id = vmswitch::kernel::SwitchPortId {
        switch: match switch_id {
            Some(s) => s.parse().context("invalid switch id")?,
            None => vmswitch::hcn::DEFAULT_SWITCH,
        },
        port: Guid::new_random(),
    };
    let _ = vmswitch::hcn::Network::open(&id.switch)
        .with_context(|| format!("could not find switch {}", id.switch))?;

    let port = vmswitch::kernel::SwitchPort::new(&id).context("failed to create switch port")?;

    let id = openvmm_defs::config::SwitchPortId {
        switch: id.switch,
        port: id.port,
    };
    Ok((id, port))
}

fn parse_endpoint(
    cli_cfg: &NicConfigCli,
    index: &mut usize,
    resources: &mut VmResources,
) -> anyhow::Result<NicConfig> {
    let _ = resources;
    let endpoint = match &cli_cfg.endpoint {
        EndpointConfigCli::Consomme { cidr, host_fwd } => {
            let ports = host_fwd
                .iter()
                .map(|fwd| {
                    use net_backend_resources::consomme::HostPortProtocol;
                    net_backend_resources::consomme::HostPortConfig {
                        protocol: match fwd.protocol {
                            cli_args::HostPortProtocolCli::Tcp => HostPortProtocol::Tcp,
                            cli_args::HostPortProtocolCli::Udp => HostPortProtocol::Udp,
                        },
                        host_address: fwd
                            .host_address
                            .map(net_backend_resources::consomme::HostIpAddress::from),
                        host_port: net_backend_resources::consomme::HostPort::Fixed(fwd.host_port),
                        guest_port: fwd.guest_port,
                    }
                })
                .collect();
            // Only wire the bind/unbind RPC channel to the first consomme
            // endpoint. Additional consomme NICs work normally but cannot be
            // targeted by runtime bind/unbind commands.
            let recv = if resources.consomme_rpc.is_none() {
                let (send, recv) = mesh::channel();
                resources.consomme_rpc = Some(send);
                Some(recv)
            } else {
                None
            };
            net_backend_resources::consomme::ConsommeHandle {
                cidr: cidr.clone(),
                static_ipv4: None,
                ports,
                recv,
            }
            .into_resource()
        }
        EndpointConfigCli::None => net_backend_resources::null::NullHandle.into_resource(),
        EndpointConfigCli::Dio { id } => {
            #[cfg(windows)]
            {
                let (port_id, port) = new_switch_port(id.as_deref())?;
                resources.switch_ports.push(port);
                net_backend_resources::dio::WindowsDirectIoHandle {
                    switch_port_id: net_backend_resources::dio::SwitchPortId {
                        switch: port_id.switch,
                        port: port_id.port,
                    },
                }
                .into_resource()
            }

            #[cfg(not(windows))]
            {
                let _ = id;
                bail!("cannot use dio on non-windows platforms")
            }
        }
        EndpointConfigCli::Tap { name } => {
            #[cfg(target_os = "linux")]
            {
                let fd = net_tap::tap::open_tap(name)
                    .with_context(|| format!("failed to open TAP device '{name}'"))?;
                net_backend_resources::tap::TapHandle { fd }.into_resource()
            }

            #[cfg(not(target_os = "linux"))]
            {
                let _ = name;
                bail!("TAP backend is only supported on Linux")
            }
        }
        EndpointConfigCli::Microvm(_) => {
            bail!("a bare IPv4/prefix --net is only supported by the microVM profile")
        }
    };

    // Pick a random MAC address.
    let mut mac_address = [0x00, 0x15, 0x5D, 0, 0, 0];
    getrandom::fill(&mut mac_address[3..]).expect("rng failure");

    // Pick a fixed instance ID based on the index.
    const BASE_INSTANCE_ID: Guid = guid::guid!("00000000-da43-11ed-936a-00155d6db52f");
    let instance_id = Guid {
        data1: *index as u32,
        ..BASE_INSTANCE_ID
    };
    *index += 1;

    Ok(NicConfig {
        vtl: cli_cfg.vtl,
        instance_id,
        endpoint,
        mac_address: mac_address.into(),
        max_queues: cli_cfg.max_queues,
        pcie_port: cli_cfg.pcie_port.clone(),
    })
}

fn microvm_network_endpoint(
    network: &openvmm_defs::config::MicrovmNetworkConfig,
    _resources: &mut VmResources,
) -> anyhow::Result<Resource<NetEndpointHandleKind>> {
    Ok(net_backend_resources::consomme::ConsommeHandle {
        cidr: None,
        static_ipv4: Some(net_backend_resources::consomme::StaticIpv4Config {
            guest_ipv4: network.guest_ipv4,
            prefix_length: network.prefix_length,
            gateway_ipv4: network.derived_gateway_ipv4,
            gateway_mac: network.gateway_mac,
        }),
        ports: Vec::new(),
        recv: None,
    }
    .into_resource())
}

#[derive(Debug)]
struct NicConfig {
    vtl: DeviceVtl,
    instance_id: Guid,
    mac_address: MacAddress,
    endpoint: Resource<NetEndpointHandleKind>,
    max_queues: Option<u16>,
    pcie_port: Option<String>,
}

impl NicConfig {
    fn into_netvsp_handle(self) -> (DeviceVtl, Resource<VmbusDeviceHandleKind>) {
        (
            self.vtl,
            netvsp_resources::NetvspHandle {
                instance_id: self.instance_id,
                mac_address: self.mac_address,
                endpoint: self.endpoint,
                max_queues: self.max_queues,
            }
            .into_resource(),
        )
    }
}

enum LayerOrDisk {
    Layer(DiskLayerDescription),
    Disk(Resource<DiskHandleKind>),
}

async fn disk_open(
    disk_cli: &DiskCliKind,
    read_only: bool,
) -> anyhow::Result<Resource<DiskHandleKind>> {
    let mut layers = Vec::new();
    disk_open_inner(disk_cli, read_only, &mut layers).await?;
    if layers.len() == 1 && matches!(layers[0], LayerOrDisk::Disk(_)) {
        let LayerOrDisk::Disk(disk) = layers.pop().unwrap() else {
            unreachable!()
        };
        Ok(disk)
    } else {
        Ok(Resource::new(disk_backend_resources::LayeredDiskHandle {
            layers: layers
                .into_iter()
                .map(|layer| match layer {
                    LayerOrDisk::Layer(layer) => layer,
                    LayerOrDisk::Disk(disk) => DiskLayerDescription {
                        layer: DiskLayerHandle(disk).into_resource(),
                        read_cache: false,
                        write_through: false,
                    },
                })
                .collect(),
        }))
    }
}

fn disk_open_inner<'a>(
    disk_cli: &'a DiskCliKind,
    read_only: bool,
    layers: &'a mut Vec<LayerOrDisk>,
) -> futures::future::BoxFuture<'a, anyhow::Result<()>> {
    Box::pin(async move {
        fn layer<T: IntoResource<DiskLayerHandleKind>>(layer: T) -> LayerOrDisk {
            LayerOrDisk::Layer(layer.into_resource().into())
        }
        fn disk<T: IntoResource<DiskHandleKind>>(disk: T) -> LayerOrDisk {
            LayerOrDisk::Disk(disk.into_resource())
        }
        match disk_cli {
            &DiskCliKind::Memory(len) => {
                layers.push(layer(RamDiskLayerHandle {
                    len: Some(len),
                    sector_size: None,
                }));
            }
            DiskCliKind::File {
                path,
                create_with_len,
                direct,
            } => layers.push(LayerOrDisk::Disk(if let Some(size) = create_with_len {
                create_disk_type(
                    path,
                    *size,
                    OpenDiskOptions {
                        read_only: false,
                        direct: *direct,
                    },
                )
                .with_context(|| format!("failed to create {}", path.display()))?
            } else {
                open_disk_type(
                    path,
                    OpenDiskOptions {
                        read_only,
                        direct: *direct,
                    },
                )
                .await
                .with_context(|| format!("failed to open {}", path.display()))?
            })),
            DiskCliKind::Blob { kind, url } => {
                layers.push(disk(disk_backend_resources::BlobDiskHandle {
                    url: url.to_owned(),
                    format: match kind {
                        cli_args::BlobKind::Flat => disk_backend_resources::BlobDiskFormat::Flat,
                        cli_args::BlobKind::Vhd1 => {
                            disk_backend_resources::BlobDiskFormat::FixedVhd1
                        }
                    },
                }))
            }
            DiskCliKind::MemoryDiff(inner) => {
                layers.push(layer(RamDiskLayerHandle {
                    len: None,
                    sector_size: None,
                }));
                disk_open_inner(inner, true, layers).await?;
            }
            DiskCliKind::PersistentReservationsWrapper(inner) => {
                layers.push(disk(disk_backend_resources::DiskWithReservationsHandle(
                    disk_open(inner, read_only).await?,
                )))
            }
            DiskCliKind::DelayDiskWrapper {
                delay_ms,
                disk: inner,
            } => layers.push(disk(DelayDiskHandle {
                delay: CellUpdater::new(Duration::from_millis(*delay_ms)).cell(),
                disk: disk_open(inner, read_only).await?,
            })),
            DiskCliKind::Crypt {
                disk: inner,
                cipher,
                key_file,
            } => layers.push(disk(disk_crypt_resources::DiskCryptHandle {
                disk: disk_open(inner, read_only).await?,
                cipher: match cipher {
                    cli_args::DiskCipher::XtsAes256 => disk_crypt_resources::Cipher::XtsAes256,
                },
                key: fs_err::read(key_file).context("failed to read key file")?,
            })),
            DiskCliKind::Sqlite {
                path,
                create_with_len,
            } => {
                // FUTURE: this code should be responsible for opening
                // file-handle(s) itself, and passing them into sqlite via a custom
                // vfs. For now though - simply check if the file exists or not, and
                // perform early validation of filesystem-level create options.
                match (create_with_len.is_some(), path.exists()) {
                    (true, true) => anyhow::bail!(
                        "cannot create new sqlite disk at {} - file already exists",
                        path.display()
                    ),
                    (false, false) => anyhow::bail!(
                        "cannot open sqlite disk at {} - file not found",
                        path.display()
                    ),
                    _ => {}
                }

                layers.push(layer(SqliteDiskLayerHandle {
                    dbhd_path: path.display().to_string(),
                    format_dbhd: create_with_len.map(|len| {
                        disk_backend_resources::layer::SqliteDiskLayerFormatParams {
                            logically_read_only: false,
                            len: Some(len),
                        }
                    }),
                }));
            }
            DiskCliKind::SqliteDiff { path, create, disk } => {
                // FUTURE: this code should be responsible for opening
                // file-handle(s) itself, and passing them into sqlite via a custom
                // vfs. For now though - simply check if the file exists or not, and
                // perform early validation of filesystem-level create options.
                match (create, path.exists()) {
                    (true, true) => anyhow::bail!(
                        "cannot create new sqlite disk at {} - file already exists",
                        path.display()
                    ),
                    (false, false) => anyhow::bail!(
                        "cannot open sqlite disk at {} - file not found",
                        path.display()
                    ),
                    _ => {}
                }

                layers.push(layer(SqliteDiskLayerHandle {
                    dbhd_path: path.display().to_string(),
                    format_dbhd: create.then_some(
                        disk_backend_resources::layer::SqliteDiskLayerFormatParams {
                            logically_read_only: false,
                            len: None,
                        },
                    ),
                }));
                disk_open_inner(disk, true, layers).await?;
            }
            DiskCliKind::AutoCacheSqlite {
                cache_path,
                key,
                disk,
            } => {
                layers.push(LayerOrDisk::Layer(DiskLayerDescription {
                    read_cache: true,
                    write_through: false,
                    layer: SqliteAutoCacheDiskLayerHandle {
                        cache_path: cache_path.clone(),
                        cache_key: key.clone(),
                    }
                    .into_resource(),
                }));
                disk_open_inner(disk, read_only, layers).await?;
            }
        }
        Ok(())
    })
}

/// Get the system page size.
pub(crate) fn system_page_size() -> u32 {
    sparse_mmap::SparseMapping::page_size() as u32
}

/// The guest architecture string, derived from the compile-time `guest_arch` cfg.
pub(crate) const GUEST_ARCH: &str = if cfg!(guest_arch = "x86_64") {
    "x86_64"
} else {
    "aarch64"
};

/// Open a snapshot directory and validate it against the current VM config.
/// Returns the shared memory handle, lifetime guards, and saved device state.
pub(crate) struct PreparedSnapshotRestore {
    shared_memory: openvmm_defs::worker::SharedMemoryFd,
    guards: openvmm_defs::worker::SnapshotRestoreGuards,
    saved_state: mesh::payload::message::ProtobufMessage,
    restore_time: Option<(Duration, u64, Option<u64>, Vec<u8>)>,
}

fn prepare_snapshot_restore(
    snapshot: openvmm_helpers::snapshot::OpenedSnapshot,
    opt: &Options,
    expected_hypervisor: &str,
    effective_command_line: Option<&str>,
    network: Option<(
        &openvmm_defs::config::MicrovmNetworkConfig,
        &net_backend_resources::egress::EgressPolicy,
        &openvmm_helpers::snapshot::SnapshotAttachment,
    )>,
    filesystem: Option<(
        &openvmm_defs::config::MicrovmFilesystemConfig,
        &Path,
        &openvmm_helpers::snapshot::SnapshotAttachment,
    )>,
    console_attachment: Option<&openvmm_helpers::snapshot::SnapshotAttachment>,
    sandbox_block_sources: &[storage_builder::MicrovmSandboxBlockSource],
) -> anyhow::Result<PreparedSnapshotRestore> {
    let base_memory_size = snapshot.manifest().memory_size_bytes;
    let manifest = snapshot.manifest();
    let expected_microvm_contract = if opt.machine == MachineProfileCli::Microvm {
        let scratch_policy = if manifest
            .machine_contract
            .as_ref()
            .and_then(|contract| contract.microvm_sandbox_blocks.last())
            .is_some_and(|block| block.artifact == openvmm_helpers::snapshot::SCRATCH_FILE_NAME)
        {
            chipset_resources::microvm::MicrovmSnapshotScratchPolicy::Paired
        } else {
            chipset_resources::microvm::MicrovmSnapshotScratchPolicy::Fresh
        };
        let sandbox_blocks =
            storage_builder::snapshot_block_contract(sandbox_block_sources, scratch_policy)?;
        Some((
            expected_hypervisor,
            effective_command_line
                .context("microVM restore requires an effective PVH command line")?,
            network,
            filesystem,
            console_attachment,
            sandbox_blocks,
        ))
    } else {
        None
    };
    prepare_snapshot_restore_for_config(
        snapshot,
        base_memory_size,
        opt.memory_size(),
        opt.processors,
        expected_microvm_contract,
    )
}

const MAX_SNAPSHOT_DOWNTIME: Duration = Duration::from_secs(30 * 24 * 60 * 60);

fn calculate_snapshot_downtime(
    capture_time: std::time::SystemTime,
    destination_time: std::time::SystemTime,
) -> anyhow::Result<Duration> {
    let downtime = destination_time
        .duration_since(capture_time)
        .context("destination wall clock is before snapshot capture time")?;
    anyhow::ensure!(
        downtime <= MAX_SNAPSHOT_DOWNTIME,
        "snapshot host downtime exceeds the supported 30-day bound"
    );
    Ok(downtime)
}

fn microvm_restore_packet(
    entropy: &[u8; 64],
    restore_online_vp_count: Option<u32>,
    restore_memory_target_requested: bool,
    restore_memory_ranges: &[openvmm_helpers::snapshot::SnapshotMemoryExpansionRange],
) -> anyhow::Result<Vec<u8>> {
    anyhow::ensure!(
        restore_memory_target_requested || restore_memory_ranges.is_empty(),
        "restore memory ranges require an explicit memory target"
    );
    let mut packet = if restore_memory_target_requested {
        // V3 is: 19-byte NUL-terminated header, u8 online-VP target (zero
        // means absent), u8 range count, repeated little-endian (u64 GPA,
        // u64 length) pairs, and 64 bytes of entropy. The exact GPA ranges
        // keep guest repair independent of the host's layout implementation.
        let range_count = u8::try_from(restore_memory_ranges.len())
            .context("restore memory range count does not fit in u8")?;
        let mut packet = b"OPENVMM_ENTROPY_V3\0".to_vec();
        let online_vp_count = restore_online_vp_count
            .map(u8::try_from)
            .transpose()
            .context("restore-online VP count does not fit in u8")?
            .unwrap_or(0);
        packet.push(online_vp_count);
        packet.push(range_count);
        for range in restore_memory_ranges {
            anyhow::ensure!(range.length != 0, "restore memory range is empty");
            range
                .gpa_start
                .checked_add(range.length)
                .context("restore memory range overflows GPA space")?;
            packet.extend_from_slice(&range.gpa_start.to_le_bytes());
            packet.extend_from_slice(&range.length.to_le_bytes());
        }
        packet
    } else if let Some(count) = restore_online_vp_count {
        let count = u8::try_from(count).context("restore-online VP count does not fit in u8")?;
        let mut packet = b"OPENVMM_ENTROPY_V2\0".to_vec();
        packet.push(count);
        packet
    } else {
        b"OPENVMM_ENTROPY_V1\0".to_vec()
    };
    packet.extend(entropy);
    Ok(packet)
}

fn microvm_generation_id(entropy: &[u8; 64]) -> [u8; 16] {
    let mut generation_id = [0; 16];
    generation_id.copy_from_slice(&entropy[..16]);
    generation_id
}

fn fresh_microvm_generation_id() -> anyhow::Result<[u8; 16]> {
    let mut generation_id = [0; 16];
    getrandom::fill(&mut generation_id).context("failed to generate microVM generation ID")?;
    Ok(generation_id)
}

pub(crate) fn fresh_microvm_restore_packet(
    restore_online_vp_count: Option<u32>,
    restore_memory_target_requested: bool,
    restore_memory_ranges: &[openvmm_helpers::snapshot::SnapshotMemoryExpansionRange],
) -> anyhow::Result<([u8; 16], Vec<u8>)> {
    let generation_id_create = openvmm_defs::profile::ProfileSpan::start();
    let mut entropy = [0_u8; 64];
    getrandom::fill(&mut entropy).context("failed to generate restore entropy")?;
    let generation_id = microvm_generation_id(&entropy);
    let packet = microvm_restore_packet(
        &entropy,
        restore_online_vp_count,
        restore_memory_target_requested,
        restore_memory_ranges,
    )?;
    generation_id_create.complete("restore", "generation_id_create", Default::default());
    Ok((generation_id, packet))
}

#[cfg(test)]
mod restore_packet_tests {
    use super::microvm_generation_id;
    use super::microvm_restore_packet;
    use openvmm_helpers::snapshot::SnapshotMemoryExpansionRange;

    #[test]
    fn restore_packet_versions_preserve_entropy_and_online_target() {
        let entropy = [0x5a; 64];
        assert_eq!(microvm_generation_id(&entropy), [0x5a; 16]);
        let v1 = microvm_restore_packet(&entropy, None, false, &[]).unwrap();
        assert_eq!(&v1[..19], b"OPENVMM_ENTROPY_V1\0");
        assert_eq!(&v1[19..], &entropy);

        let v2 = microvm_restore_packet(&entropy, Some(8), false, &[]).unwrap();
        assert_eq!(&v2[..19], b"OPENVMM_ENTROPY_V2\0");
        assert_eq!(v2[19], 8);
        assert_eq!(&v2[20..], &entropy);

        let ranges = [
            SnapshotMemoryExpansionRange {
                gpa_start: 0x2000_0000,
                length: 0x2000_0000,
            },
            SnapshotMemoryExpansionRange {
                gpa_start: 0x1_0000_0000,
                length: 0x4000_0000,
            },
        ];
        let v3 = microvm_restore_packet(&entropy, Some(4), true, &ranges).unwrap();
        assert_eq!(&v3[..19], b"OPENVMM_ENTROPY_V3\0");
        assert_eq!(v3[19], 4);
        assert_eq!(v3[20], 2);
        assert_eq!(
            &v3[21..37],
            &[
                ranges[0].gpa_start.to_le_bytes(),
                ranges[0].length.to_le_bytes(),
            ]
            .concat()
        );
        assert_eq!(
            &v3[37..53],
            &[
                ranges[1].gpa_start.to_le_bytes(),
                ranges[1].length.to_le_bytes(),
            ]
            .concat()
        );
        assert_eq!(&v3[53..], &entropy);

        let v3_without_cpu = microvm_restore_packet(&entropy, None, true, &ranges[..1]).unwrap();
        assert_eq!(&v3_without_cpu[..19], b"OPENVMM_ENTROPY_V3\0");
        assert_eq!(v3_without_cpu[19], 0);
        assert_eq!(v3_without_cpu[20], 1);
        assert_eq!(
            &v3_without_cpu[21..37],
            &[
                ranges[0].gpa_start.to_le_bytes(),
                ranges[0].length.to_le_bytes(),
            ]
            .concat()
        );
        assert_eq!(&v3_without_cpu[37..], &entropy);

        let v3_explicit_base = microvm_restore_packet(&entropy, None, true, &[]).unwrap();
        assert_eq!(&v3_explicit_base[..19], b"OPENVMM_ENTROPY_V3\0");
        assert_eq!(v3_explicit_base[19], 0);
        assert_eq!(v3_explicit_base[20], 0);
        assert_eq!(&v3_explicit_base[21..], &entropy);

        assert!(microvm_restore_packet(&entropy, None, false, &ranges[..1]).is_err());
    }
}

fn align_legacy_network_policy_contract(
    saved: &openvmm_helpers::snapshot::SnapshotMachineContract,
    expected: &mut openvmm_helpers::snapshot::SnapshotMachineContract,
) {
    let (Some(saved), Some(expected)) = (
        saved.microvm_network.as_ref(),
        expected.microvm_network.as_mut(),
    ) else {
        return;
    };
    if matches!(saved.egress_policy_encoding_version, 0 | 1) {
        expected.egress_policy_encoding_version = saved.egress_policy_encoding_version;
        expected
            .egress_policy_sha256
            .clone_from(&saved.egress_policy_sha256);
    }
}

pub(crate) fn prepare_snapshot_restore_for_config(
    snapshot: openvmm_helpers::snapshot::OpenedSnapshot,
    expected_memory_size: u64,
    selected_memory_size: u64,
    expected_vp_count: u32,
    expected_microvm_contract: Option<(
        &str,
        &str,
        Option<(
            &openvmm_defs::config::MicrovmNetworkConfig,
            &net_backend_resources::egress::EgressPolicy,
            &openvmm_helpers::snapshot::SnapshotAttachment,
        )>,
        Option<(
            &openvmm_defs::config::MicrovmFilesystemConfig,
            &Path,
            &openvmm_helpers::snapshot::SnapshotAttachment,
        )>,
        Option<&openvmm_helpers::snapshot::SnapshotAttachment>,
        Vec<openvmm_helpers::snapshot::SnapshotMicrovmSandboxBlock>,
    )>,
) -> anyhow::Result<PreparedSnapshotRestore> {
    let artifact_prepare = openvmm_defs::profile::ProfileSpan::start();
    let manifest = snapshot.manifest();
    // Validate manifest against current VM config.
    openvmm_helpers::snapshot::validate_manifest(
        manifest,
        GUEST_ARCH,
        expected_memory_size,
        expected_vp_count,
        system_page_size(),
    )?;
    let restore_time = if let Some((
        expected_hypervisor,
        effective_command_line,
        network,
        filesystem,
        console_attachment,
        sandbox_blocks,
    )) = expected_microvm_contract
    {
        let saved_contract = manifest
            .machine_contract
            .as_ref()
            .context("microVM snapshot is missing its authoritative machine contract")?;
        let network =
            network.map(|(config, policy, attachment)| (config, policy, attachment.clone()));
        let filesystem_slot = microvm_filesystem_slot_from_snapshot(saved_contract)?;
        let filesystem = saved_contract
            .microvm_filesystem
            .as_ref()
            .and(filesystem)
            .map(|(config, root_path, attachment)| (config, root_path, attachment.clone()));
        let mut expected_contract = openvmm_helpers::snapshot::microvm_machine_contract(
            expected_hypervisor,
            effective_command_line.to_owned(),
            network,
            filesystem_slot,
            filesystem,
            console_attachment.cloned(),
            sandbox_blocks,
            expected_vp_count,
            expected_memory_size,
            (saved_contract.memory_expansion_version != 0)
                .then_some(saved_contract.memory_capacity_bytes),
            saved_contract.state_unit_names.clone(),
            saved_contract.capture_wall_clock,
            saved_contract.tsc_frequency_hz,
            saved_contract.apic_frequency_hz,
            saved_contract.cpu_contract.clone(),
        )?;
        align_legacy_network_policy_contract(saved_contract, &mut expected_contract);
        openvmm_helpers::snapshot::validate_microvm_machine_contract(manifest, &expected_contract)?;
        let capture_time: std::time::SystemTime = saved_contract
            .capture_wall_clock
            .try_into()
            .context("snapshot capture wall clock is invalid")?;
        let downtime = calculate_snapshot_downtime(capture_time, std::time::SystemTime::now())?;
        Some((
            downtime,
            saved_contract.tsc_frequency_hz,
            saved_contract.apic_frequency_hz,
            saved_contract.cpu_contract.clone(),
        ))
    } else {
        None
    };

    // The manifest and state.bin inventories describe the same machine boundary.
    // Require them to agree before worker and partition construction.
    let state_msg: mesh::payload::message::ProtobufMessage =
        mesh::payload::decode(snapshot.state_bytes())
            .context("failed to decode saved state from snapshot")?;
    if let Some(contract) = &manifest.machine_contract {
        let inventory_msg: mesh::payload::message::ProtobufMessage =
            mesh::payload::decode(snapshot.state_bytes())
                .context("failed to decode saved state inventory from snapshot")?;
        let saved_state: openvmm_defs::worker::SavedState = inventory_msg
            .parse()
            .context("failed to parse saved state inventory from snapshot")?;
        anyhow::ensure!(
            saved_state.inventory == contract.state_unit_names,
            "snapshot manifest state-unit inventory does not match state.bin"
        );
    }

    snapshot.claim_for_restore()?;

    artifact_prepare.complete(
        "restore",
        "artifact_prepare",
        openvmm_defs::profile::ProfileCounters {
            logical_bytes: Some(selected_memory_size),
            ..Default::default()
        },
    );

    // Create the private mapping from a duplicate of the exact opened handle.
    // The original file and directory handles move to the worker and keep this
    // generation pinned until VM teardown.
    let cow_section_create = openvmm_defs::profile::ProfileSpan::start();
    let memory_file = snapshot.duplicate_memory_file_for_mapping(expected_memory_size)?;
    let shared_memory =
        openvmm_helpers::shared_memory::file_to_copy_on_write_memory_fd(memory_file)?;
    cow_section_create.complete(
        "restore",
        "cow_section_create",
        openvmm_defs::profile::ProfileCounters {
            logical_bytes: Some(expected_memory_size),
            ..Default::default()
        },
    );
    snapshot.validate_memory_generation(expected_memory_size)?;
    let (_, _, guards) = snapshot.into_parts();

    Ok(PreparedSnapshotRestore {
        shared_memory,
        guards,
        saved_state: state_msg,
        restore_time,
    })
}

fn do_main(pidfile_guard: &mut Option<pidfile::Pidfile>) -> anyhow::Result<i32> {
    openvmm_defs::profile::initialize();
    #[cfg(windows)]
    pal::windows::disable_hard_error_dialog();

    tracing_init::enable_tracing()?;

    // Try to run as a worker host.
    // On success the worker runs to completion and then exits the process (does
    // not return). Any worker host setup errors are return and bubbled up.
    meshworker::run_vmm_mesh_host()?;

    let opt = cli_args::parse_options();
    if let Some(path) = &opt.write_saved_state_proto {
        mesh::payload::protofile::DescriptorWriter::new(vmcore::save_restore::saved_state_roots())
            .write_to_path(path)
            .context("failed to write protobuf descriptors")?;
        return Ok(0);
    }

    if let Some(ref path) = opt.pidfile {
        *pidfile_guard = Some(pidfile::Pidfile::new(path).context("failed to create pidfile")?);
    }

    if let Some(path) = opt.relay_console_path {
        let console_title = opt.relay_console_title.unwrap_or_default();
        return console_relay::relay_console(&path, console_title.as_str()).map(|()| 0);
    }

    #[cfg(any(feature = "grpc", feature = "ttrpc"))]
    {
        let rpc = opt
            .rpc
            .as_ref()
            .map(|rpc| {
                let transport = match rpc.transport {
                    cli_args::RpcTransportCli::Auto => ttrpc::RpcTransport::Auto,
                    cli_args::RpcTransportCli::Ttrpc => ttrpc::RpcTransport::Ttrpc,
                    cli_args::RpcTransportCli::Grpc => ttrpc::RpcTransport::Grpc,
                };
                (rpc.path.as_path(), transport)
            })
            .or_else(|| {
                opt.ttrpc
                    .as_deref()
                    .map(|p| (p, ttrpc::RpcTransport::Ttrpc))
            })
            .or_else(|| opt.grpc.as_deref().map(|p| (p, ttrpc::RpcTransport::Grpc)));

        if let Some((path, transport)) = rpc {
            return block_on(async {
                let _ = std::fs::remove_file(path);
                let listener =
                    unix_socket::UnixListener::bind(path).context("failed to bind to socket")?;

                // This is a local launch
                let mut handle =
                    mesh_worker::launch_local_worker::<ttrpc::TtrpcWorker>(ttrpc::Parameters {
                        listener,
                        transport,
                    })
                    .await?;

                tracing::info!(%transport, path = %path.display(), "listening");

                // Signal the parent process that the server is ready.
                pal::close_stdout().context("failed to close stdout")?;

                handle.join().await?;

                Ok(0)
            });
        }
    }

    DefaultPool::run_with(async |driver| run_control(&driver, opt).await)
}

fn new_hvsock_service_id(port: u32) -> Guid {
    // This GUID is an embedding of the AF_VSOCK port into an
    // AF_HYPERV service ID.
    Guid {
        data1: port,
        .."00000000-facb-11e6-bd58-64006a7986d3".parse().unwrap()
    }
}

async fn run_control(driver: &DefaultDriver, opt: Options) -> anyhow::Result<i32> {
    let mut mesh = Some(VmmMesh::new(&driver, opt.single_process)?);
    let result = run_control_inner(driver, &mut mesh, opt).await;
    // If setup failed before the mesh was handed to the controller, shut it
    // down so the child host process exits cleanly without noisy logs.
    if let Some(mesh) = mesh {
        mesh.shutdown().await;
    }
    result
}

async fn run_control_inner(
    driver: &DefaultDriver,
    mesh_slot: &mut Option<VmmMesh>,
    mut opt: Options,
) -> anyhow::Result<i32> {
    let mesh = mesh_slot.as_ref().unwrap();
    let mut private_scratch_dir = None;
    let mut restore_gate_required = false;
    let restore_memory_target_requested = opt.restore_memory.is_some();
    let mut restore_memory_ranges = Vec::new();
    let artifact_open = openvmm_defs::profile::ProfileSpan::start();
    let mut restore_snapshot = opt
        .restore_snapshot
        .as_deref()
        .map(openvmm_helpers::snapshot::OpenedSnapshot::open)
        .transpose()?;
    if opt.restore_snapshot.is_some() {
        let counters = if openvmm_defs::profile::enabled() {
            restore_snapshot
                .as_ref()
                .and_then(|snapshot| snapshot.artifact_size_counters().ok())
                .map(
                    |(logical_bytes, allocated_bytes)| openvmm_defs::profile::ProfileCounters {
                        logical_bytes: Some(logical_bytes),
                        allocated_bytes: Some(allocated_bytes),
                        ..Default::default()
                    },
                )
                .unwrap_or_default()
        } else {
            Default::default()
        };
        artifact_open.complete("restore", "artifact_open", counters);
    }
    if let Some(contract) = restore_snapshot
        .as_ref()
        .and_then(|snapshot| snapshot.manifest().machine_contract.as_ref())
        && contract.machine_profile == "microvm"
    {
        anyhow::ensure!(
            opt.machine == MachineProfileCli::Microvm,
            "microVM snapshot restore requires --machine microvm"
        );
        openvmm_helpers::snapshot::validate_supported_microvm_contract(contract)?;
    }
    let restore_machine_contract = if opt.machine == MachineProfileCli::Microvm
        && let Some(snapshot) = restore_snapshot.as_ref()
    {
        let manifest = snapshot.manifest();
        let snapshot_dir = opt
            .restore_snapshot
            .as_deref()
            .expect("restore manifest requires a snapshot path");
        restore_gate_required = openvmm_helpers::snapshot::requires_post_restore_gate(manifest);
        let contract = manifest
            .machine_contract
            .as_ref()
            .context("microVM snapshot is missing its authoritative machine contract")?;
        anyhow::ensure!(
            contract.machine_profile == "microvm",
            "snapshot machine profile does not match the requested microVM machine"
        );
        openvmm_helpers::snapshot::validate_supported_microvm_contract(contract)?;
        if let Some(restore_processors) = opt.restore_processors {
            openvmm_helpers::snapshot::validate_restore_online_vp_count(
                manifest,
                restore_processors,
            )?;
            restore_gate_required = true;
        }
        let restore_memory_size = opt
            .restore_memory
            .map(|memory| memory.0)
            .unwrap_or(manifest.memory_size_bytes);
        if restore_memory_target_requested {
            restore_memory_ranges = openvmm_helpers::snapshot::validate_restore_memory_target(
                manifest,
                restore_memory_size,
            )?;
        }
        if !restore_memory_ranges.is_empty() {
            restore_gate_required = true;
        }
        anyhow::ensure!(
            opt.cmdline.is_empty(),
            "restore-time command-line overrides are not allowed"
        );
        anyhow::ensure!(
            opt.memory == Default::default()
                && !opt.deprecated_private_memory
                && !opt.deprecated_prefetch
                && !opt.deprecated_thp
                && opt.deprecated_memory_backing_file.is_none(),
            "restore-time memory overrides are not allowed"
        );
        opt.memory.size = Some(vmm_cli::MemorySize(restore_memory_size));
        if !contract.microvm_sandbox_blocks.is_empty() {
            let scratch = contract
                .microvm_sandbox_blocks
                .last()
                .filter(|block| block.role == "scratch")
                .context("microVM snapshot is missing its scratch contract")?;
            if scratch.artifact == openvmm_helpers::snapshot::SCRATCH_FILE_NAME {
                anyhow::ensure!(
                    !opt.microvm_sandbox_block.iter().any(|block| {
                        block.role == openvmm_defs::config::MicrovmSandboxBlockRole::Scratch
                    }),
                    "paired snapshot restore supplies scratch.img; do not pass a scratch block"
                );
                let source = snapshot
                    .open_paired_scratch_file()?
                    .context("paired snapshot is missing scratch.img")?;
                let parent = snapshot_dir
                    .parent()
                    .filter(|parent| !parent.as_os_str().is_empty())
                    .unwrap_or(Path::new("."));
                let temp_dir = tempfile::Builder::new()
                    .prefix(".openvmm-private-scratch-")
                    .tempdir_in(parent)
                    .context("failed to create private restore scratch directory")?;
                let private_path = temp_dir
                    .path()
                    .join(openvmm_helpers::snapshot::SCRATCH_FILE_NAME);
                openvmm_helpers::snapshot::copy_verified_file(
                    &source,
                    &private_path,
                    scratch.length,
                    &scratch.identity,
                    openvmm_helpers::snapshot::SCRATCH_FILE_NAME,
                )?;
                opt.microvm_sandbox_block
                    .push(cli_args::MicrovmSandboxBlockCli {
                        role: openvmm_defs::config::MicrovmSandboxBlockRole::Scratch,
                        disk: cli_args::DiskCli {
                            vtl: DeviceVtl::Vtl0,
                            kind: DiskCliKind::File {
                                path: private_path,
                                create_with_len: None,
                                direct: false,
                            },
                            read_only: false,
                            is_dvd: false,
                            underhill: None,
                            pcie_port: None,
                            controller: None,
                            nsid: None,
                            lun: None,
                            relay: None,
                        },
                    });
                private_scratch_dir = Some(temp_dir);
            } else {
                anyhow::ensure!(
                    opt.microvm_sandbox_block.iter().any(|block| {
                        block.role == openvmm_defs::config::MicrovmSandboxBlockRole::Scratch
                    }),
                    "fresh-scratch snapshot restore requires a scratch block"
                );
            }
        }
        Some(contract)
    } else {
        None
    };
    if restore_gate_required || opt.restore_processors.is_some() {
        opt.restore_entropy = true;
    }
    if restore_machine_contract.is_some()
        && !opt.restore_entropy
        && !restore_memory_target_requested
    {
        tracing::warn!(
            "restoring cloned guest RNG state without fresh entropy injection; cryptographic workloads are unsafe"
        );
    }
    let (mut vm_config, mut resources) = vm_config_from_command_line(
        driver,
        mesh,
        &opt,
        restore_machine_contract,
        restore_memory_target_requested,
        &restore_memory_ranges,
    )
    .await?;
    let effective_command_line = match &vm_config.load_mode {
        LoadMode::Pvh { cmdline, .. } => Some(cmdline.clone()),
        _ => None,
    };
    let microvm_sandbox_block_sources =
        std::mem::take(&mut resources.microvm_sandbox_block_sources);
    let microvm_console_attachment = resources.microvm_console_attachment.clone();
    let microvm_network = vm_config.microvm_network.clone();
    let microvm_network_attachment = resources.microvm_network_attachment.clone();
    let microvm_egress_policy = resources.microvm_egress_policy.clone();
    let microvm_filesystem_slot = vm_config
        .virtio_devices
        .iter()
        .any(|(_, device)| device.id() == "virtiofs");
    let microvm_filesystem = vm_config.microvm_filesystem.clone();
    let microvm_filesystem_root_path = resources.microvm_filesystem_root_path.clone();
    let microvm_filesystem_attachment = resources.microvm_filesystem_attachment.clone();

    let snapshot_destination = opt.snapshot_destination.as_ref().map(|path| {
        if path.is_absolute() {
            path.clone()
        } else {
            std::env::current_dir().unwrap_or_default().join(path)
        }
    });
    if let Some(root_path) = resources.microvm_filesystem_root_path.as_deref() {
        validate_microvm_filesystem_private_storage(
            root_path,
            snapshot_destination.as_deref(),
            opt.restore_snapshot.as_deref(),
            opt.memory_backing_file().map(PathBuf::as_path),
        )?;
    }
    let snapshot_memory_file = if let Some(destination) = &snapshot_destination
        && opt.memory_backing_file().is_none()
    {
        let parent = destination
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let parent_metadata = fs_err::symlink_metadata(parent)
            .with_context(|| format!("failed to inspect snapshot parent {}", parent.display()))?;
        anyhow::ensure!(
            parent_metadata.file_type().is_dir(),
            "snapshot parent is not a directory: {}",
            parent.display()
        );
        anyhow::ensure!(
            fs_err::symlink_metadata(destination)
                .is_err_and(|error| { error.kind() == io::ErrorKind::NotFound }),
            "snapshot destination already exists or cannot be inspected: {}",
            destination.display()
        );
        let file = tempfile::Builder::new()
            .prefix(".openvmm-microvm-memory-")
            .tempfile_in(parent)
            .context("failed to create snapshot memory backing")?;
        openvmm_helpers::snapshot::initialize_snapshot_memory_backing_file(
            file.as_file(),
            opt.memory_size(),
        )
        .context("failed to initialize snapshot memory backing")?;
        Some(file)
    } else {
        None
    };
    let snapshot_memory_handle = if snapshot_destination.is_some() {
        if let Some(file) = &snapshot_memory_file {
            Some(
                file.reopen()
                    .context("failed to duplicate automatic snapshot RAM handle")?,
            )
        } else {
            opt.memory_backing_file()
                .map(|path| {
                    openvmm_helpers::shared_memory::open_memory_backing_file_handle(
                        path,
                        opt.memory_size(),
                    )
                })
                .transpose()?
        }
    } else {
        None
    };

    let mut vnc_worker = None;
    if opt.gfx || opt.vnc.vnc {
        // Parse the listen address. Try as a full SocketAddr (host:port) first;
        // fall back to a bare IP, using the configured port.
        let addr: std::net::SocketAddr = if let Ok(sa) =
            opt.vnc.vnc_listen.parse::<std::net::SocketAddr>()
        {
            sa
        } else {
            let ip: std::net::IpAddr = opt.vnc.vnc_listen.parse().with_context(|| {
                format!(
                    "invalid VNC listen address: {} (expected IP address or socket address like [::1]:5900)",
                    opt.vnc.vnc_listen
                )
            })?;
            std::net::SocketAddr::new(ip, opt.vnc.vnc_port)
        };

        let socket = socket2::Socket::new(
            if addr.is_ipv6() {
                socket2::Domain::IPV6
            } else {
                socket2::Domain::IPV4
            },
            socket2::Type::STREAM,
            None,
        )
        .with_context(|| format!("creating VNC socket for {}", addr))?;

        if addr.is_ipv6() {
            if let Err(e) = socket.set_only_v6(false) {
                tracing::warn!(
                    error = %e,
                    "failed to enable dual-stack on IPv6 VNC socket, IPv4 clients may not be able to connect"
                );
            }
        }
        socket.set_reuse_address(true)?;
        socket
            .bind(&addr.into())
            .with_context(|| format!("binding VNC socket to {}", addr))?;
        socket
            .listen(128)
            .with_context(|| format!("listening on VNC socket {}", addr))?;
        let listener: TcpListener = socket.into();

        if !addr.ip().is_loopback() {
            tracing::warn!(
                address = %addr,
                "VNC server listening on non-localhost address without authentication"
            );
        }

        let input_send = vm_config.input.sender();
        let framebuffer = resources
            .framebuffer_access
            .take()
            .expect("synth video enabled");

        let vnc_host = mesh
            .make_host("vnc", None)
            .await
            .context("spawning vnc process failed")?;

        vnc_worker = Some(
            vnc_host
                .launch_worker(
                    vnc_worker_defs::VNC_WORKER_TCP,
                    VncParameters {
                        listener,
                        framebuffer,
                        input_send,
                        dirty_recv: resources.dirty_rect_recv.take(),
                        max_clients: opt.vnc.vnc_max_clients,
                        evict_oldest: opt.vnc.vnc_evict_oldest,
                    },
                )
                .await?,
        )
    }

    // spin up the debug worker
    let gdb_worker = if let Some(port) = opt.gdb {
        let listener = TcpListener::bind(format!("127.0.0.1:{}", port))
            .with_context(|| format!("binding to gdb port {}", port))?;

        let (req_tx, req_rx) = mesh::channel();
        vm_config.debugger_rpc = Some(req_rx);

        let gdb_host = mesh
            .make_host("gdb", None)
            .await
            .context("spawning gdbstub process failed")?;

        Some(
            gdb_host
                .launch_worker(
                    debug_worker_defs::DEBUGGER_WORKER,
                    debug_worker_defs::DebuggerParameters {
                        listener,
                        req_chan: req_tx,
                        vp_count: vm_config.processor_topology.proc_count,
                        target_arch: if cfg!(guest_arch = "x86_64") {
                            debug_worker_defs::TargetArch::X86_64
                        } else {
                            debug_worker_defs::TargetArch::Aarch64
                        },
                    },
                )
                .await
                .context("failed to launch gdbstub worker")?,
        )
    } else {
        None
    };

    // spin up the VM
    let (vm_rpc, rpc_recv) = mesh::channel();
    let (notify_send, notify_recv) = mesh::channel();
    let snapshot_boundary_requests = resources.microvm_snapshot_requests.take();
    let (snapshot_ready, snapshot_requests) = if snapshot_boundary_requests.is_some() {
        let (ready, requests) = mesh::channel();
        (Some(ready), Some(requests))
    } else {
        (None, None)
    };
    let hypervisor = match &opt.hypervisor {
        Some(name) => openvmm_helpers::hypervisor::hypervisor_resource(name)?,
        None if opt.machine == MachineProfileCli::Microvm => {
            openvmm_helpers::hypervisor::choose_microvm_hypervisor()?
        }
        None => openvmm_helpers::hypervisor::choose_hypervisor()?,
    };
    let source_hypervisor = hypervisor.id().to_owned();
    let vm_worker = {
        let vm_host = mesh.make_host("vm", opt.log_file.clone()).await?;

        let (
            shared_memory,
            saved_state,
            shared_memory_copy_on_write,
            restore_time,
            snapshot_restore_guards,
        ) = if opt.restore_snapshot.is_some() {
            let prepared = prepare_snapshot_restore(
                restore_snapshot
                    .take()
                    .context("snapshot restore is missing its opened generation")?,
                &opt,
                &source_hypervisor,
                effective_command_line.as_deref(),
                microvm_network
                    .as_ref()
                    .zip(microvm_egress_policy.as_ref())
                    .zip(microvm_network_attachment.as_ref())
                    .map(|((network, policy), attachment)| (network, policy, attachment)),
                microvm_filesystem
                    .as_ref()
                    .zip(microvm_filesystem_root_path.as_deref())
                    .zip(microvm_filesystem_attachment.as_ref())
                    .map(|((filesystem, root_path), attachment)| {
                        (filesystem, root_path, attachment)
                    }),
                microvm_console_attachment.as_ref(),
                &microvm_sandbox_block_sources,
            )?;
            (
                Some(prepared.shared_memory),
                Some(prepared.saved_state),
                true,
                prepared.restore_time,
                Some(prepared.guards),
            )
        } else if let Some(file) = &snapshot_memory_handle {
            let file = file
                .try_clone()
                .context("failed to duplicate snapshot RAM handle for worker")?;
            let shared_memory = openvmm_helpers::shared_memory::file_to_shared_memory_fd(file)?;
            (Some(shared_memory), None, false, None, None)
        } else {
            let shared_memory = opt
                .memory_backing_file()
                .map(|path| {
                    openvmm_helpers::shared_memory::open_memory_backing_file(
                        path,
                        opt.memory_size(),
                    )
                })
                .transpose()?;
            (shared_memory, None, false, None, None)
        };
        let restore_ready_sink = opt
            .restore_ready_path
            .as_deref()
            .map(serial_io::connect_restore_ready_sink)
            .transpose()
            .context("failed to connect restore readiness endpoint")?;

        let params = VmWorkerParameters {
            hypervisor,
            cfg: vm_config,
            saved_state,
            shared_memory,
            shared_memory_copy_on_write,
            snapshot_restore_guards,
            snapshot_boundary_requests,
            snapshot_ready,
            restore_downtime: restore_time.as_ref().map(|(downtime, _, _, _)| *downtime),
            restore_tsc_frequency_hz: restore_time.as_ref().map(|(_, frequency, _, _)| *frequency),
            restore_apic_frequency_hz: restore_time
                .as_ref()
                .and_then(|(_, _, frequency, _)| *frequency),
            restore_cpu_contract: restore_time.map(|(_, _, _, cpu_contract)| cpu_contract),
            restore_ready_sink,
            restore_gate_timeout: restore_gate_required
                .then_some(Duration::from_millis(opt.restore_gate_timeout_ms)),
            restore_vp_count: opt.restore_processors,
            rpc: rpc_recv,
            notify: notify_send,
        };
        let worker_launch = openvmm_defs::profile::ProfileSpan::start();
        let worker = vm_host
            .launch_worker(VM_WORKER, params)
            .await
            .context("failed to launch vm worker")?;
        worker_launch.complete_milestone("startup", "worker_launch", Default::default());
        worker
    };

    if opt.restore_snapshot.is_some() {
        tracing::info!("restoring VM from snapshot");
    }

    if !opt.paused {
        anyhow::ensure!(
            vm_rpc.call_failable(VmRpc::Resume, ()).await?,
            "VM failed to start; inspect the worker log for the device startup error"
        );
    }

    let paravisor_diag = Arc::new(diag_client::DiagClient::from_dialer(
        driver.clone(),
        DiagDialer {
            driver: driver.clone(),
            vm_rpc: vm_rpc.clone(),
            openhcl_vtl: if opt.vtl2 {
                DeviceVtl::Vtl2
            } else {
                DeviceVtl::Vtl0
            },
        },
    ));

    let diag_inspector = DiagInspector::new(driver.clone(), paravisor_diag.clone());

    // Create channels between the REPL and VmController.
    let (vm_controller_send, vm_controller_recv) = mesh::channel();
    let (vm_controller_event_send, vm_controller_event_recv) = mesh::channel();

    let has_vtl2 = resources.vtl2_settings.is_some();
    let serial_driver = resources
        .serial_driver
        .take()
        .expect("serial driver must outlive serial resources");

    // Build the VmController with exclusive resources.
    let controller = vm_controller::VmController {
        machine_profile: opt.machine.into(),
        mesh: mesh_slot.take().unwrap(),
        vm_worker,
        vnc_worker,
        gdb_worker,
        diag_inspector: Some(diag_inspector),
        vtl2_settings: resources.vtl2_settings,
        ged_rpc: resources.ged_rpc.clone(),
        vm_rpc: vm_rpc.clone(),
        paravisor_diag: Some(paravisor_diag),
        igvm_path: opt.igvm.clone(),
        memory_backing_file: opt.memory_backing_file().cloned().or_else(|| {
            snapshot_memory_file
                .as_ref()
                .map(|file| file.path().to_owned())
        }),
        snapshot_memory_handle,
        memory: opt.memory_size(),
        memory_capacity: opt.memory_capacity.map(|capacity| capacity.0),
        processors: opt.processors,
        log_file: opt.log_file.clone(),
        crash_dump_path: opt.crash_dump_path.clone(),
        snapshot_requests,
        snapshot_destination,
        snapshot_tier: opt.snapshot_tier,
        snapshot_quiesce_timeout: Duration::from_millis(opt.snapshot_quiesce_timeout_ms),
        source_hypervisor,
        effective_command_line,
        microvm_sandbox_block_sources,
        microvm_console_attachment,
        microvm_network,
        microvm_network_attachment,
        microvm_egress_policy,
        microvm_filesystem_slot,
        microvm_filesystem,
        microvm_filesystem_root_path,
        microvm_filesystem_attachment,
        microvm_console_socket_cleanup: resources.microvm_console_socket_cleanup.take(),
        snapshot_memory_file,
        _private_scratch_dir: private_scratch_dir,
        guest_power_actions: vm_controller::GuestPowerActions {
            shutdown: opt.guest_shutdown_action,
            reset: opt.guest_reset_action,
            crash: opt.guest_crash_action,
            watchdog: opt.guest_watchdog_action,
        },
    };

    // Spawn the VmController as a task.
    let controller_task = driver.spawn(
        "vm-controller",
        controller.run(vm_controller_recv, vm_controller_event_send, notify_recv),
    );

    // Run the REPL with shareable resources.
    let repl_result = repl::run_repl(
        driver,
        repl::ReplResources {
            vm_rpc,
            vm_controller: vm_controller_send,
            vm_controller_events: vm_controller_event_recv,
            restore_ready_pending: opt.paused && opt.restore_ready_path.is_some(),
            scsi_rpc: resources.scsi_rpc,
            nvme_vtl2_rpc: resources.nvme_vtl2_rpc,
            consomme_rpc: resources.consomme_rpc,
            shutdown_ic: resources.shutdown_ic,
            kvp_ic: resources.kvp_ic,
            console_in: resources.console_in,
            has_vtl2,
        },
    )
    .await;

    // Wait for the controller task to finish (it stops the VM worker and
    // shuts down the mesh).
    controller_task.await;
    drop(serial_driver);

    // run_repl returns the exit status: the code the guest drove via an opt-in
    // exit (VmControllerEvent::ExitRequested), or 0 when the VM stopped normally.
    repl_result
}

struct DiagDialer {
    driver: DefaultDriver,
    vm_rpc: mesh::Sender<VmRpc>,
    openhcl_vtl: DeviceVtl,
}

impl mesh_rpc::client::Dial for DiagDialer {
    type Stream = PolledSocket<unix_socket::UnixStream>;

    async fn dial(&mut self) -> io::Result<Self::Stream> {
        let service_id = new_hvsock_service_id(1);
        let socket = self
            .vm_rpc
            .call_failable(
                VmRpc::ConnectHvsock,
                (
                    CancelContext::new().with_timeout(Duration::from_secs(2)),
                    service_id,
                    self.openhcl_vtl,
                ),
            )
            .await
            .map_err(io::Error::other)?;

        PolledSocket::new(&self.driver, socket)
    }
}

/// An object that implements [`InspectMut`] by sending an inspect request over
/// TTRPC to the guest (typically the paravisor running in VTL2), then stitching
/// the response back into the inspect tree.
///
/// This also caches the TTRPC connection to the guest so that only the first
/// inspect request has to wait for the connection to be established.
pub(crate) struct DiagInspector(DiagInspectorInner);

enum DiagInspectorInner {
    NotStarted(DefaultDriver, Arc<diag_client::DiagClient>),
    Started {
        send: mesh::Sender<inspect::Deferred>,
        _task: Task<()>,
    },
    Invalid,
}

impl DiagInspector {
    pub fn new(driver: DefaultDriver, diag_client: Arc<diag_client::DiagClient>) -> Self {
        Self(DiagInspectorInner::NotStarted(driver, diag_client))
    }

    fn start(&mut self) -> &mesh::Sender<inspect::Deferred> {
        loop {
            match self.0 {
                DiagInspectorInner::NotStarted { .. } => {
                    let DiagInspectorInner::NotStarted(driver, client) =
                        std::mem::replace(&mut self.0, DiagInspectorInner::Invalid)
                    else {
                        unreachable!()
                    };
                    let (send, recv) = mesh::channel();
                    let task = driver.clone().spawn("diag-inspect", async move {
                        Self::run(&client, recv).await
                    });

                    self.0 = DiagInspectorInner::Started { send, _task: task };
                }
                DiagInspectorInner::Started { ref send, .. } => break send,
                DiagInspectorInner::Invalid => unreachable!(),
            }
        }
    }

    async fn run(
        diag_client: &diag_client::DiagClient,
        mut recv: mesh::Receiver<inspect::Deferred>,
    ) {
        while let Some(deferred) = recv.next().await {
            let info = deferred.external_request();
            let result = match info.request_type {
                inspect::ExternalRequestType::Inspect { depth } => {
                    if depth == 0 {
                        Ok(inspect::Node::Unevaluated)
                    } else {
                        // TODO: Support taking timeouts from the command line
                        diag_client
                            .inspect(info.path, Some(depth - 1), Some(Duration::from_secs(1)))
                            .await
                    }
                }
                inspect::ExternalRequestType::Update { value } => {
                    (diag_client.update(info.path, value).await).map(inspect::Node::Value)
                }
            };
            deferred.complete_external(
                result.unwrap_or_else(|err| {
                    inspect::Node::Failed(inspect::Error::Mesh(format!("{err:#}")))
                }),
                inspect::SensitivityLevel::Unspecified,
            )
        }
    }
}

impl InspectMut for DiagInspector {
    fn inspect_mut(&mut self, req: inspect::Request<'_>) {
        self.start().send(req.defer());
    }
}
