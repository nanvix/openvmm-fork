// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use anyhow::Context;
use mesh::CancelContext;
use petri::PetriHaltReason;
use petri::PetriVmBuilder;
use petri::openvmm::OpenVmmPetriBackend;
use std::time::Duration;
use vm_resource::IntoResource;
use vmm_test_macros::openvmm_test_no_agent;

#[openvmm_test_no_agent(ignore(
    reason = "requires a published microVM PVH kernel and initramfs",
    microvm_pvh_x64
))]
async fn phase_1_lifecycle(config: PetriVmBuilder<OpenVmmPetriBackend>) -> anyhow::Result<()> {
    const TIMEOUT: Duration = Duration::from_secs(30);
    const BOOT_MARKER: &str = "ALPINE-MICROVM-BOOT-OK";
    const CONTINUED_MARKER: &str = "PETRI-MICROVM-SNAPSHOT-CONTINUED";
    const RAW_MARKER: &[u8] = b"\0\r\n\x7f\xffPETRI-MICROVM-SNAPSHOT-CONTINUED";

    let mut vm = config.with_microvm_machine().run_without_agent().await?;

    CancelContext::new()
        .with_timeout(TIMEOUT)
        .until_cancelled(vm.backend().wait_for_microvm_portb_output(BOOT_MARKER))
        .await
        .context("timed out waiting for microVM boot marker")??;

    let save_error = vm
        .backend()
        .save_state()
        .await
        .expect_err("microVM host save unexpectedly succeeded");
    assert!(
        format!("{save_error:#}").contains("save is unavailable for microVM ABI version 1"),
        "unexpected microVM host-save error: {save_error:#}"
    );
    let pulse_error = vm
        .backend()
        .pulse_save_restore()
        .await
        .expect_err("microVM pulse save/restore unexpectedly succeeded");
    assert!(
        format!("{pulse_error:#}")
            .contains("save and restore are unavailable for this machine profile"),
        "unexpected microVM pulse-save error: {pulse_error:#}"
    );

    vm.backend()
        .write_microvm_portb_input(
            format!(
                "printf '\\013' | dd of=/dev/port bs=1 seek=112 count=1 conv=notrunc 2>/dev/null; \
                 rtc=$(dd if=/dev/port bs=1 skip=113 count=1 2>/dev/null | od -An -tu1 | tr -d '[:space:]'); \
                 [ \"$rtc\" = 6 ] || {{ nvx-exit 40; exit; }}; \
                 [ \"$(date +%s)\" -ge 1500000000 ] || {{ nvx-exit 41; exit; }}; \
                 irq0=$(awk '/^[[:space:]]*0:/ {{ print $2; exit }}' /proc/interrupts); \
                 [ \"${{irq0:-0}}\" -gt 0 ] || {{ nvx-exit 42; exit; }}; \
                 up1=$(awk '{{ print int($1); exit }}' /proc/uptime); sleep 1; \
                 up2=$(awk '{{ print int($1); exit }}' /proc/uptime); \
                 [ $((up2 - up1)) -ge 1 ] || {{ nvx-exit 43; exit; }}; \
                 for octal in 000 015 012 177 377; do \
                     printf \"\\\\$octal\" | dd of=/dev/port bs=1 seek=233 count=1 conv=notrunc 2>/dev/null; \
                 done; \
                 nvx-snapshot; echo {CONTINUED_MARKER}; nvx-exit 37\n"
            )
            .as_bytes(),
        )
        .await?;

    CancelContext::new()
        .with_timeout(TIMEOUT)
        .until_cancelled(vm.backend().wait_for_microvm_portb_bytes(RAW_MARKER))
        .await
        .context("microVM raw output or snapshot continuation marker was not observed")??;

    let halt = CancelContext::new()
        .with_timeout(TIMEOUT)
        .until_cancelled(vm.wait_for_halt())
        .await
        .context("timed out waiting for microVM status shutdown")??;
    assert_eq!(halt.reason, PetriHaltReason::PowerOff);
    assert!(
        halt.detail.contains("code: 37"),
        "expected microVM shutdown status 37, got {}",
        halt.detail
    );

    vm.teardown().await
}

#[openvmm_test_no_agent(ignore(
    reason = "requires a published microVM PVH kernel and initramfs",
    microvm_pvh_x64
))]
async fn phase_1_block(config: PetriVmBuilder<OpenVmmPetriBackend>) -> anyhow::Result<()> {
    use disk_backend_resources::LayeredDiskHandle;
    use disk_backend_resources::layer::RamDiskLayerHandle;
    use openvmm_defs::config::LoadMode;
    use openvmm_defs::config::VirtioBus;
    use openvmm_defs::config::append_microvm_virtio_blk_discovery;
    use virtio_resources::blk::VirtioBlkHandle;

    const TIMEOUT: Duration = Duration::from_secs(30);
    const DISK_SIZE: u64 = 8 * 1024 * 1024;
    const WORKLOAD: &[u8] = br#"#!/bin/sh
set -eu
tries=0
while [ ! -b /dev/vda ] && [ "$tries" -lt 200 ]; do
    sleep 0.05
    tries=$((tries + 1))
done
device=$(readlink -f /sys/block/vda/device) || exit 20
grep -q 'virtio_mmio.device=0x1000@0xd0003000:4' /proc/cmdline || exit 21
[ "$(cat /sys/block/vda/size)" = 16384 ] || exit 23
printf MICROVM-BLOCK-RW-OK | dd of=/dev/vda bs=512 count=1 conv=sync,notrunc 2>/dev/null
readback=$(dd if=/dev/vda bs=512 count=1 2>/dev/null | head -c 15)
[ "$readback" = MICROVM-BLOCK-RW-OK ] || exit 24
exit 37
"#;

    let modified_initrd =
        config.prepare_initrd_with_file("microvm-block-test.sh", WORKLOAD, 0o100755)?;
    let disk = LayeredDiskHandle::single_layer(RamDiskLayerHandle {
        len: Some(DISK_SIZE),
        sector_size: None,
    })
    .into_resource();

    let mut vm = config
        .with_prebuilt_initrd(modified_initrd.to_path_buf())
        .with_microvm_machine()
        .modify_backend(move |backend| {
            backend.with_custom_config(|config| {
                let LoadMode::Pvh { cmdline, .. } = &mut config.load_mode else {
                    panic!("microVM test did not produce PVH load mode");
                };
                cmdline.push_str(" nvx_exec=/microvm-block-test.sh");
                append_microvm_virtio_blk_discovery(cmdline).unwrap();
                config.virtio_devices.push((
                    VirtioBus::Mmio,
                    VirtioBlkHandle {
                        disk,
                        read_only: false,
                    }
                    .into_resource(),
                ));
            })
        })
        .run_without_agent()
        .await?;

    let halt = CancelContext::new()
        .with_timeout(TIMEOUT)
        .until_cancelled(vm.wait_for_halt())
        .await
        .context("timed out waiting for microVM block workload")??;
    assert_eq!(halt.reason, PetriHaltReason::PowerOff);
    assert!(
        halt.detail.contains("code: 37"),
        "microVM block workload failed: {}",
        halt.detail
    );

    vm.teardown().await
}
