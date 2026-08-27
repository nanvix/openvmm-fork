// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Snapshot manifest types and I/O functions for saving/restoring VM snapshots.

use anyhow::Context;
use mesh::payload::Protobuf;
use mesh::payload::Timestamp;
use sha2::Digest;
use std::collections::HashSet;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

/// Current manifest format version. Bump when making incompatible changes.
pub const MANIFEST_VERSION: u32 = 3;
/// Magic identifying the OpenVMM snapshot manifest format.
pub const SNAPSHOT_FORMAT_MAGIC: &[u8] = b"OPENVMM_SNAPSHOT_V3\0";
const LEGACY_MANIFEST_VERSION: u32 = 2;
const LEGACY_SNAPSHOT_FORMAT_MAGIC: &[u8] = b"OPENVMM_SNAPSHOT_V2\0";
/// Saved-state schema version used by the VM worker envelope.
pub const SAVED_STATE_SCHEMA_VERSION: u32 = 1;
/// Protobuf root type stored in `state.bin`.
pub const SAVED_STATE_ROOT_TYPE: &str = "openvmm.SavedState";
/// Xen PVH memory/boot layout version owned by microVM ABI v1.
pub const MICROVM_PVH_LAYOUT_VERSION: u32 = 1;
/// Clock policy applied when a snapshot is restored.
pub const ADVANCE_BY_HOST_DOWNTIME: &str = "advance_by_host_downtime";

const MANIFEST_FILE_NAME: &str = "manifest.bin";
const STATE_FILE_NAME: &str = "state.bin";
const MEMORY_FILE_NAME: &str = "memory.bin";
const MAX_MANIFEST_SIZE_BYTES: u64 = 1024 * 1024;
const MAX_SAVED_STATE_SIZE_BYTES: u64 = 256 * 1024 * 1024;
const COPY_BUFFER_SIZE: usize = 1024 * 1024;
const SHA256_SIZE: usize = 32;
const MAX_MEMORY_RANGES: usize = 128;
const MAX_DEVICES: usize = 64;
const MAX_DEVICE_RANGES: usize = 16;
const MAX_STATE_UNITS: usize = 512;
const MAX_ATTACHMENTS: usize = 64;
const MAX_ATTACHMENT_IDENTITY_BYTES: usize = 4096;
const MAX_COMMAND_LINE_BYTES: usize = 64 * 1024;
const MAX_CPU_CONTRACT_BYTES: usize = 1024 * 1024;

static STAGING_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Error publishing a snapshot.
#[derive(Debug, thiserror::Error)]
pub enum SnapshotWriteError {
    /// Nothing was published at the final destination.
    #[error(transparent)]
    BeforeCommit(#[from] anyhow::Error),
    /// The directory rename committed, but its parent could not be flushed.
    #[error("snapshot committed to {path:?}, but flushing its parent directory failed: {error:#}")]
    Committed {
        /// Final committed snapshot path.
        path: PathBuf,
        /// Parent-directory flush failure.
        #[source]
        error: anyhow::Error,
    },
}

impl SnapshotWriteError {
    /// Returns whether the final snapshot directory has been published.
    pub fn is_committed(&self) -> bool {
        matches!(self, Self::Committed { .. })
    }
}

/// A guest RAM range and its corresponding offset in `memory.bin`.
#[derive(Clone, Debug, PartialEq, Eq, Protobuf)]
#[mesh(package = "openvmm.snapshot")]
pub struct SnapshotMemoryRange {
    /// Guest physical base address.
    #[mesh(1)]
    pub gpa_start: u64,
    /// Range length in bytes.
    #[mesh(2)]
    pub length: u64,
    /// Byte offset in `memory.bin`.
    #[mesh(3)]
    pub file_offset: u64,
}

/// Processor topology that must be reproduced during restore.
#[derive(Clone, Debug, PartialEq, Eq, Protobuf)]
#[mesh(package = "openvmm.snapshot")]
pub struct SnapshotProcessorTopology {
    /// Socket count.
    #[mesh(1)]
    pub sockets: u32,
    /// Dies per socket.
    #[mesh(2)]
    pub dies_per_socket: u32,
    /// Cores per die.
    #[mesh(3)]
    pub cores_per_die: u32,
    /// Threads per core.
    #[mesh(4)]
    pub threads_per_core: u32,
    /// APIC IDs in virtual-processor order.
    #[mesh(5)]
    pub apic_ids: Vec<u32>,
}

/// An address range owned by a saved device.
#[derive(Clone, Debug, PartialEq, Eq, Protobuf)]
#[mesh(package = "openvmm.snapshot")]
pub struct SnapshotDeviceRange {
    /// Address space (`pmio` or `mmio`).
    #[mesh(1)]
    pub address_space: String,
    /// Range base address.
    #[mesh(2)]
    pub start: u64,
    /// Range length in bytes.
    #[mesh(3)]
    pub length: u64,
}

/// Guest-visible device identity and placement.
#[derive(Clone, Debug, PartialEq, Eq, Protobuf)]
#[mesh(package = "openvmm.snapshot")]
pub struct SnapshotDevice {
    /// Stable device identity.
    #[mesh(1)]
    pub stable_id: String,
    /// State-unit identity, including units with no mutable state.
    #[mesh(2)]
    pub state_unit_name: String,
    /// Device kind.
    #[mesh(3)]
    pub kind: String,
    /// Stable device order.
    #[mesh(4)]
    pub order: u32,
    /// PMIO or MMIO ranges.
    #[mesh(5)]
    pub ranges: Vec<SnapshotDeviceRange>,
    /// Interrupt number, or `None` for devices without an interrupt.
    #[mesh(6)]
    pub irq: Option<u32>,
    /// Transport kind, empty for non-transport devices.
    #[mesh(7)]
    pub transport: String,
    /// Effective feature banks in bank order.
    #[mesh(8)]
    pub feature_banks: Vec<u32>,
    /// Queue count.
    #[mesh(9)]
    pub queue_count: u32,
    /// Maximum queue sizes in queue order.
    #[mesh(10)]
    pub queue_max_sizes: Vec<u32>,
}

/// Stable identity for a host resource required by a device.
#[derive(Clone, Debug, PartialEq, Eq, Protobuf)]
#[mesh(package = "openvmm.snapshot")]
pub struct SnapshotAttachment {
    /// Stable attachment ID.
    #[mesh(1)]
    pub stable_id: String,
    /// Attachment kind.
    #[mesh(2)]
    pub kind: String,
    /// Whether restore requires the attachment.
    #[mesh(3)]
    pub required: bool,
    /// Reconnection policy.
    #[mesh(4)]
    pub reconnect_policy: String,
    /// Immutable identity kind.
    #[mesh(5)]
    pub identity_kind: String,
    /// Provider identity or SHA-256 bytes.
    #[mesh(6)]
    pub identity: Vec<u8>,
    /// Attachment length when applicable.
    #[mesh(7)]
    pub length: u64,
    /// Bounded reconnect timeout. Zero for non-client policies.
    #[mesh(8)]
    pub reconnect_timeout_ms: u64,
}

/// Canonical guest-visible identity of the microVM ABI-v1 network.
#[derive(Clone, Debug, PartialEq, Eq, Protobuf)]
#[mesh(package = "openvmm.snapshot")]
pub struct SnapshotMicrovmNetwork {
    /// Required cross-platform host-network implementation contract.
    #[mesh(9)]
    pub profile: String,
    /// Guest IPv4 address in network byte order.
    #[mesh(1)]
    pub guest_ipv4: u32,
    /// IPv4 subnet prefix length.
    #[mesh(2)]
    pub prefix_length: u32,
    /// Derived gateway IPv4 address in network byte order.
    #[mesh(3)]
    pub gateway_ipv4: u32,
    /// Deterministic guest MAC address.
    #[mesh(4)]
    pub guest_mac: Vec<u8>,
    /// Deterministic gateway MAC address.
    #[mesh(5)]
    pub gateway_mac: Vec<u8>,
    /// Canonical run-scoped egress policy mode.
    #[mesh(6)]
    pub egress_policy_mode: String,
    /// SHA-256 of the canonical run-scoped egress policy.
    #[mesh(7)]
    pub egress_policy_sha256: Vec<u8>,
    /// Whether restore must supply the same policy contract.
    #[mesh(8)]
    pub egress_policy_required: bool,
}

/// Canonical guest-visible policy of the microVM ABI-v1 filesystem.
#[derive(Clone, Debug, PartialEq, Eq, Protobuf)]
#[mesh(package = "openvmm.snapshot")]
pub struct SnapshotMicrovmFilesystem {
    /// Absolute guest mount target.
    #[mesh(1)]
    pub guest_mount_target: String,
    /// Snapshot-authoritative `ro` or `rw` access mode.
    #[mesh(2)]
    pub access_mode: String,
    /// Host attachment policy.
    #[mesh(3)]
    pub restore_mode: String,
    /// Fixed virtio-fs tag.
    #[mesh(4)]
    pub tag: String,
    /// Number of high-priority queues.
    #[mesh(5)]
    pub high_priority_queue_count: u32,
    /// Number of request queues.
    #[mesh(6)]
    pub request_queue_count: u32,
    /// DAX/shared-memory window size.
    #[mesh(7)]
    pub shared_memory_size: u64,
    /// Whether all opened files use direct I/O.
    #[mesh(8)]
    pub direct_io: bool,
    /// Guest entry-cache lifetime in nanoseconds.
    #[mesh(9)]
    pub entry_cache_timeout_ns: u64,
    /// Guest attribute-cache lifetime in nanoseconds.
    #[mesh(10)]
    pub attribute_cache_timeout_ns: u64,
}

impl SnapshotMicrovmFilesystem {
    fn new(config: &openvmm_defs::config::MicrovmFilesystemConfig) -> Self {
        Self {
            guest_mount_target: config.guest_mount_target.clone(),
            access_mode: config.access.as_str().to_owned(),
            restore_mode: "live-revalidate".to_owned(),
            tag: "microvm".to_owned(),
            high_priority_queue_count: 1,
            request_queue_count: 1,
            shared_memory_size: 0,
            direct_io: true,
            entry_cache_timeout_ns: 0,
            attribute_cache_timeout_ns: 0,
        }
    }
}

impl SnapshotMicrovmNetwork {
    fn new(
        config: &openvmm_defs::config::MicrovmNetworkConfig,
        egress_policy: &net_backend_resources::egress::EgressPolicy,
    ) -> Self {
        Self {
            profile: config.profile.as_str().to_owned(),
            guest_ipv4: u32::from(config.guest_ipv4),
            prefix_length: u32::from(config.prefix_length),
            gateway_ipv4: u32::from(config.derived_gateway_ipv4),
            guest_mac: config.guest_mac.to_bytes().to_vec(),
            gateway_mac: config.gateway_mac.to_bytes().to_vec(),
            egress_policy_mode: egress_policy.mode_name().to_owned(),
            egress_policy_sha256: sha2::Sha256::digest(egress_policy.canonical_bytes()).to_vec(),
            egress_policy_required: egress_policy.is_active(),
        }
    }
}

/// Validates a restore-time policy against the snapshot's canonical contract.
pub fn validate_microvm_network_policy(
    saved: &SnapshotMicrovmNetwork,
    policy: &net_backend_resources::egress::EgressPolicy,
) -> anyhow::Result<()> {
    let digest = sha2::Sha256::digest(policy.canonical_bytes());
    anyhow::ensure!(
        saved.egress_policy_mode == policy.mode_name()
            && saved.egress_policy_sha256 == digest.as_slice()
            && saved.egress_policy_required == policy.is_active(),
        "restore-time egress policy does not match the snapshot contract"
    );
    Ok(())
}

/// Machine composition that becomes authoritative after capture.
#[derive(Clone, Debug, PartialEq, Eq, Protobuf)]
#[mesh(package = "openvmm.snapshot")]
pub struct SnapshotMachineContract {
    /// Machine profile name.
    #[mesh(1)]
    pub machine_profile: String,
    /// microVM ABI version.
    #[mesh(2)]
    pub microvm_abi_version: u32,
    /// Source hypervisor kind.
    #[mesh(3)]
    pub source_hypervisor: String,
    /// Effective kernel command line.
    #[mesh(4)]
    pub effective_command_line: String,
    /// SHA-256 of the effective command line.
    #[mesh(5)]
    pub effective_command_line_sha256: Vec<u8>,
    /// Guest RAM ranges in stable order.
    #[mesh(6)]
    pub memory_ranges: Vec<SnapshotMemoryRange>,
    /// Processor topology.
    #[mesh(7)]
    pub topology: SnapshotProcessorTopology,
    /// Guest-visible devices in stable order.
    #[mesh(8)]
    pub devices: Vec<SnapshotDevice>,
    /// Complete state-unit inventory in stable order.
    #[mesh(9)]
    pub state_unit_names: Vec<String>,
    /// Required host attachments.
    #[mesh(10)]
    pub attachments: Vec<SnapshotAttachment>,
    /// Host wall time at the stopped capture boundary.
    #[mesh(11)]
    pub capture_wall_clock: Timestamp,
    /// Effective guest TSC frequency.
    #[mesh(12)]
    pub tsc_frequency_hz: u64,
    /// Accepted destination TSC frequency tolerance in parts per million.
    #[mesh(13)]
    pub tsc_tolerance_ppm: u32,
    /// Canonical protobuf-encoded effective CPU contract.
    #[mesh(14)]
    pub cpu_contract: Vec<u8>,
    /// SHA-256 of the canonical CPU contract.
    #[mesh(15)]
    pub cpu_contract_sha256: Vec<u8>,
    /// Version of the fixed Xen PVH boot and memory layout.
    #[mesh(16)]
    pub pvh_layout_version: u32,
    /// Policy used to advance clocks and deadlines over host downtime.
    #[mesh(17)]
    pub clock_policy: String,
    /// Static identity of the optional microVM virtio-net device.
    #[mesh(18)]
    pub microvm_network: Option<SnapshotMicrovmNetwork>,
    /// Guest-visible policy of the optional microVM virtio-fs device.
    #[mesh(19)]
    pub microvm_filesystem: Option<SnapshotMicrovmFilesystem>,
    /// Effective local APIC timer frequency.
    #[mesh(20)]
    pub apic_frequency_hz: Option<u64>,
}

impl SnapshotMachineContract {
    /// Sets the effective command line and its digest together.
    pub fn set_effective_command_line(&mut self, command_line: String) {
        self.effective_command_line_sha256 = sha2::Sha256::digest(command_line.as_bytes()).to_vec();
        self.effective_command_line = command_line;
    }

