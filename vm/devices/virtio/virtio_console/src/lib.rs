// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Virtio console device — a single-port console backed by [`SerialIo`].
//!
//! This crate implements virtio device ID 3 (console) as defined in the
//! [virtio spec §5.3](https://docs.oasis-open.org/virtio/virtio/v1.2/virtio-v1.2.html).
//! It exposes `/dev/hvc0` inside the guest and bridges it to any
//! [`SerialIo`] backend (Unix socket, named pipe, in-memory buffer, etc.).
//!
//! # Queues
//!
//! The device uses two virtio queues:
//!
//! | Queue | Direction | Purpose |
//! |-------|-----------|---------|
//! | 0 — receiveq | host → guest | Data written by the backend appears here |
//! | 1 — transmitq | guest → host | Data written by the guest is forwarded to the backend |
//!
//! # Features
//!
//! * **`F_SIZE`** — advertised so the guest can query the console dimensions
//!   (columns × rows) from config space.
//! * **`F_MULTIPORT`** — *not* supported. This is a single-port implementation.
//!
//! # Disconnect / reconnect
//!
//! Disconnect handling is explicit: callers choose whether pending guest TX
//! descriptors are discarded or retained until [`SerialIo`] reconnects.

#![forbid(unsafe_code)]

#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the physical adapter is a follow-up")
)]
pub(crate) mod control_session_broker;
pub(crate) mod control_session_protocol;
pub mod resolver;
mod spec;
#[cfg(test)]
mod tests;

use futures::AsyncRead;
use futures::AsyncWrite;
use futures_concurrency::future::Race as _;
use guestmem::GuestMemory;
use inspect::InspectMut;
use serial_core::SerialIo;
use spec::VIRTIO_CONSOLE_F_SIZE;
use spec::VirtioConsoleConfig;
use std::collections::VecDeque;
use std::future::poll_fn;
use std::pin::Pin;
use std::pin::pin;
use task_control::AsyncRun;
use task_control::Cancelled;
use task_control::InspectTaskMut;
use task_control::TaskControl;
use virtio::DeviceQueueState;
use virtio::DeviceStateValidator;
use virtio::DeviceTraits;
use virtio::DeviceTraitsSharedMemory;
use virtio::QueueResources;
use virtio::VirtioDevice;
use virtio::VirtioQueue;
use virtio::queue::QueueState;
use virtio::queue::restored_queue_front_readable_length;
use virtio::spec::VirtioDeviceFeatures;
use virtio_resources::console::VirtioConsoleDisconnectPolicy;
use vmcore::save_restore::RestoreError;
use vmcore::save_restore::SaveError;
use vmcore::save_restore::SavedStateBlob;
use vmcore::vm_task::VmTaskDriver;
use vmcore::vm_task::VmTaskDriverSource;

/// A virtio console device backed by a [`SerialIo`] backend.
#[derive(InspectMut)]
pub struct VirtioConsoleDevice {
    driver: VmTaskDriver,
    config: VirtioConsoleConfig,
    #[inspect(mut)]
    worker: TaskControl<ConsoleWorker, ConsoleWorkerState>,
}

impl VirtioConsoleDevice {
    /// Create a new virtio console device backed by the given serial I/O.
    pub fn new(driver_source: &VmTaskDriverSource, io: Box<dyn SerialIo>) -> Self {
        Self::new_with_policy(driver_source, io, VirtioConsoleDisconnectPolicy::Discard)
    }

    /// Create a console with explicit behavior while its backend is disconnected.
    pub fn new_with_policy(
        driver_source: &VmTaskDriverSource,
        io: Box<dyn SerialIo>,
        disconnect_policy: VirtioConsoleDisconnectPolicy,
    ) -> Self {
        Self::new_with_name_and_policy(driver_source, io, "virtio-console", disconnect_policy)
    }

