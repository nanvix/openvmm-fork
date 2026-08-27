// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! microVM chipset devices.

pub mod resolver;

use chipset_device::ChipsetDevice;
use chipset_device::io::IoError;
use chipset_device::io::IoResult;
use chipset_device::io::deferred::DeferredWrite;
use chipset_device::io::deferred::defer_write;
use chipset_device::pio::PortIoIntercept;
use chipset_device::poll_device::PollDevice;
use futures::AsyncRead;
use futures::AsyncWrite;
use inspect::InspectMut;
use power_resources::PowerRequest;
use power_resources::PowerRequestClient;
use serial_core::SerialIo;
use std::collections::VecDeque;
use std::io::ErrorKind;
use std::ops::RangeInclusive;
use std::pin::Pin;
use std::task::Context;
use std::task::Poll;
use std::task::Waker;
use vmcore::device_state::ChangeDeviceState;
use vmcore::save_restore::NoSavedState;
use vmcore::save_restore::RestoreError;
use vmcore::save_restore::SaveError;
use vmcore::save_restore::SaveRestore;
use vmcore::save_restore::SavedStateRoot;

const DATA_PORT: u16 = 0xe9;
const STATUS_PORT: u16 = 0xea;
const SHUTDOWN_PORT: u16 = 0x604;
const SNAPSHOT_PORT: u16 = 0x605;
const RESTORE_ENTROPY_SELECT: u8 = 0xa5;
const BUFFER_MAX: usize = 1024 * 1024;

/// Raw bidirectional microVM portb console.
#[derive(InspectMut)]
pub struct MicrovmPortb {
    #[inspect(skip)]
    io_region: (&'static str, RangeInclusive<u16>),
    #[inspect(mut)]
    io: Box<dyn SerialIo>,
    #[inspect(with = "VecDeque::len")]
    rx_buffer: VecDeque<u8>,
    #[inspect(with = "VecDeque::len")]
    tx_buffer: VecDeque<u8>,
    #[inspect(with = "VecDeque::len")]
    restore_entropy: VecDeque<u8>,
    restore_entropy_selected: bool,
    #[inspect(skip)]
    rx_waker: Option<Waker>,
    #[inspect(skip)]
    tx_waker: Option<Waker>,
}

impl MicrovmPortb {
    /// Creates a portb console using `io` as its host endpoint.
    pub fn new(io: Box<dyn SerialIo>, restore_entropy: Vec<u8>) -> Self {
        Self {
            io_region: ("microvm-portb", DATA_PORT..=STATUS_PORT),
            io,
            rx_buffer: VecDeque::new(),
            tx_buffer: VecDeque::new(),
            restore_entropy: restore_entropy.into(),
            restore_entropy_selected: false,
            rx_waker: None,
            tx_waker: None,
        }
    }

    fn poll_rx(&mut self, cx: &mut Context<'_>) {
        let mut buffer = [0; 256];
        loop {
            if self.rx_buffer.len() == BUFFER_MAX {
                self.rx_waker = Some(cx.waker().clone());
                return;
            }

            let available = BUFFER_MAX - self.rx_buffer.len();
            let read_len = available.min(buffer.len());
            match Pin::new(&mut self.io).poll_read(cx, &mut buffer[..read_len]) {
                Poll::Ready(Ok(0)) | Poll::Pending => return,
                Poll::Ready(Ok(count)) => self.rx_buffer.extend(&buffer[..count]),
                Poll::Ready(Err(error)) => {
                    tracelimit::error_ratelimited!(
                        error = &error as &dyn std::error::Error,
                        "microVM portb input failed"
                    );
                    return;
                }
            }
        }
    }

