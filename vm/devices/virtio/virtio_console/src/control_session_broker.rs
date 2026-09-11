// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Pure reconnect broker for the control console.

use crate::control_session_protocol;
use crate::control_session_protocol::Parser;
use crate::control_session_protocol::ProtocolError;
use crate::control_session_protocol::Record;
use crate::control_session_protocol::RecordType;
use std::collections::VecDeque;
use thiserror::Error;

pub const MAX_QUEUED_RECORDS_PER_LEG: usize = 16;
pub const MAX_QUEUED_BYTES_PER_LEG: usize = 1024 * 1024;

/// The broker's externally meaningful state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum BrokerState {
    AwaitGuestAttach = 1,
    AwaitGuestAck = 2,
    ReadyNoHost = 3,
    Active = 4,
    ResetPending = 5,
    Failed = 6,
}

/// An output leg driven by a later physical-I/O adapter.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutputLegId {
    Guest,
    Host,
}

/// Result of accepting input into one broker leg.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InputProgress {
    pub consumed: usize,
    pub status: InputStatus,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InputStatus {
    NeedMore,
    RecordAccepted,
    Backpressured,
}

/// Error counters safe to expose through inspection.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct BrokerCounters {
    pub protocol_errors: u64,
    pub authentication_errors: u64,
    pub sequence_errors: u64,
    pub ack_errors: u64,
    pub reset_errors: u64,
    pub reconnect_errors: u64,
    pub backpressure_errors: u64,
}

/// Pure broker failures.
#[derive(Debug, Error, Eq, PartialEq)]
pub enum BrokerError {
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
    #[error("host capability authentication failed")]
    Authentication,
    #[error("a host attachment already occupies the broker slot")]
    HostSlotBusy,
    #[error("record {record_type:?} is illegal in state {state:?}")]
    IllegalRecord {
        state: BrokerState,
        record_type: RecordType,
    },
    #[error("record instance does not match the broker")]
    InstanceMismatch,
    #[error("record epoch {actual} does not match expected epoch {expected}")]
    EpochMismatch { expected: u64, actual: u64 },
    #[error("record sequence {actual} does not match expected sequence {expected}")]
    SequenceMismatch { expected: u64, actual: u64 },
    #[error("guest acknowledged a session before RESET was emitted")]
    AckBeforeReset,
    #[error("output leg {0:?} is backpressured")]
    Backpressure(OutputLegId),
    #[error("control-session epoch overflow")]
    EpochOverflow,
    #[error("control-session sequence overflow")]
    SequenceOverflow,
    #[error("invalid output progress")]
    InvalidOutputProgress,
    #[error("invalid broker snapshot: {0}")]
    InvalidSnapshot(&'static str),
    #[error("broker is in a failed state")]
    Failed,
}

#[derive(Clone, Debug)]
struct EncodedRecord {
    bytes: Vec<u8>,
    offset: usize,
}

#[derive(Clone, Debug, Default)]
struct OutputLeg {
    current: Option<EncodedRecord>,
    queue: VecDeque<Vec<u8>>,
    queued_bytes: usize,
}

impl OutputLeg {
    fn can_enqueue(&self, encoded_len: usize) -> bool {
        self.queue.len() < MAX_QUEUED_RECORDS_PER_LEG
            && self
                .queued_bytes
                .checked_add(encoded_len)
                .is_some_and(|total| total <= MAX_QUEUED_BYTES_PER_LEG)
    }

    fn enqueue(&mut self, bytes: Vec<u8>) -> Result<(), BrokerError> {
        if !self.can_enqueue(bytes.len()) {
            return Err(BrokerError::InvalidSnapshot(
                "output queue exceeded configured bound",
            ));
        }
        self.queued_bytes += bytes.len();
        self.queue.push_back(bytes);
        Ok(())
    }

    fn begin(&mut self) -> bool {
        if self.current.is_some() {
            return true;
        }
        let Some(bytes) = self.queue.pop_front() else {
            return false;
        };
        self.queued_bytes -= bytes.len();
        self.current = Some(EncodedRecord { bytes, offset: 0 });
        true
    }

    fn peek(&self, max_bytes: usize) -> Option<&[u8]> {
        let current = self.current.as_ref()?;
        let end = current
            .offset
            .saturating_add(max_bytes)
            .min(current.bytes.len());
        Some(&current.bytes[current.offset..end])
    }

    fn advance(&mut self, count: usize) -> Result<(), BrokerError> {
        let current = self
            .current
            .as_mut()
            .ok_or(BrokerError::InvalidOutputProgress)?;
        let remaining = current.bytes.len() - current.offset;
        if count > remaining {
            return Err(BrokerError::InvalidOutputProgress);
        }
        current.offset += count;
        if current.offset == current.bytes.len() {
            self.current = None;
        }
        Ok(())
    }