    /// Create a named console with explicit disconnected-backend behavior.
    pub fn new_with_name_and_policy(
        driver_source: &VmTaskDriverSource,
        io: Box<dyn SerialIo>,
        worker_name: &'static str,
        disconnect_policy: VirtioConsoleDisconnectPolicy,
    ) -> Self {
        let driver = driver_source.simple();
        let mut worker = TaskControl::new(ConsoleWorker {
            io,
            disconnect_policy,
        });
        worker.insert(
            &driver,
            worker_name,
            ConsoleWorkerState {
                receiveq: None,
                transmitq: None,
                mem: GuestMemory::empty(),
                partial_transmit: 0,
                staged_rx: VecDeque::new(),
                input_gated: false,
            },
        );
        Self {
            driver,
            config: VirtioConsoleConfig::default(),
            worker,
        }
    }
}

impl VirtioDevice for VirtioConsoleDevice {
    fn traits(&self) -> DeviceTraits {
        let features = VirtioDeviceFeatures::new()
            .with_device_specific_low(1 << VIRTIO_CONSOLE_F_SIZE)
            .with_ring_event_idx(true)
            .with_ring_indirect_desc(true)
            .with_ring_packed(true);
        DeviceTraits {
            device_id: virtio::spec::VirtioDeviceType::CONSOLE,
            device_features: features,
            max_queues: 2, // receiveq (0) + transmitq (1)
            device_register_length: size_of::<VirtioConsoleConfig>() as u32,
            shared_memory: DeviceTraitsSharedMemory::default(),
        }
    }

    async fn read_registers_u32(&mut self, offset: u16) -> u32 {
        self.config.read_u32(offset)
    }

    async fn write_registers_u32(&mut self, _offset: u16, _val: u32) {
        // Console config is read-only from the guest perspective.
    }

    async fn start_queue(
        &mut self,
        idx: u16,
        resources: QueueResources,
        features: &VirtioDeviceFeatures,
        initial_state: Option<QueueState>,
    ) -> anyhow::Result<()> {
        let guest_memory = resources.guest_memory.clone();
        let mut queue = VirtioQueue::new(
            *features,
            resources.params,
            resources.guest_memory,
            resources.notify,
            pal_async::wait::PolledWait::new(&self.driver, resources.event)?,
            initial_state,
        )?;

        anyhow::ensure!(idx < 2, "invalid virtio-console queue index {idx}");

        self.worker.stop().await;
        let state = self.worker.state_mut().unwrap();
        if idx == 1 && state.partial_transmit != 0 {
            let work = queue
                .try_peek()
                .map_err(|error| anyhow::anyhow!(error).context("invalid restored TX queue"))?
                .ok_or_else(|| anyhow::anyhow!("restored TX offset has no current descriptor"))?;
            anyhow::ensure!(
                state.partial_transmit <= work.readable_length() as usize,
                "restored TX offset {} exceeds descriptor length {}",
                state.partial_transmit,
                work.readable_length()
            );
        }
        state.mem = guest_memory;
        if idx == 0 {
            state.receiveq = Some(queue);
        } else {
            state.transmitq = Some(queue);
        }
        self.worker.start();
        Ok(())
    }

    async fn stop_queue(&mut self, idx: u16) -> Option<QueueState> {
        // Stop the worker (shared by both queues). Once stopped, we can
        // reach into the state to take the requested queue.
        self.worker.stop().await;

        let state = self.worker.state_mut().unwrap();
        let queue = match idx {
            0 => state.receiveq.take(),
            1 => state.transmitq.take(),
            _ => return None,
        };

        // Keep the stopped worker state when both queues are gone so private
        // TX/RX progress remains available to snapshot capture.
        if state.receiveq.is_some() || state.transmitq.is_some() {
            self.worker.start();
        }

        queue.map(|q| q.queue_state())
    }

    async fn reset(&mut self) {
        self.config = VirtioConsoleConfig::default();
        let state = self.worker.state_mut().unwrap();
        state.partial_transmit = 0;
        state.staged_rx.clear();
        state.input_gated = false;
        state.mem = GuestMemory::empty();
    }

