// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration tests for OpenVMM's TTRPC interface.

// Tests for the fd-passing protocol, which shares the TTRPC socket. It is
// UNIX-only and tap `fd_name` resolution is Linux-only, so it only builds on
// Linux.
#[cfg(target_os = "linux")]
mod fd_passing;

use anyhow::Context;
use futures::AsyncBufReadExt;
use futures::AsyncReadExt;
use futures::AsyncWriteExt;
use guid::Guid;
use mesh::CancelContext;
use openvmm_ttrpc_vmservice as vmservice;
use pal_async::DefaultDriver;
use pal_async::DefaultPool;
use pal_async::pipe::PolledPipe;
use pal_async::process::PolledChild;
use pal_async::socket::PolledSocket;
use pal_async::task::Spawn;
use pal_async::task::Task;
use petri::ResolvedArtifact;
use petri::pipette::cmd;
use petri_artifacts_vmm_test::artifacts;
use std::io::Write;
use std::ops::Deref;
use std::ops::DerefMut;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use unix_socket::UnixListener;
use unix_socket::UnixStream;

petri::test!(test_ttrpc_interface, |resolver| {
    let openvmm = resolver.require(artifacts::OPENVMM_NATIVE);
    let kernel = resolver.require(artifacts::loadable::LINUX_DIRECT_TEST_KERNEL_NATIVE);
    let initrd = resolver.require(artifacts::loadable::LINUX_DIRECT_TEST_INITRD_NATIVE);
    let pipette = match petri_artifacts_common::tags::MachineArch::host() {
        petri_artifacts_common::tags::MachineArch::X86_64 => resolver
            .require(petri_artifacts_common::artifacts::PIPETTE_LINUX_X64)
            .erase(),
        petri_artifacts_common::tags::MachineArch::Aarch64 => resolver
            .require(petri_artifacts_common::artifacts::PIPETTE_LINUX_AARCH64)
            .erase(),
    };
    Some([openvmm.erase(), kernel.erase(), initrd.erase(), pipette])
});

petri::multitest!(vec![
    petri::SimpleTest::new(
        "test_ttrpc_microvm_snapshot_restore",
        |resolver| {
            Some([
                resolver.require(artifacts::OPENVMM_NATIVE).erase(),
                resolver
                    .require(artifacts::loadable::MICROVM_PVH_TEST_KERNEL_X64)
                    .erase(),
                resolver
                    .require(artifacts::loadable::MICROVM_PVH_TEST_INITRD_X64)
                    .erase(),
            ])
        },
        test_ttrpc_microvm_snapshot_restore,
    )
    .requirements(petri::requirements::TestCaseRequirements::new(
        petri::requirements::TestRequirement::RequiresCapability {
            name: petri_artifacts_common::capabilities::MICROVM_PVH,
            vmm: petri::requirements::VmmType::OpenVmm,
        },
    ))
    .into(),
]);

fn microvm_portb_config(path: &Path) -> vmservice::SerialConfig {
    vmservice::SerialConfig {
        ports: vec![vmservice::serial_config::Config {
            port: 0,
            socket_path: path.to_string_lossy().into_owned(),
            connect: false,
        }],
    }
}

fn microvm_restore_request(snapshot_path: &Path, portb_path: &Path) -> vmservice::CreateVmRequest {
    vmservice::CreateVmRequest {
        config: Some(vmservice::VmConfig {
            serial_config: Some(microvm_portb_config(portb_path)),
            machine_profile: vmservice::vm_config::MachineProfile::Microvm as i32,
            ..Default::default()
        }),
        log_id: String::new(),
        microvm_snapshot: Some(vmservice::MicrovmSnapshotConfig {
            restore_path: snapshot_path.to_string_lossy().into_owned(),
            restore_entropy: true,
            ..Default::default()
        }),
    }
}

async fn wait_for_bytes(
    reader: &mut (impl futures::AsyncRead + Unpin),
    output: &mut Vec<u8>,
    marker: &[u8],
) -> anyhow::Result<()> {
    CancelContext::new()
        .with_timeout(Duration::from_secs(60))
        .until_cancelled(async {
            let mut buffer = [0_u8; 4096];
            loop {
                let count = reader.read(&mut buffer).await?;
                anyhow::ensure!(count != 0, "portb closed before the expected marker");
                output.extend_from_slice(&buffer[..count]);
                if output.windows(marker.len()).any(|window| window == marker) {
                    return Ok(());
                }
            }
        })
        .await
        .context("timed out waiting for portb output")?
}

async fn drain_until_closed(
    reader: &mut (impl futures::AsyncRead + Unpin),
    output: &mut Vec<u8>,
) -> std::io::Result<()> {
    let mut buffer = [0_u8; 4096];
    loop {
        match reader.read(&mut buffer).await {
            Ok(0) => return Ok(()),
            Ok(count) => output.extend_from_slice(&buffer[..count]),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::ConnectionReset
                        | std::io::ErrorKind::ConnectionAborted
                        | std::io::ErrorKind::BrokenPipe
                ) =>
            {
                return Ok(());
            }
            Err(error) => return Err(error),
        }
    }
}

async fn expect_create_vm_error(
    client: &mesh_rpc::Client,
    request: vmservice::CreateVmRequest,
    expected_message: &str,
) -> anyhow::Result<()> {
    let error = client
        .call()
        .start(vmservice::Vm::CreateVm, request)
        .await
        .expect_err("invalid snapshot restore unexpectedly succeeded");
    anyhow::ensure!(
        error.message.contains(expected_message),
        "unexpected restore error (wanted '{expected_message}'): {}",
        error.message
    );
    let properties = client
        .call()
        .start(
            vmservice::Vm::PropertiesVm,
            vmservice::PropertiesVmRequest { types: Vec::new() },
        )
        .await
        .map_err(|status| anyhow::anyhow!("PropertiesVM failed: {}", status.message))?;
    anyhow::ensure!(
        properties.state == vmservice::VmState::Uninitialized as i32,
        "failed restore changed the managed VM state"
    );
    Ok(())
}

fn attachment_free_restore_request(snapshot_path: &Path) -> vmservice::CreateVmRequest {
    vmservice::CreateVmRequest {
        config: None,
        log_id: String::new(),
        microvm_snapshot: Some(vmservice::MicrovmSnapshotConfig {
            restore_path: snapshot_path.to_string_lossy().into_owned(),
            ..Default::default()
        }),
    }
}

