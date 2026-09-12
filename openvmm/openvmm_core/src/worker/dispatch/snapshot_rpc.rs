// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Keep management mutations outside the guest-requested snapshot boundary.

use mesh::error::RemoteError;
use openvmm_defs::rpc::PulseSaveRestoreError;
use openvmm_defs::rpc::VmRpc;

#[derive(Debug, thiserror::Error)]
#[error("VM mutation is unavailable while a microVM snapshot boundary is active")]
struct SnapshotBoundaryActive;

pub(super) fn filter(message: VmRpc, boundary_active: bool) -> Option<VmRpc> {
    if !boundary_active {
        return Some(message);
    }

    let operation = format!("{message:?}");
    match message {
        message @ (VmRpc::QuiesceForSnapshot(_)
        | VmRpc::ResumeAfterFailedSnapshot(_)
        | VmRpc::ReleaseSnapshotBoundary(_)
        | VmRpc::ReadMemory(_)) => return Some(message),
        VmRpc::Save(rpc) => rpc.fail(SnapshotBoundaryActive),
        VmRpc::Resume(rpc) => rpc.fail(SnapshotBoundaryActive),
        VmRpc::Reset(rpc) => rpc.fail(SnapshotBoundaryActive),
        VmRpc::AddVmbusDevice(rpc) => rpc.fail(SnapshotBoundaryActive),
        VmRpc::ConnectHvsock(rpc) => rpc.fail(SnapshotBoundaryActive),
        VmRpc::StartReloadIgvm(rpc) => rpc.fail(SnapshotBoundaryActive),
        VmRpc::CompleteReloadIgvm(rpc) => rpc.fail(SnapshotBoundaryActive),
        VmRpc::WriteMemory(rpc) => rpc.fail(SnapshotBoundaryActive),
        VmRpc::UpdateCliParams(rpc) => rpc.fail(SnapshotBoundaryActive),
        VmRpc::AddPcieDevice(rpc) => rpc.fail(SnapshotBoundaryActive),
        VmRpc::RemovePcieDevice(rpc) => rpc.fail(SnapshotBoundaryActive),
        VmRpc::AddVpciDevice(rpc) => rpc.fail(SnapshotBoundaryActive),
        VmRpc::RemoveVpciDevice(rpc) => rpc.fail(SnapshotBoundaryActive),
        VmRpc::DumpState(rpc) => rpc.fail(SnapshotBoundaryActive),
        VmRpc::PulseSaveRestore(rpc) => rpc.complete(Err(PulseSaveRestoreError::Other(
            RemoteError::new(SnapshotBoundaryActive),
        ))),
        VmRpc::Pause(rpc) | VmRpc::ClearHalt(rpc) => drop(rpc),
        VmRpc::Nmi(rpc) => drop(rpc),
    }
    tracelimit::warn_ratelimited!(
        rpc = operation,
        "rejected management RPC during microVM snapshot boundary"
    );
    None
}

#[cfg(test)]
mod tests {
    use super::filter;
    use futures::executor::block_on;
    use mesh::rpc::Rpc;
    use mesh::rpc::RpcSend;
    use openvmm_defs::rpc::VmRpc;
    use std::time::Duration;
    use test_with_tracing::test;

    #[test]
    fn mutation_outside_snapshot_reaches_dispatch() {
        for message in [
            VmRpc::WriteMemory(Rpc::detached((0, vec![1]))),
            VmRpc::Reset(Rpc::detached(())),
            VmRpc::Resume(Rpc::detached(())),
            VmRpc::Pause(Rpc::detached(())),
        ] {
            assert!(filter(message, false).is_some());
        }
    }

    #[test]
    fn snapshot_control_and_reads_reach_dispatch() {
        for message in [
            VmRpc::QuiesceForSnapshot(Rpc::detached(Duration::from_secs(1))),
            VmRpc::ResumeAfterFailedSnapshot(Rpc::detached(Duration::from_secs(1))),
            VmRpc::ReleaseSnapshotBoundary(Rpc::detached(())),
            VmRpc::ReadMemory(Rpc::detached((0, 1))),
        ] {
            assert!(filter(message, true).is_some());
        }
    }

    #[test]
    fn snapshot_mutations_fail_without_waiting_for_release() {
        let (send, mut recv) = mesh::channel();
        let write = send.call(VmRpc::WriteMemory, (0, vec![1]));
        let reset = send.call(VmRpc::Reset, ());
        let resume = send.call(VmRpc::Resume, ());
        let remove = send.call(VmRpc::RemovePcieDevice, "port".to_owned());

        for _ in 0..4 {
            assert!(filter(recv.try_recv().unwrap(), true).is_none());
        }
        for result in [block_on(write), block_on(reset), block_on(remove)] {
            let error = result.unwrap().unwrap_err();
            assert!(error.to_string().contains("snapshot boundary is active"));
        }
        assert!(block_on(resume).unwrap().is_err());

        let release = send.call(VmRpc::ReleaseSnapshotBoundary, ());
        let Some(VmRpc::ReleaseSnapshotBoundary(rpc)) = filter(recv.try_recv().unwrap(), true)
        else {
            panic!("snapshot release was blocked");
        };
        rpc.complete(Ok(()));
        block_on(release).unwrap().unwrap();
    }

    #[test]
    fn infallible_mutations_report_channel_error() {
        let (send, mut recv) = mesh::channel();
        let pause = send.call(VmRpc::Pause, ());
        let clear_halt = send.call(VmRpc::ClearHalt, ());
        let nmi = send.call(VmRpc::Nmi, 0);
        for _ in 0..3 {
            assert!(filter(recv.try_recv().unwrap(), true).is_none());
        }
        assert!(block_on(pause).is_err());
        assert!(block_on(clear_halt).is_err());
        assert!(block_on(nmi).is_err());
    }

    #[test]
    fn mutation_can_be_retried_after_boundary_release() {
        let (send, mut recv) = mesh::channel();
        let first = send.call(VmRpc::WriteMemory, (0, vec![1]));
        assert!(filter(recv.try_recv().unwrap(), true).is_none());
        assert!(block_on(first).unwrap().is_err());

        let retry = send.call(VmRpc::WriteMemory, (0, vec![2]));
        let Some(VmRpc::WriteMemory(rpc)) = filter(recv.try_recv().unwrap(), false) else {
            panic!("write was blocked after boundary release");
        };
        rpc.complete(Ok(()));
        block_on(retry).unwrap().unwrap();
    }
}