    async fn quiesce_input(&mut self) -> anyhow::Result<()> {
        self.worker.stop().await;
        let state = self.worker.state_mut().unwrap();
        state.input_gated = true;
        if state.receiveq.is_some() || state.transmitq.is_some() {
            self.worker.start();
        }
        Ok(())
    }

    async fn resume_input(&mut self) -> anyhow::Result<()> {
        self.worker.stop().await;
        let state = self.worker.state_mut().unwrap();
        state.input_gated = false;
        if state.receiveq.is_some() || state.transmitq.is_some() {
            self.worker.start();
        }
        Ok(())
    }

    fn supports_save_restore(&self) -> bool {
        true
    }

    fn save_device(&mut self) -> Result<Option<SavedStateBlob>, SaveError> {
        let (worker, state) = self.worker.get();
        let state = state.as_ref().unwrap();
        if state.receiveq.is_some() || state.transmitq.is_some() {
            return Err(SaveError::Other(anyhow::anyhow!(
                "virtio-console queues are still running"
            )));
        }
        if state.staged_rx.len() > MAX_STAGED_RX_BYTES {
            return Err(SaveError::InvalidChildSavedState(anyhow::anyhow!(
                "virtio-console staged RX exceeds its ABI bound"
            )));
        }
        Ok(Some(SavedStateBlob::new(saved_state::SavedState {
            schema_version: SAVED_STATE_VERSION,
            columns: self.config.cols.into(),
            rows: self.config.rows.into(),
            partial_transmit: state.partial_transmit as u64,
            staged_rx: state.staged_rx.iter().copied().collect(),
            disconnect_policy_id: disconnect_policy_id(worker.disconnect_policy),
        })))
    }

    fn restore_device(&mut self, state: Option<SavedStateBlob>) -> Result<(), RestoreError> {
        let (worker, runtime) = self.worker.get_mut();
        let saved = validate_saved_state(state.as_ref(), worker.disconnect_policy)?;
        let runtime = runtime.ok_or_else(|| {
            RestoreError::Other(anyhow::anyhow!(
                "virtio-console worker state is unavailable"
            ))
        })?;
        if runtime.receiveq.is_some() || runtime.transmitq.is_some() {
            return Err(RestoreError::Other(anyhow::anyhow!(
                "cannot restore a running virtio-console"
            )));
        }
        let columns = u16::try_from(saved.columns)
            .map_err(|_| invalid_saved_state("console column count is out of range"))?;
        let rows = u16::try_from(saved.rows)
            .map_err(|_| invalid_saved_state("console row count is out of range"))?;
        let partial_transmit = usize::try_from(saved.partial_transmit)
            .map_err(|_| invalid_saved_state("console TX offset is out of range"))?;

        self.config = VirtioConsoleConfig {
            cols: columns,
            rows,
        };
        runtime.partial_transmit = partial_transmit;
        runtime.staged_rx = saved.staged_rx.into();
        Ok(())
    }

    fn device_state_validator(&self) -> DeviceStateValidator {
        let disconnect_policy = self.worker.get().0.disconnect_policy;
        Box::new(move |state, features, queues, guest_memory| {
            let saved = validate_saved_state(state, disconnect_policy)?;
            validate_saved_tx_offset(saved.partial_transmit, *features, queues, guest_memory)
        })
    }
}

#[derive(InspectMut)]
struct ConsoleWorker {
    #[inspect(mut)]
    io: Box<dyn SerialIo>,
    #[inspect(skip)]
    disconnect_policy: VirtioConsoleDisconnectPolicy,
}

