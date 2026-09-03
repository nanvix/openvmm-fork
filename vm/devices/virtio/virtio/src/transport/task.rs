// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Device task and shared transport state machine for virtio transports.
//!
//! Both the PCI and MMIO transports spawn an async task that owns the
//! `Box<dyn DynVirtioDevice>` and processes commands via a mesh channel.
//! The transports become thin MMIO/PCI forwarders that send RPCs to
//! the task.

use crate::DynVirtioDevice;
use crate::QueueResources;
use crate::queue::QueueState;
use crate::spec::VirtioDeviceFeatures;
use chipset_device::io::IoError;
use chipset_device::io::IoResult;
use chipset_device::io::deferred::DeferredRead;
use chipset_device::io::deferred::DeferredWrite;
use chipset_device::io::deferred::defer_read;
use chipset_device::io::deferred::defer_write;
use futures::StreamExt;
use inspect::Inspect;
use mesh::rpc::FailableRpc;
use mesh::rpc::PendingRpc;
use mesh::rpc::Rpc;
use mesh::rpc::RpcSend;
use std::pin::Pin;
use std::task::Context;
use std::task::Poll;
use vmcore::save_restore::SaveError;
use vmcore::save_restore::SavedStateBlob;

/// Commands sent from the transport to the device task.
pub enum DeviceCommand {
    /// Guest writes DRIVER_OK — start all enabled queues.
    /// Returns true on success, false on failure (errors are logged
    /// inside the device task).
    Enable(Rpc<EnableParams, bool>),
    /// Guest writes status=0 — stop all queues, reset device.
    Disable(Rpc<(), ()>),
    /// ChangeDeviceState::stop() — stop queues, return states for resume.
    Stop(Rpc<(), StopResult>),
    /// ChangeDeviceState::start() — restart queues with saved states.
    Start(FailableRpc<StartParams, ()>),
    /// ChangeDeviceState::reset() — stop queues, reset device.
    Reset(Rpc<(), ()>),
    /// Gate host input before establishing a snapshot vCPU boundary.
    QuiesceInput(FailableRpc<(), ()>),
    /// Resume host input after a failed snapshot transaction.
    ResumeInput(FailableRpc<(), ()>),
    /// Config register read at byte offset with byte length.
    ReadConfig {
        offset: u16,
        len: u8,
        completion: ConfigReadCompletion,
        deferred: DeferredRead,
    },
    /// Config register write at byte offset with raw data.
    WriteConfig {
        offset: u16,
        len: u8,
        data: [u8; 8],
        deferred: DeferredWrite,
    },
    /// Queue notification, serialized with private-state activation.
    Kick { idx: u16, event: pal_event::Event },
    /// Inspect the device state.
    Inspect(inspect::Deferred),
}

/// How a deferred config read completion should be sized and positioned.
#[derive(Copy, Clone, Debug)]
pub enum ConfigReadCompletion {
    /// Complete exactly `len` bytes at their natural position — used when the
    /// caller polls the deferred read with a `len`-sized buffer (direct MMIO).
    Exact,
    /// Complete a full 4-byte dword with the accessed `len` bytes left-aligned
    /// into the low bytes — used by the `VIRTIO_PCI_CAP_PCI_CFG` `pci_cfg_data`
    /// window, which the PCI config bus polls with a 4-byte dword buffer. Per
    /// the virtio spec the accessed bytes occupy the first `cap.length` bytes.
    LeftAlignedDword,
}

/// Parameters for the Enable command.
pub struct EnableParams {
    pub queues: Vec<(u16, QueueResources, Option<QueueState>)>,
    pub features: VirtioDeviceFeatures,
}

/// Parameters for the Start command.
pub struct StartParams {
    pub queues: Vec<(u16, QueueResources, Option<QueueState>)>,
    pub features: VirtioDeviceFeatures,
    pub device_state: DeviceRestoreState,
    pub active: bool,
}

/// Whether this start follows restore, and its optional private payload.
#[derive(Clone)]
pub enum DeviceRestoreState {
    NotRestored,
    Restored(Option<SavedStateBlob>),
}

impl DeviceRestoreState {
    pub fn is_restored(&self) -> bool {
        matches!(self, Self::Restored(_))
    }
}