    /// Sets the canonical CPU compatibility contract and its digest together.
    pub fn set_cpu_compatibility_contract(&mut self, cpu_contract: Vec<u8>) {
        self.cpu_contract_sha256 = sha2::Sha256::digest(&cpu_contract).to_vec();
        self.cpu_contract = cpu_contract;
    }
}

/// Builds the authoritative no-block microVM ABI-v1 machine contract.
pub fn microvm_v1_machine_contract(
    source_hypervisor: &str,
    effective_command_line: String,
    network: Option<(
        &openvmm_defs::config::MicrovmNetworkConfig,
        &net_backend_resources::egress::EgressPolicy,
        SnapshotAttachment,
    )>,
    filesystem: Option<(
        &openvmm_defs::config::MicrovmFilesystemConfig,
        SnapshotAttachment,
    )>,
    console_attachment: Option<SnapshotAttachment>,
    memory_size: u64,
    state_unit_names: Vec<String>,
    capture_wall_clock: Timestamp,
    tsc_frequency_hz: u64,
    apic_frequency_hz: Option<u64>,
    cpu_contract: Vec<u8>,
) -> anyhow::Result<SnapshotMachineContract> {
    anyhow::ensure!(
        matches!(source_hypervisor, "kvm" | "mshv" | "whp"),
        "microVM snapshots require the KVM, MSHV, or WHP hypervisor"
    );
    const LOW_RAM_END: u64 = 3 * 1024 * 1024 * 1024;
    const HIGH_RAM_START: u64 = 4 * 1024 * 1024 * 1024;

    let low_length = memory_size.min(LOW_RAM_END);
    let mut memory_ranges = vec![SnapshotMemoryRange {
        gpa_start: 0,
        length: low_length,
        file_offset: 0,
    }];
    if memory_size > LOW_RAM_END {
        memory_ranges.push(SnapshotMemoryRange {
            gpa_start: HIGH_RAM_START,
            length: memory_size - LOW_RAM_END,
            file_offset: LOW_RAM_END,
        });
    }

    let device = |stable_id: &str,
                  state_unit_name: &str,
                  kind: &str,
                  ranges: Vec<SnapshotDeviceRange>,
                  irq: Option<u32>,
                  order: u32| SnapshotDevice {
        stable_id: stable_id.to_owned(),
        state_unit_name: state_unit_name.to_owned(),
        kind: kind.to_owned(),
        order,
        ranges,
        irq,
        transport: String::new(),
        feature_banks: Vec::new(),
        queue_count: 0,
        queue_max_sizes: Vec::new(),
    };
    let pmio = |start, length| SnapshotDeviceRange {
        address_space: "pmio".to_owned(),
        start,
        length,
    };
    let mmio = |start, length| SnapshotDeviceRange {
        address_space: "mmio".to_owned(),
        start,
        length,
    };

    let mut devices = vec![
        device("partition", "partition", "partition", Vec::new(), None, 0),
        device("vp0", "partition", "vcpu", Vec::new(), None, 1),
        device("vmtime", "vmtime", "clock", Vec::new(), None, 2),
        device(
            "pic",
            "pic",
            "pic",
            vec![pmio(0x20, 2), pmio(0xa0, 2)],
            None,
            3,
        ),
        device(
            "ioapic",
            "ioapic",
            "ioapic",
            vec![mmio(0xfec0_0000, 0x1000)],
            None,
            4,
        ),
        device(
            "lapic",
            "partition",
            "lapic",
            vec![mmio(0xfee0_0000, 0x1000)],
            None,
            5,
        ),
        device("pit", "pit", "pit", vec![pmio(0x40, 4)], Some(0), 6),
        device("rtc", "rtc", "rtc", vec![pmio(0x70, 2)], Some(8), 7),
        device(
            "microvm-portb",
            "microvm-portb",
            "portb",
            vec![pmio(0xe9, 2)],
            None,
            8,
        ),
        device(
            "microvm-shutdown",
            "microvm-shutdown",
            "shutdown",
            vec![pmio(0x604, 1)],
            None,
            9,
        ),
        device(
            "microvm-snapshot-request",
            "microvm-snapshot-request",
            "snapshot-request",
            vec![pmio(0x605, 1)],
            None,
            10,
        ),
    ];
    let mut attachments = Vec::new();
    let microvm_network = if let Some((network, egress_policy, attachment)) = network {
        let policy_is_valid = matches!(source_hypervisor, "kvm" | "mshv" | "whp")
            && attachment.reconnect_policy == "recreate-endpoint"
            && !attachment.required
            && attachment.identity_kind == "user-mode-nat"
            && attachment.identity == b"consomme";
        anyhow::ensure!(
            attachment.stable_id == "net:microvm0"
                && attachment.kind == "virtio-net"
                && policy_is_valid
                && !attachment.identity.is_empty()
                && attachment.identity.len() <= MAX_ATTACHMENT_IDENTITY_BYTES
                && attachment.length == 0
                && attachment.reconnect_timeout_ms == 0,
            "microVM network attachment has an unsupported endpoint policy"
        );
        let irq = openvmm_defs::config::microvm_virtio_net_irq(Some(source_hypervisor))?;
        let discovery = format!(
            "virtio_mmio.device={:#x}@{:#x}:{irq}",
            openvmm_defs::config::MICROVM_VIRTIO_MMIO_LEN,
            openvmm_defs::config::MICROVM_VIRTIO_NET_MMIO_BASE,
        );
        let tokens = effective_command_line
            .split_ascii_whitespace()
            .collect::<HashSet<_>>();
        anyhow::ensure!(
            tokens.contains(discovery.as_str())
                && network
                    .command_line_fragment()
                    .split_ascii_whitespace()
                    .all(|token| tokens.contains(token)),
            "microVM network command line does not match its saved identity"
        );
        devices.push(SnapshotDevice {
            stable_id: "net:microvm0".to_owned(),
            state_unit_name: format!(
                "virtio-net-{}",
                openvmm_defs::config::MICROVM_VIRTIO_NET_MMIO_BASE
            ),
            kind: "virtio-net".to_owned(),
            order: devices.len() as u32,
            ranges: vec![mmio(
                openvmm_defs::config::MICROVM_VIRTIO_NET_MMIO_BASE,
                openvmm_defs::config::MICROVM_VIRTIO_MMIO_LEN,
            )],
            irq: Some(irq),
            transport: "virtio-mmio".to_owned(),
            feature_banks: vec![
                openvmm_defs::config::MICROVM_VIRTIO_NET_FEATURES as u32,
                (openvmm_defs::config::MICROVM_VIRTIO_NET_FEATURES >> 32) as u32,
            ],
            queue_count: 2,
            queue_max_sizes: vec![256, 256],
        });
        attachments.push(attachment);
        Some(SnapshotMicrovmNetwork::new(network, egress_policy))
    } else {
        None
    };
    let microvm_filesystem = if let Some((filesystem, attachment)) = filesystem {
        anyhow::ensure!(
            attachment.stable_id == "fs:microvm0"
                && attachment.kind == "virtio-fs"
                && attachment.required
                && attachment.reconnect_policy == "live-revalidate"
                && match source_hypervisor {
                    "kvm" | "mshv" => attachment.identity_kind == "unix-device-inode-v1",
                    "whp" => attachment.identity_kind == "windows-volume-file-id-v1",
                    _ => false,
                }
                && !attachment.identity.is_empty()
                && attachment.identity.len() <= MAX_ATTACHMENT_IDENTITY_BYTES
                && attachment.length == 0
                && attachment.reconnect_timeout_ms == 0,
            "microVM filesystem attachment has an unsupported live-revalidation policy"
        );
        let discovery = format!(
            "virtio_mmio.device={:#x}@{:#x}:{}",
            openvmm_defs::config::MICROVM_VIRTIO_MMIO_LEN,
            openvmm_defs::config::MICROVM_VIRTIO_FS_MMIO_BASE,
            openvmm_defs::config::MICROVM_VIRTIO_FS_IRQ,
        );
        let tokens = effective_command_line
            .split_ascii_whitespace()
            .collect::<HashSet<_>>();
        anyhow::ensure!(
            tokens.contains(discovery.as_str())
                && filesystem
                    .command_line_fragment()
                    .split_ascii_whitespace()
                    .all(|token| tokens.contains(token)),
            "microVM filesystem command line does not match its saved policy"
        );
        devices.push(SnapshotDevice {
            stable_id: "fs:microvm0".to_owned(),
            state_unit_name: format!(
                "virtiofs-{}",
                openvmm_defs::config::MICROVM_VIRTIO_FS_MMIO_BASE
            ),
            kind: "virtio-fs".to_owned(),
            order: devices.len() as u32,
            ranges: vec![mmio(
                openvmm_defs::config::MICROVM_VIRTIO_FS_MMIO_BASE,
                openvmm_defs::config::MICROVM_VIRTIO_MMIO_LEN,
            )],
            irq: Some(openvmm_defs::config::MICROVM_VIRTIO_FS_IRQ),
            transport: "virtio-mmio".to_owned(),
            feature_banks: vec![
                openvmm_defs::config::MICROVM_VIRTIO_FS_FEATURES as u32,
                (openvmm_defs::config::MICROVM_VIRTIO_FS_FEATURES >> 32) as u32,
            ],
            queue_count: 2,
            queue_max_sizes: vec![256, 256],
        });
        attachments.push(attachment);
        Some(SnapshotMicrovmFilesystem::new(filesystem))
    } else {
        None
    };
    if let Some(attachment) = console_attachment {
        let policy_is_valid = match attachment.reconnect_policy.as_str() {
            "recreate-listener" => {
                !attachment.required
                    && attachment.reconnect_timeout_ms == 0
                    && matches!(
                        attachment.identity_kind.as_str(),
                        "unix-socket" | "named-pipe" | "tcp"
                    )
            }
            "reconnect-client" => {
                attachment.required
                    && attachment.reconnect_timeout_ms
                        == openvmm_defs::config::MICROVM_CONSOLE_RECONNECT_TIMEOUT_MS
                    && matches!(
                        attachment.identity_kind.as_str(),
                        "unix-socket" | "named-pipe" | "tcp"
                    )
            }
            "require-inherited-attachment" => {
                attachment.required
                    && attachment.reconnect_timeout_ms == 0
                    && attachment.identity_kind == "provider"
                    && attachment.identity == b"console"
            }
            "discard-while-disconnected" => {
                !attachment.required
                    && attachment.reconnect_timeout_ms == 0
                    && attachment.identity_kind == "disconnected"
                    && attachment.identity == b"discard"
            }
            _ => false,
        };
        anyhow::ensure!(
            attachment.stable_id == "console:microvm-virtio0"
                && attachment.kind == "virtio-console"
                && policy_is_valid
                && !attachment.identity.is_empty()
                && attachment.identity.len() <= MAX_ATTACHMENT_IDENTITY_BYTES
                && attachment.length == 0,
            "microVM console attachment has an unsupported reconnect policy"
        );
        devices.push(SnapshotDevice {
            stable_id: "console:microvm-virtio0".to_owned(),
            state_unit_name: format!(
                "virtio-console-{}",
                openvmm_defs::config::MICROVM_VIRTIO_CONSOLE_MMIO_BASE
            ),
            kind: "virtio-console".to_owned(),
            order: devices.len() as u32,
            ranges: vec![mmio(
                openvmm_defs::config::MICROVM_VIRTIO_CONSOLE_MMIO_BASE,
                openvmm_defs::config::MICROVM_VIRTIO_MMIO_LEN,
            )],
            irq: Some(openvmm_defs::config::MICROVM_VIRTIO_CONSOLE_IRQ),
            transport: "virtio-mmio".to_owned(),
            feature_banks: vec![0x3000_0001, 0x0000_0003],
            queue_count: 2,
            queue_max_sizes: vec![256, 256],
        });
        attachments.push(attachment);
    }

    let mut contract = SnapshotMachineContract {
        machine_profile: "microvm".to_owned(),
        microvm_abi_version: openvmm_defs::config::MICROVM_ABI_VERSION_1,
        source_hypervisor: source_hypervisor.to_owned(),
        effective_command_line: String::new(),
        effective_command_line_sha256: Vec::new(),
        memory_ranges,
        topology: SnapshotProcessorTopology {
            sockets: 1,
            dies_per_socket: 1,
            cores_per_die: 1,
            threads_per_core: 1,
            apic_ids: vec![0],
        },
        devices,
        state_unit_names,
        attachments,
        capture_wall_clock,
        tsc_frequency_hz,
        tsc_tolerance_ppm: 0,
        cpu_contract: Vec::new(),
        cpu_contract_sha256: Vec::new(),
        pvh_layout_version: MICROVM_PVH_LAYOUT_VERSION,
        clock_policy: ADVANCE_BY_HOST_DOWNTIME.to_owned(),
        microvm_network,
        microvm_filesystem,
        apic_frequency_hz,
    };
    contract.set_effective_command_line(effective_command_line);
    contract.set_cpu_compatibility_contract(cpu_contract);
    validate_machine_contract_shape(&contract, memory_size, 1)?;
    Ok(contract)
}

/// Manifest describing a VM snapshot.
#[derive(Clone, Protobuf)]
#[mesh(package = "openvmm.snapshot")]
pub struct SnapshotManifest {
    /// Manifest format version.
    #[mesh(1)]
    pub version: u32,
    /// When the snapshot was created.
    #[mesh(2)]
    pub created_at: Timestamp,
    /// OpenVMM version that created the snapshot.
    #[mesh(3)]
    pub openvmm_version: String,
    /// Guest RAM size in bytes.
    #[mesh(4)]
    pub memory_size_bytes: u64,
    /// Number of virtual processors.
    #[mesh(5)]
    pub vp_count: u32,
    /// Page size in bytes.
    #[mesh(6)]
    pub page_size: u32,
    /// Architecture string ("x86_64" or "aarch64").
    #[mesh(7)]
    pub architecture: String,
    /// Length of `state.bin` in bytes.
    #[mesh(8)]
    pub state_size_bytes: u64,
    /// Legacy v2 SHA-256 digest of `state.bin`; empty in v3.
    #[mesh(9)]
    pub state_sha256: Vec<u8>,
    /// Legacy v2 SHA-256 digest of `memory.bin`; empty in v3.
    #[mesh(10)]
    pub memory_sha256: Vec<u8>,
    /// Authoritative machine composition for versioned machine profiles.
    #[mesh(11)]
    pub machine_contract: Option<SnapshotMachineContract>,
    /// Snapshot format magic.
    #[mesh(12)]
    pub format_magic: Vec<u8>,
    /// Version of the serialized VM saved-state schema.
    #[mesh(13)]
    pub saved_state_schema_version: u32,
    /// Fully qualified protobuf root type stored in `state.bin`.
    #[mesh(14)]
    pub saved_state_root_type: String,
}

/// Write a snapshot to the given directory.
///
/// The final directory must not already exist. The snapshot consists of:
/// - `manifest.bin` — protobuf-encoded [`SnapshotManifest`]
/// - `state.bin` — raw device saved-state bytes
/// - `memory.bin` — private copy of the memory backing file
///
/// The files are written and flushed in a unique sibling staging directory.
/// The completed staging directory is then renamed to `dir` in one operation.
pub fn write_snapshot(
    dir: &Path,
    manifest: &SnapshotManifest,
    saved_state_bytes: &[u8],
    memory_file_path: &Path,
) -> Result<(), SnapshotWriteError> {
    let memory_file = open_regular_file(memory_file_path, "memory backing file")?;
    write_snapshot_from_memory_file(dir, manifest, saved_state_bytes, &memory_file)
}

/// Writes a snapshot from the exact open file handle backing guest RAM.
///
/// Capture callers should prefer this over [`write_snapshot`] so replacing the
/// backing pathname cannot substitute different bytes after VM construction.
pub fn write_snapshot_from_memory_file(
    dir: &Path,
    manifest: &SnapshotManifest,
    saved_state_bytes: &[u8],
    memory_file: &std::fs::File,
) -> Result<(), SnapshotWriteError> {
    let mut staging = stage_snapshot(dir, manifest, saved_state_bytes, memory_file)?;
    ensure_path_absent(dir, "snapshot destination")?;
    staging.publish(dir)?;

    let parent = snapshot_parent(dir);
    sync_directory(parent).map_err(|error| SnapshotWriteError::Committed {
        path: dir.to_owned(),
        error,
    })?;
    Ok(())
}

fn stage_snapshot(
    dir: &Path,
    manifest: &SnapshotManifest,
    saved_state_bytes: &[u8],
    memory_file: &std::fs::File,
) -> anyhow::Result<StagingDirectory> {
    validate_manifest_header(manifest)?;
    anyhow::ensure!(
        manifest.version == MANIFEST_VERSION,
        "snapshot manifest version {} is not supported for writing (expected {})",
        manifest.version,
        MANIFEST_VERSION,
    );
    anyhow::ensure!(
        u64::try_from(saved_state_bytes.len()).unwrap_or(u64::MAX) <= MAX_SAVED_STATE_SIZE_BYTES,
        "saved state exceeds the maximum size of {MAX_SAVED_STATE_SIZE_BYTES} bytes",
    );

    let parent = snapshot_parent(dir);
    validate_directory(parent, "snapshot parent directory")?;
    ensure_path_absent(dir, "snapshot destination")?;

    let staging = StagingDirectory::create(parent, dir)?;
    let state_path = staging.path().join(STATE_FILE_NAME);
    let memory_path = staging.path().join(MEMORY_FILE_NAME);
    let manifest_path = staging.path().join(MANIFEST_FILE_NAME);

    write_bytes(&state_path, saved_state_bytes, "saved state")?;
    copy_exact(memory_file, &memory_path, manifest.memory_size_bytes)?;

    let mut published_manifest = manifest.clone();
    published_manifest.state_size_bytes = saved_state_bytes.len() as u64;
    published_manifest.state_sha256.clear();
    published_manifest.memory_sha256.clear();

    let manifest_bytes = mesh::payload::encode(published_manifest);
    anyhow::ensure!(
        manifest_bytes.len() as u64 <= MAX_MANIFEST_SIZE_BYTES,
        "snapshot manifest exceeds the maximum size of {MAX_MANIFEST_SIZE_BYTES} bytes",
    );
    write_bytes(&manifest_path, &manifest_bytes, "snapshot manifest")?;

    sync_directory(staging.path())?;
    Ok(staging)
}

/// Read a snapshot from the given directory.
///
/// Returns the decoded manifest and raw saved-state bytes after structurally
/// validating all three artifacts. `expected_memory_size` bounds memory.
pub fn read_snapshot(
    dir: &Path,
    expected_memory_size: u64,
) -> anyhow::Result<(SnapshotManifest, Vec<u8>)> {
    let (manifest, state_bytes, _) = read_snapshot_with_memory(dir, expected_memory_size)?;
    Ok((manifest, state_bytes))
}

/// Reads and structurally validates only `manifest.bin`.
///
/// Restore uses this before machine composition; all artifacts are opened and
/// structurally validated again before partition creation.
pub fn read_snapshot_manifest(dir: &Path) -> anyhow::Result<SnapshotManifest> {
    validate_snapshot_directory(dir)?;
    let manifest_bytes = read_bounded_file(
        &dir.join(MANIFEST_FILE_NAME),
        MAX_MANIFEST_SIZE_BYTES,
        "snapshot manifest",
    )?;
    let manifest: SnapshotManifest =
        mesh::payload::decode(&manifest_bytes).context("failed to decode snapshot manifest")?;
    validate_manifest_header(&manifest)?;
    validate_manifest_version(&manifest)?;
    if let Some(contract) = &manifest.machine_contract {
        validate_machine_contract_shape(contract, manifest.memory_size_bytes, manifest.vp_count)?;
    }
    Ok(manifest)
}

/// Read and structurally validate a snapshot, returning its exact memory handle.
///
/// The returned file is positioned at offset zero and must be used directly
/// for restore so a path replacement cannot substitute different memory.
pub fn read_snapshot_with_memory(
    dir: &Path,
    expected_memory_size: u64,
) -> anyhow::Result<(SnapshotManifest, Vec<u8>, std::fs::File)> {
    validate_snapshot_directory(dir)?;

    let manifest_bytes = read_bounded_file(
        &dir.join(MANIFEST_FILE_NAME),
        MAX_MANIFEST_SIZE_BYTES,
        "snapshot manifest",
    )?;
    let manifest: SnapshotManifest =
        mesh::payload::decode(&manifest_bytes).context("failed to decode snapshot manifest")?;
    validate_manifest_header(&manifest)?;
    validate_manifest_version(&manifest)?;
    if let Some(contract) = &manifest.machine_contract {
        validate_machine_contract_shape(contract, manifest.memory_size_bytes, manifest.vp_count)?;
    }
    anyhow::ensure!(
        manifest.state_size_bytes <= MAX_SAVED_STATE_SIZE_BYTES,
        "state.bin length in the manifest exceeds the maximum size of \
         {MAX_SAVED_STATE_SIZE_BYTES} bytes",
    );

    let state_bytes = read_bounded_file(
        &dir.join(STATE_FILE_NAME),
        MAX_SAVED_STATE_SIZE_BYTES,
        "saved state",
    )?;
    anyhow::ensure!(
        state_bytes.len() as u64 == manifest.state_size_bytes,
        "state.bin size ({} bytes) doesn't match manifest ({} bytes)",
        state_bytes.len(),
        manifest.state_size_bytes,
    );
    anyhow::ensure!(
        manifest.memory_size_bytes == expected_memory_size,
        "memory.bin size in the manifest ({} bytes) doesn't match expected ({expected_memory_size} bytes)",
        manifest.memory_size_bytes,
    );
    let memory_path = dir.join(MEMORY_FILE_NAME);
    let memory_file = open_file_with_length(&memory_path, expected_memory_size, MEMORY_FILE_NAME)?;

    Ok((manifest, state_bytes, memory_file))
}

fn snapshot_parent(dir: &Path) -> &Path {
    match dir.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

fn path_exists(path: &Path) -> anyhow::Result<bool> {
    match fs_err::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err).with_context(|| format!("failed to inspect {}", path.display())),
    }
}