#[derive(InspectMut)]
struct ConsoleWorkerState {
    receiveq: Option<VirtioQueue>,
    transmitq: Option<VirtioQueue>,
    mem: GuestMemory,
    /// Bytes already written for the current transmitq descriptor.
    /// Must survive cancel/restart to avoid re-sending data.
    partial_transmit: usize,
    #[inspect(with = "VecDeque::len")]
    staged_rx: VecDeque<u8>,
    input_gated: bool,
}

impl InspectTaskMut<ConsoleWorkerState> for ConsoleWorker {
    fn inspect_mut(&mut self, req: inspect::Request<'_>, state: Option<&mut ConsoleWorkerState>) {
        req.respond().merge(self).merge(state);
    }
}

impl AsyncRun<ConsoleWorkerState> for ConsoleWorker {
    async fn run(
        &mut self,
        stop: &mut task_control::StopTask<'_>,
        state: &mut ConsoleWorkerState,
    ) -> Result<(), Cancelled> {
        stop.until_stopped(self.run_loop(state)).await.map(|r| {
            if let Err(err) = r {
                tracelimit::error_ratelimited!(
                    error = &err as &dyn std::error::Error,
                    "virtio-console worker loop failed"
                );
            }
        })
    }
}

/// Maximum buffer size for a single read/write operation and for accepted,
/// not-yet-delivered host input.
const BUF_SIZE: usize = 4096;
const MAX_STAGED_RX_BYTES: usize = BUF_SIZE;
const SAVED_STATE_VERSION: u32 = 1;

fn disconnect_policy_id(policy: VirtioConsoleDisconnectPolicy) -> u32 {
    match policy {
        VirtioConsoleDisconnectPolicy::Discard => 0,
        VirtioConsoleDisconnectPolicy::Retain => 1,
    }
}

fn invalid_saved_state(message: impl Into<String>) -> RestoreError {
    RestoreError::InvalidSavedState(anyhow::anyhow!(message.into()))
}

fn validate_saved_state(
    state: Option<&SavedStateBlob>,
    disconnect_policy: VirtioConsoleDisconnectPolicy,
) -> Result<saved_state::SavedState, RestoreError> {
    let state = state.ok_or_else(|| invalid_saved_state("missing console private state"))?;
    let saved: saved_state::SavedState = state.parse()?;
    if saved.schema_version != SAVED_STATE_VERSION {
        return Err(invalid_saved_state(format!(
            "unsupported console schema version {}",
            saved.schema_version
        )));
    }
    u16::try_from(saved.columns)
        .map_err(|_| invalid_saved_state("console column count is out of range"))?;
    u16::try_from(saved.rows)
        .map_err(|_| invalid_saved_state("console row count is out of range"))?;
    usize::try_from(saved.partial_transmit)
        .map_err(|_| invalid_saved_state("console TX offset is out of range"))?;
    if saved.staged_rx.len() > MAX_STAGED_RX_BYTES {
        return Err(invalid_saved_state(
            "console staged RX exceeds its ABI bound",
        ));
    }
    if saved.disconnect_policy_id != disconnect_policy_id(disconnect_policy) {
        return Err(invalid_saved_state(
            "console disconnect policy does not match the saved policy",
        ));
    }
    Ok(saved)
}

fn validate_saved_tx_offset(
    partial_transmit: u64,
    features: VirtioDeviceFeatures,
    queues: &[DeviceQueueState],
    guest_memory: &GuestMemory,
) -> Result<(), RestoreError> {
    if partial_transmit == 0 {
        return Ok(());
    }
    let transmitq = queues
        .get(1)
        .ok_or_else(|| invalid_saved_state("console saved state has no transmit queue"))?;
    if !transmitq.params.enable {
        return Err(invalid_saved_state(
            "console TX offset requires an enabled transmit queue",
        ));
    }
    let readable_length = restored_queue_front_readable_length(
        features,
        transmitq.params,
        guest_memory.clone(),
        transmitq.queue_state,
    )
    .map_err(|error| {
        RestoreError::InvalidSavedState(
            anyhow::Error::new(error).context("console transmit queue is invalid"),
        )
    })?
    .ok_or_else(|| invalid_saved_state("console TX offset has no current descriptor"))?;
    if partial_transmit > readable_length {
        return Err(invalid_saved_state(format!(
            "console TX offset {partial_transmit} exceeds descriptor length {readable_length}"
        )));
    }
    Ok(())
}