/// Queue and device-private state captured after a device has stopped.
pub struct StopResult {
    pub queues: Vec<(bool, Option<QueueState>)>,
    pub device_state: Result<Option<SavedStateBlob>, SaveError>,
}

/// Transport-side state machine tracking in-flight device operations.
///
/// When the guest writes DRIVER_OK, the transport sends an Enable RPC to
/// the device task and transitions to `Enabling`.  The guest's MMIO/PCI
/// write is deferred (via [`chipset_device::io::IoResult::Defer`]) so the
/// writing VCPU blocks until the enable completes.  Concurrent transport
/// register accesses from other VCPUs are stalled and replayed once the
/// operation finishes.  Device-config register accesses are not stalled —
/// they are forwarded to the device task via the channel and serialized
/// with Enable/Disable naturally.
///
/// Similarly, when the guest writes STATUS=0 with queues active, a Disable
/// RPC is sent and the write is deferred until teardown is complete.
#[derive(Inspect)]
#[inspect(tag = "state")]
pub enum TransportState {
    Ready,
    Enabling {
        #[inspect(skip)]
        rpc: PendingRpc<bool>,
    },
    Disabling {
        #[inspect(skip)]
        rpc: PendingRpc<()>,
    },
}

/// Result from polling the transport state machine.
#[must_use]
pub enum TransportStateResult {
    EnableComplete(bool),
    DisableComplete,
}

impl TransportState {
    pub fn is_busy(&self) -> bool {
        !matches!(self, TransportState::Ready)
    }

    /// Send Enable to the device task and transition to `Enabling`.
    ///
    /// Panics if the transport is not `Ready`.
    pub fn start_enable(
        &mut self,
        sender: &mesh::Sender<DeviceCommand>,
        queues: Vec<(u16, QueueResources, Option<QueueState>)>,
        features: VirtioDeviceFeatures,
    ) {
        assert!(!self.is_busy());
        let rpc = sender.call(DeviceCommand::Enable, EnableParams { queues, features });
        *self = TransportState::Enabling { rpc };
    }

    /// Send Disable to the device task and transition to `Disabling`.
    ///
    /// Panics if the transport is not `Ready`.
    pub fn start_disable(&mut self, sender: &mesh::Sender<DeviceCommand>) {
        assert!(!self.is_busy());
        let rpc = sender.call(DeviceCommand::Disable, ());
        *self = TransportState::Disabling { rpc };
    }

    pub fn poll(&mut self, cx: &mut Context<'_>) -> Poll<TransportStateResult> {
        match self {
            TransportState::Ready => Poll::Pending,
            TransportState::Enabling { rpc } => {
                let result = std::task::ready!(Pin::new(rpc).poll(cx));
                *self = TransportState::Ready;
                Poll::Ready(TransportStateResult::EnableComplete(
                    result.unwrap_or(false),
                ))
            }
            TransportState::Disabling { rpc } => {
                let _ = std::task::ready!(Pin::new(rpc).poll(cx));
                *self = TransportState::Ready;
                Poll::Ready(TransportStateResult::DisableComplete)
            }
        }
    }

    /// Wait for any in-flight enable or disable to complete, returning
    /// the result so the caller can apply the same side-effects as
    /// `poll_device`.
    pub async fn drain(&mut self) -> Option<TransportStateResult> {
        match std::mem::replace(self, TransportState::Ready) {
            TransportState::Enabling { rpc } => {
                let result = rpc.await.unwrap_or(false);
                Some(TransportStateResult::EnableComplete(result))
            }
            TransportState::Disabling { rpc } => {
                let _ = rpc.await;
                Some(TransportStateResult::DisableComplete)
            }
            TransportState::Ready => None,
        }
    }
}

/// Owns the virtio device and processes commands from the transport.
struct DeviceTask {
    device: Box<dyn DynVirtioDevice>,
    device_type: u16,
    max_queues: u16,
    pending_restore: DeviceRestoreState,
    queue_events: Vec<Option<pal_event::Event>>,
    pending_kicks: Vec<bool>,
    started_queues: Vec<bool>,
}