    fn poll_tx(&mut self, cx: &mut Context<'_>) {
        while !self.tx_buffer.is_empty() {
            let (buffer, _) = self.tx_buffer.as_slices();
            match Pin::new(&mut self.io).poll_write(cx, buffer) {
                Poll::Ready(Ok(0)) => break,
                Poll::Ready(Ok(count)) => {
                    self.tx_buffer.drain(..count);
                }
                Poll::Ready(Err(error)) if error.kind() == ErrorKind::BrokenPipe => break,
                Poll::Ready(Err(error)) => {
                    tracelimit::error_ratelimited!(
                        len = buffer.len(),
                        error = &error as &dyn std::error::Error,
                        "microVM portb output failed; dropping buffered bytes"
                    );
                    self.tx_buffer.clear();
                }
                Poll::Pending => break,
            }
        }
        if self.tx_buffer.is_empty() {
            self.tx_waker = Some(cx.waker().clone());
        }
    }

    fn wake_tx(&mut self) {
        if let Some(waker) = self.tx_waker.take() {
            waker.wake();
        }
    }

    fn wake_rx(&mut self) {
        if let Some(waker) = self.rx_waker.take() {
            waker.wake();
        }
    }
}

impl ChangeDeviceState for MicrovmPortb {
    fn start(&mut self) {}

    async fn stop(&mut self) {
        // Drain every byte the endpoint accepts immediately. Any remaining
        // VMM-owned bytes are serialized and retried against the reconstructed
        // endpoint after restore.
        self.poll_tx(&mut Context::from_waker(Waker::noop()));
    }

    async fn reset(&mut self) {
        self.rx_buffer.clear();
        self.tx_buffer.clear();
        self.restore_entropy.clear();
        self.restore_entropy_selected = false;
    }
}

impl ChipsetDevice for MicrovmPortb {
    fn supports_pio(&mut self) -> Option<&mut dyn PortIoIntercept> {
        Some(self)
    }

    fn supports_poll_device(&mut self) -> Option<&mut dyn PollDevice> {
        Some(self)
    }
}

impl PollDevice for MicrovmPortb {
    fn poll_device(&mut self, cx: &mut Context<'_>) {
        if !self.io.is_connected() {
            match self.io.poll_connect(cx) {
                Poll::Ready(Ok(())) => {}
                Poll::Ready(Err(error)) => {
                    tracelimit::error_ratelimited!(
                        error = &error as &dyn std::error::Error,
                        "microVM portb backend connection failed"
                    );
                    return;
                }
                Poll::Pending => return,
            }
        }
        self.poll_rx(cx);
        self.poll_tx(cx);
    }
}

impl PortIoIntercept for MicrovmPortb {
    fn io_read(&mut self, io_port: u16, data: &mut [u8]) -> IoResult {
        if data.is_empty() {
            return IoResult::Err(IoError::InvalidAccessSize);
        }
        data.fill(0);
        match io_port {
            DATA_PORT => {
                if self.restore_entropy_selected {
                    data[0] = self.restore_entropy.pop_front().unwrap_or(0);
                    if self.restore_entropy.is_empty() {
                        self.restore_entropy_selected = false;
                    }
                } else {
                    data[0] = self.rx_buffer.pop_front().unwrap_or(0);
                    self.wake_rx();
                }
            }
            STATUS_PORT => {
                data[0] = u8::from(!self.restore_entropy_selected && !self.rx_buffer.is_empty())
            }
            _ => return IoResult::Err(IoError::InvalidRegister),
        }
        IoResult::Ok
    }

    fn io_write(&mut self, io_port: u16, data: &[u8]) -> IoResult {
        match io_port {
            DATA_PORT => {
                let available = BUFFER_MAX - self.tx_buffer.len();
                self.tx_buffer.extend(data.iter().copied().take(available));
                if data.len() > available {
                    tracelimit::warn_ratelimited!(
                        dropped = data.len() - available,
                        "microVM portb output buffer full; dropping newest bytes"
                    );
                }
                self.wake_tx();
            }
            STATUS_PORT => {
                if data.first() == Some(&RESTORE_ENTROPY_SELECT) && !self.restore_entropy.is_empty()
                {
                    self.restore_entropy_selected = true;
                }
            }
            _ => return IoResult::Err(IoError::InvalidRegister),
        }
        IoResult::Ok
    }