fn test_ttrpc_microvm_snapshot_restore(
    params: petri::PetriTestParams<'_>,
    [openvmm, kernel, initrd]: [ResolvedArtifact; 3],
) -> anyhow::Result<()> {
    const MEMORY_BYTES: u64 = 128 * 1024 * 1024;
    const BOOT_MARKER: &[u8] = b"ALPINE-MICROVM-BOOT-OK";
    const RESTORE_MARKER: &[u8] = b"TTRPC-PHASE2-RESTORED";

    let tempdir = if cfg!(target_os = "linux") {
        tempfile::Builder::new()
            .prefix("openvmm-ttrpc-phase2-")
            .tempdir_in("/tmp")
    } else {
        tempfile::tempdir()
    }?;
    let snapshot_path = tempdir.path().join("snapshot");

    DefaultPool::run_with(async |driver| {
        let failed_snapshot_path = tempdir.path().join("failed-snapshot");
        let rpc_path = tempdir.path().join("failed-capture-rpc.sock");
        let pidfile_path = tempdir.path().join("failed-capture.pid");
        let portb_path = tempdir.path().join("failed-capture-portb.sock");
        let (mut failed_child, failed_client, _failed_stderr_task) =
            launch_openvmm(&driver, &params, &openvmm, &rpc_path, &pidfile_path).await?;
        failed_client
            .call()
            .start(
                vmservice::Vm::CreateVm,
                vmservice::CreateVmRequest {
                    config: Some(vmservice::VmConfig {
                        memory_config: Some(vmservice::MemoryConfig {
                            memory_mb: MEMORY_BYTES / 1024 / 1024,
                            ..Default::default()
                        }),
                        processor_config: Some(vmservice::ProcessorConfig {
                            processor_count: 1,
                            ..Default::default()
                        }),
                        serial_config: Some(microvm_portb_config(&portb_path)),
                        boot_config: Some(vmservice::vm_config::BootConfig::PvhBoot(
                            vmservice::PvhBoot {
                                kernel_path: kernel.get().to_string_lossy().into_owned(),
                                initrd_path: initrd.get().to_string_lossy().into_owned(),
                                kernel_cmdline: String::new(),
                            },
                        )),
                        machine_profile: vmservice::vm_config::MachineProfile::Microvm as i32,
                        ..Default::default()
                    }),
                    log_id: String::new(),
                    microvm_snapshot: Some(vmservice::MicrovmSnapshotConfig {
                        destination_path: failed_snapshot_path.to_string_lossy().into_owned(),
                        quiesce_timeout_ms: 5_000,
                        ..Default::default()
                    }),
                },
            )
            .await
            .map_err(|status| {
                anyhow::anyhow!("failed-capture CreateVM failed: {}", status.message)
            })?;
        for attempt in 0..100 {
            std::fs::create_dir(tempdir.path().join(format!(
                ".failed-snapshot.staging-{}-0-{attempt}",
                failed_child.get().id()
            )))?;
        }
        let failed_portb = PolledSocket::new(&driver, UnixStream::connect(&portb_path)?)?;
        let (mut failed_read, mut failed_write) = failed_portb.split();
        failed_client
            .call()
            .start(vmservice::Vm::ResumeVm, ())
            .await
            .map_err(|status| {
                anyhow::anyhow!("failed-capture ResumeVM failed: {}", status.message)
            })?;
        let mut failed_output = Vec::new();
        wait_for_bytes(&mut failed_read, &mut failed_output, BOOT_MARKER).await?;
        failed_write
            .write_all(b"nvx-snapshot; echo TTRPC-PHASE2-ROLLBACK-CONTINUED; nvx-exit 38\n")
            .await?;
        failed_write.flush().await?;
        CancelContext::new()
            .with_timeout(Duration::from_secs(60))
            .until_cancelled(drain_until_closed(&mut failed_read, &mut failed_output))
            .await
            .context("timed out waiting for failed capture rollback")??;
        anyhow::ensure!(
            failed_output
                .split(|byte| *byte == b'\n')
                .map(|line| line.strip_suffix(b"\r").unwrap_or(line))
                .filter(|line| *line == b"TTRPC-PHASE2-ROLLBACK-CONTINUED")
                .count()
                == 1,
            "guest did not continue exactly once after failed snapshot publication"
        );
        anyhow::ensure!(
            !failed_snapshot_path.exists(),
            "failed snapshot publication exposed a final directory"
        );
        anyhow::ensure!(
            failed_child.wait().await?.success(),
            "failed-capture server exited abnormally"
        );
        anyhow::ensure!(
            !pidfile_path.exists(),
            "failed-capture server ignored the guest process-exit event"
        );

        let rpc_path = tempdir.path().join("capture-rpc.sock");
        let pidfile_path = tempdir.path().join("capture.pid");
        let portb_path = tempdir.path().join("capture-portb.sock");
        let (mut child, client, _stderr_task) =
            launch_openvmm(&driver, &params, &openvmm, &rpc_path, &pidfile_path).await?;

        client
            .call()
            .start(
                vmservice::Vm::CreateVm,
                vmservice::CreateVmRequest {
                    config: Some(vmservice::VmConfig {
                        memory_config: Some(vmservice::MemoryConfig {
                            memory_mb: MEMORY_BYTES / 1024 / 1024,
                            ..Default::default()
                        }),
                        processor_config: Some(vmservice::ProcessorConfig {
                            processor_count: 1,
                            ..Default::default()
                        }),
                        serial_config: Some(microvm_portb_config(&portb_path)),
                        boot_config: Some(vmservice::vm_config::BootConfig::PvhBoot(
                            vmservice::PvhBoot {
                                kernel_path: kernel.get().to_string_lossy().into_owned(),
                                initrd_path: initrd.get().to_string_lossy().into_owned(),
                                kernel_cmdline: String::new(),
                            },
                        )),
                        machine_profile: vmservice::vm_config::MachineProfile::Microvm as i32,
                        ..Default::default()
                    }),
                    log_id: String::new(),
                    microvm_snapshot: Some(vmservice::MicrovmSnapshotConfig {
                        destination_path: snapshot_path.to_string_lossy().into_owned(),
                        quiesce_timeout_ms: 5_000,
                        ..Default::default()
                    }),
                },
            )
            .await
            .map_err(|status| anyhow::anyhow!("CreateVM failed: {}", status.message))?;

        let portb = PolledSocket::new(&driver, UnixStream::connect(&portb_path)?)?;
        let (mut portb_read, mut portb_write) = portb.split();
        let mut source_output = Vec::new();
        client
            .call()
            .start(vmservice::Vm::ResumeVm, ())
            .await
            .map_err(|status| anyhow::anyhow!("ResumeVM failed: {}", status.message))?;
        wait_for_bytes(&mut portb_read, &mut source_output, BOOT_MARKER).await?;
        portb_write
            .write_all(b"nvx-snapshot; echo TTRPC-PHASE2-RESTORED; nvx-exit 37\n")
            .await?;
        portb_write.flush().await?;
        CancelContext::new()
            .with_timeout(Duration::from_secs(10))
            .until_cancelled(drain_until_closed(&mut portb_read, &mut source_output))
            .await
            .context("timed out draining captured source output")??;
        anyhow::ensure!(
            source_output
                .split(|byte| *byte == b'\n')
                .map(|line| line.strip_suffix(b"\r").unwrap_or(line))
                .filter(|line| *line == RESTORE_MARKER)
                .count()
                == 0,
            "captured source executed past the snapshot boundary"
        );
        openvmm_helpers::snapshot::read_snapshot(&snapshot_path, MEMORY_BYTES)
            .context("TTRPC capture produced an invalid snapshot")?;
        anyhow::ensure!(child.wait().await?.success(), "capture server failed");
        anyhow::ensure!(
            !pidfile_path.exists(),
            "capture source PID remained alive after snapshot commit"
        );

        for restore_index in 0..2 {
            let rpc_path = tempdir
                .path()
                .join(format!("restore-{restore_index}-rpc.sock"));
            let pidfile_path = tempdir.path().join(format!("restore-{restore_index}.pid"));
            let portb_path = tempdir
                .path()
                .join(format!("restore-{restore_index}-portb.sock"));
            let (mut child, client, _stderr_task) =
                launch_openvmm(&driver, &params, &openvmm, &rpc_path, &pidfile_path).await?;

            if restore_index == 0 {
                let state_path = snapshot_path.join("state.bin");
                let state_bytes = std::fs::read(&state_path)?;
                let mut corrupt_state = state_bytes.clone();
                corrupt_state[0] ^= 0xff;
                std::fs::write(&state_path, &corrupt_state)?;
                expect_create_vm_error(
                    &client,
                    attachment_free_restore_request(&snapshot_path),
                    "digest mismatch",
                )
                .await?;
                std::fs::write(&state_path, &state_bytes)?;

                std::fs::write(snapshot_path.join("unexpected.bin"), b"unexpected")?;
                expect_create_vm_error(
                    &client,
                    attachment_free_restore_request(&snapshot_path),
                    "unexpected artifact",
                )
                .await?;
                std::fs::remove_file(snapshot_path.join("unexpected.bin"))?;

                let manifest_path = snapshot_path.join("manifest.bin");
                let manifest_bytes = std::fs::read(&manifest_path)?;
                let manifest = openvmm_helpers::snapshot::read_snapshot_manifest(&snapshot_path)?;

                let mut wrong_backend = manifest.clone();
                wrong_backend
                    .machine_contract
                    .as_mut()
                    .context("snapshot is missing its machine contract")?
                    .source_hypervisor = if cfg!(windows) { "kvm" } else { "whp" }.to_owned();
                std::fs::write(&manifest_path, mesh::payload::encode(wrong_backend))?;
                expect_create_vm_error(
                    &client,
                    attachment_free_restore_request(&snapshot_path),
                    "does not match destination",
                )
                .await?;
                std::fs::write(&manifest_path, &manifest_bytes)?;

                let mut wrong_topology = manifest.clone();
                wrong_topology
                    .machine_contract
                    .as_mut()
                    .context("snapshot is missing its machine contract")?
                    .topology
                    .apic_ids[0] = 1;
                std::fs::write(&manifest_path, mesh::payload::encode(wrong_topology))?;
                expect_create_vm_error(
                    &client,
                    attachment_free_restore_request(&snapshot_path),
                    "processor topology",
                )
                .await?;
                std::fs::write(&manifest_path, &manifest_bytes)?;

                let mut wrong_cpu = manifest.clone();
                let contract = wrong_cpu
                    .machine_contract
                    .as_mut()
                    .context("snapshot is missing its machine contract")?;
                let mut cpu_contract: virt::x86::CpuCompatibilityContract =
                    mesh::payload::decode(&contract.cpu_contract)?;
                cpu_contract.physical_address_width ^= 1;
                contract.set_cpu_compatibility_contract(mesh::payload::encode(cpu_contract));
                std::fs::write(&manifest_path, mesh::payload::encode(wrong_cpu))?;
                expect_create_vm_error(
                    &client,
                    attachment_free_restore_request(&snapshot_path),
                    "destination CPU contract does not match",
                )
                .await?;
                std::fs::write(&manifest_path, &manifest_bytes)?;

                let mut wrong_tsc = manifest.clone();
                wrong_tsc
                    .machine_contract
                    .as_mut()
                    .context("snapshot is missing its machine contract")?
                    .tsc_frequency_hz += 1;
                std::fs::write(&manifest_path, mesh::payload::encode(wrong_tsc))?;
                expect_create_vm_error(
                    &client,
                    attachment_free_restore_request(&snapshot_path),
                    "destination TSC frequency",
                )
                .await?;
                std::fs::write(&manifest_path, &manifest_bytes)?;

                let mut wrong_inventory = manifest;
                wrong_inventory
                    .machine_contract
                    .as_mut()
                    .context("snapshot is missing its machine contract")?
                    .state_unit_names
                    .reverse();
                std::fs::write(&manifest_path, mesh::payload::encode(wrong_inventory))?;
                expect_create_vm_error(
                    &client,
                    attachment_free_restore_request(&snapshot_path),
                    "state-unit inventory does not match",
                )
                .await?;
                std::fs::write(&manifest_path, &manifest_bytes)?;

                let mut conflicting = microvm_restore_request(&snapshot_path, &portb_path);
                conflicting.config.as_mut().unwrap().memory_config =
                    Some(vmservice::MemoryConfig {
                        memory_mb: 64,
                        ..Default::default()
                    });
                let error = client
                    .call()
                    .start(vmservice::Vm::CreateVm, conflicting)
                    .await
                    .expect_err("restore-time memory override unexpectedly succeeded");
                anyhow::ensure!(
                    error
                        .message
                        .contains("restore configuration may contain only"),
                    "unexpected restore override error: {}",
                    error.message
                );
            }

            client
                .call()
                .start(
                    vmservice::Vm::CreateVm,
                    microvm_restore_request(&snapshot_path, &portb_path),
                )
                .await
                .map_err(|status| anyhow::anyhow!("restore CreateVM failed: {}", status.message))?;
            let portb = PolledSocket::new(&driver, UnixStream::connect(&portb_path)?)?;
            let (mut portb_read, _portb_write) = portb.split();
            client
                .call()
                .start(vmservice::Vm::ResumeVm, ())
                .await
                .map_err(|status| anyhow::anyhow!("restore ResumeVM failed: {}", status.message))?;
            let mut output = Vec::new();
            wait_for_bytes(&mut portb_read, &mut output, RESTORE_MARKER).await?;
            CancelContext::new()
                .with_timeout(Duration::from_secs(10))
                .until_cancelled(drain_until_closed(&mut portb_read, &mut output))
                .await
                .context("timed out draining restored guest output")??;
            anyhow::ensure!(
                output
                    .split(|byte| *byte == b'\n')
                    .map(|line| line.strip_suffix(b"\r").unwrap_or(line))
                    .filter(|line| *line == RESTORE_MARKER)
                    .count()
                    == 1,
                "restore {restore_index} did not continue exactly once"
            );
            openvmm_helpers::snapshot::read_snapshot(&snapshot_path, MEMORY_BYTES)
                .with_context(|| format!("restore {restore_index} modified snapshot artifacts"))?;
            anyhow::ensure!(
                child.wait().await?.success(),
                "restore server {restore_index} failed"
            );
            anyhow::ensure!(
                !pidfile_path.exists(),
                "restore server {restore_index} ignored the guest process-exit event"
            );
        }

        Ok(())
    })
}