impl DeviceTask {
    fn stage_restore(&mut self, state: DeviceRestoreState, active: bool) {
        if state.is_restored() {
            tracing::debug!(
                target: "virtio_restore",
                event = "restore_staged",
                device_type = self.device_type,
                trigger = if active { "active-start" } else { "inactive-start" },
                queue_index = -1,
                restored_progress = false,
                success = true,
                "virtio restore lifecycle"
            );
            self.pending_restore = state;
        }
    }

    fn apply_pending_restore(
        &mut self,
        trigger: &'static str,
        queue_index: Option<u16>,
    ) -> Result<(), vmcore::save_restore::RestoreError> {
        let DeviceRestoreState::Restored(state) = &self.pending_restore else {
            return Ok(());
        };
        let result = self.device.restore_device(state.clone());
        tracing::debug!(
            target: "virtio_restore",
            event = "private_state_apply",
            device_type = self.device_type,
            trigger,
            queue_index = queue_index.map(i32::from).unwrap_or(-1),
            restored_progress = false,
            success = result.is_ok(),
            "virtio restore lifecycle"
        );
        if result.is_ok() {
            self.pending_restore = DeviceRestoreState::NotRestored;
        }
        result
    }

    async fn enable(&mut self, params: EnableParams) -> bool {
        if let Err(error) = self.apply_pending_restore("driver-ok", None) {
            tracelimit::error_ratelimited!(
                error = &error as &dyn std::error::Error,
                "virtio device restore failed during enable"
            );
            self.stop_all_queues().await;
            self.clear_queue_events();
            self.device.reset().await;
            self.pending_restore = DeviceRestoreState::NotRestored;
            return false;
        }
        let mut started_events = Vec::with_capacity(params.queues.len());
        for (idx, resources, initial_state) in params.queues {
            let event = resources.event.clone();
            let restored_progress = initial_state.is_some();
            let result = self
                .device
                .start_queue(idx, resources, &params.features, initial_state)
                .await;
            tracing::debug!(
                target: "virtio_restore",
                event = "queue_start",
                device_type = self.device_type,
                trigger = "driver-ok",
                queue_index = i32::from(idx),
                restored_progress,
                success = result.is_ok(),
                "virtio restore lifecycle"
            );
            if let Err(err) = result {
                tracelimit::error_ratelimited!(
                    error = &*err as &dyn std::error::Error,
                    idx,
                    "virtio device start_queue failed"
                );
                self.stop_all_queues().await;
                self.clear_queue_events();
                self.device.reset().await;
                self.pending_restore = DeviceRestoreState::NotRestored;
                return false;
            }
            self.started_queues[idx as usize] = true;
            started_events.push((idx, event));
        }
        for (idx, event) in started_events {
            self.dispatch_pending_kick(idx, &event);
        }
        true
    }

    async fn disable(&mut self) {
        self.stop_all_queues().await;
        self.clear_queue_events();
        self.device.reset().await;
        self.pending_restore = DeviceRestoreState::NotRestored;
    }

    async fn stop(&mut self) -> StopResult {
        let mut states = Vec::with_capacity(self.max_queues as usize);
        for idx in 0..self.max_queues {
            let was_started = std::mem::take(&mut self.started_queues[idx as usize]);
            states.push((was_started, self.device.stop_queue(idx).await));
        }
        StopResult {
            queues: states,
            device_state: match &self.pending_restore {
                DeviceRestoreState::NotRestored => self.device.save_device(),
                DeviceRestoreState::Restored(state) => Ok(state.clone()),
            },
        }
    }

    async fn start(&mut self, params: StartParams) -> anyhow::Result<()> {
        self.stage_restore(params.device_state, params.active);
        if !params.active {
            return Ok(());
        }
        self.apply_pending_restore("active-start", None)?;
        let mut started_events = Vec::with_capacity(params.queues.len());
        for (idx, resources, initial_state) in params.queues {
            let event = resources.event.clone();
            let restored_progress = initial_state.is_some();
            let result = self
                .device
                .start_queue(idx, resources, &params.features, initial_state)
                .await;
            tracing::debug!(
                target: "virtio_restore",
                event = "queue_start",
                device_type = self.device_type,
                trigger = "active-start",
                queue_index = i32::from(idx),
                restored_progress,
                success = result.is_ok(),
                "virtio restore lifecycle"
            );
            if let Err(error) = result {
                tracelimit::error_ratelimited!(
                    error = &*error as &dyn std::error::Error,
                    idx,
                    "virtio device start_queue failed on resume"
                );
                self.stop_all_queues().await;
                return Err(error);
            }
            self.started_queues[idx as usize] = true;
            started_events.push((idx, event));
        }
        for (idx, event) in started_events {
            self.dispatch_pending_kick(idx, &event);
        }
        Ok(())
    }