    fn discard_unstarted(&mut self) {
        self.queue.clear();
        self.queued_bytes = 0;
        if self
            .current
            .as_ref()
            .is_some_and(|current| current.offset == 0)
        {
            self.current = None;
        }
    }

    fn clear(&mut self) {
        self.current = None;
        self.queue.clear();
        self.queued_bytes = 0;
    }
}

/// Pure protocol and reconnect state machine.
pub struct ControlSessionBroker {
    instance_id: [u8; 16],
    capability: [u8; 32],
    epoch: u64,
    state: BrokerState,
    guest_parser: Parser,
    host_parser: Parser,
    guest_output: OutputLeg,
    host_output: OutputLeg,
    guest_receive_sequence: u64,
    guest_send_sequence: u64,
    host_receive_sequence: u64,
    host_send_sequence: u64,
    host_connected: bool,
    host_authenticated: bool,
    host_rejected: bool,
    host_bound_epoch: u64,
    pending_guest_record: Option<Record>,
    pending_host_record: Option<Record>,
    counters: BrokerCounters,
}

impl ControlSessionBroker {
    pub fn new(instance_id: [u8; 16], capability: [u8; 32]) -> Self {
        Self {
            instance_id,
            capability,
            epoch: 1,
            state: BrokerState::AwaitGuestAttach,
            guest_parser: Parser::new(),
            host_parser: Parser::new(),
            guest_output: OutputLeg::default(),
            host_output: OutputLeg::default(),
            guest_receive_sequence: 0,
            guest_send_sequence: 0,
            host_receive_sequence: 0,
            host_send_sequence: 0,
            host_connected: false,
            host_authenticated: false,
            host_rejected: false,
            host_bound_epoch: 0,
            pending_guest_record: None,
            pending_host_record: None,
            counters: BrokerCounters::default(),
        }
    }

    pub fn state(&self) -> BrokerState {
        self.state
    }

