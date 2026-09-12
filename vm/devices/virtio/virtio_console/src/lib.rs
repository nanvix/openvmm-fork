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
use pal_async::timer::Instant;
use pal_async::timer::PolledTimer;
use serial_core::SerialIo;
use spec::VIRTIO_CONSOLE_F_SIZE;
use spec::VirtioConsoleConfig;
use std::collections::VecDeque;
use std::future::poll_fn;
use std::pin::Pin;
use std::pin::pin;
use std::task::Context;
use std::task::Poll;
use std::time::Duration;
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
use virtio_resources::console::VirtioControlConsoleBrokerConfig;
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
            mode: ConsoleWorkerMode::Direct {
                io,
                disconnect_policy,
            },
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

    /// Creates the control console with a reconnectable host endpoint and a
    /// VMM-resident session broker.
    pub fn new_broker(
        driver_source: &VmTaskDriverSource,
        host_io: Box<dyn SerialIo>,
        config: VirtioControlConsoleBrokerConfig,
    ) -> Self {
        let driver = driver_source.simple();
        let transport_state = if host_io.is_connected() {
            HostTransportState::Connected
        } else {
            HostTransportState::WaitingForDisconnect
        };
        let broker = control_session_broker::ControlSessionBroker::new(
            config.instance_id,
            config.capability,
        );
        let auth_timer = PolledTimer::new(&driver);
        let mut worker = TaskControl::new(ConsoleWorker {
            mode: ConsoleWorkerMode::Broker(Box::new(BrokerWorker {
                host_io,
                broker,
                config,
                transport_state,
                host_input: VecDeque::new(),
                auth_timer,
                auth_deadline: None,
            })),
        });
        worker.insert(
            &driver,
            "virtio-control-console",
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
        let (worker, mut state) = self.worker.get_mut();
        let state = state.as_mut().unwrap();
        state.partial_transmit = 0;
        state.staged_rx.clear();
        state.input_gated = false;
        state.mem = GuestMemory::empty();
        if let ConsoleWorkerMode::Broker(mode) = &mut worker.mode {
            mode.broker.reset_for_device();
            mode.host_input.clear();
            mode.auth_deadline = None;
            mode.transport_state = if mode.host_io.disconnect_current().is_ok() {
                HostTransportState::WaitingForConnect
            } else {
                HostTransportState::WaitingForDisconnect
            };
        }
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
        let (schema_version, staged_rx, disconnect_policy_id, broker) = match &worker.mode {
            ConsoleWorkerMode::Direct {
                disconnect_policy, ..
            } => (
                DIRECT_SAVED_STATE_VERSION,
                state.staged_rx.iter().copied().collect(),
                disconnect_policy_id(*disconnect_policy),
                None,
            ),
            ConsoleWorkerMode::Broker(mode) => {
                if !state.staged_rx.is_empty() {
                    return Err(SaveError::InvalidChildSavedState(anyhow::anyhow!(
                        "control-console broker has direct-mode staged RX"
                    )));
                }
                let broker = saved_state::SavedBrokerSnapshot::from(mode.broker.snapshot());
                (BROKER_SAVED_STATE_VERSION, Vec::new(), 0, Some(broker))
            }
        };
        Ok(Some(SavedStateBlob::new(saved_state::SavedState {
            schema_version,
            columns: self.config.cols.into(),
            rows: self.config.rows.into(),
            partial_transmit: state.partial_transmit as u64,
            staged_rx,
            disconnect_policy_id,
            broker,
        })))
    }

    fn restore_device(&mut self, state: Option<SavedStateBlob>) -> Result<(), RestoreError> {
        let (worker, runtime) = self.worker.get_mut();
        let mut saved = validate_saved_state(state.as_ref(), worker.mode.validation_mode())?;
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

        let restored_broker = match &mut worker.mode {
            ConsoleWorkerMode::Direct { .. } => None,
            ConsoleWorkerMode::Broker(mode) => {
                let saved_broker = saved
                    .broker
                    .take()
                    .ok_or_else(|| invalid_saved_state("missing control-console broker state"))?;
                let snapshot = saved_broker.try_into().map_err(invalid_saved_state)?;
                let broker = control_session_broker::ControlSessionBroker::restore(
                    snapshot,
                    mode.config.instance_id,
                    mode.config.capability,
                )
                .map_err(|error| invalid_saved_state(error.to_string()))?;
                if mode.host_io.is_connected() {
                    mode.host_io.disconnect_current().map_err(|error| {
                        RestoreError::Other(
                            anyhow::Error::new(error)
                                .context("failed to disconnect control-console host for restore"),
                        )
                    })?;
                }
                Some(broker)
            }
        };

        self.config = VirtioConsoleConfig {
            cols: columns,
            rows,
        };
        runtime.partial_transmit = partial_transmit;
        runtime.staged_rx = saved.staged_rx.into();
        if let (ConsoleWorkerMode::Broker(mode), Some(broker)) = (&mut worker.mode, restored_broker)
        {
            mode.broker = broker;
            mode.host_input.clear();
            mode.transport_state = HostTransportState::WaitingForConnect;
            mode.auth_deadline = None;
        }
        Ok(())
    }

    fn device_state_validator(&self) -> DeviceStateValidator {
        let mode = self.worker.get().0.mode.validation_mode();
        Box::new(move |state, features, queues, guest_memory| {
            let saved = validate_saved_state(state, mode)?;
            validate_saved_tx_offset(saved.partial_transmit, *features, queues, guest_memory)?;
            Ok(())
        })
    }
}