    async fn reset(&mut self) {
        self.stop_all_queues().await;
        self.clear_queue_events();
        self.device.reset().await;
        self.pending_restore = DeviceRestoreState::NotRestored;
    }

    async fn quiesce_input(&mut self) -> anyhow::Result<()> {
        self.device.quiesce_input().await
    }

    async fn resume_input(&mut self) -> anyhow::Result<()> {
        self.device.resume_input().await
    }

    async fn stop_all_queues(&mut self) {
        for idx in 0..self.max_queues {
            self.device.stop_queue(idx).await;
            self.started_queues[idx as usize] = false;
        }
    }

    fn clear_queue_events(&mut self) {
        for event in self.queue_events.iter().flatten() {
            while event.try_wait() {}
        }
        self.pending_kicks.fill(false);
    }

    fn dispatch_pending_kick(&mut self, idx: u16, event: &pal_event::Event) {
        if !std::mem::take(&mut self.pending_kicks[idx as usize]) {
            return;
        }
        event.signal();
        tracing::debug!(
            target: "virtio_restore",
            event = "kick_dispatch",
            device_type = self.device_type,
            trigger = "driver-ok",
            queue_index = i32::from(idx),
            restored_progress = false,
            success = true,
            "virtio restore lifecycle"
        );
    }
}