    #[cfg_attr(not(test), expect(dead_code, reason = "used by protocol tests"))]
    pub fn instance_id(&self) -> [u8; 16] {
        self.instance_id
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn counters(&self) -> &BrokerCounters {
        &self.counters
    }

    pub fn host_is_authenticated(&self) -> bool {
        self.host_authenticated
    }

    pub fn host_is_connected(&self) -> bool {
        self.host_connected
    }

    pub fn output_record_count(&self, leg: OutputLegId) -> usize {
        let output = self.output(leg);
        output.queue.len() + usize::from(output.current.is_some())
    }

    pub fn output_byte_count(&self, leg: OutputLegId) -> usize {
        let output = self.output(leg);
        output.queued_bytes
            + output
                .current
                .as_ref()
                .map_or(0, |current| current.bytes.len() - current.offset)
    }

    pub fn guest_parser_buffered_bytes(&self) -> usize {
        let snapshot = self.guest_parser.snapshot();
        snapshot.header_count + snapshot.body_bytes.len()
    }

    /// Resets the device-facing protocol state without changing constructor
    /// credentials. A still-open host transport must be detached separately by
    /// the physical adapter before it can attach again.
    pub fn reset_for_device(&mut self) {
        *self = Self::new(self.instance_id, self.capability);
    }

    /// Reserves the single host slot after OS-level identity verification.
    pub fn begin_host_attachment(&mut self) -> Result<(), BrokerError> {
        if self.host_connected {
            increment(&mut self.counters.reconnect_errors);
            return Err(BrokerError::HostSlotBusy);
        }
        if self.state == BrokerState::Failed {
            return Err(BrokerError::Failed);
        }
        self.host_connected = true;
        self.host_authenticated = false;
        self.host_rejected = false;
        self.host_bound_epoch = 0;
        self.host_parser = Parser::new();
        self.host_output.clear();
        self.pending_host_record = None;
        self.host_receive_sequence = 0;
        self.host_send_sequence = 0;
        Ok(())
    }

    pub fn accept_guest_input(&mut self, input: &[u8]) -> Result<InputProgress, BrokerError> {
        if self.state == BrokerState::Failed {
            return Err(BrokerError::Failed);
        }
        if let Some(record) = self.pending_guest_record.take() {
            if let Err(error) = self.handle_guest_record(record.clone()) {
                if error == BrokerError::Backpressure(OutputLegId::Host) {
                    self.pending_guest_record = Some(record);
                    return Ok(InputProgress {
                        consumed: 0,
                        status: InputStatus::Backpressured,
                    });
                }
                return Err(error);
            }
            if input.is_empty() {
                return Ok(InputProgress {
                    consumed: 0,
                    status: InputStatus::RecordAccepted,
                });
            }
        }

        let progress = match self.guest_parser.accept(input) {
            Ok(progress) => progress,
            Err(error) => {
                increment(&mut self.counters.protocol_errors);
                return Err(error.into());
            }
        };
        let Some(record) = progress.record else {
            return Ok(InputProgress {
                consumed: progress.consumed,
                status: InputStatus::NeedMore,
            });
        };
        match self.handle_guest_record(record.clone()) {
            Ok(()) => Ok(InputProgress {
                consumed: progress.consumed,
                status: InputStatus::RecordAccepted,
            }),
            Err(BrokerError::Backpressure(OutputLegId::Host)) => {
                self.pending_guest_record = Some(record);
                Ok(InputProgress {
                    consumed: progress.consumed,
                    status: InputStatus::Backpressured,
                })
            }
            Err(error) => Err(error),
        }
    }

    pub fn accept_host_input(&mut self, input: &[u8]) -> Result<InputProgress, BrokerError> {
        if !self.host_connected || self.host_rejected {
            increment(&mut self.counters.protocol_errors);
            return Err(BrokerError::IllegalRecord {
                state: self.state,
                record_type: RecordType::HostAttach,
            });
        }
        if self.state == BrokerState::Failed {
            return Err(BrokerError::Failed);
        }
        if let Some(record) = self.pending_host_record.take() {
            if let Err(error) = self.handle_host_record(record.clone()) {
                if error == BrokerError::Backpressure(OutputLegId::Guest) {
                    self.pending_host_record = Some(record);
                    return Ok(InputProgress {
                        consumed: 0,
                        status: InputStatus::Backpressured,
                    });
                }
                return Err(error);
            }
            if input.is_empty() {
                return Ok(InputProgress {
                    consumed: 0,
                    status: InputStatus::RecordAccepted,
                });
            }
        }

        let progress = match self.host_parser.accept(input) {
            Ok(progress) => progress,
            Err(error) => {
                increment(&mut self.counters.protocol_errors);
                return Err(error.into());
            }
        };
        let Some(record) = progress.record else {
            return Ok(InputProgress {
                consumed: progress.consumed,
                status: InputStatus::NeedMore,
            });
        };
        match self.handle_host_record(record.clone()) {
            Ok(()) => Ok(InputProgress {
                consumed: progress.consumed,
                status: InputStatus::RecordAccepted,
            }),
            Err(BrokerError::Backpressure(OutputLegId::Guest)) => {
                self.pending_host_record = Some(record);
                Ok(InputProgress {
                    consumed: progress.consumed,
                    status: InputStatus::Backpressured,
                })
            }
            Err(error) => Err(error),
        }
    }

    /// Drops a host attachment. Only loss of an active authenticated host
    /// advances the epoch.
    pub fn host_disconnected(&mut self) -> Result<(), BrokerError> {
        let active_loss =
            self.host_connected && self.host_authenticated && self.state == BrokerState::Active;
        self.host_connected = false;
        self.host_authenticated = false;
        self.host_rejected = false;
        self.host_bound_epoch = 0;
        self.host_parser = Parser::new();
        self.host_output.clear();
        self.pending_host_record = None;
        self.host_receive_sequence = 0;
        self.host_send_sequence = 0;
        if !active_loss {
            return Ok(());
        }

        self.state = BrokerState::ResetPending;
        let Some(next_epoch) = self.epoch.checked_add(1) else {
            self.state = BrokerState::Failed;
            increment(&mut self.counters.reset_errors);
            return Err(BrokerError::EpochOverflow);
        };
        self.epoch = next_epoch;
        self.guest_output.discard_unstarted();
        self.pending_guest_record = None;
        self.guest_receive_sequence = 0;
        self.guest_send_sequence = 0;
        self.enqueue_guest_control(RecordType::Reset)?;
        Ok(())
    }

    /// Promotes one complete record to the current physical record.
    pub fn begin_output(&mut self, leg: OutputLegId) -> bool {
        self.output_mut(leg).begin()
    }

    /// Returns bytes from the current physical record without advancing them.
    pub fn peek_output(&self, leg: OutputLegId, max_bytes: usize) -> Option<&[u8]> {
        self.output(leg).peek(max_bytes)
    }

    /// Commits bytes reported written by the physical adapter.
    pub fn advance_output(&mut self, leg: OutputLegId, count: usize) -> Result<(), BrokerError> {
        let reset_completed =
            if leg == OutputLegId::Guest && self.state == BrokerState::ResetPending {
                self.guest_output
                    .current
                    .as_ref()
                    .filter(|current| current.bytes.len() - current.offset == count)
                    .map(|current| control_session_protocol::decode_exact(&current.bytes))
                    .transpose()?
                    .is_some_and(|record| {
                        record.record_type == RecordType::Reset
                            && record.instance_id == self.instance_id
                            && record.epoch == self.epoch
                    })
            } else {
                false
            };
        self.output_mut(leg).advance(count)?;
        if reset_completed {
            self.state = BrokerState::AwaitGuestAck;
        }
        Ok(())
    }

    fn handle_guest_record(&mut self, record: Record) -> Result<(), BrokerError> {
        match self.state {
            BrokerState::AwaitGuestAttach => {
                if record.record_type != RecordType::GuestAttach {
                    return self.illegal(record.record_type);
                }
                self.enqueue_guest_control(RecordType::Reset)?;
                self.state = BrokerState::ResetPending;
                Ok(())
            }
            BrokerState::AwaitGuestAck | BrokerState::ResetPending => {
                self.handle_guest_while_awaiting_ack(record)
            }
            BrokerState::ReadyNoHost => {
                if record.record_type == RecordType::Ack {
                    increment(&mut self.counters.ack_errors);
                }
                self.illegal(record.record_type)
            }
            BrokerState::Active => {
                if record.record_type == RecordType::Ack {
                    increment(&mut self.counters.ack_errors);
                    return self.illegal(record.record_type);
                }
                if record.record_type != RecordType::Data {
                    return self.illegal(record.record_type);
                }
                self.validate_current_guest_record(&record)?;
                let encoded = self.make_host_data(&record.payload)?;
                if !self.host_output.can_enqueue(encoded.len()) {
                    increment(&mut self.counters.backpressure_errors);
                    return Err(BrokerError::Backpressure(OutputLegId::Host));
                }
                self.host_output.enqueue(encoded)?;
                self.advance_guest_receive_sequence()?;
                self.advance_host_send_sequence()?;
                Ok(())
            }
            BrokerState::Failed => Err(BrokerError::Failed),
        }
    }

    fn handle_guest_while_awaiting_ack(&mut self, record: Record) -> Result<(), BrokerError> {
        if self.state == BrokerState::ResetPending
            && record.record_type == RecordType::Ack
            && record.instance_id == self.instance_id
            && record.epoch == self.epoch
        {
            increment(&mut self.counters.ack_errors);
            return Err(BrokerError::AckBeforeReset);
        }
        if record.record_type != RecordType::Ack {
            if record.instance_id == self.instance_id && record.epoch < self.epoch {
                return Ok(());
            }
            return self.illegal(record.record_type);
        }
        if record.instance_id != self.instance_id {
            increment(&mut self.counters.ack_errors);
            return Err(BrokerError::InstanceMismatch);
        }
        if record.epoch != self.epoch {
            increment(&mut self.counters.ack_errors);
            return Err(BrokerError::EpochMismatch {
                expected: self.epoch,
                actual: record.epoch,
            });
        }
        if record.sequence != self.guest_receive_sequence {
            increment(&mut self.counters.ack_errors);
            increment(&mut self.counters.sequence_errors);
            return Err(BrokerError::SequenceMismatch {
                expected: self.guest_receive_sequence,
                actual: record.sequence,
            });
        }
        self.advance_guest_receive_sequence()?;
        if self.host_authenticated {
            self.enqueue_host_control(RecordType::Ready, Vec::new())?;
            self.state = BrokerState::Active;
        } else {
            self.state = BrokerState::ReadyNoHost;
        }
        Ok(())
    }

    fn handle_host_record(&mut self, record: Record) -> Result<(), BrokerError> {
        if !self.host_authenticated {
            if record.record_type != RecordType::HostAttach {
                return self.illegal(record.record_type);
            }
            if !constant_time_eq_32(&self.capability, &record.payload) {
                increment(&mut self.counters.authentication_errors);
                self.host_rejected = true;
                // Authentication rejection is close-only; the adapter detaches the host.
                return Err(BrokerError::Authentication);
            }
            self.host_authenticated = true;
            self.host_bound_epoch = self.epoch;
            self.host_receive_sequence = 0;
            self.host_send_sequence = 0;
            self.host_output.clear();
            match self.state {
                BrokerState::ReadyNoHost => {
                    self.enqueue_host_control(RecordType::Ready, Vec::new())?;
                    self.state = BrokerState::Active;
                }
                BrokerState::AwaitGuestAttach
                | BrokerState::AwaitGuestAck
                | BrokerState::ResetPending => {
                    self.enqueue_host_control(RecordType::Wait, Vec::new())?;
                }
                BrokerState::Active => {
                    increment(&mut self.counters.reconnect_errors);
                    return Err(BrokerError::HostSlotBusy);
                }
                BrokerState::Failed => return Err(BrokerError::Failed),
            }
            return Ok(());
        }

        if self.state != BrokerState::Active || record.record_type != RecordType::Data {
            return self.illegal(record.record_type);
        }
        if record.instance_id != self.instance_id {
            increment(&mut self.counters.protocol_errors);
            return Err(BrokerError::InstanceMismatch);
        }
        if record.epoch != self.host_bound_epoch || record.epoch != self.epoch {
            increment(&mut self.counters.protocol_errors);
            return Err(BrokerError::EpochMismatch {
                expected: self.epoch,
                actual: record.epoch,
            });
        }
        if record.sequence != self.host_receive_sequence {
            increment(&mut self.counters.sequence_errors);
            return Err(BrokerError::SequenceMismatch {
                expected: self.host_receive_sequence,
                actual: record.sequence,
            });
        }
        let encoded = self.make_guest_data(&record.payload)?;
        if !self.guest_output.can_enqueue(encoded.len()) {
            increment(&mut self.counters.backpressure_errors);
            return Err(BrokerError::Backpressure(OutputLegId::Guest));
        }
        self.guest_output.enqueue(encoded)?;
        self.advance_host_receive_sequence()?;
        self.advance_guest_send_sequence()?;
        Ok(())
    }

    fn validate_current_guest_record(&mut self, record: &Record) -> Result<(), BrokerError> {
        if record.instance_id != self.instance_id {
            increment(&mut self.counters.protocol_errors);
            return Err(BrokerError::InstanceMismatch);
        }
        if record.epoch != self.epoch {
            increment(&mut self.counters.protocol_errors);
            return Err(BrokerError::EpochMismatch {
                expected: self.epoch,
                actual: record.epoch,
            });
        }
        if record.sequence != self.guest_receive_sequence {
            increment(&mut self.counters.sequence_errors);
            return Err(BrokerError::SequenceMismatch {
                expected: self.guest_receive_sequence,
                actual: record.sequence,
            });
        }
        Ok(())
    }

    fn enqueue_guest_control(&mut self, record_type: RecordType) -> Result<(), BrokerError> {
        let record = Record::session(
            record_type,
            self.instance_id,
            self.epoch,
            self.guest_send_sequence,
            Vec::new(),
        );
        let encoded = control_session_protocol::encode(&record)?;
        if !self.guest_output.can_enqueue(encoded.len()) {
            increment(&mut self.counters.backpressure_errors);
            return Err(BrokerError::Backpressure(OutputLegId::Guest));
        }
        self.guest_output.enqueue(encoded)?;
        self.advance_guest_send_sequence()
    }

    fn enqueue_host_control(
        &mut self,
        record_type: RecordType,
        payload: Vec<u8>,
    ) -> Result<(), BrokerError> {
        let record = Record::session(
            record_type,
            self.instance_id,
            self.epoch,
            self.host_send_sequence,
            payload,
        );
        let encoded = control_session_protocol::encode(&record)?;
        if !self.host_output.can_enqueue(encoded.len()) {
            increment(&mut self.counters.backpressure_errors);
            return Err(BrokerError::Backpressure(OutputLegId::Host));
        }
        self.host_output.enqueue(encoded)?;
        self.advance_host_send_sequence()
    }

    fn make_guest_data(&self, payload: &[u8]) -> Result<Vec<u8>, BrokerError> {
        Ok(control_session_protocol::encode(&Record::session(
            RecordType::Data,
            self.instance_id,
            self.epoch,
            self.guest_send_sequence,
            payload.to_vec(),
        ))?)
    }

    fn make_host_data(&self, payload: &[u8]) -> Result<Vec<u8>, BrokerError> {
        Ok(control_session_protocol::encode(&Record::session(
            RecordType::Data,
            self.instance_id,
            self.epoch,
            self.host_send_sequence,
            payload.to_vec(),
        ))?)
    }

    fn advance_guest_receive_sequence(&mut self) -> Result<(), BrokerError> {
        advance_sequence(&mut self.guest_receive_sequence, &mut self.state)
    }

    fn advance_guest_send_sequence(&mut self) -> Result<(), BrokerError> {
        advance_sequence(&mut self.guest_send_sequence, &mut self.state)
    }

    fn advance_host_receive_sequence(&mut self) -> Result<(), BrokerError> {
        advance_sequence(&mut self.host_receive_sequence, &mut self.state)
    }

    fn advance_host_send_sequence(&mut self) -> Result<(), BrokerError> {
        advance_sequence(&mut self.host_send_sequence, &mut self.state)
    }

    fn illegal<T>(&mut self, record_type: RecordType) -> Result<T, BrokerError> {
        increment(&mut self.counters.protocol_errors);
        Err(BrokerError::IllegalRecord {
            state: self.state,
            record_type,
        })
    }

    fn output(&self, leg: OutputLegId) -> &OutputLeg {
        match leg {
            OutputLegId::Guest => &self.guest_output,
            OutputLegId::Host => &self.host_output,
        }
    }

    fn output_mut(&mut self, leg: OutputLegId) -> &mut OutputLeg {
        match leg {
            OutputLegId::Guest => &mut self.guest_output,
            OutputLegId::Host => &mut self.host_output,
        }
    }
}

fn advance_sequence(sequence: &mut u64, state: &mut BrokerState) -> Result<(), BrokerError> {
    let Some(next) = sequence.checked_add(1) else {
        *state = BrokerState::Failed;
        return Err(BrokerError::SequenceOverflow);
    };
    *sequence = next;
    Ok(())
}

fn increment(counter: &mut u64) {
    *counter = counter.saturating_add(1);
}

fn constant_time_eq_32(expected: &[u8; 32], actual: &[u8]) -> bool {
    let mut difference = actual.len() ^ expected.len();
    for (index, expected_byte) in expected.iter().enumerate() {
        let actual_byte = actual.get(index).copied().unwrap_or(0);
        difference |= usize::from(*expected_byte ^ actual_byte);
    }
    difference == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    const INSTANCE: [u8; 16] = [0x22; 16];
    const CAPABILITY: [u8; 32] = [0x33; 32];

    fn feed_guest(
        broker: &mut ControlSessionBroker,
        record: &Record,
    ) -> Result<InputProgress, BrokerError> {
        let bytes = control_session_protocol::encode(record)?;
        broker.accept_guest_input(&bytes)
    }

    fn feed_host(
        broker: &mut ControlSessionBroker,
        record: &Record,
    ) -> Result<InputProgress, BrokerError> {
        let bytes = control_session_protocol::encode(record)?;
        broker.accept_host_input(&bytes)
    }

    fn drain_record(
        broker: &mut ControlSessionBroker,
        leg: OutputLegId,
    ) -> Result<Record, BrokerError> {
        assert!(broker.begin_output(leg));
        let bytes = broker
            .peek_output(leg, usize::MAX)
            .ok_or(BrokerError::InvalidOutputProgress)?
            .to_vec();
        broker.advance_output(leg, bytes.len())?;
        Ok(control_session_protocol::decode_exact(&bytes)?)
    }

    fn ack(broker: &ControlSessionBroker) -> Record {
        Record::session(
            RecordType::Ack,
            broker.instance_id(),
            broker.epoch(),
            0,
            Vec::new(),
        )
    }

    fn make_active() -> Result<ControlSessionBroker, BrokerError> {
        let mut broker = ControlSessionBroker::new(INSTANCE, CAPABILITY);
        feed_guest(
            &mut broker,
            &Record::bootstrap(RecordType::GuestAttach, Vec::new()),
        )?;
        assert_eq!(
            drain_record(&mut broker, OutputLegId::Guest)?.record_type,
            RecordType::Reset
        );
        let guest_ack = ack(&broker);
        feed_guest(&mut broker, &guest_ack)?;
        broker.begin_host_attachment()?;
        feed_host(
            &mut broker,
            &Record::bootstrap(RecordType::HostAttach, CAPABILITY.to_vec()),
        )?;
        assert_eq!(
            drain_record(&mut broker, OutputLegId::Host)?.record_type,
            RecordType::Ready
        );
        assert_eq!(broker.state(), BrokerState::Active);
        Ok(broker)
    }

    #[test]
    fn cold_boot_authentication_and_independent_sequences() -> Result<(), BrokerError> {
        let mut broker = ControlSessionBroker::new(INSTANCE, CAPABILITY);
        feed_guest(
            &mut broker,
            &Record::bootstrap(RecordType::GuestAttach, Vec::new()),
        )?;
        let reset = drain_record(&mut broker, OutputLegId::Guest)?;
        assert_eq!((reset.epoch, reset.sequence), (1, 0));

        broker.begin_host_attachment()?;
        feed_host(
            &mut broker,
            &Record::bootstrap(RecordType::HostAttach, CAPABILITY.to_vec()),
        )?;
        let wait = drain_record(&mut broker, OutputLegId::Host)?;
        assert_eq!((wait.record_type, wait.sequence), (RecordType::Wait, 0));

        let guest_ack = ack(&broker);
        feed_guest(&mut broker, &guest_ack)?;
        let ready = drain_record(&mut broker, OutputLegId::Host)?;
        assert_eq!((ready.record_type, ready.sequence), (RecordType::Ready, 1));

        feed_guest(
            &mut broker,
            &Record::session(RecordType::Data, INSTANCE, 1, 1, b"guest".to_vec()),
        )?;
        feed_host(
            &mut broker,
            &Record::session(RecordType::Data, INSTANCE, 1, 0, b"host".to_vec()),
        )?;
        let to_host = drain_record(&mut broker, OutputLegId::Host)?;
        let to_guest = drain_record(&mut broker, OutputLegId::Guest)?;
        assert_eq!((to_host.sequence, to_host.payload), (2, b"guest".to_vec()));
        assert_eq!((to_guest.sequence, to_guest.payload), (1, b"host".to_vec()));
        Ok(())
    }

    #[test]
    fn wrong_capability_fails_without_advancing_epoch() -> Result<(), BrokerError> {
        let mut broker = ControlSessionBroker::new(INSTANCE, CAPABILITY);
        broker.begin_host_attachment()?;
        let error = feed_host(
            &mut broker,
            &Record::bootstrap(RecordType::HostAttach, vec![0x44; 32]),
        )
        .expect_err("wrong capability must fail");
        assert_eq!(error, BrokerError::Authentication);
        assert_eq!(broker.epoch(), 1);
        assert!(!broker.host_is_authenticated());
        assert_eq!(broker.output_record_count(OutputLegId::Host), 0);
        assert_eq!(broker.counters().authentication_errors, 1);
        Ok(())
    }

    #[test]
    fn stale_future_wrong_and_duplicate_acks_are_rejected() -> Result<(), BrokerError> {
        let mut broker = ControlSessionBroker::new(INSTANCE, CAPABILITY);
        feed_guest(
            &mut broker,
            &Record::bootstrap(RecordType::GuestAttach, Vec::new()),
        )?;
        let early_ack = ack(&broker);
        assert_eq!(
            feed_guest(&mut broker, &early_ack),
            Err(BrokerError::AckBeforeReset)
        );
        assert_eq!(
            drain_record(&mut broker, OutputLegId::Guest)?.record_type,
            RecordType::Reset
        );
        let wrong_instance = Record::session(RecordType::Ack, [9; 16], 1, 0, Vec::new());
        assert_eq!(
            feed_guest(&mut broker, &wrong_instance),
            Err(BrokerError::InstanceMismatch)
        );
        let stale = Record::session(RecordType::Ack, INSTANCE, 0, 0, Vec::new());
        assert!(matches!(
            feed_guest(&mut broker, &stale),
            Err(BrokerError::EpochMismatch { actual: 0, .. })
        ));
        let future = Record::session(RecordType::Ack, INSTANCE, 2, 0, Vec::new());
        assert!(matches!(
            feed_guest(&mut broker, &future),
            Err(BrokerError::EpochMismatch { actual: 2, .. })
        ));
        let wrong_sequence = Record::session(RecordType::Ack, INSTANCE, 1, 1, Vec::new());
        assert!(matches!(
            feed_guest(&mut broker, &wrong_sequence),
            Err(BrokerError::SequenceMismatch { actual: 1, .. })
        ));
        let guest_ack = ack(&broker);
        feed_guest(&mut broker, &guest_ack)?;
        assert!(
            feed_guest(
                &mut broker,
                &Record::session(RecordType::Ack, INSTANCE, 1, 0, Vec::new())
            )
            .is_err()
        );
        assert_eq!(broker.counters().ack_errors, 6);
        Ok(())
    }

    #[test]
    fn ready_without_host_rejects_data() -> Result<(), BrokerError> {
        let mut broker = ControlSessionBroker::new(INSTANCE, CAPABILITY);
        feed_guest(
            &mut broker,
            &Record::bootstrap(RecordType::GuestAttach, Vec::new()),
        )?;
        assert_eq!(
            drain_record(&mut broker, OutputLegId::Guest)?.record_type,
            RecordType::Reset
        );
        let guest_ack = ack(&broker);
        feed_guest(&mut broker, &guest_ack)?;
        assert_eq!(broker.state(), BrokerState::ReadyNoHost);
        assert!(
            feed_guest(
                &mut broker,
                &Record::session(RecordType::Data, INSTANCE, 1, 1, vec![1])
            )
            .is_err()
        );
        assert!(broker.pending_guest_record.is_none());
        Ok(())
    }

    #[test]
    fn partial_output_disconnect_finishes_record_then_reset() -> Result<(), BrokerError> {
        let mut broker = make_active()?;
        for (sequence, payload) in [(0, b"first".as_slice()), (1, b"second".as_slice())] {
            feed_host(
                &mut broker,
                &Record::session(RecordType::Data, INSTANCE, 1, sequence, payload.to_vec()),
            )?;
        }
        assert!(broker.begin_output(OutputLegId::Guest));
        let first_len = broker
            .peek_output(OutputLegId::Guest, usize::MAX)
            .ok_or(BrokerError::InvalidOutputProgress)?
            .len();
        broker.advance_output(OutputLegId::Guest, first_len - 1)?;
        broker.host_disconnected()?;
        assert_eq!(broker.state(), BrokerState::ResetPending);
        assert_eq!(broker.epoch(), 2);

        let remainder = broker
            .peek_output(OutputLegId::Guest, usize::MAX)
            .ok_or(BrokerError::InvalidOutputProgress)?
            .len();
        assert_eq!(remainder, 1);
        broker.advance_output(OutputLegId::Guest, remainder)?;
        let reset = drain_record(&mut broker, OutputLegId::Guest)?;
        assert_eq!(
            (reset.record_type, reset.epoch, reset.sequence),
            (RecordType::Reset, 2, 0)
        );
        assert!(!broker.begin_output(OutputLegId::Guest));
        Ok(())
    }

    #[test]
    fn disconnect_after_last_byte_drops_unstarted_next_record() -> Result<(), BrokerError> {
        let mut broker = make_active()?;
        for sequence in 0..2 {
            feed_host(
                &mut broker,
                &Record::session(
                    RecordType::Data,
                    INSTANCE,
                    1,
                    sequence,
                    vec![sequence as u8],
                ),
            )?;
        }
        let _first = drain_record(&mut broker, OutputLegId::Guest)?;
        broker.host_disconnected()?;
        let next = drain_record(&mut broker, OutputLegId::Guest)?;
        assert_eq!(next.record_type, RecordType::Reset);
        Ok(())
    }

    #[test]
    fn disconnect_at_every_host_record_offset_forwards_no_partial_data() -> Result<(), BrokerError>
    {
        let encoded = control_session_protocol::encode(&Record::session(
            RecordType::Data,
            INSTANCE,
            1,
            0,
            b"body-with-NVXS".to_vec(),
        ))?;
        for split in 0..encoded.len() {
            let mut broker = make_active()?;
            let progress = broker.accept_host_input(&encoded[..split])?;
            assert_eq!(progress.status, InputStatus::NeedMore);
            broker.host_disconnected()?;
            let output = drain_record(&mut broker, OutputLegId::Guest)?;
            assert_eq!(output.record_type, RecordType::Reset, "split {split}");
            assert!(!broker.begin_output(OutputLegId::Guest), "split {split}");
        }
        Ok(())
    }

    #[test]
    fn rapid_reconnect_waits_without_another_epoch() -> Result<(), BrokerError> {
        let mut broker = make_active()?;
        broker.host_disconnected()?;
        assert_eq!(broker.epoch(), 2);
        broker.begin_host_attachment()?;
        feed_host(
            &mut broker,
            &Record::bootstrap(RecordType::HostAttach, CAPABILITY.to_vec()),
        )?;
        assert_eq!(
            drain_record(&mut broker, OutputLegId::Host)?.record_type,
            RecordType::Wait
        );
        assert_eq!(broker.epoch(), 2);
        assert_eq!(
            broker.begin_host_attachment(),
            Err(BrokerError::HostSlotBusy)
        );

        assert_eq!(
            drain_record(&mut broker, OutputLegId::Guest)?.record_type,
            RecordType::Reset
        );
        let current_ack = Record::session(RecordType::Ack, INSTANCE, 2, 0, Vec::new());
        feed_guest(&mut broker, &current_ack)?;
        assert_eq!(broker.state(), BrokerState::Active);
        assert_eq!(
            drain_record(&mut broker, OutputLegId::Host)?.record_type,
            RecordType::Ready
        );
        Ok(())
    }

    #[test]
    fn bounded_queue_returns_backpressure_without_data_loss() -> Result<(), BrokerError> {
        let mut broker = make_active()?;
        let payload = vec![0x5a; control_session_protocol::MAX_DATA_LEN];
        let mut sequence = 0;
        loop {
            let progress = feed_guest(
                &mut broker,
                &Record::session(RecordType::Data, INSTANCE, 1, sequence + 1, payload.clone()),
            )?;
            if progress.status == InputStatus::Backpressured {
                break;
            }
            sequence += 1;
            assert!(sequence <= MAX_QUEUED_RECORDS_PER_LEG as u64);
        }
        assert!(broker.pending_guest_record.is_some());
        assert_eq!(broker.counters().backpressure_errors, 1);
        let _ = drain_record(&mut broker, OutputLegId::Host)?;
        let progress = broker.accept_guest_input(&[])?;
        assert_eq!(progress.status, InputStatus::RecordAccepted);
        assert!(broker.pending_guest_record.is_none());
        Ok(())
    }

    #[test]
    fn epoch_overflow_fails_closed() -> Result<(), BrokerError> {
        let mut broker = make_active()?;
        broker.epoch = u64::MAX;
        assert_eq!(broker.host_disconnected(), Err(BrokerError::EpochOverflow));
        assert_eq!(broker.state(), BrokerState::Failed);
        assert_eq!(broker.counters().reset_errors, 1);
        Ok(())
    }

    #[test]
    fn device_reset_discards_attachment_and_protocol_progress() -> Result<(), BrokerError> {
        let mut broker = make_active()?;
        feed_guest(
            &mut broker,
            &Record::session(RecordType::Data, INSTANCE, 1, 1, b"queued".to_vec()),
        )?;
        assert!(broker.output_record_count(OutputLegId::Host) != 0);

        broker.reset_for_device();

        assert_eq!(broker.state(), BrokerState::AwaitGuestAttach);
        assert_eq!(broker.epoch(), 1);
        assert!(!broker.host_is_connected());
        assert!(!broker.host_is_authenticated());
        assert_eq!(broker.output_record_count(OutputLegId::Guest), 0);
        assert_eq!(broker.output_record_count(OutputLegId::Host), 0);
        assert_eq!(broker.guest_parser_buffered_bytes(), 0);
        Ok(())
    }
}