fn test_ttrpc_interface(
    params: petri::PetriTestParams<'_>,
    [openvmm, kernel_path, initrd_path, pipette_path]: [ResolvedArtifact; 4],
) -> anyhow::Result<()> {
    // All temporary files for this test live under a single temp directory
    // that is cleaned up automatically when it is dropped at the end of the
    // test.
    let tempdir = tempfile::tempdir()?;
    let socket_path = tempdir.path().join("ttrpc.sock");
    let pidfile_path = tempdir.path().join("openvmm.pid");

    let initrd = std::fs::read(initrd_path.get()).context("failed to read test initrd")?;
    let pipette = std::fs::read(pipette_path.get()).context("failed to read pipette")?;
    let pipette_initrd = initrd_cpio::inject_into_initrd(&initrd, "pipette", &pipette, 0o100755)
        .context("failed to inject pipette into test initrd")?;
    let mut pipette_initrd_file = tempfile::NamedTempFile::new_in(tempdir.path())
        .context("failed to create initrd temp file")?;
    pipette_initrd_file
        .write_all(&pipette_initrd)
        .context("failed to write initrd temp file")?;

    // The serial console device differs by architecture: x86 exposes a 16550
    // UART as `ttyS0`, while aarch64 exposes a PL011 UART as `ttyAMA0`.
    let console = match petri_artifacts_common::tags::MachineArch::host() {
        petri_artifacts_common::tags::MachineArch::X86_64 => "ttyS0",
        petri_artifacts_common::tags::MachineArch::Aarch64 => "ttyAMA0",
    };

    DefaultPool::run_with(async |driver| {
        let (mut child, client, _stderr_task) =
            launch_openvmm(&driver, &params, &openvmm, &socket_path, &pidfile_path).await?;

        let query_props = || {
            client.call().start(
                vmservice::Vm::PropertiesVm,
                vmservice::PropertiesVmRequest { types: Vec::new() },
            )
        };

        let caps = client
            .call()
            .start(vmservice::Vm::CapabilitiesVm, ())
            .await
            .unwrap();
        assert!(
            caps.supported_resources.iter().any(|r| r.resource
                == vmservice::capabilities_vm_response::Resource::Scsi as i32
                && r.add),
            "SCSI add should be advertised as a supported resource"
        );
        assert!(
            caps.supported_resources.iter().any(|r| r.resource
                == vmservice::capabilities_vm_response::Resource::Vpci as i32
                && r.add
                && r.remove
                && !r.update),
            "vPCI add/remove should be advertised as supported"
        );
        assert_eq!(
            caps.supported_guest_os,
            vec![vmservice::capabilities_vm_response::SupportedGuestOs::Linux as i32],
            "only Linux direct boot is supported"
        );

        let props = query_props().await.unwrap();
        assert_eq!(
            props.state,
            vmservice::VmState::Uninitialized as i32,
            "no VM created yet, expected UNINITIALIZED"
        );

        client
            .call()
            .start(
                vmservice::Vm::CreateVm,
                vmservice::CreateVmRequest {
                    config: Some(vmservice::VmConfig::default()),
                    log_id: String::new(),
                    microvm_snapshot: None,
                },
            )
            .await
            .unwrap_err();
        let props = query_props().await.unwrap();
        assert_eq!(
            props.state,
            vmservice::VmState::Uninitialized as i32,
            "a failed CreateVm must leave state UNINITIALIZED"
        );

        // Backing files for the PCIe storage devices created on iteration 0
        // (virtio-blk and an NVMe namespace) and for the vmbus SCSI disk. They
        // are plain raw disks.
        let nvme_disk_path = tempdir.path().join("nvme.img");
        let blk_disk_path = tempdir.path().join("blk.img");
        let scsi_disk_path = tempdir.path().join("scsi.img");
        for path in [&nvme_disk_path, &blk_disk_path, &scsi_disk_path] {
            std::fs::File::create(path)?.set_len(1024 * 1024)?;
        }

        for i in 0..3 {
            let com1_path = tempdir.path().join(format!("com1-{i}.sock"));
            let console_path = tempdir.path().join(format!("console-{i}.sock"));
            let virtiofs_root = tempdir.path().join(format!("virtiofs-{i}"));
            std::fs::create_dir_all(&virtiofs_root)?;
            let hvsocket_path = tempdir.path().join(format!("hvsocket-{i}"));
            let pipette_listener = if i == 0 {
                let path = format!(
                    "{}_{}",
                    hvsocket_path.to_string_lossy(),
                    pipette_client::PIPETTE_PORT
                );
                Some(UnixListener::bind(path)?)
            } else {
                None
            };

            let consomme_nic_id = Guid::new_random().to_string();

            // On iteration 0, test `connect: true` for both serial and
            // virtio console by pre-creating listeners that the VM will
            // connect to. On other iterations, test the default
            // `connect: false` (VM creates the socket).
            let use_connect = i == 0;
            let com1_listener = if use_connect {
                Some(UnixListener::bind(&com1_path).unwrap())
            } else {
                None
            };
            let console_listener = if use_connect {
                Some(UnixListener::bind(&console_path).unwrap())
            } else {
                None
            };

            // On iteration 0, exercise the richer CreateVM surface: a NUMA
            // topology (replacing flat memory), an explicit processor topology,
            // and a PCIe topology with virtio + NVMe devices behind root ports
            // and a switch, plus an empty hotplug port used below for
            // AddPcieDevice/RemovePcieDevice. Other iterations use the simpler
            // flat-memory configuration so the flat path stays covered too.
            let (memory_config, numa_config, processor_config, pcie) = if i == 0 {
                let switch = vmservice::PcieSwitch {
                    name: "sw0".to_string(),
                    downstream_ports: vec![
                        vmservice::PciePort {
                            name: "sw0-dp0".to_string(),
                            hotplug: false,
                            attached: Some(attachment_device(virtio_device(
                                vmservice::virtio_device::Kind::Blk(vmservice::VirtioBlk {
                                    backend: Some(file_disk(&blk_disk_path)),
                                    read_only: false,
                                }),
                            ))),
                            acs_capabilities_supported: Some(1),
                            devfn: None,
                        },
                        vmservice::PciePort {
                            name: "sw0-dp1".to_string(),
                            hotplug: false,
                            attached: None,
                            devfn: None,
                            acs_capabilities_supported: None,
                        },
                    ],
                };
                let root_complex = vmservice::PcieRootComplex {
                    name: "rc0".to_string(),
                    segment: 0,
                    start_bus: 0,
                    end_bus: 255,
                    low_mmio: 64 * 1024 * 1024,
                    high_mmio: 1024 * 1024 * 1024,
                    root_ports: vec![
                        // virtio-rng behind a root port.
                        pcie_root_port(
                            "rp0",
                            false,
                            Some(attachment_device(virtio_device(
                                vmservice::virtio_device::Kind::Rng(vmservice::VirtioRng {}),
                            ))),
                        ),
                        // NVMe controller with a file-backed namespace.
                        pcie_root_port(
                            "rp1",
                            false,
                            Some(attachment_device(vmservice::PcieDeviceKind {
                                kind: Some(vmservice::pcie_device_kind::Kind::Nvme(
                                    vmservice::NvmeConfig {
                                        controller_id: "nvme0".to_string(),
                                        namespaces: vec![vmservice::NvmeNamespace {
                                            nsid: 1,
                                            backend: Some(file_disk(&nvme_disk_path)),
                                            read_only: false,
                                        }],
                                    },
                                )),
                            })),
                        ),
                        // virtio-net (consomme) behind a root port.
                        pcie_root_port(
                            "rp2",
                            false,
                            Some(attachment_device(virtio_device(
                                vmservice::virtio_device::Kind::Net(vmservice::VirtioNet {
                                    max_queues: None,
                                    mac_address: "00-15-5D-12-12-13".to_string(),
                                    backend: Some(vmservice::NicBackend {
                                        kind: Some(vmservice::nic_backend::Kind::Consomme(
                                            vmservice::ConsommeBackend {
                                                cidr: String::new(),
                                                ports: vec![],
                                            },
                                        )),
                                    }),
                                }),
                            ))),
                        ),
                        // A switch hosting a virtio-blk device on its first
                        // downstream port.
                        pcie_root_port("rp3", false, Some(attachment_switch(switch))),
                        // Empty hotplug-capable port for AddPcieDevice.
                        pcie_root_port("rphp", true, None),
                    ],
                    ..Default::default()
                };
                (
                    None,
                    Some(vmservice::NumaConfig {
                        nodes: vec![
                            vmservice::NumaNode {
                                memory: Some(vmservice::NodeMemoryConfig {
                                    memory_mb: 128,
                                    ..Default::default()
                                }),
                                vps: None,
                            },
                            vmservice::NumaNode {
                                memory: Some(vmservice::NodeMemoryConfig {
                                    memory_mb: 128,
                                    ..Default::default()
                                }),
                                vps: None,
                            },
                        ],
                        distances: vec![vmservice::NumaDistance {
                            src: 0,
                            dst: 1,
                            distance: 20,
                        }],
                    }),
                    Some(vmservice::ProcessorConfig {
                        processor_count: 2,
                        ..Default::default()
                    }),
                    Some(vmservice::PcieTopologyConfig {
                        root_complexes: vec![root_complex],
                        generic_initiators: vec![vmservice::PcieGenericInitiator {
                            port_name: "sw0-dp0".to_string(),
                            node: 1,
                        }],
                    }),
                )
            } else {
                (
                    Some(vmservice::MemoryConfig {
                        memory_mb: 256,
                        ..Default::default()
                    }),
                    None,
                    Some(vmservice::ProcessorConfig {
                        processor_count: 2,
                        ..Default::default()
                    }),
                    None,
                )
            };

            let (boot_initrd_path, kernel_cmdline) = if i == 0 {
                (
                    pipette_initrd_file.path(),
                    format!(
                        "console={console} rdinit=/pipette panic=-1 initcall_blacklist=virtio_vsock_init"
                    ),
                )
            } else {
                let guest_command = if i == 1 { "sleep 30" } else { "poweroff -f" };
                (
                    initrd_path.get(),
                    format!("console={console} rdinit=/bin/busybox panic=-1 -- {guest_command}"),
                )
            };

            client
                .call()
                .start(
                    vmservice::Vm::CreateVm,
                    vmservice::CreateVmRequest {
                        config: Some(vmservice::VmConfig {
                            memory_config,
                            numa_config,
                            processor_config,
                            pcie,
                            boot_config: Some(vmservice::vm_config::BootConfig::DirectBoot(
                                vmservice::DirectBoot {
                                    kernel_path: kernel_path.get().to_string_lossy().to_string(),
                                    initrd_path: boot_initrd_path.to_string_lossy().to_string(),
                                    kernel_cmdline,
                                },
                            )),
                            serial_config: Some(vmservice::SerialConfig {
                                ports: vec![vmservice::serial_config::Config {
                                    port: 0,
                                    socket_path: com1_path.to_string_lossy().into(),
                                    connect: use_connect,
                                }],
                            }),
                            devices_config: Some(vmservice::DevicesConfig {
                                nic_config: vec![vmservice::NicConfig {
                                    nic_id: consomme_nic_id.clone(),
                                    mac_address: "00-15-5D-12-12-12".to_string(),
                                    backend: Some(vmservice::nic_config::Backend::Consomme(
                                        vmservice::ConsommeBackend {
                                            cidr: String::new(),
                                            ports: vec![],
                                        },
                                    )),
                                    ..Default::default()
                                }],
                                virtio_console: Some(vmservice::VirtioConsoleConfig {
                                    socket_path: console_path.to_string_lossy().into(),
                                    connect: use_connect,
                                }),
                                virtiofs_config: vec![vmservice::VirtioFsConfig {
                                    tag: "testfs".to_string(),
                                    root_path: virtiofs_root.to_string_lossy().into(),
                                }],
                                // A SCSI controller keeps a request channel
                                // alive for the lifetime of the VM, which used
                                // to stop the VM worker from ever finishing its
                                // stop. Attach a disk so that the teardown and
                                // quit paths below cover that.
                                scsi_disks: vec![vmservice::ScsiDisk {
                                    controller: 0,
                                    lun: 0,
                                    host_path: scsi_disk_path.to_string_lossy().into(),
                                    ..Default::default()
                                }],
                                ..Default::default()
                            }),
                            hvsocket_config: (i == 0).then(|| vmservice::HvSocketConfig {
                                path: hvsocket_path.to_string_lossy().to_string(),
                            }),
                            ..Default::default()
                        }),
                        log_id: String::new(),
                        microvm_snapshot: None,
                    },
                )
                .await
                .unwrap();

            let props = query_props().await.unwrap();
            assert_eq!(
                props.state,
                vmservice::VmState::Paused as i32,
                "VM should be PAUSED immediately after CreateVm"
            );
            assert!(
                props.memory_stats.is_none() && props.processor_stats.is_none(),
                "memory/processor stats should be unset, not zeroed"
            );

            // Invalid protocols exercise Consomme update/remove without binding a port.
            for modify_type in [vmservice::ModifyType::Update, vmservice::ModifyType::Remove] {
                let err = client
                    .call()
                    .start(
                        vmservice::Vm::ModifyResource,
                        vmservice::ModifyResourceRequest {
                            r#type: modify_type as i32,
                            resource: Some(
                                vmservice::modify_resource_request::Resource::NicConfig(
                                    vmservice::NicConfig {
                                        nic_id: consomme_nic_id.clone(),
                                        mac_address: "00-15-5D-12-12-12".to_string(),
                                        backend: Some(vmservice::nic_config::Backend::Consomme(
                                            vmservice::ConsommeBackend {
                                                cidr: String::new(),
                                                ports: vec![vmservice::PortConfig {
                                                    host_port: 8080,
                                                    guest_port: 80,
                                                    protocol: 99,
                                                }],
                                            },
                                        )),
                                        ..Default::default()
                                    },
                                ),
                            ),
                        },
                    )
                    .await
                    .unwrap_err();
                assert!(
                    err.message.contains("invalid protocol"),
                    "expected invalid protocol error, got: {}",
                    err.message
                );
            }

            // On iteration 0, hot-add a virtio-rng device to the empty
            // hotplug-capable port and then hot-remove it, exercising the
            // AddPcieDevice/RemovePcieDevice RPCs.
            if i == 0 {
                client
                    .call()
                    .start(
                        vmservice::Vm::AddPcieDevice,
                        vmservice::AddPcieDeviceRequest {
                            port_name: "rphp".to_string(),
                            device: Some(virtio_device(vmservice::virtio_device::Kind::Rng(
                                vmservice::VirtioRng {},
                            ))),
                        },
                    )
                    .await
                    .unwrap();

                client
                    .call()
                    .start(
                        vmservice::Vm::RemovePcieDevice,
                        vmservice::RemovePcieDeviceRequest {
                            port_name: "rphp".to_string(),
                        },
                    )
                    .await
                    .unwrap();
            }

            // Get the serial connection - either by accepting on our listener
            // (connect: true) or connecting to the VM's socket (connect: false).
            let com1 = if let Some(listener) = com1_listener {
                let (stream, _) = listener.accept().unwrap();
                stream
            } else {
                UnixStream::connect(&com1_path).unwrap()
            };

            // Get the console connection the same way.
            let console = if let Some(listener) = console_listener {
                let (stream, _) = listener.accept().unwrap();
                stream
            } else {
                UnixStream::connect(&console_path).unwrap()
            };

            let _com1_task = driver.spawn(
                "com1",
                petri::log_task(
                    params.logger.log_file("linux").unwrap(),
                    PolledSocket::new(&driver, com1).unwrap(),
                    "linux com1",
                ),
            );

            let _console_task = driver.spawn(
                "console",
                petri::log_task(
                    params.logger.log_file("virtio-console").unwrap(),
                    PolledSocket::new(&driver, console).unwrap(),
                    "virtio console",
                ),
            );

            assert_eq!(
                client
                    .call()
                    .timeout(Some(Duration::from_millis(100)))
                    .start(vmservice::Vm::WaitVm, (),)
                    .await
                    .unwrap_err()
                    .code,
                mesh_rpc::service::Code::DeadlineExceeded as i32
            );

            let waiter = client.call().start(vmservice::Vm::WaitVm, ());

            match i {
                0 | 2 => {
                    client
                        .call()
                        .start(vmservice::Vm::ResumeVm, ())
                        .await
                        .unwrap();

                    let props = query_props().await.unwrap();
                    assert_eq!(
                        props.state,
                        vmservice::VmState::Running as i32,
                        "after ResumeVm, expected RUNNING"
                    );

                    if let Some(listener) = pipette_listener {
                        let mut listener = PolledSocket::new(&driver, listener)?;
                        let (conn, _) = listener.accept().await?;
                        let conn = PolledSocket::new(&driver, conn)?;
                        let agent = pipette_client::PipetteClient::new(
                            &driver,
                            conn,
                            params.logger.output_dir(),
                        )
                        .await?;
                        validate_pcie_config(&agent).await?;
                        agent.power_off().await?;
                    }

                    waiter.await.unwrap();

                    let props = query_props().await.unwrap();
                    assert_eq!(
                        props.state,
                        vmservice::VmState::Halted as i32,
                        "guest powered off, expected HALTED"
                    );
                    assert!(
                        props.halt_reason.as_deref().is_some_and(|r| !r.is_empty()),
                        "HALTED state should carry a halt_reason"
                    );

                    if i == 0 {
                        client
                            .call()
                            .start(vmservice::Vm::TeardownVm, ())
                            .await
                            .unwrap();

                        let props = query_props().await.unwrap();
                        assert_eq!(
                            props.state,
                            vmservice::VmState::Uninitialized as i32,
                            "after TeardownVm, expected UNINITIALIZED"
                        );
                        assert!(
                            props.halt_reason.is_none(),
                            "after TeardownVm, halt_reason should be cleared"
                        );

                        client
                            .call()
                            .start(vmservice::Vm::WaitVm, ())
                            .await
                            .unwrap_err();
                    } else {
                        let _ = client.call().start(vmservice::Vm::Quit, ()).await;
                    }
                }
                1 => {
                    client
                        .call()
                        .start(vmservice::Vm::ResumeVm, ())
                        .await
                        .unwrap();

                    let props = query_props().await.unwrap();
                    assert_eq!(
                        props.state,
                        vmservice::VmState::Running as i32,
                        "after ResumeVm, expected RUNNING"
                    );

                    client
                        .call()
                        .start(vmservice::Vm::PauseVm, ())
                        .await
                        .unwrap();

                    let props = query_props().await.unwrap();
                    assert_eq!(
                        props.state,
                        vmservice::VmState::Paused as i32,
                        "after PauseVm, expected PAUSED"
                    );

                    client
                        .call()
                        .start(vmservice::Vm::TeardownVm, ())
                        .await
                        .unwrap();

                    waiter.await.unwrap_err();
                }
                _ => unreachable!(),
            }
        }

        let exit_status = child.wait().await?;

        // Surface the OpenVMM exit status so that abnormal exits (e.g. an abort
        // from a panic — the workspace uses `panic = 'abort'`) are visible in
        // test logs alongside any pidfile/cleanup assertion below.
        tracing::info!(?exit_status, "openvmm exited");
        assert!(
            exit_status.success(),
            "openvmm exited abnormally: {:?}",
            exit_status
        );

        // Verify the pidfile was cleaned up on exit.
        assert!(
            !pidfile_path.exists(),
            "pidfile should be removed after exit"
        );

        Ok(())
    })
}

