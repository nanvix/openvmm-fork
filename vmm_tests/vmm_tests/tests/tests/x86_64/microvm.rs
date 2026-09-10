// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use anyhow::Context;
use mesh::CancelContext;
use petri::PetriHaltReason;
use petri::PetriVmBuilder;
use petri::openvmm::OpenVmmPetriBackend;
use std::time::Duration;
use vmm_test_macros::openvmm_test_no_agent;

#[openvmm_test_no_agent(microvm_test_pvh_x64)]
async fn phase_1_lifecycle(config: PetriVmBuilder<OpenVmmPetriBackend>) -> anyhow::Result<()> {
    const TIMEOUT: Duration = Duration::from_secs(30);
    const BOOT_MARKER: &str = "OPENVMM-PVH-TEST-READY";
    const RAW_MARKER: &[u8] = b"\0\r\n\x7f\xffPVH-ECHO";
    const COMMAND_PING: u8 = 1;
    const COMMAND_ECHO: u8 = 2;
    const COMMAND_SNAPSHOT: u8 = 3;
    const COMMAND_STATE: u8 = 5;
    const COMMAND_SHUTDOWN: u8 = 6;

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
        format!("{save_error:#}").contains("save is unavailable for microVM"),
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
        .write_microvm_portb_input(&[COMMAND_PING])
        .await?;
    CancelContext::new()
        .with_timeout(TIMEOUT)
        .until_cancelled(vm.backend().wait_for_microvm_portb_output("PONG"))
        .await
        .context("microVM PVH guest did not answer ping")??;

    vm.backend()
        .write_microvm_portb_input(&[COMMAND_ECHO])
        .await?;
    CancelContext::new()
        .with_timeout(TIMEOUT)
        .until_cancelled(vm.backend().wait_for_microvm_portb_bytes(RAW_MARKER))
        .await
        .context("microVM raw binary echo was not observed")??;

    vm.backend()
        .write_microvm_portb_input(&[COMMAND_SNAPSHOT])
        .await?;
    CancelContext::new()
        .with_timeout(TIMEOUT)
        .until_cancelled(
            vm.backend()
                .wait_for_microvm_portb_output("SNAPSHOT-CONTINUED=1"),
        )
        .await
        .context("microVM snapshot request did not return to the guest")??;

    vm.backend()
        .write_microvm_portb_input(&[COMMAND_STATE])
        .await?;
    CancelContext::new()
        .with_timeout(TIMEOUT)
        .until_cancelled(vm.backend().wait_for_microvm_portb_output("STATE=1"))
        .await
        .context("microVM guest state did not persist after the snapshot request")??;

    vm.backend()
        .write_microvm_portb_input(&[COMMAND_SHUTDOWN, 37])
        .await?;

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