fn ensure_path_absent(path: &Path, description: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !path_exists(path)?,
        "{description} already exists: {}",
        path.display(),
    );
    Ok(())
}

fn validate_directory(path: &Path, description: &str) -> anyhow::Result<()> {
    let metadata = fs_err::symlink_metadata(path)
        .with_context(|| format!("failed to inspect {description} {}", path.display()))?;
    anyhow::ensure!(
        metadata.file_type().is_dir(),
        "{description} is not a directory: {}",
        path.display(),
    );
    Ok(())
}

struct StagingDirectory {
    path: Option<PathBuf>,
}

impl StagingDirectory {
    fn create(parent: &Path, destination: &Path) -> anyhow::Result<Self> {
        let destination_name = destination
            .file_name()
            .context("snapshot destination must name a directory")?
            .to_string_lossy();
        let sequence = STAGING_SEQUENCE.fetch_add(1, Ordering::Relaxed);

        for attempt in 0..100_u32 {
            let path = parent.join(format!(
                ".{destination_name}.staging-{}-{sequence}-{attempt}",
                std::process::id()
            ));
            match create_private_directory(&path) {
                Ok(()) => return Ok(Self { path: Some(path) }),
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(err) => {
                    return Err(err).with_context(|| {
                        format!("failed to create staging directory {}", path.display())
                    });
                }
            }
        }

        anyhow::bail!(
            "failed to allocate a unique snapshot staging directory in {}",
            parent.display()
        )
    }

    fn path(&self) -> &Path {
        self.path.as_deref().expect("staging path is present")
    }