    fn get_static_regions(&mut self) -> &[(&str, RangeInclusive<u16>)] {
        std::slice::from_ref(&self.io_region)
    }
}

impl SaveRestore for MicrovmPortb {
    type SavedState = MicrovmPortbSavedState;

    fn save(&mut self) -> Result<Self::SavedState, SaveError> {
        Ok(MicrovmPortbSavedState {
            rx_buffer: self.rx_buffer.iter().copied().collect(),
            tx_buffer: self.tx_buffer.iter().copied().collect(),
        })
    }

    fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError> {
        if state.rx_buffer.len() > BUFFER_MAX || state.tx_buffer.len() > BUFFER_MAX {
            return Err(RestoreError::InvalidSavedState(
                std::io::Error::new(
                    ErrorKind::InvalidData,
                    format!("microVM portb buffer exceeds {BUFFER_MAX} bytes"),
                )
                .into(),
            ));
        }
        self.rx_buffer = state.rx_buffer.into();
        self.tx_buffer = state.tx_buffer.into();
        self.restore_entropy_selected = false;
        Ok(())
    }
}

/// Saved guest-visible bytes owned by the microVM portb device.
#[derive(mesh::payload::Protobuf, SavedStateRoot)]
#[mesh(package = "chipset.microvm_portb")]
pub struct MicrovmPortbSavedState {
    /// Host bytes accepted but not consumed by the guest.
    #[mesh(1)]
    pub rx_buffer: Vec<u8>,
    /// Guest bytes accepted but not acknowledged by the host endpoint.
    #[mesh(2)]
    pub tx_buffer: Vec<u8>,
}

/// microVM process-status shutdown port.
#[derive(InspectMut)]
pub struct MicrovmShutdown {
    #[inspect(skip)]
    io_region: (&'static str, RangeInclusive<u16>),
    #[inspect(skip)]
    power_request: PowerRequestClient,
}

impl MicrovmShutdown {
    /// Creates the shutdown device.
    pub fn new(power_request: PowerRequestClient) -> Self {
        Self {
            io_region: ("microvm-shutdown", SHUTDOWN_PORT..=SHUTDOWN_PORT),
            power_request,
        }
    }
}

impl ChangeDeviceState for MicrovmShutdown {
    fn start(&mut self) {}
    async fn stop(&mut self) {}
    async fn reset(&mut self) {}
}

impl ChipsetDevice for MicrovmShutdown {
    fn supports_pio(&mut self) -> Option<&mut dyn PortIoIntercept> {
        Some(self)
    }
}

impl PortIoIntercept for MicrovmShutdown {
    fn io_read(&mut self, io_port: u16, data: &mut [u8]) -> IoResult {
        if io_port != SHUTDOWN_PORT {
            return IoResult::Err(IoError::InvalidRegister);
        }
        data.fill(0xff);
        IoResult::Ok
    }

    fn io_write(&mut self, io_port: u16, data: &[u8]) -> IoResult {
        if io_port != SHUTDOWN_PORT {
            return IoResult::Err(IoError::InvalidRegister);
        }
        self.power_request
            .power_request(PowerRequest::PowerOffWithStatus {
                code: data.first().copied().unwrap_or(0),
            });
        IoResult::Ok
    }

    fn get_static_regions(&mut self) -> &[(&str, RangeInclusive<u16>)] {
        std::slice::from_ref(&self.io_region)
    }
}

impl SaveRestore for MicrovmShutdown {
    type SavedState = NoSavedState;

    fn save(&mut self) -> Result<Self::SavedState, SaveError> {
        Ok(NoSavedState)
    }