petri::test!(test_ttrpc_uefi_boot, |resolver| {
    let openvmm = resolver.require(artifacts::OPENVMM_NATIVE);
    let (firmware, guest_disk) = match petri_artifacts_common::tags::MachineArch::host() {
        petri_artifacts_common::tags::MachineArch::X86_64 => (
            resolver
                .require(artifacts::loadable::UEFI_FIRMWARE_X64)
                .erase(),
            resolver
                .require(artifacts::test_vhd::GUEST_TEST_UEFI_X64)
                .erase(),
        ),
        petri_artifacts_common::tags::MachineArch::Aarch64 => (
            resolver
                .require(artifacts::loadable::UEFI_FIRMWARE_AARCH64)
                .erase(),
            resolver
                .require(artifacts::test_vhd::GUEST_TEST_UEFI_AARCH64)
                .erase(),
        ),
    };
    Some([openvmm.erase(), firmware, guest_disk])
});

/// Boots a VM with UEFI firmware over ttrpc, using the `guest_test_uefi` image
/// as the boot disk on a vmbus SCSI controller.
///
/// The `guest_test_uefi` EFI application prints its banner to the UEFI console,
/// which the firmware routes to COM1. Seeing that banner proves the firmware
/// loaded, enumerated the SCSI disk, and launched the application off of it --
/// rather than merely proving the firmware started.
fn test_ttrpc_uefi_boot(
    params: petri::PetriTestParams<'_>,
    [openvmm, firmware_path, guest_disk_path]: [ResolvedArtifact; 3],
) -> anyhow::Result<()> {
    let tempdir = tempfile::tempdir()?;
    let socket_path = tempdir.path().join("ttrpc.sock");
    let pidfile_path = tempdir.path().join("openvmm.pid");
    let com1_path = tempdir.path().join("com1.sock");

    DefaultPool::run_with(async |driver| {
        let (mut child, client, _stderr_task) =
            launch_openvmm(&driver, &params, &openvmm, &socket_path, &pidfile_path).await?;

        client
            .call()
            .start(
                vmservice::Vm::CreateVm,
                vmservice::CreateVmRequest {
                    config: Some(vmservice::VmConfig {
                        memory_config: Some(vmservice::MemoryConfig {
                            memory_mb: 512,
                            ..Default::default()
                        }),
                        processor_config: Some(vmservice::ProcessorConfig {
                            processor_count: 1,
                            ..Default::default()
                        }),
                        boot_config: Some(vmservice::vm_config::BootConfig::Uefi(
                            vmservice::Uefi {
                                firmware_path: firmware_path.get().to_string_lossy().to_string(),
                                initial_variables: Some(vmservice::uefi::InitialVariables {
                                    secure_boot_template: vmservice::uefi::initial_variables::SecureBootTemplate::MicrosoftWindows as i32,
                                }),
                                secure_boot_enabled: false,
                            },
                        )),
                        // The UEFI watchdog that ends `guest_test_uefi` reports
                        // a reset on aarch64 but a triple fault on x64, so halt
                        // on both rather than rebooting forever.
                        guest_power_actions: Some(vmservice::vm_config::GuestPowerActions {
                            reset: vmservice::vm_config::GuestPowerAction::Halt as i32,
                            watchdog: vmservice::vm_config::GuestPowerAction::Halt as i32,
                            ..Default::default()
                        }),
                        serial_config: Some(vmservice::SerialConfig {
                            ports: vec![vmservice::serial_config::Config {
                                port: 0,
                                socket_path: com1_path.to_string_lossy().into(),
                                connect: false,
                            }],
                        }),
                        devices_config: Some(vmservice::DevicesConfig {
                            scsi_disks: vec![vmservice::ScsiDisk {
                                controller: 0,
                                lun: 0,
                                host_path: guest_disk_path.get().to_string_lossy().to_string(),
                                read_only: true,
                                ..Default::default()
                            }],
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    log_id: String::new(),
                    microvm_snapshot: None,
                },
            )
            .await
            .unwrap();

        let com1 = PolledSocket::new(&driver, UnixStream::connect(&com1_path)?)?;

        // Drain COM1 for as long as the VM is alive, rather than stopping once
        // the banner shows up. The 16550 only reports its transmit holding
        // register empty once the backend has taken the data, so a guest that
        // keeps printing with nothing draining the socket stalls indefinitely.
        let (marker_send, marker_recv) = mesh::oneshot();
        let _com1_task = driver.spawn(
            "com1",
            log_serial(
                params.logger.log_file("uefi")?,
                com1,
                UEFI_BANNER,
                marker_send,
            ),
        );

        // Start waiting for the halt before resuming, so that a guest that
        // reaches the end of its run quickly cannot beat us to it.
        let waiter = client.call().start(vmservice::Vm::WaitVm, ());

        client
            .call()
            .start(vmservice::Vm::ResumeVm, ())
            .await
            .unwrap();

        // The firmware takes a while to initialize and enumerate the SCSI
        // controller before it can launch anything off of the disk.
        CancelContext::new()
            .with_timeout(Duration::from_secs(120))
            .until_cancelled(marker_recv)
            .await
            .context("timed out waiting for the guest UEFI application to run")?
            .context("com1 closed before the guest UEFI application ran")?;

        // `guest_test_uefi` deliberately halts once it has finished its run,
        // so waiting for the halt confirms the guest ran to completion rather
        // than just reaching its first line of output.
        CancelContext::new()
            .with_timeout(Duration::from_secs(120))
            .until_cancelled(waiter)
            .await
            .context("timed out waiting for the guest to halt")?
            .unwrap();

        let props = client
            .call()
            .start(
                vmservice::Vm::PropertiesVm,
                vmservice::PropertiesVmRequest { types: Vec::new() },
            )
            .await
            .unwrap();
        assert_eq!(
            props.state,
            vmservice::VmState::Halted as i32,
            "guest stopped, expected HALTED"
        );
        let halt_reason = props.halt_reason.unwrap_or_default();
        let expected_halt_reason = match petri_artifacts_common::tags::MachineArch::host() {
            petri_artifacts_common::tags::MachineArch::X86_64 => "TripleFault",
            petri_artifacts_common::tags::MachineArch::Aarch64 => "Reset",
        };
        assert!(
            halt_reason.contains(expected_halt_reason),
            "expected a {expected_halt_reason} halt, got {halt_reason:?}"
        );

        // Tearing down a VM that has a SCSI controller used to hang here, so
        // this also covers that: the teardown has to drop the controller's
        // request channel before waiting for the VM worker to stop.
        client
            .call()
            .start(vmservice::Vm::TeardownVm, ())
            .await
            .unwrap();

        let _ = client.call().start(vmservice::Vm::Quit, ()).await;

        let exit_status = child.wait().await?;
        tracing::info!(?exit_status, "openvmm exited");
        assert!(
            exit_status.success(),
            "openvmm exited abnormally: {:?}",
            exit_status
        );

        Ok(())
    })
}

/// The first thing `guest_test_uefi` prints once the firmware hands off to it.
const UEFI_BANNER: &str = "UEFI vendor =";

/// Logs everything read from `reader` until the stream ends, signalling
/// `marker_send` the first time `marker` appears in the output.
///
/// This keeps running after the marker is seen: the guest stalls if its serial
/// output is not drained, so the reader has to stay attached for the lifetime
/// of the VM.
async fn log_serial(
    log_file: petri::PetriLogFile,
    reader: impl futures::AsyncRead + Unpin,
    marker: &str,
    marker_send: mesh::OneshotSender<()>,
) {
    let mut marker_send = Some(marker_send);
    let marker = marker.as_bytes();
    let mut reader = futures::io::BufReader::new(reader);
    let mut line = Vec::new();
    loop {
        match reader.read_until(b'\n', &mut line).await {
            Ok(0) => break,
            Ok(_) => {}
            Err(err) => {
                tracing::warn!(
                    error = &err as &dyn std::error::Error,
                    "error reading from com1"
                );
                break;
            }
        }

        if line.windows(marker.len()).any(|window| window == marker) {
            if let Some(send) = marker_send.take() {
                send.send(());
            }
        }

        log_file.write_entry(String::from_utf8_lossy(&line).trim_end());
        line.clear();
    }
}

/// Wraps a `PcieDeviceKind` as a device attachment behind a PCIe port.
fn attachment_device(device: vmservice::PcieDeviceKind) -> vmservice::PcieAttachment {
    vmservice::PcieAttachment {
        kind: Some(vmservice::pcie_attachment::Kind::Device(device)),
    }
}

/// Wraps a `PcieSwitch` as a switch attachment behind a PCIe port.
fn attachment_switch(switch: vmservice::PcieSwitch) -> vmservice::PcieAttachment {
    vmservice::PcieAttachment {
        kind: Some(vmservice::pcie_attachment::Kind::Switch(switch)),
    }
}

/// Builds a PCIe root port with the given name, hotplug flag, and optional
/// attached device/switch.
fn pcie_root_port(
    name: &str,
    hotplug: bool,
    attached: Option<vmservice::PcieAttachment>,
) -> vmservice::PciePort {
    vmservice::PciePort {
        name: name.to_string(),
        hotplug,
        attached,
        devfn: None,
        acs_capabilities_supported: None,
    }
}

/// Wraps a virtio device function kind as a `PcieDeviceKind`.
fn virtio_device(kind: vmservice::virtio_device::Kind) -> vmservice::PcieDeviceKind {
    vmservice::PcieDeviceKind {
        kind: Some(vmservice::pcie_device_kind::Kind::Virtio(
            vmservice::VirtioDevice { kind: Some(kind) },
        )),
    }
}

/// Spawns `openvmm --rpc path=<socket_path>,transport=ttrpc --pidfile
/// <pidfile_path>`, waits for it to signal readiness (by closing stdout),
/// validates the pidfile, and connects a ttrpc client.
///
/// Returns the child process, a connected ttrpc client, and the stderr-pump
/// task (which must be kept alive for the child's lifetime).
async fn launch_openvmm(
    driver: &DefaultDriver,
    params: &petri::PetriTestParams<'_>,
    openvmm: &ResolvedArtifact,
    socket_path: &Path,
    pidfile_path: &Path,
) -> anyhow::Result<(OpenvmmChild, mesh_rpc::Client, Task<anyhow::Result<()>>)> {
    tracing::info!(socket_path = %socket_path.display(), "launching OpenVMM with ttrpc");

    let (stderr_read, stderr_write) = pal::pipe_pair()?;
    let (stdout_read, stdout_write) = pal::pipe_pair()?;
    let child = std::process::Command::new(openvmm)
        .arg("--rpc")
        .arg(format!("path={},transport=ttrpc", socket_path.display()))
        .arg("--pidfile")
        .arg(pidfile_path)
        .stdin(Stdio::null())
        .stdout(stdout_write)
        .stderr(stderr_write)
        .spawn()?;

    // Wrap the child immediately so that the error paths below (and any test
    // failure after this function returns) tear the process down.
    let mut child = OpenvmmChild(PolledChild::<std::process::Child>::new(driver, child)?);

    // Start pumping stderr immediately so the pipe buffer doesn't fill up and
    // block the child.
    let stderr_task = driver.spawn(
        "stderr",
        petri::log_task(
            params.logger.log_file("stderr")?,
            PolledPipe::new(driver, stderr_read)?,
            "openvmm stderr",
        ),
    );

    // Wait for stdout to close (readiness signal). If the child crashes at
    // startup, stdout closes too and we detect the exit when the pidfile is
    // missing.
    let mut stdout = PolledPipe::new(driver, stdout_read)?;
    let mut buf = [0u8; 1];
    let n = stdout
        .read(&mut buf)
        .await
        .context("reading from openvmm stdout")?;
    anyhow::ensure!(n == 0, "openvmm wrote unexpected data to stdout");
    drop(stdout);

    // Verify the pidfile was created with the correct PID. If it's missing,
    // wait briefly for the child to exit (the PidfileGuard deletes it on drop)
    // and report the exit status.
    let pid_content = match std::fs::read_to_string(pidfile_path) {
        Ok(s) => s,
        Err(e) => {
            let wait_result = CancelContext::new()
                .with_timeout(Duration::from_secs(10))
                .until_cancelled(child.wait())
                .await;
            match wait_result {
                Ok(Ok(status)) => {
                    let _ = stderr_task.await;
                    anyhow::bail!("openvmm exited with {status} before pidfile was created");
                }
                _ => {
                    return Err(e).context("failed to read pidfile");
                }
            }
        }
    };
    assert_eq!(
        pid_content,
        format!("{}\n", child.get().id()),
        "pidfile should contain the child PID"
    );

    let client = mesh_rpc::Client::new(
        driver,
        mesh_rpc::client::UnixDialier::new(driver.clone(), socket_path.to_path_buf()),
    );

    Ok((child, client, stderr_task))
}

/// Owns the OpenVMM process launched by [`launch_openvmm`], killing it on drop.
///
/// [`std::process::Child`] deliberately does *not* kill the process when it is
/// dropped. Without this guard, any test that fails or panics before reaching
/// its `TeardownVM`/`Quit` calls leaves an orphaned OpenVMM process behind,
/// still running its VM and still holding the ttrpc socket.
struct OpenvmmChild(PolledChild<std::process::Child>);

impl Deref for OpenvmmChild {
    type Target = PolledChild<std::process::Child>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for OpenvmmChild {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Drop for OpenvmmChild {
    fn drop(&mut self) {
        let child = self.0.get_mut();
        // `kill` reports success for an already-reaped child, so ask `try_wait`
        // whether the process is actually gone rather than relying on that.
        if let Ok(Some(_)) = child.try_wait() {
            return;
        }
        tracing::warn!("killing openvmm, which was still running at the end of the test");
        if let Err(err) = child.kill() {
            tracing::warn!(
                error = &err as &dyn std::error::Error,
                "failed to kill openvmm"
            );
            return;
        }
        // Reap the process so it doesn't linger as a zombie. It was just
        // killed, so this returns promptly.
        let _ = child.wait();
    }
}

/// Builds a file-backed disk backend for the given path.
fn file_disk(path: &Path) -> vmservice::DiskBackend {
    vmservice::DiskBackend {
        kind: Some(vmservice::disk_backend::Kind::File(vmservice::FileDisk {
            path: path.to_string_lossy().into(),
            direct: false,
        })),
    }
}

async fn validate_pcie_config(agent: &pipette_client::PipetteClient) -> anyhow::Result<()> {
    let sh = agent.unix_shell();
    let devices = cmd!(sh, "ls /sys/bus/pci/devices").read().await?;
    let mut device = None;
    for bdf in devices.split_whitespace() {
        let class = sh
            .read_file(format!("/sys/bus/pci/devices/{bdf}/class"))
            .await?;
        if class.trim() == "0x010000" {
            device = Some(bdf);
            break;
        }
    }
    let device = device.context("virtio-blk PCI device not found")?;

    let mut bdf = device.split([':', '.']);
    let segment = u16::from_str_radix(bdf.next().context("missing PCI segment")?, 16)?;
    let bus = u8::from_str_radix(bdf.next().context("missing PCI bus")?, 16)?;
    let device_number = u8::from_str_radix(bdf.next().context("missing PCI device")?, 16)?;
    let function = u8::from_str_radix(bdf.next().context("missing PCI function")?, 16)?;
    anyhow::ensure!(bdf.next().is_none(), "invalid PCI BDF {device}");

    let srat = agent.read_file("/sys/firmware/acpi/tables/SRAT").await?;
    anyhow::ensure!(
        srat.get(..4) == Some(b"SRAT"),
        "guest SRAT has an invalid signature"
    );
    let mut offset = 48;
    let mut found_generic_initiator = false;
    while offset + 2 <= srat.len() {
        let entry_len = srat[offset + 1] as usize;
        anyhow::ensure!(
            entry_len >= 2 && offset + entry_len <= srat.len(),
            "guest SRAT contains an invalid entry at offset {offset:#x}"
        );
        if srat[offset] == 5 && entry_len == 32 {
            let proximity_domain =
                u32::from_le_bytes(srat[offset + 4..offset + 8].try_into().unwrap());
            let entry_segment =
                u16::from_le_bytes(srat[offset + 8..offset + 10].try_into().unwrap());
            let entry_bus = srat[offset + 10];
            let entry_devfn = srat[offset + 11];
            let flags = u32::from_le_bytes(srat[offset + 24..offset + 28].try_into().unwrap());
            if srat[offset + 3] == 1
                && proximity_domain == 1
                && entry_segment == segment
                && entry_bus == bus
                && entry_devfn == (device_number << 3) | function
                && flags & 1 != 0
            {
                found_generic_initiator = true;
                break;
            }
        }
        offset += entry_len;
    }
    anyhow::ensure!(
        found_generic_initiator,
        "guest SRAT has no enabled Generic Initiator entry for {device} on NUMA node 1"
    );

    let device_path = cmd!(sh, "readlink -f /sys/bus/pci/devices/{device}")
        .read()
        .await?;
    let port_path = Path::new(device_path.trim())
        .parent()
        .and_then(Path::to_str)
        .context("PCI device has no parent port")?;
    let config = sh.read_file_raw(format!("{port_path}/config")).await?;
    let mut capability_offset = 0x100;
    let acs_offset = loop {
        let header = u32::from_le_bytes(
            config
                .get(capability_offset..capability_offset + 4)
                .context("invalid PCIe extended capability offset")?
                .try_into()
                .unwrap(),
        );
        let capability_id = header as u16;
        if capability_id == 0x000d {
            break capability_offset;
        }

        let next_offset = (header >> 20) as usize;
        anyhow::ensure!(
            next_offset > capability_offset && next_offset.is_multiple_of(4),
            "parent port has no ACS capability"
        );
        capability_offset = next_offset;
    };
    let acs_capabilities = u16::from_le_bytes(
        config
            .get(acs_offset + 4..acs_offset + 6)
            .context("parent port has no ACS capability register")?
            .try_into()
            .unwrap(),
    );
    anyhow::ensure!(
        acs_capabilities == 1,
        "expected ACS capability mask 0x0001, got {acs_capabilities:#06x}"
    );

    Ok(())
}