    fn publish(&mut self, destination: &Path) -> anyhow::Result<()> {
        let staging_path = self.path();
        rename_no_replace(staging_path, destination).with_context(|| {
            format!(
                "failed to publish snapshot {} to {}",
                staging_path.display(),
                destination.display()
            )
        })?;
        self.path = None;
        Ok(())
    }
}

fn create_private_directory(path: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        let descriptor: pal::windows::security::LocalSecurityDescriptor =
            "D:P(A;;FA;;;SY)(A;;FA;;;OW)".parse()?;
        pal::windows::security::create_directory_with_security(path, &descriptor)
    }
    #[cfg(not(windows))]
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    #[cfg(not(windows))]
    return builder.create(path);
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn rename_no_replace(source: &Path, destination: &Path) -> anyhow::Result<()> {
    let source_parent = snapshot_parent(source);
    let destination_parent = snapshot_parent(destination);
    anyhow::ensure!(
        source_parent == destination_parent,
        "snapshot staging and destination directories have different parents",
    );
    let source_name = source
        .file_name()
        .context("snapshot staging path must name a directory")?;
    let destination_name = destination
        .file_name()
        .context("snapshot destination must name a directory")?;
    let parent = std::fs::File::open(destination_parent).with_context(|| {
        format!(
            "failed to open snapshot parent directory {}",
            destination_parent.display()
        )
    })?;
    nix::fcntl::renameat2(
        &parent,
        source_name,
        &parent,
        destination_name,
        nix::fcntl::RenameFlags::RENAME_NOREPLACE,
    )?;
    Ok(())
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
fn rename_no_replace(source: &Path, destination: &Path) -> anyhow::Result<()> {
    fs_err::rename(source, destination)?;
    Ok(())
}

impl Drop for StagingDirectory {
    fn drop(&mut self) {
        if let Some(path) = &self.path {
            let _ = fs_err::remove_dir_all(path);
        }
    }
}

fn create_file(path: &Path, description: &str) -> anyhow::Result<std::fs::File> {
    #[cfg(windows)]
    {
        let descriptor: pal::windows::security::LocalSecurityDescriptor =
            "D:P(A;;FA;;;SY)(A;;FA;;;OW)".parse()?;
        pal::windows::security::create_file_with_security(path, &descriptor)
            .with_context(|| format!("failed to create {description} at {}", path.display()))
    }
    #[cfg(not(windows))]
    let mut options = fs_err::OpenOptions::new();
    #[cfg(not(windows))]
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use fs_err::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    #[cfg(not(windows))]
    return options
        .open(path)
        .map(Into::into)
        .with_context(|| format!("failed to create {description} at {}", path.display()));
}

fn write_bytes(path: &Path, bytes: &[u8], description: &str) -> anyhow::Result<()> {
    let mut file = create_file(path, description)?;
    file.write_all(bytes)
        .with_context(|| format!("failed to write {description}"))?;
    file.sync_all()
        .with_context(|| format!("failed to flush {description}"))?;
    Ok(())
}

fn copy_exact(
    source_file: &std::fs::File,
    destination_path: &Path,
    expected_length: u64,
) -> anyhow::Result<()> {
    let source_metadata = source_file
        .metadata()
        .context("failed to inspect memory backing file")?;
    anyhow::ensure!(
        source_metadata.file_type().is_file(),
        "memory backing handle is not a regular file"
    );
    let mut source = source_file
        .try_clone()
        .context("failed to duplicate memory backing file handle")?;
    source
        .seek(SeekFrom::Start(0))
        .context("failed to rewind memory backing file")?;
    let source_length = source
        .metadata()
        .context("failed to inspect memory backing file")?
        .len();
    anyhow::ensure!(
        source_length == expected_length,
        "memory backing file size ({source_length} bytes) doesn't match manifest ({expected_length} bytes)",
    );
    let mut destination = create_file(destination_path, "snapshot memory")?;
    let mut total = 0_u64;
    let mut buffer = vec![0_u8; COPY_BUFFER_SIZE];
    let limit = expected_length
        .checked_add(1)
        .context("memory backing file length cannot be bounded")?;
    let mut source = Read::by_ref(&mut source).take(limit);

    loop {
        let count = source
            .read(&mut buffer)
            .context("failed to read memory backing file")?;
        if count == 0 {
            break;
        }
        destination
            .write_all(&buffer[..count])
            .context("failed to write snapshot memory")?;
        total = total
            .checked_add(count as u64)
            .context("memory backing file length overflowed u64")?;
    }

    anyhow::ensure!(
        total == expected_length,
        "memory backing file changed while it was being copied (expected {expected_length} bytes, copied {total} bytes)",
    );

    destination
        .sync_all()
        .context("failed to flush snapshot memory")?;
    Ok(())
}

fn validate_snapshot_directory(dir: &Path) -> anyhow::Result<()> {
    let metadata = fs_err::symlink_metadata(dir)
        .with_context(|| format!("failed to inspect snapshot directory {}", dir.display()))?;
    anyhow::ensure!(
        metadata.file_type().is_dir(),
        "snapshot path is not a directory: {}",
        dir.display(),
    );

    let mut entry_count = 0_usize;
    for entry in fs_err::read_dir(dir)
        .with_context(|| format!("failed to enumerate snapshot directory {}", dir.display()))?
    {
        let entry = entry.context("failed to inspect snapshot directory entry")?;
        let name = entry.file_name();
        anyhow::ensure!(
            name == MANIFEST_FILE_NAME || name == STATE_FILE_NAME || name == MEMORY_FILE_NAME,
            "unexpected artifact in snapshot directory: {}",
            entry.path().display(),
        );
        entry_count += 1;
    }
    anyhow::ensure!(entry_count == 3, "snapshot directory is incomplete");
    Ok(())
}

fn open_regular_file(path: &Path, description: &str) -> anyhow::Result<std::fs::File> {
    let path_metadata = fs_err::symlink_metadata(path)
        .with_context(|| format!("failed to inspect {description} at {}", path.display()))?;
    anyhow::ensure!(
        path_metadata.file_type().is_file(),
        "{description} is not a regular file: {}",
        path.display(),
    );

    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }

    let file = options
        .open(path)
        .with_context(|| format!("failed to open {description} at {}", path.display()))?;
    let metadata = file
        .metadata()
        .with_context(|| format!("failed to inspect opened {description}"))?;
    anyhow::ensure!(
        metadata.file_type().is_file(),
        "{description} is not a regular file: {}",
        path.display(),
    );
    Ok(file)
}

fn read_bounded_file(path: &Path, maximum_size: u64, description: &str) -> anyhow::Result<Vec<u8>> {
    let file = open_regular_file(path, description)?;
    let length = file
        .metadata()
        .with_context(|| format!("failed to inspect opened {description}"))?
        .len();
    anyhow::ensure!(
        length <= maximum_size,
        "{description} is {length} bytes, exceeding the maximum of {maximum_size} bytes",
    );
    let capacity = usize::try_from(length).context("artifact length does not fit in usize")?;
    let mut bytes = Vec::with_capacity(capacity);
    file.take(maximum_size + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("failed to read {description}"))?;
    anyhow::ensure!(
        bytes.len() as u64 == length,
        "{description} changed while it was being read",
    );
    Ok(bytes)
}

fn validate_sha256(digest: &[u8], description: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        digest.len() == SHA256_SIZE,
        "{description} SHA-256 digest has invalid length {}",
        digest.len(),
    );
    Ok(())
}

fn verify_digest(bytes: &[u8], expected: &[u8], description: &str) -> anyhow::Result<()> {
    let actual: [u8; 32] = sha2::Sha256::digest(bytes).into();
    anyhow::ensure!(
        actual.as_slice() == expected,
        "{description} SHA-256 digest mismatch",
    );
    Ok(())
}