    fn restore(&mut self, NoSavedState: Self::SavedState) -> Result<(), RestoreError> {
        Ok(())
    }
}

/// Snapshot-request port with at most one unacknowledged notification.
#[derive(InspectMut)]
pub struct MicrovmSnapshotRequest {
    #[inspect(skip)]
    io_region: (&'static str, RangeInclusive<u16>),
    #[inspect(skip)]
    notify: Option<mesh::Sender<chipset_resources::microvm::MicrovmSnapshotBoundaryRequest>>,
    input_gate_timeout: std::time::Duration,
    #[inspect(skip)]
    pending: Option<PendingSnapshotWrite>,
    #[inspect(skip)]
    poll_waker: Option<Waker>,
}

struct PendingSnapshotWrite {
    release_write: mesh::OneshotReceiver<()>,
    deferred_write: Option<DeferredWrite>,
    write_completed: Option<mesh::OneshotSender<()>>,
    transaction_complete: mesh::rpc::PendingRpc<()>,
    write_released: bool,
}

impl MicrovmSnapshotRequest {
    /// Creates a snapshot-request device with an optional asynchronous notification target.
    pub fn new(
        notify: Option<mesh::Sender<chipset_resources::microvm::MicrovmSnapshotBoundaryRequest>>,
        input_gate_timeout: std::time::Duration,
    ) -> Self {
        Self {
            io_region: ("microvm-snapshot-request", SNAPSHOT_PORT..=SNAPSHOT_PORT),
            notify,
            input_gate_timeout,
            pending: None,
            poll_waker: None,
        }
    }
}

impl ChangeDeviceState for MicrovmSnapshotRequest {
    fn start(&mut self) {}
    async fn stop(&mut self) {
        if self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.write_released)
        {
            return;
        }
        if let Some(mut pending) = self.pending.take() {
            if let Some(deferred_write) = pending.deferred_write.take() {
                deferred_write.complete_error(IoError::InvalidRegister);
            }
            if let Some(write_completed) = pending.write_completed.take() {
                write_completed.send(());
            }
        }
    }
    async fn reset(&mut self) {
        self.stop().await;
    }
}

impl ChipsetDevice for MicrovmSnapshotRequest {
    fn supports_pio(&mut self) -> Option<&mut dyn PortIoIntercept> {
        Some(self)
    }

    fn supports_poll_device(&mut self) -> Option<&mut dyn PollDevice> {
        Some(self)
    }
}

impl PollDevice for MicrovmSnapshotRequest {
    fn poll_device(&mut self, cx: &mut Context<'_>) {
        use std::future::Future;

        self.poll_waker = Some(cx.waker().clone());

        let Some(pending) = &mut self.pending else {
            return;
        };
        if !pending.write_released && Pin::new(&mut pending.release_write).poll(cx).is_ready() {
            pending.write_released = true;
            if let Some(deferred_write) = pending.deferred_write.take() {
                deferred_write.complete();
            }
            if let Some(write_completed) = pending.write_completed.take() {
                write_completed.send(());
            }
        }
        if Pin::new(&mut pending.transaction_complete)
            .poll(cx)
            .is_ready()
        {
            self.pending = None;
        }
    }
}

impl PortIoIntercept for MicrovmSnapshotRequest {
    fn io_read(&mut self, io_port: u16, data: &mut [u8]) -> IoResult {
        if io_port != SNAPSHOT_PORT {
            return IoResult::Err(IoError::InvalidRegister);
        }
        data.fill(0xff);
        IoResult::Ok
    }

    fn io_write(&mut self, io_port: u16, data: &[u8]) -> IoResult {
        use mesh::rpc::RpcSend;

        if io_port != SNAPSHOT_PORT {
            return IoResult::Err(IoError::InvalidRegister);
        }
        if self.pending.is_some() {
            tracelimit::warn_ratelimited!("coalescing duplicate microVM snapshot request");
            return IoResult::Ok;
        }
        if let Some(notify) = &self.notify {
            let scratch_policy = if data.first().copied().unwrap_or(0) == 0 {
                chipset_resources::microvm::MicrovmSnapshotScratchPolicy::Fresh
            } else {
                chipset_resources::microvm::MicrovmSnapshotScratchPolicy::Paired
            };
            let (deferred_write, token) = defer_write();
            let (release_write, release_recv) = mesh::oneshot();
            let (write_completed, write_completed_recv) = mesh::oneshot();
            let transaction_complete = notify.call(
                |transaction_complete| chipset_resources::microvm::MicrovmSnapshotBoundaryRequest {
                    scratch_policy,
                    release_write,
                    write_completed: write_completed_recv,
                    input_gate_timeout: self.input_gate_timeout,
                    transaction_complete,
                },
                (),
            );
            self.pending = Some(PendingSnapshotWrite {
                release_write: release_recv,
                deferred_write: Some(deferred_write),
                write_completed: Some(write_completed),
                transaction_complete,
                write_released: false,
            });
            if let Some(waker) = &self.poll_waker {
                waker.wake_by_ref();
            }
            return IoResult::Defer(token);
        }
        IoResult::Ok
    }