struct ConsoleWorker {
    mode: ConsoleWorkerMode,
}

enum ConsoleWorkerMode {
    Direct {
        io: Box<dyn SerialIo>,
        disconnect_policy: VirtioConsoleDisconnectPolicy,
    },
    Broker(Box<BrokerWorker>),
}

struct BrokerWorker {
    host_io: Box<dyn SerialIo>,
    broker: control_session_broker::ControlSessionBroker,
    config: VirtioControlConsoleBrokerConfig,
    transport_state: HostTransportState,
    host_input: VecDeque<u8>,
    auth_timer: PolledTimer,
    auth_deadline: Option<Instant>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HostTransportState {
    WaitingForDisconnect,
    WaitingForConnect,
    Connected,
    Disabled,
}

#[derive(Clone, Copy)]
enum SavedStateMode {
    Direct(VirtioConsoleDisconnectPolicy),
    Broker { current_instance_id: [u8; 16] },
}

impl ConsoleWorkerMode {
    fn validation_mode(&self) -> SavedStateMode {
        match self {
            Self::Direct {
                disconnect_policy, ..
            } => SavedStateMode::Direct(*disconnect_policy),
            Self::Broker(mode) => SavedStateMode::Broker {
                current_instance_id: mode.config.instance_id,
            },
        }
    }
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
        let mut response = req.respond();
        response.merge(state);
        match &mut self.mode {
            ConsoleWorkerMode::Direct { io, .. } => {
                response.field("mode", "direct").field_mut("io", io);
            }
            ConsoleWorkerMode::Broker(mode) => {
                let counters = mode.broker.counters();
                response
                    .field("mode", "broker")
                    .field("broker_state", format!("{:?}", mode.broker.state()))
                    .field("epoch", mode.broker.epoch())
                    .field("host_transport", format!("{:?}", mode.transport_state))
                    .field("host_authenticated", mode.broker.host_is_authenticated())
                    .field(
                        "guest_output_records",
                        mode.broker
                            .output_record_count(control_session_broker::OutputLegId::Guest),
                    )
                    .field(
                        "guest_output_bytes",
                        mode.broker
                            .output_byte_count(control_session_broker::OutputLegId::Guest),
                    )
                    .field(
                        "host_output_records",
                        mode.broker
                            .output_record_count(control_session_broker::OutputLegId::Host),
                    )
                    .field(
                        "host_output_bytes",
                        mode.broker
                            .output_byte_count(control_session_broker::OutputLegId::Host),
                    )
                    .field(
                        "guest_parser_bytes",
                        mode.broker.guest_parser_buffered_bytes(),
                    )
                    .field("host_input_bytes", mode.host_input.len())
                    .field("protocol_errors", counters.protocol_errors)
                    .field("authentication_errors", counters.authentication_errors)
                    .field("sequence_errors", counters.sequence_errors)
                    .field("ack_errors", counters.ack_errors)
                    .field("reset_errors", counters.reset_errors)
                    .field("reconnect_errors", counters.reconnect_errors)
                    .field("backpressure_errors", counters.backpressure_errors);
            }
        }
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
const MAX_SAVED_STATE_BYTES: usize = 512 * 1024;
const DIRECT_SAVED_STATE_VERSION: u32 = 1;
const BROKER_SAVED_STATE_VERSION: u32 = 2;
#[cfg(test)]
const SAVED_STATE_VERSION: u32 = DIRECT_SAVED_STATE_VERSION;

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
    mode: SavedStateMode,
) -> Result<saved_state::SavedState, RestoreError> {
    let state = state.ok_or_else(|| invalid_saved_state("missing console private state"))?;
    if state.encoded_len() > MAX_SAVED_STATE_BYTES {
        return Err(invalid_saved_state(
            "console private state exceeds its encoded size bound",
        ));
    }
    let saved: saved_state::SavedState = state.parse()?;
    match mode {
        SavedStateMode::Direct(disconnect_policy) => {
            if saved.schema_version != DIRECT_SAVED_STATE_VERSION {
                return Err(invalid_saved_state(format!(
                    "direct console requires schema version {DIRECT_SAVED_STATE_VERSION}, got {}",
                    saved.schema_version
                )));
            }
            if saved.broker.is_some() {
                return Err(invalid_saved_state(
                    "direct console saved state contains broker state",
                ));
            }
            if saved.disconnect_policy_id != disconnect_policy_id(disconnect_policy) {
                return Err(invalid_saved_state(
                    "console disconnect policy does not match the saved policy",
                ));
            }
        }
        SavedStateMode::Broker {
            current_instance_id,
        } => {
            if saved.schema_version != BROKER_SAVED_STATE_VERSION {
                return Err(invalid_saved_state(format!(
                    "control console requires schema version {BROKER_SAVED_STATE_VERSION}, got {}",
                    saved.schema_version
                )));
            }
            if !saved.staged_rx.is_empty() {
                return Err(invalid_saved_state(
                    "control-console saved state contains direct-mode staged RX",
                ));
            }
            let broker = saved
                .broker
                .as_ref()
                .ok_or_else(|| invalid_saved_state("missing control-console broker state"))?;
            let snapshot = validate_saved_broker(broker)?;
            control_session_broker::ControlSessionBroker::validate_restore(
                &snapshot,
                current_instance_id,
            )
            .map_err(|error| invalid_saved_state(error.to_string()))?;
        }
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
    use crate::control_session_broker::BrokerCounters;
    use crate::control_session_broker::BrokerSnapshot;
    use crate::control_session_broker::EncodedRecordSnapshot;
    use crate::control_session_broker::OutputSnapshot;
    use crate::control_session_protocol::ParserSnapshot;
    use crate::control_session_protocol::Record;
    use crate::control_session_protocol::RecordType;
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
        #[mesh(7)]
        pub broker: Option<SavedBrokerSnapshot>,
    }

    #[derive(Protobuf)]
    #[mesh(package = "virtio.console")]
    pub struct SavedBrokerSnapshot {
        #[mesh(1)]
        pub state: u32,
        #[mesh(2)]
        pub instance_id: Vec<u8>,
        #[mesh(3)]
        pub drain_foreign_instance_records: bool,
        #[mesh(4)]
        pub epoch: u64,
        #[mesh(5)]
        pub guest_parser: SavedParserSnapshot,
        #[mesh(6)]
        pub guest_output: SavedOutputSnapshot,
        #[mesh(7)]
        pub host_output: SavedOutputSnapshot,
        #[mesh(8)]
        pub guest_receive_sequence: u64,
        #[mesh(9)]
        pub guest_send_sequence: u64,
        #[mesh(10)]
        pub host_receive_sequence: u64,
        #[mesh(11)]
        pub host_send_sequence: u64,
        #[mesh(12)]
        pub pending_guest_record: Option<SavedRecord>,
        #[mesh(13)]
        pub pending_host_record: Option<SavedRecord>,
        #[mesh(14)]
        pub counters: SavedBrokerCounters,
    }

    #[derive(Protobuf)]
    #[mesh(package = "virtio.console")]
    pub struct SavedParserSnapshot {
        #[mesh(1)]
        pub header_bytes: Vec<u8>,
        #[mesh(2)]
        pub header_count: u32,
        #[mesh(3)]
        pub body_bytes: Vec<u8>,
        #[mesh(4)]
        pub declared_body_len: Option<u32>,
    }

    #[derive(Protobuf)]
    #[mesh(package = "virtio.console")]
    pub struct SavedEncodedRecord {
        #[mesh(1)]
        pub bytes: Vec<u8>,
        #[mesh(2)]
        pub offset: u64,
    }

    #[derive(Protobuf)]
    #[mesh(package = "virtio.console")]
    pub struct SavedOutputSnapshot {
        #[mesh(1)]
        pub current: Option<SavedEncodedRecord>,
        #[mesh(2)]
        pub queued_records: Vec<Vec<u8>>,
    }

    #[derive(Protobuf)]
    #[mesh(package = "virtio.console")]
    pub struct SavedRecord {
        #[mesh(1)]
        pub record_type: u32,
        #[mesh(2)]
        pub instance_id: Vec<u8>,
        #[mesh(3)]
        pub epoch: u64,
        #[mesh(4)]
        pub sequence: u64,
        #[mesh(5)]
        pub payload: Vec<u8>,
    }

    #[derive(Protobuf)]
    #[mesh(package = "virtio.console")]
    pub struct SavedBrokerCounters {
        #[mesh(1)]
        pub protocol_errors: u64,
        #[mesh(2)]
        pub authentication_errors: u64,
        #[mesh(3)]
        pub sequence_errors: u64,
        #[mesh(4)]
        pub ack_errors: u64,
        #[mesh(5)]
        pub reset_errors: u64,
        #[mesh(6)]
        pub reconnect_errors: u64,
        #[mesh(7)]
        pub backpressure_errors: u64,
    }

    impl From<BrokerSnapshot> for SavedBrokerSnapshot {
        fn from(snapshot: BrokerSnapshot) -> Self {
            Self {
                state: snapshot.state.into(),
                instance_id: snapshot.instance_id.to_vec(),
                drain_foreign_instance_records: snapshot.drain_foreign_instance_records,
                epoch: snapshot.epoch,
                guest_parser: snapshot.guest_parser.into(),
                guest_output: snapshot.guest_output.into(),
                host_output: snapshot.host_output.into(),
                guest_receive_sequence: snapshot.guest_receive_sequence,
                guest_send_sequence: snapshot.guest_send_sequence,
                host_receive_sequence: snapshot.host_receive_sequence,
                host_send_sequence: snapshot.host_send_sequence,
                pending_guest_record: snapshot.pending_guest_record.map(Into::into),
                pending_host_record: snapshot.pending_host_record.map(Into::into),
                counters: snapshot.counters.into(),
            }
        }
    }

    impl TryFrom<SavedBrokerSnapshot> for BrokerSnapshot {
        type Error = String;

        fn try_from(snapshot: SavedBrokerSnapshot) -> Result<Self, Self::Error> {
            let state = u8::try_from(snapshot.state)
                .map_err(|_| "broker state is out of range".to_string())?;
            let instance_id = fixed_bytes(snapshot.instance_id, "broker instance ID")?;
            let broker = Self {
                state,
                instance_id,
                drain_foreign_instance_records: snapshot.drain_foreign_instance_records,
                epoch: snapshot.epoch,
                guest_parser: snapshot.guest_parser.try_into()?,
                guest_output: snapshot.guest_output.try_into()?,
                host_output: snapshot.host_output.try_into()?,
                guest_receive_sequence: snapshot.guest_receive_sequence,
                guest_send_sequence: snapshot.guest_send_sequence,
                host_receive_sequence: snapshot.host_receive_sequence,
                host_send_sequence: snapshot.host_send_sequence,
                pending_guest_record: snapshot
                    .pending_guest_record
                    .map(TryInto::try_into)
                    .transpose()?,
                pending_host_record: snapshot
                    .pending_host_record
                    .map(TryInto::try_into)
                    .transpose()?,
                counters: snapshot.counters.into(),
            };
            crate::control_session_broker::ControlSessionBroker::validate_snapshot(&broker)
                .map_err(|error| error.to_string())?;
            Ok(broker)
        }
    }

    impl From<ParserSnapshot> for SavedParserSnapshot {
        fn from(snapshot: ParserSnapshot) -> Self {
            Self {
                header_bytes: snapshot.header_bytes,
                header_count: snapshot.header_count as u32,
                body_bytes: snapshot.body_bytes,
                declared_body_len: snapshot.declared_body_len,
            }
        }
    }

    impl TryFrom<SavedParserSnapshot> for ParserSnapshot {
        type Error = String;

        fn try_from(snapshot: SavedParserSnapshot) -> Result<Self, Self::Error> {
            Ok(Self {
                header_bytes: snapshot.header_bytes,
                header_count: snapshot.header_count as usize,
                body_bytes: snapshot.body_bytes,
                declared_body_len: snapshot.declared_body_len,
            })
        }
    }

    impl From<OutputSnapshot> for SavedOutputSnapshot {
        fn from(snapshot: OutputSnapshot) -> Self {
            Self {
                current: snapshot.current.map(Into::into),
                queued_records: snapshot.queued_records,
            }
        }
    }

    impl TryFrom<SavedOutputSnapshot> for OutputSnapshot {
        type Error = String;

        fn try_from(snapshot: SavedOutputSnapshot) -> Result<Self, Self::Error> {
            Ok(Self {
                current: snapshot.current.map(TryInto::try_into).transpose()?,
                queued_records: snapshot.queued_records,
            })
        }
    }

    impl From<EncodedRecordSnapshot> for SavedEncodedRecord {
        fn from(record: EncodedRecordSnapshot) -> Self {
            Self {
                bytes: record.bytes,
                offset: record.offset as u64,
            }
        }
    }

    impl TryFrom<SavedEncodedRecord> for EncodedRecordSnapshot {
        type Error = String;

        fn try_from(record: SavedEncodedRecord) -> Result<Self, Self::Error> {
            Ok(Self {
                bytes: record.bytes,
                offset: usize::try_from(record.offset)
                    .map_err(|_| "broker output offset is out of range".to_string())?,
            })
        }
    }

    impl From<Record> for SavedRecord {
        fn from(record: Record) -> Self {
            Self {
                record_type: record.record_type as u32,
                instance_id: record.instance_id.to_vec(),
                epoch: record.epoch,
                sequence: record.sequence,
                payload: record.payload,
            }
        }
    }

    impl TryFrom<SavedRecord> for Record {
        type Error = String;

        fn try_from(record: SavedRecord) -> Result<Self, Self::Error> {
            let record_type = u8::try_from(record.record_type)
                .map_err(|_| "broker record type is out of range".to_string())?;
            Ok(Self {
                record_type: RecordType::try_from(record_type)
                    .map_err(|error| error.to_string())?,
                instance_id: fixed_bytes(record.instance_id, "broker record instance ID")?,
                epoch: record.epoch,
                sequence: record.sequence,
                payload: record.payload,
            })
        }
    }

    impl From<BrokerCounters> for SavedBrokerCounters {
        fn from(counters: BrokerCounters) -> Self {
            Self {
                protocol_errors: counters.protocol_errors,
                authentication_errors: counters.authentication_errors,
                sequence_errors: counters.sequence_errors,
                ack_errors: counters.ack_errors,
                reset_errors: counters.reset_errors,
                reconnect_errors: counters.reconnect_errors,
                backpressure_errors: counters.backpressure_errors,
            }
        }
    }

    impl From<SavedBrokerCounters> for BrokerCounters {
        fn from(counters: SavedBrokerCounters) -> Self {
            Self {
                protocol_errors: counters.protocol_errors,
                authentication_errors: counters.authentication_errors,
                sequence_errors: counters.sequence_errors,
                ack_errors: counters.ack_errors,
                reset_errors: counters.reset_errors,
                reconnect_errors: counters.reconnect_errors,
                backpressure_errors: counters.backpressure_errors,
            }
        }
    }

    fn fixed_bytes<const N: usize>(bytes: Vec<u8>, name: &str) -> Result<[u8; N], String> {
        bytes
            .try_into()
            .map_err(|_| format!("{name} must be exactly {N} bytes"))
    }
}

fn validate_saved_broker(
    saved: &saved_state::SavedBrokerSnapshot,
) -> Result<control_session_broker::BrokerSnapshot, RestoreError> {
    use control_session_protocol::HEADER_LEN;
    use control_session_protocol::MAX_DATA_LEN;

    if !(1..=6).contains(&saved.state) {
        return Err(invalid_saved_state("invalid broker state"));
    }
    if saved.instance_id.len() != 16 || saved.instance_id.iter().all(|byte| *byte == 0) {
        return Err(invalid_saved_state("invalid broker instance ID"));
    }
    if saved.epoch == 0 {
        return Err(invalid_saved_state("saved broker epoch is zero"));
    }
    if saved.guest_parser.header_bytes.len() != HEADER_LEN
        || saved.guest_parser.header_count as usize > HEADER_LEN
        || saved.guest_parser.body_bytes.len() > MAX_DATA_LEN
        || saved
            .guest_parser
            .declared_body_len
            .is_some_and(|length| length as usize > MAX_DATA_LEN)
    {
        return Err(invalid_saved_state("invalid broker parser bounds"));
    }
    validate_saved_output(&saved.guest_output)?;
    validate_saved_output(&saved.host_output)?;
    if let Some(record) = &saved.pending_guest_record {
        validate_saved_record(record)?;
    }
    if let Some(record) = &saved.pending_host_record {
        validate_saved_record(record)?;
    }

    let snapshot = saved_state::SavedBrokerSnapshot {
        state: saved.state,
        instance_id: saved.instance_id.clone(),
        drain_foreign_instance_records: saved.drain_foreign_instance_records,
        epoch: saved.epoch,
        guest_parser: saved_state::SavedParserSnapshot {
            header_bytes: saved.guest_parser.header_bytes.clone(),
            header_count: saved.guest_parser.header_count,
            body_bytes: saved.guest_parser.body_bytes.clone(),
            declared_body_len: saved.guest_parser.declared_body_len,
        },
        guest_output: clone_saved_output(&saved.guest_output),
        host_output: clone_saved_output(&saved.host_output),
        guest_receive_sequence: saved.guest_receive_sequence,
        guest_send_sequence: saved.guest_send_sequence,
        host_receive_sequence: saved.host_receive_sequence,
        host_send_sequence: saved.host_send_sequence,
        pending_guest_record: saved.pending_guest_record.as_ref().map(clone_saved_record),
        pending_host_record: saved.pending_host_record.as_ref().map(clone_saved_record),
        counters: saved_state::SavedBrokerCounters {
            protocol_errors: saved.counters.protocol_errors,
            authentication_errors: saved.counters.authentication_errors,
            sequence_errors: saved.counters.sequence_errors,
            ack_errors: saved.counters.ack_errors,
            reset_errors: saved.counters.reset_errors,
            reconnect_errors: saved.counters.reconnect_errors,
            backpressure_errors: saved.counters.backpressure_errors,
        },
    };
    snapshot.try_into().map_err(invalid_saved_state)
}

fn validate_saved_output(saved: &saved_state::SavedOutputSnapshot) -> Result<(), RestoreError> {
    use control_session_broker::MAX_QUEUED_BYTES_PER_LEG;
    use control_session_broker::MAX_QUEUED_RECORDS_PER_LEG;
    use control_session_protocol::HEADER_LEN;
    use control_session_protocol::MAX_DATA_LEN;

    if saved.queued_records.len() > MAX_QUEUED_RECORDS_PER_LEG {
        return Err(invalid_saved_state("too many broker output records"));
    }
    let mut queued_bytes = 0usize;
    for bytes in &saved.queued_records {
        if !(HEADER_LEN..=HEADER_LEN + MAX_DATA_LEN).contains(&bytes.len()) {
            return Err(invalid_saved_state("invalid broker output record length"));
        }
        control_session_protocol::decode_exact(bytes)
            .map_err(|error| invalid_saved_state(error.to_string()))?;
        queued_bytes = queued_bytes
            .checked_add(bytes.len())
            .ok_or_else(|| invalid_saved_state("broker output byte count overflow"))?;
        if queued_bytes > MAX_QUEUED_BYTES_PER_LEG {
            return Err(invalid_saved_state(
                "broker output bytes exceed configured bound",
            ));
        }
    }
    if let Some(current) = &saved.current {
        if !(HEADER_LEN..=HEADER_LEN + MAX_DATA_LEN).contains(&current.bytes.len()) {
            return Err(invalid_saved_state(
                "invalid current broker output record length",
            ));
        }
        control_session_protocol::decode_exact(&current.bytes)
            .map_err(|error| invalid_saved_state(error.to_string()))?;
        let offset = usize::try_from(current.offset)
            .map_err(|_| invalid_saved_state("broker output offset is out of range"))?;
        if offset >= current.bytes.len() {
            return Err(invalid_saved_state(
                "broker output offset exceeds record length",
            ));
        }
    }
    Ok(())
}

fn validate_saved_record(saved: &saved_state::SavedRecord) -> Result<(), RestoreError> {
    let record_type = u8::try_from(saved.record_type)
        .ok()
        .and_then(|record_type| control_session_protocol::RecordType::try_from(record_type).ok())
        .ok_or_else(|| invalid_saved_state("invalid pending broker record type"))?;
    let payload_len = saved.payload.len();
    let payload_is_valid = match record_type {
        control_session_protocol::RecordType::GuestAttach
        | control_session_protocol::RecordType::Reset
        | control_session_protocol::RecordType::Ack
        | control_session_protocol::RecordType::Wait
        | control_session_protocol::RecordType::Ready => payload_len == 0,
        control_session_protocol::RecordType::HostAttach => payload_len == 32,
        control_session_protocol::RecordType::Data => {
            (1..=control_session_protocol::MAX_DATA_LEN).contains(&payload_len)
        }
        control_session_protocol::RecordType::Error => payload_len == 4,
    };
    if saved.instance_id.len() != 16 || !payload_is_valid {
        return Err(invalid_saved_state("invalid pending broker record"));
    }
    Ok(())
}

fn clone_saved_output(
    saved: &saved_state::SavedOutputSnapshot,
) -> saved_state::SavedOutputSnapshot {
    saved_state::SavedOutputSnapshot {
        current: saved
            .current
            .as_ref()
            .map(|record| saved_state::SavedEncodedRecord {
                bytes: record.bytes.clone(),
                offset: record.offset,
            }),
        queued_records: saved.queued_records.clone(),
    }
}

fn clone_saved_record(saved: &saved_state::SavedRecord) -> saved_state::SavedRecord {
    saved_state::SavedRecord {
        record_type: saved.record_type,
        instance_id: saved.instance_id.clone(),
        epoch: saved.epoch,
        sequence: saved.sequence,
        payload: saved.payload.clone(),
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
    #[error("control-session broker error")]
    Broker(#[source] control_session_broker::BrokerError),
}

impl ConsoleWorker {
    async fn run_loop(&mut self, state: &mut ConsoleWorkerState) -> Result<(), WorkerError> {
        match &mut self.mode {
            ConsoleWorkerMode::Direct {
                io,
                disconnect_policy,
            } => run_direct_loop(io, *disconnect_policy, state).await,
            ConsoleWorkerMode::Broker(mode) => mode.run_loop(state).await,
        }
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
                                            return Poll::Ready(Err(WorkerError::Virtio(error)));
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

impl BrokerWorker {
    async fn run_loop(&mut self, state: &mut ConsoleWorkerState) -> Result<(), WorkerError> {
        if state.receiveq.is_none() && state.transmitq.is_none() {
            std::future::pending::<()>().await;
        }
        match poll_fn(|cx| self.poll_once(state, cx)).await {
            Ok(()) => Ok(()),
            Err(error) => {
                tracelimit::error_ratelimited!(
                    error = &error as &dyn std::error::Error,
                    "control-console worker faulted"
                );
                if let Err(detach_error) = self.detach_host() {
                    tracelimit::error_ratelimited!(
                        error = &detach_error as &dyn std::error::Error,
                        "control-console host detach failed after worker fault"
                    );
                }
                // Keep TaskControl restartable. Device reset clears the broker
                // and queue state before the worker is started again.
                std::future::pending().await
            }
        }
    }

    fn poll_once(
        &mut self,
        state: &mut ConsoleWorkerState,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), WorkerError>> {
        let mut made_progress = false;

        match self.transport_state {
            HostTransportState::WaitingForDisconnect => match self.host_io.poll_disconnect(cx) {
                Poll::Ready(Ok(())) => {
                    self.detach_host()?;
                    self.transport_state = HostTransportState::WaitingForConnect;
                    made_progress = true;
                }
                Poll::Ready(Err(error)) => {
                    self.disable_host_transport(error)?;
                    made_progress = true;
                }
                Poll::Pending => {}
            },
            HostTransportState::WaitingForConnect => match self.host_io.poll_connect(cx) {
                Poll::Ready(Ok(())) => {
                    self.transport_state = HostTransportState::Connected;
                    self.begin_verified_host_attachment()?;
                    made_progress = true;
                }
                Poll::Ready(Err(error)) => {
                    self.disable_host_transport(error)?;
                    made_progress = true;
                }
                Poll::Pending => {}
            },
            HostTransportState::Connected => {
                if !self.broker.host_is_connected() {
                    self.begin_verified_host_attachment()?;
                    made_progress = true;
                }
                match self.host_io.poll_disconnect(cx) {
                    Poll::Ready(Ok(())) => {
                        self.detach_host()?;
                        made_progress = true;
                    }
                    Poll::Ready(Err(error)) => {
                        self.disable_host_transport(error)?;
                        made_progress = true;
                    }
                    Poll::Pending => {}
                }
            }
            HostTransportState::Disabled => {}
        }

        if self.transport_state == HostTransportState::Connected
            && self.broker.host_is_connected()
            && !self.broker.host_is_authenticated()
            && let Some(deadline) = self.auth_deadline
            && self.auth_timer.poll_until(cx, deadline).is_ready()
        {
            tracelimit::warn_ratelimited!("control-console host authentication timed out");
            self.detach_host()?;
            made_progress = true;
        }

        made_progress |= self.poll_guest_output(state, cx)?;
        made_progress |= self.poll_guest_input(state, cx)?;

        if self.transport_state == HostTransportState::Connected {
            made_progress |= self.poll_host_output(cx)?;
            if self.transport_state == HostTransportState::Connected && !state.input_gated {
                made_progress |= self.poll_host_input(cx)?;
            }
            if self.broker.host_is_authenticated() {
                self.auth_deadline = None;
            }
        }

        if made_progress {
            cx.waker().wake_by_ref();
        }
        Poll::Pending
    }

    fn poll_guest_output(
        &mut self,
        state: &mut ConsoleWorkerState,
        cx: &mut Context<'_>,
    ) -> Result<bool, WorkerError> {
        let Some(receiveq) = state.receiveq.as_mut() else {
            return Ok(false);
        };
        if !self
            .broker
            .begin_output(control_session_broker::OutputLegId::Guest)
        {
            return Ok(false);
        }

        let work = match receiveq.try_peek().map_err(WorkerError::Virtio)? {
            Some(work) => work,
            None => {
                return Ok(receiveq.poll_kick(cx).is_ready());
            }
        };
        let writeable_len = work
            .payload()
            .iter()
            .filter(|payload| payload.writeable)
            .map(|payload| payload.length as usize)
            .sum::<usize>();
        if writeable_len == 0 {
            let work = work.consume();
            receiveq.complete(work, 0);
            return Ok(true);
        }

        let mut bytes = [0; BUF_SIZE];
        let count = {
            let output = self
                .broker
                .peek_output(
                    control_session_broker::OutputLegId::Guest,
                    writeable_len.min(BUF_SIZE),
                )
                .ok_or(control_session_broker::BrokerError::InvalidOutputProgress)
                .map_err(WorkerError::Broker)?;
            bytes[..output.len()].copy_from_slice(output);
            output.len()
        };
        let work = work.consume();
        if let Err(error) = work.write(&state.mem, &bytes[..count]) {
            tracelimit::error_ratelimited!(
                error = &error as &dyn std::error::Error,
                "failed to write broker output to guest receive buffer"
            );
            receiveq.complete(work, 0);
        } else {
            self.broker
                .advance_output(control_session_broker::OutputLegId::Guest, count)
                .map_err(WorkerError::Broker)?;
            receiveq.complete(work, count as u32);
        }
        Ok(true)
    }

    fn poll_guest_input(
        &mut self,
        state: &mut ConsoleWorkerState,
        cx: &mut Context<'_>,
    ) -> Result<bool, WorkerError> {
        if self.broker.has_pending_guest_record() {
            let progress = self
                .broker
                .accept_guest_input(&[])
                .map_err(WorkerError::Broker)?;
            if progress.status != control_session_broker::InputStatus::Backpressured {
                return Ok(true);
            }
        }

        let Some(transmitq) = state.transmitq.as_mut() else {
            return Ok(false);
        };
        let work = match transmitq.try_peek().map_err(WorkerError::Virtio)? {
            Some(work) => work,
            None => {
                return Ok(transmitq.poll_kick(cx).is_ready());
            }
        };
        let readable_len = work.readable_length() as usize;
        if state.partial_transmit >= readable_len {
            state.partial_transmit = 0;
            let work = work.consume();
            transmitq.complete(work, 0);
            return Ok(true);
        }

        let mut bytes = [0; BUF_SIZE];
        let requested = (readable_len - state.partial_transmit).min(BUF_SIZE);
        let read = work
            .read_at_offset(
                state.partial_transmit as u64,
                &state.mem,
                &mut bytes[..requested],
            )
            .map_err(WorkerError::GuestMemory)?;
        if read == 0 {
            return Ok(false);
        }

        let mut offset = 0;
        while offset < read {
            let progress = self
                .broker
                .accept_guest_input(&bytes[offset..read])
                .map_err(WorkerError::Broker)?;
            offset += progress.consumed;
            state.partial_transmit += progress.consumed;
            if progress.status == control_session_broker::InputStatus::Backpressured
                || progress.consumed == 0
            {
                break;
            }
        }
        if state.partial_transmit == readable_len {
            state.partial_transmit = 0;
            let work = work.consume();
            transmitq.complete(work, 0);
        }
        Ok(offset != 0)
    }

    fn poll_host_output(&mut self, cx: &mut Context<'_>) -> Result<bool, WorkerError> {
        if !self
            .broker
            .begin_output(control_session_broker::OutputLegId::Host)
        {
            return Ok(false);
        }
        let mut bytes = [0; BUF_SIZE];
        let count = {
            let output = self
                .broker
                .peek_output(control_session_broker::OutputLegId::Host, BUF_SIZE)
                .ok_or(control_session_broker::BrokerError::InvalidOutputProgress)
                .map_err(WorkerError::Broker)?;
            bytes[..output.len()].copy_from_slice(output);
            output.len()
        };
        match Pin::new(&mut *self.host_io).poll_write(cx, &bytes[..count]) {
            Poll::Ready(Ok(0)) => {
                self.detach_host()?;
                Ok(true)
            }
            Poll::Ready(Ok(written)) => {
                self.broker
                    .advance_output(control_session_broker::OutputLegId::Host, written)
                    .map_err(WorkerError::Broker)?;
                Ok(true)
            }
            Poll::Ready(Err(_)) => {
                self.detach_host()?;
                Ok(true)
            }
            Poll::Pending => Ok(false),
        }
    }

    fn poll_host_input(&mut self, cx: &mut Context<'_>) -> Result<bool, WorkerError> {
        if self.broker.has_pending_host_record() || !self.host_input.is_empty() {
            let progress = if self.broker.has_pending_host_record() {
                self.broker.accept_host_input(&[])
            } else {
                let input = self.host_input.make_contiguous();
                self.broker.accept_host_input(input)
            };
            match progress {
                Ok(progress) => {
                    self.host_input.drain(..progress.consumed);
                    return Ok(progress.consumed != 0
                        || progress.status != control_session_broker::InputStatus::Backpressured);
                }
                Err(error) => {
                    tracelimit::warn_ratelimited!(
                        error = &error as &dyn std::error::Error,
                        "control-console host protocol input rejected"
                    );
                    self.detach_host()?;
                    return Ok(true);
                }
            }
        }

        let mut bytes = [0; BUF_SIZE];
        match Pin::new(&mut *self.host_io).poll_read(cx, &mut bytes) {
            Poll::Ready(Ok(0)) => {
                self.detach_host()?;
                Ok(true)
            }
            Poll::Ready(Ok(read)) => {
                self.host_input.extend(&bytes[..read]);
                Ok(true)
            }
            Poll::Ready(Err(_)) => {
                self.detach_host()?;
                Ok(true)
            }
            Poll::Pending => Ok(false),
        }
    }

    fn detach_host(&mut self) -> Result<(), WorkerError> {
        let broker_result = self.broker.host_disconnected().map_err(WorkerError::Broker);
        self.host_input.clear();
        self.auth_deadline = None;
        self.transport_state = if self.host_io.disconnect_current().is_ok() {
            HostTransportState::WaitingForConnect
        } else {
            HostTransportState::WaitingForDisconnect
        };
        broker_result
    }

    fn begin_verified_host_attachment(&mut self) -> Result<(), WorkerError> {
        let identity = self.host_io.local_peer_identity();
        if !matches!(
            identity,
            Ok(Some(ref identity)) if identity == &self.config.expected_peer_identity
        ) {
            tracelimit::warn_ratelimited!(
                "control-console host rejected because its local peer identity is unavailable or unexpected"
            );
            self.detach_host()?;
            return Ok(());
        }
        match self.broker.begin_host_attachment() {
            Ok(()) => {
                self.auth_deadline = Some(
                    Instant::now()
                        .saturating_add(Duration::from_millis(self.config.auth_timeout_ms)),
                );
            }
            Err(error) => {
                tracelimit::warn_ratelimited!(
                    error = &error as &dyn std::error::Error,
                    "control-console host attachment rejected"
                );
                self.auth_deadline = None;
                self.detach_host()?;
            }
        }
        Ok(())
    }

    fn disable_host_transport(&mut self, error: std::io::Error) -> Result<(), WorkerError> {
        tracelimit::error_ratelimited!(
            error = &error as &dyn std::error::Error,
            "control-console host transport disabled after a lifecycle error"
        );
        self.detach_host()?;
        self.transport_state = HostTransportState::Disabled;
        Ok(())
    }
}