mod saved_state {
    use mesh::payload::Protobuf;
    use vmcore::save_restore::SavedStateRoot;

    #[derive(Protobuf, SavedStateRoot)]
    #[mesh(package = "virtio.console")]
    pub struct SavedState {
        #[mesh(1)]
        pub schema_version: u32,
        #[mesh(2)]
        pub columns: u32,
        #[mesh(3)]
        pub rows: u32,
        #[mesh(4)]
        pub partial_transmit: u64,
        #[mesh(5)]
        pub staged_rx: Vec<u8>,
        #[mesh(6)]
        pub disconnect_policy_id: u32,
    }
}

#[derive(Debug, thiserror::Error)]
enum WorkerError {
    #[error("virtio queue error")]
    Virtio(#[source] std::io::Error),
    #[error("serial I/O error")]
    Serial(#[source] std::io::Error),
    #[error("guest memory error")]
    GuestMemory(#[source] guestmem::GuestMemoryError),
}

impl ConsoleWorker {
    async fn run_loop(&mut self, state: &mut ConsoleWorkerState) -> Result<(), WorkerError> {
        run_direct_loop(&mut self.io, self.disconnect_policy, state).await
    }
}

async fn run_direct_loop(
    serial_io: &mut Box<dyn SerialIo>,
    disconnect_policy: VirtioConsoleDisconnectPolicy,
    state: &mut ConsoleWorkerState,
) -> Result<(), WorkerError> {
    // This loop must be cancel safe because TaskControl may stop it at any
    // await point.
    let mut connected: bool = serial_io.is_connected();
    let receiveq = &mut state.receiveq;
    let transmitq = &mut state.transmitq;
    let mut io = parking_lot::Mutex::new(serial_io);
    let mem = &state.mem;
    let partial_transmit = &mut state.partial_transmit;
    let staged_rx = &mut state.staged_rx;
    let input_gated = state.input_gated;

    // If neither queue is present, there's nothing to do.
    if receiveq.is_none() && transmitq.is_none() {
        std::future::pending::<()>().await;
    }
    loop {
        if !connected {
            poll_fn(|cx| io.get_mut().poll_disconnect(cx))
                .await
                .map_err(WorkerError::Serial)?;
            // Wait for the backend to connect, discarding any guest tx data
            // in the meantime.
            let wait_connect = async {
                poll_fn(|cx| io.get_mut().poll_connect(cx))
                    .await
                    .map_err(WorkerError::Serial)?;
                Ok::<_, WorkerError>(true)
            };
            let drain_tx = async {
                if disconnect_policy == VirtioConsoleDisconnectPolicy::Retain {
                    return std::future::pending().await;
                }
                let Some(transmitq) = transmitq.as_mut() else {
                    std::future::pending().await
                };
                loop {
                    let work = transmitq.peek().await.map_err(WorkerError::Virtio)?;
                    let work = work.consume();
                    transmitq.complete(work, 0);
                    *partial_transmit = 0;
                }
            };
            // Give wait_connect priority so that drain_tx cannot
            // consume a descriptor on the same poll cycle where
            // the backend becomes connected.
            connected = match futures::future::select(pin!(wait_connect), pin!(drain_tx)).await {
                futures::future::Either::Left((result, _))
                | futures::future::Either::Right((result, _)) => result?,
            };
        } else {
            let rx = async {
                if input_gated {
                    return std::future::pending().await;
                }
                'rx: loop {
                    if staged_rx.is_empty() {
                        let mut buf = [0u8; BUF_SIZE];
                        let read = poll_fn(|cx| {
                            if let Some(receiveq) = receiveq.as_mut() {
                                loop {
                                    match receiveq.try_peek() {
                                        Ok(Some(work)) => {
                                            let writeable_len = work
                                                .payload()
                                                .iter()
                                                .filter(|payload| payload.writeable)
                                                .map(|payload| payload.length as usize)
                                                .sum::<usize>();
                                            if writeable_len != 0 {
                                                break;
                                            }
                                            let work = work.consume();
                                            receiveq.complete(work, 0);
                                        }
                                        Ok(None) => {
                                            let _ = receiveq.poll_kick(cx);
                                            break;
                                        }
                                        Err(error) => {
                                            return std::task::Poll::Ready(Err(
                                                WorkerError::Virtio(error),
                                            ));
                                        }
                                    }
                                }
                            }
                            Pin::new(&mut **io.lock())
                                .poll_read(cx, &mut buf)
                                .map(|result| result.map_err(WorkerError::Serial))
                        })
                        .await;
                        let read = match read {
                            Ok(read) => read,
                            Err(WorkerError::Serial(_)) => break 'rx Ok(false),
                            Err(error) => return Err(error),
                        };
                        if read == 0 {
                            break 'rx Ok(false);
                        }
                        staged_rx.extend(&buf[..read]);
                    }

                    let Some(receiveq) = receiveq.as_mut() else {
                        std::future::pending().await
                    };
                    let work = receiveq.peek().await.map_err(WorkerError::Virtio)?;
                    let writeable_len = work
                        .payload()
                        .iter()
                        .filter(|p| p.writeable)
                        .map(|p| p.length as usize)
                        .sum::<usize>();
                    if writeable_len == 0 {
                        // Guest posted a zero-length buffer; complete it
                        // immediately without calling poll_read (which
                        // would return Ok(0) and look like a disconnect).
                        let work = work.consume();
                        receiveq.complete(work, 0);
                        continue 'rx;
                    }
                    let n = staged_rx.len().min(writeable_len);
                    let work = work.consume();
                    if let Err(err) = work.write(mem, &staged_rx.make_contiguous()[..n]) {
                        tracelimit::error_ratelimited!(
                            error = &err as &dyn std::error::Error,
                            "failed to write to guest receive buffer"
                        );
                        receiveq.complete(work, 0);
                    } else {
                        staged_rx.drain(..n);
                        receiveq.complete(work, n as u32);
                    }
                }
            };
            let tx = async {
                let Some(transmitq) = transmitq.as_mut() else {
                    std::future::pending().await
                };
                'tx: loop {
                    let work = transmitq.peek().await.map_err(WorkerError::Virtio)?;
                    let readable_len = work.readable_length() as usize;
                    let mut buf = [0u8; BUF_SIZE];
                    while *partial_transmit < readable_len {
                        let n = work
                            .read_at_offset(*partial_transmit as u64, mem, &mut buf)
                            .map_err(WorkerError::GuestMemory)?;
                        let mut written_this_chunk = 0;
                        while written_this_chunk < n {
                            match poll_fn(|cx| {
                                Pin::new(&mut **io.lock())
                                    .poll_write(cx, &buf[written_this_chunk..n])
                            })
                            .await
                            {
                                Ok(0) => {
                                    break 'tx Ok(false);
                                }
                                Ok(written) => {
                                    written_this_chunk += written;
                                    *partial_transmit += written;
                                }
                                Err(_) => {
                                    // Backend disconnected. Leave
                                    // partial_transmit as-is so we can
                                    // resume if the backend reconnects
                                    // before the descriptor is drained.
                                    break 'tx Ok(false);
                                }
                            }
                        }
                    }
                    *partial_transmit = 0;
                    let work = work.consume();
                    transmitq.complete(work, 0);
                }
            };

            // Run rx and tx concurrently; if either signals disconnect, loop
            // back to the disconnected state.
            connected = (rx, tx).race().await?;
        }
    }
}