    fn get_static_regions(&mut self) -> &[(&str, RangeInclusive<u16>)] {
        std::slice::from_ref(&self.io_region)
    }
}

impl SaveRestore for MicrovmSnapshotRequest {
    type SavedState = NoSavedState;

    fn save(&mut self) -> Result<Self::SavedState, SaveError> {
        Ok(NoSavedState)
    }

    fn restore(&mut self, NoSavedState: Self::SavedState) -> Result<(), RestoreError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::AsyncRead;
    use futures::AsyncWrite;
    use parking_lot::Mutex;
    use serial_core::disconnected::Disconnected;
    use std::io;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::task::Context;
    use std::task::Poll;

    struct ConnectWithByte {
        connected: bool,
        byte: Option<u8>,
    }

    impl InspectMut for ConnectWithByte {
        fn inspect_mut(&mut self, req: inspect::Request<'_>) {
            req.respond();
        }
    }

    impl SerialIo for ConnectWithByte {
        fn is_connected(&self) -> bool {
            self.connected
        }

        fn poll_connect(&mut self, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            self.connected = true;
            Poll::Ready(Ok(()))
        }

        fn poll_disconnect(&mut self, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Pending
        }
    }

    impl AsyncRead for ConnectWithByte {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            data: &mut [u8],
        ) -> Poll<io::Result<usize>> {
            let Some(byte) = self.byte.take() else {
                return Poll::Pending;
            };
            data[0] = byte;
            Poll::Ready(Ok(1))
        }
    }