fn open_file_with_length(
    path: &Path,
    expected_length: u64,
    artifact_name: &str,
) -> anyhow::Result<std::fs::File> {
    let file = open_regular_file(path, artifact_name)?;
    let length = file
        .metadata()
        .with_context(|| format!("failed to inspect opened {artifact_name}"))?
        .len();
    anyhow::ensure!(
        length == expected_length,
        "{artifact_name} size ({length} bytes) doesn't match manifest ({expected_length} bytes)",
    );
    Ok(file)
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> anyhow::Result<()> {
    fs_err::File::open(path)
        .with_context(|| format!("failed to open directory {} for flush", path.display()))?
        .sync_all()
        .with_context(|| format!("failed to flush directory {}", path.display()))
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> anyhow::Result<()> {
    // Windows has no portable directory-flush equivalent. Artifact handles are
    // flushed before the same-volume directory rename, which remains the
    // publication boundary; crash durability of that rename is filesystem-defined.
    Ok(())
}

/// Validate that a snapshot manifest is compatible with the running VM config.
///
/// Checks version, architecture, memory size, VP count, and page size.
/// Returns `Ok(())` if the manifest matches, or an error describing the
/// first mismatch found.
pub fn validate_manifest(
    manifest: &SnapshotManifest,
    expected_arch: &str,
    expected_memory_size: u64,
    expected_vp_count: u32,
    expected_page_size: u32,
) -> anyhow::Result<()> {
    validate_manifest_header(manifest)?;
    validate_manifest_version(manifest)?;

    if manifest.architecture != expected_arch {
        anyhow::bail!(
            "snapshot architecture '{}' doesn't match expected '{}'",
            manifest.architecture,
            expected_arch,
        );
    }

    if manifest.memory_size_bytes != expected_memory_size {
        anyhow::bail!(
            "snapshot memory size ({} bytes) doesn't match expected ({} bytes)",
            manifest.memory_size_bytes,
            expected_memory_size,
        );
    }

    if manifest.vp_count != expected_vp_count {
        anyhow::bail!(
            "snapshot VP count ({}) doesn't match expected ({})",
            manifest.vp_count,
            expected_vp_count,
        );
    }

    if manifest.page_size != expected_page_size {
        anyhow::bail!(
            "snapshot page size ({}) doesn't match expected ({})",
            manifest.page_size,
            expected_page_size,
        );
    }

    Ok(())
}

/// Validate and exactly compare an authoritative microVM machine contract.
pub fn validate_microvm_machine_contract(
    manifest: &SnapshotManifest,
    expected: &SnapshotMachineContract,
) -> anyhow::Result<()> {
    let contract = manifest
        .machine_contract
        .as_ref()
        .context("snapshot is missing the authoritative machine contract")?;
    validate_machine_contract_shape(contract, manifest.memory_size_bytes, manifest.vp_count)?;

    anyhow::ensure!(
        contract.machine_profile == "microvm",
        "snapshot machine profile '{}' is not microvm",
        contract.machine_profile,
    );
    anyhow::ensure!(
        contract.machine_profile == expected.machine_profile,
        "snapshot machine profile doesn't match the requested machine"
    );
    anyhow::ensure!(
        contract.microvm_abi_version == expected.microvm_abi_version,
        "snapshot microVM ABI version {} doesn't match expected {}",
        contract.microvm_abi_version,
        expected.microvm_abi_version,
    );
    anyhow::ensure!(
        contract.source_hypervisor == expected.source_hypervisor,
        "snapshot source hypervisor '{}' doesn't match expected '{}'",
        contract.source_hypervisor,
        expected.source_hypervisor,
    );
    anyhow::ensure!(
        contract.pvh_layout_version == expected.pvh_layout_version,
        "snapshot PVH layout version doesn't match the requested machine"
    );
    anyhow::ensure!(
        contract.clock_policy == expected.clock_policy,
        "snapshot clock policy doesn't match the requested machine"
    );
    anyhow::ensure!(
        contract.effective_command_line == expected.effective_command_line,
        "snapshot effective command line doesn't match the requested machine"
    );
    anyhow::ensure!(
        contract.memory_ranges == expected.memory_ranges,
        "snapshot RAM layout doesn't match the requested machine"
    );
    anyhow::ensure!(
        contract.topology == expected.topology,
        "snapshot processor topology doesn't match the requested machine"
    );
    anyhow::ensure!(
        contract.devices == expected.devices,
        "snapshot device inventory, order, or configuration doesn't match the requested machine"
    );
    anyhow::ensure!(
        contract.state_unit_names == expected.state_unit_names,
        "snapshot state-unit inventory or order doesn't match the requested machine"
    );
    anyhow::ensure!(
        contract.attachments == expected.attachments,
        "snapshot attachment inventory doesn't match the supplied attachments"
    );
    anyhow::ensure!(
        contract.microvm_network == expected.microvm_network,
        "snapshot static network identity doesn't match the requested machine"
    );
    anyhow::ensure!(
        contract.microvm_filesystem == expected.microvm_filesystem,
        "snapshot filesystem policy doesn't match the requested machine"
    );
    anyhow::ensure!(
        contract.tsc_frequency_hz == expected.tsc_frequency_hz
            && contract.tsc_tolerance_ppm == expected.tsc_tolerance_ppm,
        "snapshot TSC frequency contract doesn't match the destination"
    );
    anyhow::ensure!(
        contract.apic_frequency_hz == expected.apic_frequency_hz,
        "snapshot APIC frequency contract doesn't match the destination"
    );
    anyhow::ensure!(
        contract.cpu_contract == expected.cpu_contract,
        "snapshot CPU compatibility contract doesn't match the destination"
    );
    Ok(())
}

fn validate_machine_contract_shape(
    contract: &SnapshotMachineContract,
    memory_size: u64,
    vp_count: u32,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        contract.pvh_layout_version == MICROVM_PVH_LAYOUT_VERSION,
        "snapshot PVH layout version {} is unsupported",
        contract.pvh_layout_version
    );
    anyhow::ensure!(
        contract.clock_policy == ADVANCE_BY_HOST_DOWNTIME,
        "snapshot clock policy '{}' is unsupported",
        contract.clock_policy
    );
    anyhow::ensure!(
        contract.effective_command_line.len() < MAX_COMMAND_LINE_BYTES,
        "snapshot command line exceeds the 64-KiB limit"
    );
    anyhow::ensure!(
        !contract.effective_command_line.contains('\0'),
        "snapshot command line contains an embedded NUL"
    );
    validate_sha256(
        &contract.effective_command_line_sha256,
        "effective command line",
    )?;
    verify_digest(
        contract.effective_command_line.as_bytes(),
        &contract.effective_command_line_sha256,
        "effective command line",
    )?;
    anyhow::ensure!(
        contract.tsc_frequency_hz != 0,
        "snapshot TSC frequency must be nonzero"
    );
    if let Some(apic_frequency_hz) = contract.apic_frequency_hz {
        anyhow::ensure!(
            apic_frequency_hz != 0,
            "snapshot APIC frequency must be nonzero"
        );
    }
    anyhow::ensure!(
        !contract.cpu_contract.is_empty() && contract.cpu_contract.len() <= MAX_CPU_CONTRACT_BYTES,
        "snapshot CPU contract size is invalid"
    );
    validate_sha256(&contract.cpu_contract_sha256, "CPU contract")?;
    verify_digest(
        &contract.cpu_contract,
        &contract.cpu_contract_sha256,
        "CPU contract",
    )?;
    let _: std::time::SystemTime = contract
        .capture_wall_clock
        .try_into()
        .context("snapshot capture wall clock is invalid")?;

    anyhow::ensure!(
        !contract.memory_ranges.is_empty() && contract.memory_ranges.len() <= MAX_MEMORY_RANGES,
        "snapshot RAM range count is invalid"
    );
    let mut total_memory = 0_u64;
    for (index, range) in contract.memory_ranges.iter().enumerate() {
        anyhow::ensure!(range.length != 0, "snapshot RAM range {index} is empty");
        let gpa_end = range
            .gpa_start
            .checked_add(range.length)
            .with_context(|| format!("snapshot RAM range {index} overflows GPA space"))?;
        let file_end = range
            .file_offset
            .checked_add(range.length)
            .with_context(|| format!("snapshot RAM range {index} overflows file space"))?;
        anyhow::ensure!(
            file_end <= memory_size,
            "snapshot RAM range {index} exceeds memory.bin"
        );
        total_memory = total_memory
            .checked_add(range.length)
            .context("snapshot total RAM size overflows")?;
        for previous in &contract.memory_ranges[..index] {
            let previous_gpa_end = previous.gpa_start + previous.length;
            let previous_file_end = previous.file_offset + previous.length;
            anyhow::ensure!(
                gpa_end <= previous.gpa_start || range.gpa_start >= previous_gpa_end,
                "snapshot RAM ranges overlap in GPA space"
            );
            anyhow::ensure!(
                file_end <= previous.file_offset || range.file_offset >= previous_file_end,
                "snapshot RAM ranges overlap in memory.bin"
            );
        }
    }
    anyhow::ensure!(
        total_memory == memory_size,
        "snapshot RAM ranges cover {total_memory} bytes, expected {memory_size}"
    );

    let topology = &contract.topology;
    let topology_vp_count = u64::from(topology.sockets)
        .checked_mul(u64::from(topology.dies_per_socket))
        .and_then(|count| count.checked_mul(u64::from(topology.cores_per_die)))
        .and_then(|count| count.checked_mul(u64::from(topology.threads_per_core)))
        .context("snapshot processor topology overflows")?;
    anyhow::ensure!(
        topology_vp_count == u64::from(vp_count) && topology.apic_ids.len() == vp_count as usize,
        "snapshot processor topology doesn't describe {vp_count} virtual processors"
    );
    ensure_unique(&topology.apic_ids, "APIC ID")?;

    if let Some(network) = &contract.microvm_network {
        anyhow::ensure!(
            network.profile == openvmm_defs::config::MicrovmNetworkProfile::Portable.as_str(),
            "snapshot microVM network profile '{}' is unsupported",
            network.profile
        );
        let prefix_length = u8::try_from(network.prefix_length)
            .context("snapshot network prefix does not fit in u8")?;
        let parsed = format!(
            "{}/{}",
            std::net::Ipv4Addr::from(network.guest_ipv4),
            prefix_length
        )
        .parse::<openvmm_defs::config::MicrovmNetworkConfig>()
        .context("snapshot static network identity is invalid")?;
        anyhow::ensure!(
            network.gateway_ipv4 == u32::from(parsed.derived_gateway_ipv4)
                && network.guest_mac == parsed.guest_mac.to_bytes()
                && network.gateway_mac == parsed.gateway_mac.to_bytes(),
            "snapshot static network identity is not canonical"
        );
        anyhow::ensure!(
            matches!(
                network.egress_policy_mode.as_str(),
                "allow-all" | "allow-list" | "block-list" | "endpoint"
            ) && network.egress_policy_required == (network.egress_policy_mode != "allow-all"),
            "snapshot egress policy requirement is invalid"
        );
        validate_sha256(&network.egress_policy_sha256, "egress policy")?;
    }
    if let Some(filesystem) = &contract.microvm_filesystem {
        let access = match filesystem.access_mode.as_str() {
            "ro" => openvmm_defs::config::MicrovmFilesystemAccess::ReadOnly,
            "rw" => openvmm_defs::config::MicrovmFilesystemAccess::ReadWrite,
            mode => anyhow::bail!("snapshot filesystem access mode '{mode}' is unsupported"),
        };
        let parsed = openvmm_defs::config::MicrovmFilesystemConfig::new(
            filesystem.guest_mount_target.clone(),
            access,
        )
        .context("snapshot filesystem policy is invalid")?;
        anyhow::ensure!(
            *filesystem == SnapshotMicrovmFilesystem::new(&parsed),
            "snapshot filesystem policy is not canonical"
        );
    }

    anyhow::ensure!(
        contract.devices.len() <= MAX_DEVICES,
        "snapshot device inventory is too large"
    );
    let mut device_ids = HashSet::new();
    for (index, device) in contract.devices.iter().enumerate() {
        anyhow::ensure!(
            device.order == index as u32,
            "snapshot device order is not canonical"
        );
        anyhow::ensure!(
            !device.stable_id.is_empty() && device_ids.insert(device.stable_id.as_str()),
            "snapshot contains an empty or duplicate device ID"
        );
        anyhow::ensure!(
            !device.state_unit_name.is_empty(),
            "snapshot device '{}' has no state-unit name",
            device.stable_id,
        );
        anyhow::ensure!(
            device.ranges.len() <= MAX_DEVICE_RANGES,
            "snapshot device '{}' has too many address ranges",
            device.stable_id,
        );
        for range in &device.ranges {
            anyhow::ensure!(
                matches!(range.address_space.as_str(), "pmio" | "mmio") && range.length != 0,
                "snapshot device '{}' has an invalid address range",
                device.stable_id,
            );
            range.start.checked_add(range.length).with_context(|| {
                format!("snapshot device '{}' range overflows", device.stable_id)
            })?;
        }
        anyhow::ensure!(
            device.queue_count as usize == device.queue_max_sizes.len(),
            "snapshot device '{}' queue inventory is inconsistent",
            device.stable_id,
        );
    }

    anyhow::ensure!(
        !contract.state_unit_names.is_empty() && contract.state_unit_names.len() <= MAX_STATE_UNITS,
        "snapshot state-unit inventory size is invalid"
    );
    ensure_unique(&contract.state_unit_names, "state-unit name")?;
    let state_units = contract
        .state_unit_names
        .iter()
        .map(String::as_str)
        .collect::<HashSet<_>>();
    for device in &contract.devices {
        anyhow::ensure!(
            state_units.contains(device.state_unit_name.as_str()),
            "snapshot device '{}' references an unknown state unit",
            device.stable_id,
        );
    }

    anyhow::ensure!(
        contract.attachments.len() <= MAX_ATTACHMENTS,
        "snapshot attachment inventory is too large"
    );
    let mut attachment_ids = HashSet::new();
    for attachment in &contract.attachments {
        anyhow::ensure!(
            !attachment.stable_id.is_empty()
                && attachment_ids.insert(attachment.stable_id.as_str()),
            "snapshot contains an empty or duplicate attachment ID"
        );
        anyhow::ensure!(
            !attachment.kind.is_empty()
                && !attachment.reconnect_policy.is_empty()
                && !attachment.identity_kind.is_empty()
                && !attachment.identity.is_empty()
                && attachment.identity.len() <= MAX_ATTACHMENT_IDENTITY_BYTES,
            "snapshot attachment '{}' has an incomplete identity",
            attachment.stable_id,
        );
        anyhow::ensure!(
            match attachment.reconnect_policy.as_str() {
                "recreate-listener" => {
                    !attachment.required && attachment.reconnect_timeout_ms == 0
                }
                "reconnect-client" => {
                    attachment.required && attachment.reconnect_timeout_ms != 0
                }
                "require-inherited-attachment" => {
                    attachment.required && attachment.reconnect_timeout_ms == 0
                }
                "discard-while-disconnected" => {
                    !attachment.required && attachment.reconnect_timeout_ms == 0
                }
                "recreate-endpoint" => {
                    !attachment.required && attachment.reconnect_timeout_ms == 0
                }
                "live-revalidate" => {
                    attachment.required
                        && attachment.reconnect_timeout_ms == 0
                        && attachment.length == 0
                }
                _ => false,
            },
            "snapshot attachment '{}' has an invalid reconnect policy",
            attachment.stable_id,
        );
    }
    Ok(())
}

fn validate_manifest_header(manifest: &SnapshotManifest) -> anyhow::Result<()> {
    let expected_magic = match manifest.version {
        LEGACY_MANIFEST_VERSION => LEGACY_SNAPSHOT_FORMAT_MAGIC,
        MANIFEST_VERSION => SNAPSHOT_FORMAT_MAGIC,
        version => anyhow::bail!(
            "snapshot manifest version {version} is not supported (expected {LEGACY_MANIFEST_VERSION} or {MANIFEST_VERSION})"
        ),
    };
    anyhow::ensure!(
        manifest.format_magic == expected_magic,
        "snapshot format magic is invalid"
    );
    anyhow::ensure!(
        manifest.saved_state_schema_version == SAVED_STATE_SCHEMA_VERSION,
        "snapshot saved-state schema version {} is unsupported",
        manifest.saved_state_schema_version
    );
    anyhow::ensure!(
        manifest.saved_state_root_type == SAVED_STATE_ROOT_TYPE,
        "snapshot saved-state root type '{}' is unsupported",
        manifest.saved_state_root_type
    );
    Ok(())
}

fn validate_manifest_version(manifest: &SnapshotManifest) -> anyhow::Result<()> {
    match manifest.version {
        LEGACY_MANIFEST_VERSION => {
            anyhow::ensure!(
                manifest.state_sha256.len() == SHA256_SIZE,
                "legacy state.bin SHA-256 digest has invalid length {}",
                manifest.state_sha256.len(),
            );
            anyhow::ensure!(
                manifest.memory_sha256.len() == SHA256_SIZE,
                "legacy memory.bin SHA-256 digest has invalid length {}",
                manifest.memory_sha256.len(),
            );
        }
        MANIFEST_VERSION => {
            anyhow::ensure!(
                manifest.state_sha256.is_empty() && manifest.memory_sha256.is_empty(),
                "snapshot manifest version {MANIFEST_VERSION} must not contain legacy artifact digests",
            );
        }
        version => anyhow::bail!(
            "snapshot manifest version {version} is not supported (expected {LEGACY_MANIFEST_VERSION} or {MANIFEST_VERSION})"
        ),
    }
    Ok(())
}

fn ensure_unique<T>(values: &[T], description: &str) -> anyhow::Result<()>
where
    T: Eq + std::hash::Hash,
{
    let mut unique = HashSet::with_capacity(values.len());
    anyhow::ensure!(
        values.iter().all(|value| unique.insert(value)),
        "snapshot contains a duplicate {description}"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: build a test manifest with sensible defaults.
    fn test_manifest() -> SnapshotManifest {
        SnapshotManifest {
            version: MANIFEST_VERSION,
            created_at: Timestamp {
                seconds: 1234567890,
                nanos: 0,
            },
            openvmm_version: "test-0.1.0".to_string(),
            memory_size_bytes: 1024,
            vp_count: 2,
            page_size: 4096,
            architecture: "x86_64".to_string(),
            state_size_bytes: 0,
            state_sha256: Vec::new(),
            memory_sha256: Vec::new(),
            machine_contract: None,
            format_magic: SNAPSHOT_FORMAT_MAGIC.to_vec(),
            saved_state_schema_version: SAVED_STATE_SCHEMA_VERSION,
            saved_state_root_type: SAVED_STATE_ROOT_TYPE.to_owned(),
        }
    }

    fn test_machine_contract() -> SnapshotMachineContract {
        let mut contract = SnapshotMachineContract {
            machine_profile: "microvm".to_owned(),
            microvm_abi_version: 1,
            source_hypervisor: "whp".to_owned(),
            effective_command_line: String::new(),
            effective_command_line_sha256: Vec::new(),
            memory_ranges: vec![SnapshotMemoryRange {
                gpa_start: 0,
                length: 1024,
                file_offset: 0,
            }],
            topology: SnapshotProcessorTopology {
                sockets: 1,
                dies_per_socket: 1,
                cores_per_die: 2,
                threads_per_core: 1,
                apic_ids: vec![0, 1],
            },
            devices: vec![
                SnapshotDevice {
                    stable_id: "portb".to_owned(),
                    state_unit_name: "portb".to_owned(),
                    kind: "portb".to_owned(),
                    order: 0,
                    ranges: vec![SnapshotDeviceRange {
                        address_space: "pmio".to_owned(),
                        start: 0xe9,
                        length: 2,
                    }],
                    irq: None,
                    transport: String::new(),
                    feature_banks: Vec::new(),
                    queue_count: 0,
                    queue_max_sizes: Vec::new(),
                },
                SnapshotDevice {
                    stable_id: "shutdown".to_owned(),
                    state_unit_name: "shutdown".to_owned(),
                    kind: "shutdown".to_owned(),
                    order: 1,
                    ranges: vec![SnapshotDeviceRange {
                        address_space: "pmio".to_owned(),
                        start: 0x604,
                        length: 1,
                    }],
                    irq: None,
                    transport: String::new(),
                    feature_banks: Vec::new(),
                    queue_count: 0,
                    queue_max_sizes: Vec::new(),
                },
            ],
            state_unit_names: vec!["portb".to_owned(), "shutdown".to_owned()],
            attachments: Vec::new(),
            capture_wall_clock: std::time::SystemTime::now().into(),
            tsc_frequency_hz: 1_000_000_000,
            tsc_tolerance_ppm: 0,
            cpu_contract: Vec::new(),
            cpu_contract_sha256: Vec::new(),
            pvh_layout_version: MICROVM_PVH_LAYOUT_VERSION,
            clock_policy: ADVANCE_BY_HOST_DOWNTIME.to_owned(),
            microvm_network: None,
            microvm_filesystem: None,
            apic_frequency_hz: Some(1_000_000_000),
        };
        contract.set_effective_command_line("console=hvc0".to_owned());
        contract.set_cpu_compatibility_contract(vec![1, 2, 3]);
        contract
    }

    fn microvm_console_attachment() -> SnapshotAttachment {
        SnapshotAttachment {
            stable_id: "console:microvm-virtio0".to_owned(),
            kind: "virtio-console".to_owned(),
            required: false,
            reconnect_policy: "recreate-listener".to_owned(),
            identity_kind: "tcp".to_owned(),
            identity: b"127.0.0.1:5555".to_vec(),
            length: 0,
            reconnect_timeout_ms: 0,
        }
    }

    fn microvm_network_attachment(source_hypervisor: &str) -> SnapshotAttachment {
        assert!(matches!(source_hypervisor, "kvm" | "mshv" | "whp"));
        SnapshotAttachment {
            stable_id: "net:microvm0".to_owned(),
            kind: "virtio-net".to_owned(),
            required: false,
            reconnect_policy: "recreate-endpoint".to_owned(),
            identity_kind: "user-mode-nat".to_owned(),
            identity: b"consomme".to_vec(),
            length: 0,
            reconnect_timeout_ms: 0,
        }
    }

    fn microvm_filesystem_attachment(source_hypervisor: &str) -> SnapshotAttachment {
        SnapshotAttachment {
            stable_id: "fs:microvm0".to_owned(),
            kind: "virtio-fs".to_owned(),
            required: true,
            reconnect_policy: "live-revalidate".to_owned(),
            identity_kind: match source_hypervisor {
                "kvm" | "mshv" => "unix-device-inode-v1",
                "whp" => "windows-volume-file-id-v1",
                _ => unreachable!(),
            }
            .to_owned(),
            identity: b"root-object-v1".to_vec(),
            length: 0,
            reconnect_timeout_ms: 0,
        }
    }

    fn generated_network_contract(source_hypervisor: &str) -> SnapshotMachineContract {
        let network: openvmm_defs::config::MicrovmNetworkConfig = "10.0.0.2/24".parse().unwrap();
        let egress_policy = net_backend_resources::egress::EgressPolicy::new(
            network.guest_ipv4,
            network.derived_gateway_ipv4,
            net_backend_resources::egress::EgressPolicyMode::AllowList(vec![
                "192.0.2.0/24".parse().unwrap(),
            ]),
        );
        let irq = openvmm_defs::config::microvm_virtio_net_irq(Some(source_hypervisor)).unwrap();
        let command_line = format!(
            "earlycon=xe9 console=hvc0 reboot=t panic=-1 virtio_mmio.device=0x1000@0xd0000000:{irq} {}",
            network.command_line_fragment_with_dns(true)
        );
        microvm_v1_machine_contract(
            source_hypervisor,
            command_line,
            Some((
                &network,
                &egress_policy,
                microvm_network_attachment(source_hypervisor),
            )),
            None,
            None,
            1024,
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

    fn generated_console_contract() -> SnapshotMachineContract {
        microvm_v1_machine_contract(
            "whp",
            "earlycon=xe9 console=hvc1 reboot=t panic=-1 virtio_mmio.device=0x1000@0xd0002000:7"
                .to_owned(),
            None,
            None,
            Some(microvm_console_attachment()),
            1024,
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
                "virtio-console-3489669120",
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

    fn generated_filesystem_contract(source_hypervisor: &str) -> SnapshotMachineContract {
        let filesystem = openvmm_defs::config::MicrovmFilesystemConfig::new(
            "/mnt/share".to_owned(),
            openvmm_defs::config::MicrovmFilesystemAccess::ReadOnly,
        )
        .unwrap();
        let command_line = format!(
            "earlycon=xe9 console=hvc0 reboot=t panic=-1 virtio_mmio.device=0x1000@0xd0001000:6 {}",
            filesystem.command_line_fragment()
        );
        microvm_v1_machine_contract(
            source_hypervisor,
            command_line,
            None,
            Some((
                &filesystem,
                microvm_filesystem_attachment(source_hypervisor),
            )),
            None,
            1024,
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

    #[test]
    fn generated_microvm_console_contract_has_fixed_abi() {
        let contract = generated_console_contract();
        let console = contract.devices.last().unwrap();
        assert_eq!(console.stable_id, "console:microvm-virtio0");
        assert_eq!(console.state_unit_name, "virtio-console-3489669120");
        assert_eq!(console.ranges[0].start, 0xd000_2000);
        assert_eq!(console.ranges[0].length, 0x1000);
        assert_eq!(console.irq, Some(7));
        assert_eq!(console.transport, "virtio-mmio");
        assert_eq!(console.feature_banks, [0x3000_0001, 0x0000_0003]);
        assert_eq!(console.queue_max_sizes, [256, 256]);
        assert_eq!(contract.attachments, [microvm_console_attachment()]);
    }

    #[test]
    fn generated_microvm_network_contract_has_backend_specific_fixed_abi() {
        for (source_hypervisor, irq) in [("kvm", 10), ("mshv", 10), ("whp", 5)] {
            let contract = generated_network_contract(source_hypervisor);
            let network_device = contract.devices.last().unwrap();
            assert_eq!(network_device.stable_id, "net:microvm0");
            assert_eq!(network_device.state_unit_name, "virtio-net-3489660928");
            assert_eq!(network_device.ranges[0].start, 0xd000_0000);
            assert_eq!(network_device.ranges[0].length, 0x1000);
            assert_eq!(network_device.irq, Some(irq));
            assert_eq!(network_device.transport, "virtio-mmio");
            assert_eq!(network_device.feature_banks, [0x20, 0x1]);
            assert_eq!(network_device.queue_count, 2);
            assert_eq!(network_device.queue_max_sizes, [256, 256]);
            assert_eq!(
                contract.attachments,
                [microvm_network_attachment(source_hypervisor)]
            );

            let network = contract.microvm_network.unwrap();
            assert_eq!(network.profile, "portable");
            assert_eq!(
                network.guest_ipv4,
                u32::from(std::net::Ipv4Addr::new(10, 0, 0, 2))
            );
            assert_eq!(network.prefix_length, 24);
            assert_eq!(
                network.gateway_ipv4,
                u32::from(std::net::Ipv4Addr::new(10, 0, 0, 1))
            );
            assert_eq!(network.guest_mac, [0x52, 0x54, 0, 0, 0, 2]);
            assert_eq!(network.gateway_mac, [0x52, 0x54, 0, 0, 0, 1]);
            assert_eq!(network.egress_policy_mode, "allow-list");
            assert_eq!(network.egress_policy_sha256.len(), 32);
            assert!(network.egress_policy_required);
        }
    }

    #[test]
    fn generated_microvm_filesystem_contract_has_fixed_abi() {
        for source_hypervisor in ["kvm", "mshv", "whp"] {
            let contract = generated_filesystem_contract(source_hypervisor);
            let filesystem_device = contract.devices.last().unwrap();
            assert_eq!(filesystem_device.stable_id, "fs:microvm0");
            assert_eq!(filesystem_device.state_unit_name, "virtiofs-3489665024");
            assert_eq!(filesystem_device.ranges[0].start, 0xd000_1000);
            assert_eq!(filesystem_device.ranges[0].length, 0x1000);
            assert_eq!(filesystem_device.irq, Some(6));
            assert_eq!(filesystem_device.transport, "virtio-mmio");
            assert_eq!(filesystem_device.feature_banks, [0x3000_0000, 0x0000_0003]);
            assert_eq!(filesystem_device.queue_count, 2);
            assert_eq!(filesystem_device.queue_max_sizes, [256, 256]);
            assert_eq!(
                contract.attachments,
                [microvm_filesystem_attachment(source_hypervisor)]
            );

            let filesystem = contract.microvm_filesystem.unwrap();
            assert_eq!(filesystem.guest_mount_target, "/mnt/share");
            assert_eq!(filesystem.access_mode, "ro");
            assert_eq!(filesystem.restore_mode, "live-revalidate");
            assert_eq!(filesystem.tag, "microvm");
            assert_eq!(filesystem.high_priority_queue_count, 1);
            assert_eq!(filesystem.request_queue_count, 1);
            assert_eq!(filesystem.shared_memory_size, 0);
            assert!(filesystem.direct_io);
            assert_eq!(filesystem.entry_cache_timeout_ns, 0);
            assert_eq!(filesystem.attribute_cache_timeout_ns, 0);
        }
    }

    #[test]
    fn validate_microvm_filesystem_contract_rejects_policy_change() {
        let contract = generated_filesystem_contract("whp");
        let mut manifest = test_manifest();
        manifest.memory_size_bytes = 1024;
        manifest.vp_count = 1;
        manifest.machine_contract = Some(contract.clone());
        manifest
            .machine_contract
            .as_mut()
            .unwrap()
            .microvm_filesystem
            .as_mut()
            .unwrap()
            .request_queue_count = 2;

        let error = validate_microvm_machine_contract(&manifest, &contract).unwrap_err();
        assert!(error.to_string().contains("not canonical"));
    }

    #[test]
    fn validate_microvm_network_contract_rejects_noncanonical_identity() {
        let contract = generated_network_contract("whp");
        let mut manifest = test_manifest();
        manifest.memory_size_bytes = 1024;
        manifest.vp_count = 1;
        manifest.machine_contract = Some(contract.clone());
        manifest
            .machine_contract
            .as_mut()
            .unwrap()
            .microvm_network
            .as_mut()
            .unwrap()
            .guest_mac[5] = 3;

        let error = validate_microvm_machine_contract(&manifest, &contract).unwrap_err();
        assert!(error.to_string().contains("not canonical"));
    }

    #[test]
    fn validate_microvm_network_contract_rejects_unsupported_profile() {
        let contract = generated_network_contract("whp");
        let mut manifest = test_manifest();
        manifest.memory_size_bytes = 1024;
        manifest.vp_count = 1;
        manifest.machine_contract = Some(contract.clone());
        manifest
            .machine_contract
            .as_mut()
            .unwrap()
            .microvm_network
            .as_mut()
            .unwrap()
            .profile = "other".to_owned();

        let error = validate_microvm_machine_contract(&manifest, &contract).unwrap_err();
        assert!(error.to_string().contains("profile"));
    }

    #[test]
    fn validate_microvm_network_contract_rejects_legacy_missing_profile() {
        let contract = generated_network_contract("whp");
        let mut manifest = test_manifest();
        manifest.memory_size_bytes = 1024;
        manifest.vp_count = 1;
        manifest.machine_contract = Some(contract.clone());
        manifest
            .machine_contract
            .as_mut()
            .unwrap()
            .microvm_network
            .as_mut()
            .unwrap()
            .profile
            .clear();

        let error = validate_microvm_machine_contract(&manifest, &contract).unwrap_err();
        assert!(error.to_string().contains("profile '' is unsupported"));
    }

    #[test]
    fn validate_microvm_console_contract_rejects_attachment_change() {
        let contract = generated_console_contract();
        let mut manifest = test_manifest();
        manifest.memory_size_bytes = 1024;
        manifest.vp_count = 1;
        manifest.machine_contract = Some(contract.clone());
        let mut expected = contract;
        expected.attachments[0].identity = b"127.0.0.1:6666".to_vec();
        let error = validate_microvm_machine_contract(&manifest, &expected).unwrap_err();
        assert!(error.to_string().contains("attachment inventory"));
    }

    #[test]
    fn validate_microvm_machine_contract_ok() {
        let mut manifest = test_manifest();
        let contract = test_machine_contract();
        manifest.machine_contract = Some(contract.clone());
        validate_microvm_machine_contract(&manifest, &contract).unwrap();
    }

    #[test]
    fn validate_microvm_machine_contract_rejects_abi() {
        let mut manifest = test_manifest();
        let contract = test_machine_contract();
        manifest.machine_contract = Some(contract.clone());
        let mut expected = contract;
        expected.microvm_abi_version = 2;
        let err = validate_microvm_machine_contract(&manifest, &expected).unwrap_err();
        assert!(err.to_string().contains("ABI version"));
    }

    #[test]
    fn validate_microvm_machine_contract_rejects_backend() {
        let mut manifest = test_manifest();
        let contract = test_machine_contract();
        manifest.machine_contract = Some(contract.clone());
        let mut expected = contract;
        expected.source_hypervisor = "kvm".to_owned();
        let err = validate_microvm_machine_contract(&manifest, &expected).unwrap_err();
        assert!(err.to_string().contains("source hypervisor"));
    }

    #[test]
    fn validate_microvm_machine_contract_rejects_command_line() {
        let mut manifest = test_manifest();
        let contract = test_machine_contract();
        manifest.machine_contract = Some(contract.clone());
        let mut expected = contract;
        expected.set_effective_command_line("console=other".to_owned());
        let err = validate_microvm_machine_contract(&manifest, &expected).unwrap_err();
        assert!(err.to_string().contains("command line"));
    }

    #[test]
    fn validate_microvm_machine_contract_rejects_cpu_contract() {
        let mut manifest = test_manifest();
        let contract = test_machine_contract();
        manifest.machine_contract = Some(contract.clone());
        let mut expected = contract;
        expected.set_cpu_compatibility_contract(vec![9, 9, 9]);
        let err = validate_microvm_machine_contract(&manifest, &expected).unwrap_err();
        assert!(err.to_string().contains("CPU compatibility"));
    }

    #[test]
    fn validate_microvm_machine_contract_rejects_tsc_frequency() {
        let mut manifest = test_manifest();
        let contract = test_machine_contract();
        manifest.machine_contract = Some(contract.clone());
        let mut expected = contract;
        expected.tsc_frequency_hz += 1;
        let err = validate_microvm_machine_contract(&manifest, &expected).unwrap_err();
        assert!(err.to_string().contains("TSC frequency"));
    }

    #[test]
    fn validate_microvm_machine_contract_rejects_apic_frequency() {
        let mut manifest = test_manifest();
        let contract = test_machine_contract();
        manifest.machine_contract = Some(contract.clone());
        let mut expected = contract;
        expected.apic_frequency_hz = expected.apic_frequency_hz.map(|frequency| frequency + 1);
        let err = validate_microvm_machine_contract(&manifest, &expected).unwrap_err();
        assert!(err.to_string().contains("APIC frequency"));
    }

    #[test]
    fn validate_microvm_machine_contract_accepts_legacy_apic_frequency() {
        let mut manifest = test_manifest();
        let mut contract = test_machine_contract();
        contract.apic_frequency_hz = None;
        manifest.machine_contract = Some(contract.clone());

        validate_microvm_machine_contract(&manifest, &contract).unwrap();
    }

    #[test]
    fn validate_microvm_machine_contract_rejects_pvh_layout() {
        let mut manifest = test_manifest();
        let contract = test_machine_contract();
        manifest.machine_contract = Some(contract.clone());
        let mut expected = contract;
        expected.pvh_layout_version += 1;
        let err = validate_microvm_machine_contract(&manifest, &expected).unwrap_err();
        assert!(err.to_string().contains("PVH layout"));
    }

    #[test]
    fn validate_microvm_machine_contract_rejects_clock_policy() {
        let mut manifest = test_manifest();
        let contract = test_machine_contract();
        manifest.machine_contract = Some(contract.clone());
        let mut expected = contract;
        expected.clock_policy = "freeze_during_downtime".to_owned();
        let err = validate_microvm_machine_contract(&manifest, &expected).unwrap_err();
        assert!(err.to_string().contains("clock policy"));
    }

    #[test]
    fn validate_microvm_machine_contract_rejects_topology() {
        let mut manifest = test_manifest();
        let contract = test_machine_contract();
        manifest.machine_contract = Some(contract.clone());
        let mut expected = contract;
        expected.topology.apic_ids.swap(0, 1);
        let err = validate_microvm_machine_contract(&manifest, &expected).unwrap_err();
        assert!(err.to_string().contains("processor topology"));
    }

    #[test]
    fn validate_microvm_machine_contract_rejects_device_order() {
        let mut manifest = test_manifest();
        let contract = test_machine_contract();
        manifest.machine_contract = Some(contract.clone());
        let mut expected = contract;
        expected.devices.reverse();
        let err = validate_microvm_machine_contract(&manifest, &expected).unwrap_err();
        assert!(err.to_string().contains("device inventory"));
    }

    #[test]
    fn validate_microvm_machine_contract_rejects_removed_device() {
        let mut manifest = test_manifest();
        let contract = test_machine_contract();
        manifest.machine_contract = Some(contract.clone());
        let mut expected = contract;
        expected.devices.pop();
        let err = validate_microvm_machine_contract(&manifest, &expected).unwrap_err();
        assert!(err.to_string().contains("device inventory"));
    }

    #[test]
    fn validate_microvm_machine_contract_rejects_state_unit_order() {
        let mut manifest = test_manifest();
        let contract = test_machine_contract();
        manifest.machine_contract = Some(contract.clone());
        let mut expected = contract;
        expected.state_unit_names.reverse();
        let err = validate_microvm_machine_contract(&manifest, &expected).unwrap_err();
        assert!(err.to_string().contains("state-unit inventory"));
    }

    #[test]
    fn validate_microvm_machine_contract_rejects_duplicate_state_unit() {
        let mut manifest = test_manifest();
        let mut contract = test_machine_contract();
        contract.state_unit_names.push("portb".to_owned());
        manifest.machine_contract = Some(contract.clone());
        let err = validate_microvm_machine_contract(&manifest, &contract).unwrap_err();
        assert!(err.to_string().contains("duplicate state-unit name"));
    }

    #[test]
    fn validate_microvm_machine_contract_rejects_overlapping_ram() {
        let mut manifest = test_manifest();
        let mut contract = test_machine_contract();
        contract.memory_ranges = vec![
            SnapshotMemoryRange {
                gpa_start: 0,
                length: 768,
                file_offset: 0,
            },
            SnapshotMemoryRange {
                gpa_start: 512,
                length: 256,
                file_offset: 768,
            },
        ];
        manifest.machine_contract = Some(contract.clone());
        let err = validate_microvm_machine_contract(&manifest, &contract).unwrap_err();
        assert!(err.to_string().contains("overlap in GPA space"));
    }

    #[test]
    fn write_read_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");

        // Create a fake memory backing file in the same directory (same fs).
        let mem_path = dir.path().join("memory.bin");
        std::fs::write(&mem_path, vec![0_u8; 1024]).unwrap();

        let manifest = test_manifest();
        let state = b"saved-state-data";

        write_snapshot(&snap_dir, &manifest, state, &mem_path).unwrap();

        let (read_manifest, read_state) = read_snapshot(&snap_dir, 1024).unwrap();
        assert_eq!(read_manifest.version, manifest.version);
        assert_eq!(read_manifest.memory_size_bytes, manifest.memory_size_bytes);
        assert_eq!(read_manifest.vp_count, manifest.vp_count);
        assert_eq!(read_manifest.architecture, manifest.architecture);
        assert_eq!(read_manifest.state_size_bytes, state.len() as u64);
        assert!(read_manifest.state_sha256.is_empty());
        assert!(read_manifest.memory_sha256.is_empty());
        assert_eq!(read_state, state);

        // memory.bin should exist in the snapshot directory.
        assert!(snap_dir.join("memory.bin").exists());
    }

    #[test]
    fn write_snapshot_rejects_missing_parent() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("a").join("b").join("c");

        let mem_path = dir.path().join("memory.bin");
        std::fs::write(&mem_path, vec![0_u8; 1024]).unwrap();

        let err = write_snapshot(&snap_dir, &test_manifest(), b"state", &mem_path).unwrap_err();
        assert!(!err.is_committed());
        assert!(err.to_string().contains("snapshot parent directory"));
        assert!(!snap_dir.exists());
    }

    #[test]
    fn write_snapshot_copies_memory() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        let mem_path = dir.path().join("memory.bin");
        std::fs::write(&mem_path, b"SAMEFILE").unwrap();

        let mut manifest = test_manifest();
        manifest.memory_size_bytes = 8;
        write_snapshot(&snap_dir, &manifest, b"state", &mem_path).unwrap();
        std::fs::write(&mem_path, b"MODIFIED").unwrap();

        assert_eq!(
            std::fs::read(snap_dir.join(MEMORY_FILE_NAME)).unwrap(),
            b"SAMEFILE"
        );
    }

    #[test]
    fn exact_memory_handle_survives_path_replacement_before_publish() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        let mem_path = dir.path().join("memory.bin");
        let moved_path = dir.path().join("mapped-memory.bin");
        std::fs::write(&mem_path, vec![0x5a_u8; 1024]).unwrap();
        let memory_file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&mem_path)
            .unwrap();
        std::fs::rename(&mem_path, &moved_path).unwrap();
        std::fs::write(&mem_path, vec![0xa5_u8; 1024]).unwrap();

        write_snapshot_from_memory_file(&snap_dir, &test_manifest(), b"state", &memory_file)
            .unwrap();

        assert_eq!(
            std::fs::read(snap_dir.join(MEMORY_FILE_NAME)).unwrap(),
            vec![0x5a_u8; 1024]
        );
    }

    #[test]
    fn write_snapshot_rejects_existing_destination() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        std::fs::create_dir(&snap_dir).unwrap();
        std::fs::write(snap_dir.join("sentinel"), b"keep").unwrap();
        let mem_path = dir.path().join("memory.bin");
        std::fs::write(&mem_path, vec![0_u8; 1024]).unwrap();

        let err = write_snapshot(&snap_dir, &test_manifest(), b"state", &mem_path).unwrap_err();
        assert!(err.to_string().contains("already exists"));
        assert_eq!(std::fs::read(snap_dir.join("sentinel")).unwrap(), b"keep");
    }

    #[test]
    fn publish_does_not_replace_destination_created_after_staging() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        let mut staging = StagingDirectory::create(dir.path(), &snap_dir).unwrap();
        std::fs::create_dir(&snap_dir).unwrap();
        std::fs::write(snap_dir.join("sentinel"), b"keep").unwrap();

        assert!(staging.publish(&snap_dir).is_err());
        assert_eq!(std::fs::read(snap_dir.join("sentinel")).unwrap(), b"keep");
    }

    #[test]
    fn write_snapshot_rejects_oversized_memory_without_publishing() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        let mem_path = dir.path().join("memory.bin");
        std::fs::write(&mem_path, vec![0_u8; 1025]).unwrap();

        let err = write_snapshot(&snap_dir, &test_manifest(), b"state", &mem_path).unwrap_err();
        assert!(err.to_string().contains("doesn't match manifest"));
        assert!(!snap_dir.exists());
        assert!(std::fs::read_dir(dir.path()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains("staging")
        }));
    }

    #[test]
    fn write_snapshot_rejects_oversized_manifest_without_publishing() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        let mem_path = dir.path().join("memory.bin");
        std::fs::write(&mem_path, vec![0_u8; 1024]).unwrap();
        let mut manifest = test_manifest();
        manifest.openvmm_version = "x".repeat(MAX_MANIFEST_SIZE_BYTES as usize);

        let err = write_snapshot(&snap_dir, &manifest, b"state", &mem_path).unwrap_err();
        assert!(err.to_string().contains("manifest exceeds"));
        assert!(!snap_dir.exists());
    }

    #[test]
    fn read_snapshot_accepts_same_length_state_change_without_legacy_checksum_validation() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        let mem_path = dir.path().join("memory.bin");
        std::fs::write(&mem_path, vec![0_u8; 1024]).unwrap();
        write_snapshot(&snap_dir, &test_manifest(), b"state", &mem_path).unwrap();
        std::fs::write(snap_dir.join(STATE_FILE_NAME), b"other").unwrap();

        let (_, state) = read_snapshot(&snap_dir, 1024).unwrap();
        assert_eq!(state, b"other");
    }

    #[test]
    fn read_snapshot_accepts_same_length_memory_change_without_legacy_checksum_validation() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        let mem_path = dir.path().join("memory.bin");
        std::fs::write(&mem_path, vec![0_u8; 1024]).unwrap();
        write_snapshot(&snap_dir, &test_manifest(), b"state", &mem_path).unwrap();
        std::fs::write(snap_dir.join(MEMORY_FILE_NAME), vec![1_u8; 1024]).unwrap();

        let (_, _, mut memory) = read_snapshot_with_memory(&snap_dir, 1024).unwrap();
        let mut bytes = Vec::new();
        memory.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, vec![1_u8; 1024]);
    }

    #[test]
    fn read_snapshot_accepts_legacy_v2_manifest_without_verifying_digests() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        let mem_path = dir.path().join("memory.bin");
        std::fs::write(&mem_path, vec![0_u8; 1024]).unwrap();
        write_snapshot(&snap_dir, &test_manifest(), b"state", &mem_path).unwrap();
        let mut manifest = read_snapshot_manifest(&snap_dir).unwrap();
        manifest.version = LEGACY_MANIFEST_VERSION;
        manifest.format_magic = LEGACY_SNAPSHOT_FORMAT_MAGIC.to_vec();
        manifest.state_sha256 = vec![0xa5; SHA256_SIZE];
        manifest.memory_sha256 = vec![0x5a; SHA256_SIZE];
        std::fs::write(
            snap_dir.join(MANIFEST_FILE_NAME),
            mesh::payload::encode(manifest),
        )
        .unwrap();
        std::fs::write(snap_dir.join(STATE_FILE_NAME), b"other").unwrap();
        std::fs::write(snap_dir.join(MEMORY_FILE_NAME), vec![1_u8; 1024]).unwrap();

        let (manifest, state) = read_snapshot(&snap_dir, 1024).unwrap();
        assert_eq!(manifest.version, LEGACY_MANIFEST_VERSION);
        assert_eq!(state, b"other");
    }

    #[test]
    fn read_snapshot_rejects_malformed_legacy_digest_lengths() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        let mem_path = dir.path().join("memory.bin");
        std::fs::write(&mem_path, vec![0_u8; 1024]).unwrap();
        write_snapshot(&snap_dir, &test_manifest(), b"state", &mem_path).unwrap();
        let mut manifest = read_snapshot_manifest(&snap_dir).unwrap();
        manifest.version = LEGACY_MANIFEST_VERSION;
        manifest.format_magic = LEGACY_SNAPSHOT_FORMAT_MAGIC.to_vec();
        manifest.state_sha256 = vec![0; SHA256_SIZE - 1];
        manifest.memory_sha256 = vec![0; SHA256_SIZE];
        std::fs::write(
            snap_dir.join(MANIFEST_FILE_NAME),
            mesh::payload::encode(manifest.clone()),
        )
        .unwrap();
        let error = read_snapshot(&snap_dir, 1024).err().unwrap();
        assert!(error.to_string().contains("state.bin SHA-256 digest"));

        manifest.state_sha256 = vec![0; SHA256_SIZE];
        manifest.memory_sha256 = vec![0; SHA256_SIZE + 1];
        std::fs::write(
            snap_dir.join(MANIFEST_FILE_NAME),
            mesh::payload::encode(manifest),
        )
        .unwrap();
        let error = read_snapshot(&snap_dir, 1024).err().unwrap();
        assert!(error.to_string().contains("memory.bin SHA-256 digest"));
    }

    #[test]
    fn read_snapshot_still_requires_exact_memory_length() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        let mem_path = dir.path().join("memory.bin");
        std::fs::write(&mem_path, vec![0_u8; 1024]).unwrap();
        write_snapshot(&snap_dir, &test_manifest(), b"state", &mem_path).unwrap();
        std::fs::write(snap_dir.join(MEMORY_FILE_NAME), vec![1_u8; 1023]).unwrap();

        let error = read_snapshot(&snap_dir, 1024).err().unwrap();
        assert!(error.to_string().contains("memory.bin size"));
    }

    #[test]
    fn read_snapshot_rejects_legacy_digests_in_v3_manifest() {
        let mut manifest = test_manifest();
        manifest.state_sha256 = vec![0; SHA256_SIZE];
        let error = validate_manifest(&manifest, "x86_64", 1024, 2, 4096).unwrap_err();
        assert!(error.to_string().contains("legacy artifact digests"));
    }

    #[test]
    fn read_snapshot_rejects_corrupt_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        let mem_path = dir.path().join("memory.bin");
        std::fs::write(&mem_path, vec![0_u8; 1024]).unwrap();
        write_snapshot(&snap_dir, &test_manifest(), b"state", &mem_path).unwrap();
        std::fs::write(snap_dir.join(MANIFEST_FILE_NAME), b"not-a-manifest").unwrap();

        let err = read_snapshot(&snap_dir, 1024).err().unwrap();
        assert!(err.to_string().contains("decode snapshot manifest"));
    }

    #[test]
    fn read_snapshot_rejects_truncated_state() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        let mem_path = dir.path().join("memory.bin");
        std::fs::write(&mem_path, vec![0_u8; 1024]).unwrap();
        write_snapshot(&snap_dir, &test_manifest(), b"state", &mem_path).unwrap();
        std::fs::write(snap_dir.join(STATE_FILE_NAME), b"sta").unwrap();

        let err = read_snapshot(&snap_dir, 1024).err().unwrap();
        assert!(err.to_string().contains("state.bin size"));
    }

    #[test]
    fn read_snapshot_rejects_truncated_memory() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        let mem_path = dir.path().join("memory.bin");
        std::fs::write(&mem_path, vec![0_u8; 1024]).unwrap();
        write_snapshot(&snap_dir, &test_manifest(), b"state", &mem_path).unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(snap_dir.join(MEMORY_FILE_NAME))
            .unwrap()
            .set_len(512)
            .unwrap();

        let err = read_snapshot(&snap_dir, 1024).err().unwrap();
        assert!(err.to_string().contains("memory.bin size"));
    }

    #[test]
    fn read_snapshot_rejects_oversized_state() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        let mem_path = dir.path().join("memory.bin");
        std::fs::write(&mem_path, vec![0_u8; 1024]).unwrap();
        write_snapshot(&snap_dir, &test_manifest(), b"state", &mem_path).unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(snap_dir.join(STATE_FILE_NAME))
            .unwrap()
            .set_len(MAX_SAVED_STATE_SIZE_BYTES + 1)
            .unwrap();

        let err = read_snapshot(&snap_dir, 1024).err().unwrap();
        assert!(err.to_string().contains("saved state is"));
        assert!(err.to_string().contains("exceeding the maximum"));
    }

    #[test]
    fn read_snapshot_rejects_unexpected_artifact() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        let mem_path = dir.path().join("memory.bin");
        std::fs::write(&mem_path, vec![0_u8; 1024]).unwrap();
        write_snapshot(&snap_dir, &test_manifest(), b"state", &mem_path).unwrap();
        std::fs::write(snap_dir.join("extra.bin"), b"unexpected").unwrap();

        let err = read_snapshot(&snap_dir, 1024).err().unwrap();
        assert!(err.to_string().contains("unexpected artifact"));
    }

    #[cfg(unix)]
    #[test]
    fn read_snapshot_rejects_symlinked_artifact() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        let mem_path = dir.path().join("memory.bin");
        std::fs::write(&mem_path, vec![0_u8; 1024]).unwrap();
        write_snapshot(&snap_dir, &test_manifest(), b"state", &mem_path).unwrap();
        let state_path = snap_dir.join(STATE_FILE_NAME);
        let moved_state_path = snap_dir.join("state-target.bin");
        std::fs::rename(&state_path, &moved_state_path).unwrap();
        symlink(&moved_state_path, &state_path).unwrap();

        let err = read_snapshot(&snap_dir, 1024).err().unwrap();
        assert!(
            err.to_string().contains("unexpected artifact")
                || err.to_string().contains("not a regular file")
        );
    }

    #[test]
    fn opened_memory_handle_survives_path_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        let mem_path = dir.path().join("memory.bin");
        std::fs::write(&mem_path, vec![0x5a_u8; 1024]).unwrap();
        write_snapshot(&snap_dir, &test_manifest(), b"state", &mem_path).unwrap();

        let (_, _, mut opened_memory) = read_snapshot_with_memory(&snap_dir, 1024).unwrap();
        let original_path = snap_dir.join(MEMORY_FILE_NAME);
        let moved_path = snap_dir.join("opened-memory.bin");
        std::fs::rename(&original_path, &moved_path).unwrap();
        std::fs::write(&original_path, vec![0xa5_u8; 1024]).unwrap();

        let mut bytes = Vec::new();
        opened_memory.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, vec![0x5a_u8; 1024]);
    }

    #[test]
    fn read_snapshot_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        // No files written — read should fail.
        let result = read_snapshot(dir.path(), 1024);
        assert!(result.is_err());
    }

    #[test]
    fn validate_manifest_ok() {
        let manifest = test_manifest();
        validate_manifest(&manifest, "x86_64", 1024, 2, 4096).unwrap();
    }

    #[test]
    fn validate_manifest_wrong_arch() {
        let manifest = test_manifest();
        let err = validate_manifest(&manifest, "aarch64", 1024, 2, 4096).unwrap_err();
        assert!(
            err.to_string().contains("architecture"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn validate_manifest_wrong_memory_size() {
        let manifest = test_manifest();
        let err = validate_manifest(&manifest, "x86_64", 9999, 2, 4096).unwrap_err();
        assert!(
            err.to_string().contains("memory size"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn validate_manifest_wrong_vp_count() {
        let manifest = test_manifest();
        let err = validate_manifest(&manifest, "x86_64", 1024, 99, 4096).unwrap_err();
        assert!(
            err.to_string().contains("VP count"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn validate_manifest_wrong_page_size() {
        let manifest = test_manifest();
        let err = validate_manifest(&manifest, "x86_64", 1024, 2, 65536).unwrap_err();
        assert!(
            err.to_string().contains("page size"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn validate_manifest_wrong_version() {
        let mut manifest = test_manifest();
        manifest.version = 999;
        let err = validate_manifest(&manifest, "x86_64", 1024, 2, 4096).unwrap_err();
        assert!(
            err.to_string().contains("version"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn validate_manifest_wrong_magic() {
        let mut manifest = test_manifest();
        manifest.format_magic = b"NOT_OPENVMM".to_vec();
        let err = validate_manifest(&manifest, "x86_64", 1024, 2, 4096).unwrap_err();
        assert!(err.to_string().contains("format magic"));
    }

    #[test]
    fn validate_manifest_wrong_saved_state_schema() {
        let mut manifest = test_manifest();
        manifest.saved_state_schema_version += 1;
        let err = validate_manifest(&manifest, "x86_64", 1024, 2, 4096).unwrap_err();
        assert!(err.to_string().contains("schema version"));
    }

    #[test]
    fn validate_manifest_wrong_saved_state_root() {
        let mut manifest = test_manifest();
        manifest.saved_state_root_type = "other.SavedState".to_owned();
        let err = validate_manifest(&manifest, "x86_64", 1024, 2, 4096).unwrap_err();
        assert!(err.to_string().contains("root type"));
    }
}