/// Runs the device task, processing commands from the transport.
pub async fn run_device_task(
    device: Box<dyn DynVirtioDevice>,
    mut recv: mesh::Receiver<DeviceCommand>,
) {
    let traits = device.traits();
    let max_queues = traits.max_queues;
    let mut task = DeviceTask {
        device_type: traits.device_id.0,
        max_queues,
        device,
        pending_restore: DeviceRestoreState::NotRestored,
        queue_events: vec![None; max_queues as usize],
        pending_kicks: vec![false; max_queues as usize],
        started_queues: vec![false; max_queues as usize],
    };

    while let Some(cmd) = recv.next().await {
        match cmd {
            DeviceCommand::Enable(rpc) => {
                rpc.handle(async |params| task.enable(params).await).await;
            }
            DeviceCommand::Disable(rpc) => {
                rpc.handle(async |()| task.disable().await).await;
            }
            DeviceCommand::Stop(rpc) => {
                rpc.handle(async |()| task.stop().await).await;
            }
            DeviceCommand::Start(rpc) => {
                // Start is used by ChangeDeviceState::start(), which is
                // sync and uses Rpc::detached() — errors are logged here
                // but not propagated to the transport.
                // TODO: update ChangeDeviceState to allow async start()
                // so failures can be handled by the transport.
                rpc.handle_failable(async |params| task.start(params).await)
                    .await;
            }
            DeviceCommand::Reset(rpc) => {
                rpc.handle(async |()| task.reset().await).await;
            }
            DeviceCommand::QuiesceInput(rpc) => {
                rpc.handle_failable(async |()| task.quiesce_input().await)
                    .await;
            }
            DeviceCommand::ResumeInput(rpc) => {
                rpc.handle_failable(async |()| task.resume_input().await)
                    .await;
            }
            DeviceCommand::ReadConfig {
                offset,
                len,
                completion,
                deferred,
            } => {
                if let Err(error) = task.apply_pending_restore("config-read", None) {
                    tracelimit::error_ratelimited!(
                        error = &error as &dyn std::error::Error,
                        "virtio device restore failed before config read"
                    );
                    deferred.complete_error(IoError::NoResponse);
                    continue;
                }
                let start_word = offset & !3;
                let end = offset as usize + len as usize;
                let mut buf = [0u8; 12];
                for word_off in (start_word as usize..end).step_by(4) {
                    let val = task.device.read_registers_u32(word_off as u16).await;
                    let i = word_off - start_word as usize;
                    buf[i..i + 4].copy_from_slice(&val.to_ne_bytes());
                }
                let byte_off = (offset - start_word) as usize;
                let data = &buf[byte_off..byte_off + len as usize];
                match completion {
                    ConfigReadCompletion::Exact => deferred.complete(data),
                    ConfigReadCompletion::LeftAlignedDword => {
                        let mut dword = [0u8; 4];
                        dword[..len as usize].copy_from_slice(data);
                        deferred.complete(&dword);
                    }
                }
            }
            DeviceCommand::WriteConfig {
                offset,
                len,
                data,
                deferred,
            } => {
                if let Err(error) = task.apply_pending_restore("config-write", None) {
                    tracelimit::error_ratelimited!(
                        error = &error as &dyn std::error::Error,
                        "virtio device restore failed before config write"
                    );
                    deferred.complete_error(IoError::NoResponse);
                    continue;
                }
                if len == 4 && offset & 3 == 0 {
                    task.device
                        .write_registers_u32(
                            offset,
                            u32::from_ne_bytes(data[..4].try_into().unwrap()),
                        )
                        .await;
                } else {
                    let start_word = offset & !3;
                    let end = offset as usize + len as usize;
                    let byte_off = (offset - start_word) as usize;
                    let mut buf = [0u8; 12];
                    for word_off in (start_word as usize..end).step_by(4) {
                        let val = task.device.read_registers_u32(word_off as u16).await;
                        let i = word_off - start_word as usize;
                        buf[i..i + 4].copy_from_slice(&val.to_ne_bytes());
                    }
                    buf[byte_off..byte_off + len as usize].copy_from_slice(&data[..len as usize]);
                    for word_off in (start_word as usize..end).step_by(4) {
                        let i = word_off - start_word as usize;
                        let val = u32::from_ne_bytes(buf[i..i + 4].try_into().unwrap());
                        task.device.write_registers_u32(word_off as u16, val).await;
                    }
                }
                deferred.complete();
            }
            DeviceCommand::Kick { idx, event } => {
                if let Some(slot) = task.queue_events.get_mut(idx as usize) {
                    *slot = Some(event.clone());
                }
                if let Err(error) = task.apply_pending_restore("kick", Some(idx)) {
                    tracelimit::error_ratelimited!(
                        error = &error as &dyn std::error::Error,
                        idx,
                        "virtio device restore failed before queue kick"
                    );
                    continue;
                }
                if task.started_queues.get(idx as usize) == Some(&true) {
                    event.signal();
                    tracing::debug!(
                        target: "virtio_restore",
                        event = "kick_dispatch",
                        device_type = task.device_type,
                        trigger = "kick",
                        queue_index = i32::from(idx),
                        restored_progress = false,
                        success = true,
                        "virtio restore lifecycle"
                    );
                } else if let Some(pending) = task.pending_kicks.get_mut(idx as usize) {
                    let newly_staged = !*pending;
                    *pending = true;
                    if newly_staged {
                        tracing::debug!(
                            target: "virtio_restore",
                            event = "kick_staged",
                            device_type = task.device_type,
                            trigger = "kick",
                            queue_index = i32::from(idx),
                            restored_progress = false,
                            success = true,
                            "virtio restore lifecycle"
                        );
                    }
                }
            }
            DeviceCommand::Inspect(deferred) => {
                deferred.inspect(&mut *task.device);
            }
        }
    }
}

/// Send a config read to the device task, returning a deferred IO token.
pub fn defer_config_read(
    sender: &mesh::Sender<DeviceCommand>,
    offset: u16,
    len: u8,
    completion: ConfigReadCompletion,
) -> IoResult {
    let (deferred, token) = defer_read();
    sender.send(DeviceCommand::ReadConfig {
        offset,
        len,
        completion,
        deferred,
    });
    IoResult::Defer(token)
}

/// Send a config write to the device task, returning a deferred IO token.
pub fn defer_config_write(
    sender: &mesh::Sender<DeviceCommand>,
    offset: u16,
    bytes: &[u8],
) -> IoResult {
    let (deferred, token) = defer_write();
    let mut data = [0u8; 8];
    data[..bytes.len()].copy_from_slice(bytes);
    sender.send(DeviceCommand::WriteConfig {
        offset,
        len: bytes.len() as u8,
        data,
        deferred,
    });
    IoResult::Defer(token)
}