    impl AsyncWrite for ConnectWithByte {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            data: &[u8],
        ) -> Poll<io::Result<usize>> {
            Poll::Ready(Ok(data.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[test]
    fn portb_preserves_wide_binary_output_and_zero_fills_reads() {
        let mut portb = MicrovmPortb::new(Box::new(Disconnected), Vec::new());
        assert!(matches!(
            portb.io_write(DATA_PORT, b"\0\xffA\x80"),
            IoResult::Ok
        ));
        assert_eq!(portb.tx_buffer, b"\0\xffA\x80");

        portb.rx_buffer.extend([0x5a, 0x6b]);
        let mut status = [0xff; 4];
        assert!(matches!(
            portb.io_read(STATUS_PORT, &mut status),
            IoResult::Ok
        ));
        assert_eq!(status, [1, 0, 0, 0]);

        let mut data = [0xff; 4];
        assert!(matches!(portb.io_read(DATA_PORT, &mut data), IoResult::Ok));
        assert_eq!(data, [0x5a, 0, 0, 0]);
    }

    #[test]
    fn pending_portb_bytes_survive_restore_and_fresh_entropy_is_private() {
        let mut portb = MicrovmPortb::new(Box::new(Disconnected), Vec::new());
        portb.rx_buffer.extend([1, 2, 3]);
        portb.tx_buffer.extend([4, 5, 6]);
        let state = portb.save().unwrap();

        let mut restored = MicrovmPortb::new(Box::new(Disconnected), vec![7, 8]);
        restored.restore(state).unwrap();
        assert_eq!(restored.rx_buffer, [1, 2, 3]);
        assert_eq!(restored.tx_buffer, [4, 5, 6]);

        let mut data = [0];
        for expected in [1, 2, 3] {
            assert!(matches!(
                restored.io_read(DATA_PORT, &mut data),
                IoResult::Ok
            ));
            assert_eq!(data, [expected]);
        }
        assert!(matches!(
            restored.io_read(DATA_PORT, &mut data),
            IoResult::Ok
        ));
        assert_eq!(data, [0]);
        assert_eq!(restored.restore_entropy, [7, 8]);

        assert!(matches!(
            restored.io_write(STATUS_PORT, &[RESTORE_ENTROPY_SELECT]),
            IoResult::Ok
        ));
        assert!(matches!(
            restored.io_read(STATUS_PORT, &mut data),
            IoResult::Ok
        ));
        assert_eq!(data, [0]);
        for expected in [7, 8] {
            assert!(matches!(
                restored.io_read(DATA_PORT, &mut data),
                IoResult::Ok
            ));
            assert_eq!(data, [expected]);
        }
    }

    #[test]
    fn accepted_connection_is_polled_immediately() {
        let mut portb = MicrovmPortb::new(
            Box::new(ConnectWithByte {
                connected: false,
                byte: Some(0x5a),
            }),
            Vec::new(),
        );
        portb.poll_device(&mut Context::from_waker(Waker::noop()));
        assert_eq!(portb.rx_buffer, [0x5a]);
    }

    #[test]
    fn lifecycle_ports_preserve_status_and_remain_nonblocking() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&requests);
        let mut shutdown =
            MicrovmShutdown::new((move |request| captured.lock().push(request)).into());
        assert!(matches!(
            shutdown.io_write(SHUTDOWN_PORT, &[37, 99]),
            IoResult::Ok
        ));
        assert_eq!(
            *requests.lock(),
            [PowerRequest::PowerOffWithStatus { code: 37 }]
        );
        requests.lock().clear();
        assert!(matches!(
            shutdown.io_write(SHUTDOWN_PORT, &[0x25]),
            IoResult::Ok
        ));
        assert_eq!(
            *requests.lock(),
            [PowerRequest::PowerOffWithStatus { code: 0x25 }]
        );

        let mut snapshot = MicrovmSnapshotRequest::new(None, std::time::Duration::from_secs(1));
        let mut data = [0; 4];
        assert!(matches!(
            snapshot.io_read(SNAPSHOT_PORT, &mut data),
            IoResult::Ok
        ));
        assert_eq!(data, [0xff; 4]);
        assert!(matches!(
            snapshot.io_write(SNAPSHOT_PORT, &[1]),
            IoResult::Ok
        ));
    }

    #[test]
    fn snapshot_requests_are_coalesced_until_acknowledged() {
        let (send, mut recv) = mesh::channel();
        let mut snapshot =
            MicrovmSnapshotRequest::new(Some(send), std::time::Duration::from_secs(1));
        snapshot.poll_device(&mut Context::from_waker(Waker::noop()));

        let IoResult::Defer(mut deferred_write) = snapshot.io_write(SNAPSHOT_PORT, &[1]) else {
            panic!("snapshot write was not deferred");
        };
        assert!(matches!(
            snapshot.io_write(SNAPSHOT_PORT, &[2, 3, 4, 5]),
            IoResult::Ok
        ));
        let mut first = recv.try_recv().unwrap();
        assert_eq!(
            first.scratch_policy,
            chipset_resources::microvm::MicrovmSnapshotScratchPolicy::Paired
        );
        assert!(recv.try_recv().is_err());
        assert!(
            deferred_write
                .poll_write(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );

        first.release_write.send(());
        snapshot.poll_device(&mut Context::from_waker(Waker::noop()));
        assert!(
            deferred_write
                .poll_write(&mut Context::from_waker(Waker::noop()))
                .is_ready()
        );
        assert!(
            Pin::new(&mut first.write_completed)
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_ready()
        );

        first.transaction_complete.complete(());
        snapshot.poll_device(&mut Context::from_waker(Waker::noop()));
        assert!(matches!(
            snapshot.io_write(SNAPSHOT_PORT, &[]),
            IoResult::Defer(_)
        ));
        assert_eq!(
            recv.try_recv().unwrap().scratch_policy,
            chipset_resources::microvm::MicrovmSnapshotScratchPolicy::Fresh
        );
    }
}
