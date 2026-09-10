// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::StalledIo;
use super::core::TransportOps;
use super::core::VirtioTransportCore;
use super::task::ConfigReadCompletion;
use super::task::defer_config_read;
use super::task::defer_config_write;
use crate::DynVirtioDevice;
use crate::MAX_QUEUE_SIZE;
use crate::spec::VIRTIO_MMIO_INTERRUPT_STATUS_CONFIG_CHANGE;
use crate::spec::VIRTIO_MMIO_INTERRUPT_STATUS_USED_BUFFER;
use crate::spec::mmio::VirtioMmioRegister;
use chipset_device::ChipsetDevice;
use chipset_device::io::IoResult;
use chipset_device::io::deferred::defer_read;
use chipset_device::io::deferred::defer_write;
use chipset_device::mmio::MmioIntercept;
use chipset_device::poll_device::PollDevice;
use device_emulators::ReadWriteRequestType;
use device_emulators::read_as_u32_chunks;
use device_emulators::write_as_u32_chunks;
use guestmem::DoorbellRegistration;
use guestmem::GuestMemory;
use guestmem::GuestMemoryError;
use inspect::Inspect;
use inspect::InspectMut;
#[cfg(target_os = "linux")]
use pal_async::driver::PollImpl;
#[cfg(target_os = "linux")]
use pal_async::driver::SpawnDriver as MmioDriver;
#[cfg(target_os = "linux")]
use pal_async::fd::PollFdReady;
#[cfg(target_os = "linux")]
use pal_async::interest::InterestSlot;
#[cfg(target_os = "linux")]
use pal_async::interest::PollEvents;
#[cfg(not(target_os = "linux"))]
use pal_async::task::Spawn as MmioDriver;
#[cfg(target_os = "linux")]
use pal_async::task::Task;
#[cfg(target_os = "linux")]
use pal_event::Event;
use parking_lot::Mutex;
use std::fmt;
#[cfg(target_os = "linux")]
use std::future::poll_fn;
use std::ops::RangeInclusive;
#[cfg(target_os = "linux")]
use std::os::fd::AsFd;
#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::task::Context;
use vmcore::device_state::ChangeDeviceState;
use vmcore::interrupt::Interrupt;
use vmcore::line_interrupt::LineInterrupt;

/// MMIO-specific transport state.
#[derive(Inspect)]
struct MmioTransport {
    #[inspect(skip)]
    fixed_mmio_region: (&'static str, RangeInclusive<u64>),
    #[inspect(hex)]
    device_id: u32,
    #[inspect(hex)]
    vendor_id: u32,
    interrupt_state: Arc<Mutex<InterruptState>>,
    #[cfg(target_os = "linux")]
    #[inspect(skip)]
    _interrupt_ack_task: Option<Task<()>>,
    #[cfg(target_os = "linux")]
    #[inspect(skip)]
    _interrupt_ack_doorbell: Option<Box<dyn Send + Sync>>,
    #[cfg(target_os = "linux")]
    #[inspect(skip)]
    interrupt_ack_event: Option<Event>,
}

#[cfg(target_os = "linux")]
struct InterruptAckWait {
    ready: PollImpl<dyn PollFdReady>,
    event: Event,
    interrupt_state: Arc<Mutex<InterruptState>>,
}

#[cfg(target_os = "linux")]
impl InterruptAckWait {
    async fn run(mut self) {
        loop {
            poll_fn(|cx| {
                self.ready
                    .poll_fd_ready(cx, InterestSlot::Read, PollEvents::IN)
            })
            .await;
            self.ready.clear_fd_ready(InterestSlot::Read);
            let mut state = self.interrupt_state.lock();
            if self.event.try_wait() {
                state.acknowledge_used_buffer();
            }
        }
    }
}

#[derive(Inspect)]
struct InterruptState {
    interrupt: LineInterrupt,
    status: u32,
    used_buffer_generation: u64,
    observed_used_buffer_generation: Option<u64>,
    shared_status: Option<SharedInterruptStatus>,
}

#[derive(Inspect)]
struct SharedInterruptStatus {
    #[inspect(skip)]
    guest_memory: GuestMemory,
    #[inspect(hex)]
    gpa: u64,
}

impl SharedInterruptStatus {
    fn update(&self, operation: impl Fn(u32) -> u32) -> Result<u32, GuestMemoryError> {
        let mut current = self.guest_memory.read_plain::<u32>(self.gpa)?;
        loop {
            let new = operation(current);
            match self.guest_memory.compare_exchange(self.gpa, current, new)? {
                Ok(_) => return Ok(current),
                Err(actual) => current = actual,
            }
        }
    }

    fn load(&self) -> Result<u32, GuestMemoryError> {
        self.update(|current| current)
    }

    fn store(&self, value: u32) -> Result<u32, GuestMemoryError> {
        self.update(|_| value)
    }

    fn fetch_or(&self, bits: u32) -> Result<u32, GuestMemoryError> {
        self.update(|current| current | bits)
    }
}

impl InterruptState {
    fn update(&mut self, is_set: bool, bits: u32) {
        if let Some(shared_status) = &self.shared_status {
            if is_set {
                let old_status = shared_status
                    .fetch_or(bits)
                    .expect("validated shared interrupt-status memory became inaccessible");
                if old_status == 0 {
                    self.interrupt.set_level(true);
                    self.interrupt.set_level(false);
                }
            }
            return;
        }

        if is_set {
            if bits & VIRTIO_MMIO_INTERRUPT_STATUS_USED_BUFFER != 0 {
                self.used_buffer_generation = self.used_buffer_generation.wrapping_add(1);
            }
            self.status |= bits;
        } else {
            if bits & VIRTIO_MMIO_INTERRUPT_STATUS_USED_BUFFER != 0 {
                self.observed_used_buffer_generation = None;
            }
            self.status &= !bits;
        }
        self.interrupt.set_level(self.status != 0);
    }

    fn read_status(&mut self) -> u32 {
        if let Some(shared_status) = &self.shared_status {
            return shared_status
                .load()
                .expect("validated shared interrupt-status memory became inaccessible");
        }
        if self.status & VIRTIO_MMIO_INTERRUPT_STATUS_USED_BUFFER != 0 {
            self.observed_used_buffer_generation = Some(self.used_buffer_generation);
        }
        self.status
    }

    #[cfg(target_os = "linux")]
    fn acknowledge_used_buffer(&mut self) {
        if self.shared_status.is_some() {
            return;
        }
        if self.observed_used_buffer_generation == Some(self.used_buffer_generation) {
            self.update(false, VIRTIO_MMIO_INTERRUPT_STATUS_USED_BUFFER);
        } else {
            self.observed_used_buffer_generation = None;
        }
    }
}

impl MmioTransport {
    fn lock_interrupt_state(&self) -> parking_lot::MutexGuard<'_, InterruptState> {
        let state = self.interrupt_state.lock();
        #[cfg(target_os = "linux")]
        let mut state = state;
        #[cfg(target_os = "linux")]
        if let Some(event) = &self.interrupt_ack_event {
            if event.try_wait() {
                state.acknowledge_used_buffer();
            }
        }
        state
    }

    fn read_interrupt_status(&self) -> u32 {
        self.lock_interrupt_state().read_status()
    }

    fn reset_interrupt_state(&self) {
        let mut state = self.lock_interrupt_state();
        if let Some(shared_status) = &state.shared_status {
            shared_status
                .store(0)
                .expect("validated shared interrupt-status memory became inaccessible");
        }
        state.status = 0;
        state.used_buffer_generation = 0;
        state.observed_used_buffer_generation = None;
        state.interrupt.set_level(false);
    }
}

impl Drop for MmioTransport {
    fn drop(&mut self) {
        let state = self.interrupt_state.lock();
        if let Some(shared_status) = &state.shared_status {
            let _ = shared_status.store(0);
        }
        state.interrupt.set_level(false);
    }
}

/// Interrupt-status delivery used by a virtio-mmio transport.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum VirtioMmioInterruptMode {
    /// Virtio MMIO status reads, acknowledgement writes, and a level interrupt.
    #[default]
    Legacy,
    /// A shared atomic status word and a pulse on each zero-to-nonzero transition.
    SharedStatus { status_gpa: u64 },
}

impl TransportOps for MmioTransport {
    fn create_queue_interrupt(&mut self, _idx: usize, _msix_vector: u16) -> Interrupt {
        let interrupt_state = self.interrupt_state.clone();
        Interrupt::from_fn(move || {
            interrupt_state
                .lock()
                .update(true, VIRTIO_MMIO_INTERRUPT_STATUS_USED_BUFFER);
        })
    }

    fn signal_config_change(&mut self) {
        self.interrupt_state
            .lock()
            .update(true, VIRTIO_MMIO_INTERRUPT_STATUS_CONFIG_CHANGE);
    }

    fn reset_interrupts(&mut self) {
        self.reset_interrupt_state();
    }

    fn doorbell_region(&mut self) -> Option<(u64, u32)> {
        let base = (*self.fixed_mmio_region.1.start() & !0xfff)
            + VirtioMmioRegister::QUEUE_NOTIFY.0 as u64;
        Some((base, 4))
    }
}

/// Run a virtio device over MMIO
#[derive(InspectMut)]
pub struct VirtioMmioDevice {
    #[inspect(flatten)]
    core: VirtioTransportCore,
    #[inspect(flatten)]
    mmio: MmioTransport,
}

impl fmt::Debug for VirtioMmioDevice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VirtioMmioDevice").finish()
    }
}

impl VirtioMmioDevice {
    pub fn new(
        device: Box<dyn DynVirtioDevice>,
        driver: &impl MmioDriver,
        guest_memory: GuestMemory,
        interrupt: LineInterrupt,
        doorbell_registration: Option<Arc<dyn DoorbellRegistration>>,
        mmio_gpa: u64,
        mmio_len: u64,
    ) -> std::io::Result<Self> {
        Self::new_with_disabled_features(
            device,
            driver,
            guest_memory,
            interrupt,
            doorbell_registration,
            mmio_gpa,
            mmio_len,
            0,
        )
    }

    /// Creates an MMIO transport after masking guest-visible device features.
    pub fn new_with_disabled_features(
        device: Box<dyn DynVirtioDevice>,
        driver: &impl MmioDriver,
        guest_memory: GuestMemory,
        interrupt: LineInterrupt,
        doorbell_registration: Option<Arc<dyn DoorbellRegistration>>,
        mmio_gpa: u64,
        mmio_len: u64,
        disabled_features: u64,
    ) -> std::io::Result<Self> {
        Self::new_with_disabled_features_and_interrupt_mode(
            device,
            driver,
            guest_memory,
            interrupt,
            doorbell_registration,
            mmio_gpa,
            mmio_len,
            disabled_features,
            VirtioMmioInterruptMode::Legacy,
        )
    }

    /// Creates an MMIO transport with explicit interrupt-status delivery.
    pub fn new_with_disabled_features_and_interrupt_mode(
        device: Box<dyn DynVirtioDevice>,
        driver: &impl MmioDriver,
        guest_memory: GuestMemory,
        interrupt: LineInterrupt,
        doorbell_registration: Option<Arc<dyn DoorbellRegistration>>,
        mmio_gpa: u64,
        mmio_len: u64,
        disabled_features: u64,
        interrupt_mode: VirtioMmioInterruptMode,
    ) -> std::io::Result<Self> {
        let traits = device.traits();
        #[cfg(target_os = "linux")]
        let supports_accelerated_doorbells = device.supports_accelerated_doorbells();
        let shared_status = match interrupt_mode {
            VirtioMmioInterruptMode::Legacy => None,
            VirtioMmioInterruptMode::SharedStatus { status_gpa } => {
                if !status_gpa.is_multiple_of(size_of::<u32>() as u64) {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        format!(
                            "shared interrupt-status GPA {status_gpa:#x} is not naturally aligned"
                        ),
                    ));
                }
                let shared_status = SharedInterruptStatus {
                    guest_memory: guest_memory.clone(),
                    gpa: status_gpa,
                };
                shared_status.store(0).map_err(|error| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        format!(
                            "shared interrupt-status GPA {status_gpa:#x} is inaccessible: {error}"
                        ),
                    )
                })?;
                Some(shared_status)
            }
        };
        let interrupt_state = Arc::new(Mutex::new(InterruptState {
            interrupt,
            status: 0,
            used_buffer_generation: 0,
            observed_used_buffer_generation: None,
            shared_status,
        }));
        #[cfg(target_os = "linux")]
        let (interrupt_ack_event, interrupt_ack_task, interrupt_ack_doorbell) = if interrupt_mode
            == VirtioMmioInterruptMode::Legacy
            && supports_accelerated_doorbells
            && let Some(registration) = &doorbell_registration
        {
            let event = Event::new();
            match registration.register_doorbell(
                mmio_gpa + VirtioMmioRegister::INTERRUPT_ACK.0 as u64,
                Some(VIRTIO_MMIO_INTERRUPT_STATUS_USED_BUFFER.into()),
                Some(4),
                &event,
            ) {
                Ok(doorbell) => match driver.new_dyn_fd_ready(event.as_fd().as_raw_fd()) {
                    Ok(ready) => {
                        let interrupt_ack_event = event.clone();
                        let task = driver.spawn(
                            "virtio-mmio-interrupt-ack",
                            InterruptAckWait {
                                ready,
                                event,
                                interrupt_state: interrupt_state.clone(),
                            }
                            .run(),
                        );
                        (Some(interrupt_ack_event), Some(task), Some(doorbell))
                    }
                    Err(_) => (None, None, None),
                },
                Err(_) => (None, None, None),
            }
        } else {
            (None, None, None)
        };

        let core = VirtioTransportCore::new_with_disabled_features(
            device,
            driver,
            guest_memory,
            doorbell_registration,
            disabled_features,
        )?;

        Ok(Self {
            core,
            mmio: MmioTransport {
                fixed_mmio_region: ("virtio-chipset", mmio_gpa..=(mmio_gpa + mmio_len - 1)),
                device_id: traits.device_id.0 as u32,
                vendor_id: 0x1af4,
                interrupt_state,
                #[cfg(target_os = "linux")]
                _interrupt_ack_task: interrupt_ack_task,
                #[cfg(target_os = "linux")]
                _interrupt_ack_doorbell: interrupt_ack_doorbell,
                #[cfg(target_os = "linux")]
                interrupt_ack_event,
            },
        })
    }

    /// Synchronous transport register read for tests.
    #[cfg(test)]
    pub(crate) fn read_u32(&mut self, address: u64) -> u32 {
        self.read_u32_local((address & 0xfff) as u16)
    }

    /// Synchronous transport register write for tests.
    #[cfg(test)]
    pub(crate) fn write_u32(&mut self, address: u64, val: u32) {
        self.write_u32_local((address & 0xfff) as u16, val);
    }

    /// Read a transport register as a u32.
    fn read_u32_local(&mut self, offset: u16) -> u32 {
        assert!(offset & 3 == 0);
        let queue_select = self.core.queue_select as usize;
        match VirtioMmioRegister(offset) {
            VirtioMmioRegister::MAGIC_VALUE => u32::from_le_bytes(*b"virt"),
            VirtioMmioRegister::VERSION => 2,
            VirtioMmioRegister::DEVICE_ID => self.mmio.device_id,
            VirtioMmioRegister::VENDOR_ID => self.mmio.vendor_id,
            VirtioMmioRegister::DEVICE_FEATURES => self
                .core
                .device_feature
                .bank(self.core.device_feature_select as usize),
            VirtioMmioRegister::DEVICE_FEATURES_SEL => self.core.device_feature_select,
            VirtioMmioRegister::DRIVER_FEATURES => self
                .core
                .driver_feature
                .bank(self.core.driver_feature_select as usize),
            VirtioMmioRegister::DRIVER_FEATURES_SEL => self.core.driver_feature_select,
            VirtioMmioRegister::QUEUE_SEL => self.core.queue_select,
            VirtioMmioRegister::QUEUE_NUM_MAX => self
                .core
                .queues
                .get(queue_select)
                .map_or(0, |qd| qd.initial_size.into()),
            VirtioMmioRegister::QUEUE_NUM => self
                .core
                .queues
                .get(queue_select)
                .map_or(0, |qd| qd.params.size as u32),
            VirtioMmioRegister::QUEUE_READY => {
                self.core
                    .queues
                    .get(queue_select)
                    .is_some_and(|qd| qd.params.enable) as u32
            }
            VirtioMmioRegister::QUEUE_NOTIFY => 0,
            VirtioMmioRegister::INTERRUPT_STATUS => self.mmio.read_interrupt_status(),
            VirtioMmioRegister::INTERRUPT_ACK => 0,
            VirtioMmioRegister::STATUS => self.core.device_status.as_u32(),
            VirtioMmioRegister::QUEUE_DESC_LOW => self
                .core
                .queues
                .get(queue_select)
                .map_or(0, |qd| qd.params.desc_addr as u32),
            VirtioMmioRegister::QUEUE_DESC_HIGH => self
                .core
                .queues
                .get(queue_select)
                .map_or(0, |qd| (qd.params.desc_addr >> 32) as u32),
            VirtioMmioRegister::QUEUE_AVAIL_LOW => self
                .core
                .queues
                .get(queue_select)
                .map_or(0, |qd| qd.params.avail_addr as u32),
            VirtioMmioRegister::QUEUE_AVAIL_HIGH => self
                .core
                .queues
                .get(queue_select)
                .map_or(0, |qd| (qd.params.avail_addr >> 32) as u32),
            VirtioMmioRegister::QUEUE_USED_LOW => self
                .core
                .queues
                .get(queue_select)
                .map_or(0, |qd| qd.params.used_addr as u32),
            VirtioMmioRegister::QUEUE_USED_HIGH => self
                .core
                .queues
                .get(queue_select)
                .map_or(0, |qd| (qd.params.used_addr >> 32) as u32),
            VirtioMmioRegister::CONFIG_GENERATION => self.core.config_generation,
            _ => 0xffffffff,
        }
    }

    /// Write a transport register as a u32.
    fn write_u32_local(&mut self, offset: u16, val: u32) {
        assert!(offset & 3 == 0);
        let queue_select = self.core.queue_select as usize;
        let queues_locked = self.core.device_status.driver_ok();
        let features_locked = queues_locked || self.core.device_status.features_ok();
        match VirtioMmioRegister(offset) {
            VirtioMmioRegister::DEVICE_FEATURES_SEL => self.core.device_feature_select = val,
            VirtioMmioRegister::DRIVER_FEATURES => {
                let bank = self.core.driver_feature_select as usize;
                if !features_locked && bank < 2 {
                    self.core
                        .driver_feature
                        .set_bank(bank, val & self.core.device_feature.bank(bank));
                }
            }
            VirtioMmioRegister::DRIVER_FEATURES_SEL => self.core.driver_feature_select = val,
            VirtioMmioRegister::QUEUE_SEL => self.core.queue_select = val,
            VirtioMmioRegister::QUEUE_NUM => {
                if !queues_locked && queue_select < self.core.queues.len() {
                    let val = val as u16;
                    let queue = &mut self.core.queues[queue_select].params;
                    if val > MAX_QUEUE_SIZE {
                        queue.size = MAX_QUEUE_SIZE;
                    } else {
                        queue.size = val;
                    }
                }
            }
            VirtioMmioRegister::QUEUE_READY => {
                if !queues_locked && queue_select < self.core.queues.len() {
                    self.core.queues[queue_select].params.enable = val != 0;
                }
            }
            VirtioMmioRegister::QUEUE_NOTIFY => {
                self.core.notify_queue(val);
            }
            VirtioMmioRegister::INTERRUPT_ACK => {
                self.mmio.interrupt_state.lock().update(false, val);
            }
            VirtioMmioRegister::STATUS => {
                self.core.write_device_status(&mut self.mmio, val as u8);
            }
            VirtioMmioRegister::QUEUE_DESC_LOW => {
                if !queues_locked && queue_select < self.core.queues.len() {
                    let queue = &mut self.core.queues[queue_select].params;
                    queue.desc_addr = queue.desc_addr & 0xffffffff00000000 | val as u64;
                }
            }
            VirtioMmioRegister::QUEUE_DESC_HIGH => {
                if !queues_locked && queue_select < self.core.queues.len() {
                    let queue = &mut self.core.queues[queue_select].params;
                    queue.desc_addr = (val as u64) << 32 | queue.desc_addr & 0xffffffff;
                }
            }
            VirtioMmioRegister::QUEUE_AVAIL_LOW => {
                if !queues_locked && queue_select < self.core.queues.len() {
                    let queue = &mut self.core.queues[queue_select].params;
                    queue.avail_addr = queue.avail_addr & 0xffffffff00000000 | val as u64;
                }
            }
            VirtioMmioRegister::QUEUE_AVAIL_HIGH => {
                if !queues_locked && queue_select < self.core.queues.len() {
                    let queue = &mut self.core.queues[queue_select].params;
                    queue.avail_addr = (val as u64) << 32 | queue.avail_addr & 0xffffffff;
                }
            }
            VirtioMmioRegister::QUEUE_USED_LOW => {
                if !queues_locked && queue_select < self.core.queues.len() {
                    let queue = &mut self.core.queues[queue_select].params;
                    queue.used_addr = queue.used_addr & 0xffffffff00000000 | val as u64;
                }
            }
            VirtioMmioRegister::QUEUE_USED_HIGH => {
                if !queues_locked && queue_select < self.core.queues.len() {
                    let queue = &mut self.core.queues[queue_select].params;
                    queue.used_addr = (val as u64) << 32 | queue.used_addr & 0xffffffff;
                }
            }
            _ => (),
        }
    }

    /// Read transport registers via sub-word chunk handling.
    fn read_transport(&mut self, offset: u16, data: &mut [u8]) {
        read_as_u32_chunks(offset, data, |offset| self.read_u32_local(offset));
    }

    /// Write transport registers via sub-word chunk handling.
    fn write_transport(&mut self, offset: u16, data: &[u8]) {
        write_as_u32_chunks(offset, data, |offset, request_type| match request_type {
            ReadWriteRequestType::Write(value) => {
                self.write_u32_local(offset, value);
                None
            }
            ReadWriteRequestType::Read => Some(self.read_u32_local(offset)),
        });
    }

    /// Replay MMIO accesses that were stalled while the transport was busy.
    fn replay_stalled_io(&mut self) {
        let stalled = std::mem::take(&mut self.core.stalled_io);
        let mut iter = stalled.into_iter();
        for io in &mut iter {
            match io {
                StalledIo::Read {
                    address,
                    len,
                    deferred,
                } => {
                    let mut buf = vec![0u8; len];
                    self.read_transport((address & 0xfff) as u16, &mut buf);
                    deferred.complete(&buf);
                }
                StalledIo::Write {
                    address,
                    data,
                    len,
                    deferred,
                } => {
                    self.write_transport((address & 0xfff) as u16, &data[..len]);
                    if self.core.state.is_busy() {
                        self.core.pending_status_deferred = Some(deferred);
                        break;
                    }
                    deferred.complete();
                }
            }
        }
        self.core.stalled_io = iter.collect();
    }
}

impl ChangeDeviceState for VirtioMmioDevice {
    fn start(&mut self) {
        self.core.start(&mut self.mmio);
    }

    async fn start_fallible(&mut self) -> anyhow::Result<()> {
        self.core.start_fallible(&mut self.mmio).await
    }

    async fn quiesce_input(&mut self) -> anyhow::Result<()> {
        self.core.quiesce_input().await
    }

    async fn resume_input(&mut self) -> anyhow::Result<()> {
        self.core.resume_input().await
    }

    async fn stop(&mut self) {
        self.core.stop(&mut self.mmio).await;
    }

    async fn reset(&mut self) {
        self.core.reset(&mut self.mmio).await;
    }
}

impl PollDevice for VirtioMmioDevice {
    fn poll_device(&mut self, cx: &mut Context<'_>) {
        self.core.poll_device(&mut self.mmio, cx);
        if !self.core.stalled_io.is_empty() && !self.core.state.is_busy() {
            self.replay_stalled_io();
        }
    }
}

impl ChipsetDevice for VirtioMmioDevice {
    fn supports_mmio(&mut self) -> Option<&mut dyn MmioIntercept> {
        Some(self)
    }

    fn supports_poll_device(&mut self) -> Option<&mut dyn PollDevice> {
        Some(self)
    }
}

mod saved_state {
    mod state {
        use crate::transport::saved_state::state::CommonQueueState;
        use crate::transport::saved_state::state::CommonSavedState;
        use mesh::payload::Protobuf;
        use vmcore::save_restore::SavedStateBlob;
        use vmcore::save_restore::SavedStateRoot;

        #[derive(Protobuf)]
        #[mesh(package = "virtio.transport.mmio")]
        pub struct SavedQueueState {
            #[mesh(1)]
            pub common: CommonQueueState,
        }

        #[derive(Protobuf, SavedStateRoot)]
        #[mesh(package = "virtio.transport.mmio")]
        pub struct SavedState {
            #[mesh(1)]
            pub common: CommonSavedState,
            #[mesh(2)]
            pub queues: Vec<SavedQueueState>,
            #[mesh(3)]
            pub interrupt_status: u32,
            #[mesh(4)]
            pub device_state: Option<SavedStateBlob>,
        }
    }

    use super::*;
    use vmcore::save_restore::SaveRestore;

    impl SaveRestore for VirtioMmioDevice {
        type SavedState = state::SavedState;

        fn save(&mut self) -> Result<Self::SavedState, vmcore::save_restore::SaveError> {
            let common = self.core.save_common()?;
            let queues = (0..self.core.queues.len())
                .map(|i| state::SavedQueueState {
                    common: self.core.save_queue_common(i),
                })
                .collect();
            let device_state = self.core.take_device_state()?;
            Ok(state::SavedState {
                common,
                queues,
                interrupt_status: self.mmio.read_interrupt_status(),
                device_state,
            })
        }

        fn restore(
            &mut self,
            state: Self::SavedState,
        ) -> Result<(), vmcore::save_restore::RestoreError> {
            self.mmio.reset_interrupt_state();
            let saved_queue_count = state.queues.len();
            self.core.restore_common(
                &mut self.mmio,
                &state.common,
                state.device_state,
                state.queues.into_iter().map(|sq| (sq.common, 0)),
                saved_queue_count,
            )?;

            // Restore MMIO-specific interrupt state.
            {
                let mut is = self.mmio.interrupt_state.lock();
                if let Some(shared_status) = &is.shared_status {
                    shared_status
                        .store(state.interrupt_status)
                        .map_err(|error| {
                            vmcore::save_restore::RestoreError::InvalidSavedState(error.into())
                        })?;
                    is.interrupt.set_level(false);
                } else {
                    is.status = state.interrupt_status;
                    is.used_buffer_generation = 0;
                    is.observed_used_buffer_generation =
                        (is.status & VIRTIO_MMIO_INTERRUPT_STATUS_USED_BUFFER != 0).then_some(0);
                    is.interrupt.set_level(is.status != 0);
                }
            }

            Ok(())
        }
    }
}

impl MmioIntercept for VirtioMmioDevice {
    fn mmio_read(&mut self, address: u64, data: &mut [u8]) -> IoResult {
        let offset = (address & 0xfff) as u16;
        if offset >= VirtioMmioRegister::CONFIG.0 {
            return defer_config_read(
                &self.core.device_sender,
                offset - VirtioMmioRegister::CONFIG.0,
                data.len() as u8,
                ConfigReadCompletion::Exact,
            );
        }
        if self.core.state.is_busy() {
            let (deferred, token) = defer_read();
            self.core.stalled_io.push(StalledIo::Read {
                address,
                len: data.len(),
                deferred,
            });
            return IoResult::Defer(token);
        }
        self.read_transport(offset, data);
        IoResult::Ok
    }

    fn mmio_write(&mut self, address: u64, data: &[u8]) -> IoResult {
        let offset = (address & 0xfff) as u16;
        if offset >= VirtioMmioRegister::CONFIG.0 {
            return defer_config_write(
                &self.core.device_sender,
                offset - VirtioMmioRegister::CONFIG.0,
                data,
            );
        }
        if self.core.state.is_busy() {
            let (deferred, token) = defer_write();
            let mut buf = [0u8; 8];
            buf[..data.len()].copy_from_slice(data);
            self.core.stalled_io.push(StalledIo::Write {
                address,
                data: buf,
                len: data.len(),
                deferred,
            });
            return IoResult::Defer(token);
        }
        self.write_transport(offset, data);
        if self.core.state.is_busy() {
            let (deferred, token) = defer_write();
            self.core.pending_status_deferred = Some(deferred);
            return IoResult::Defer(token);
        }
        IoResult::Ok
    }

    fn get_static_regions(&mut self) -> &[(&str, RangeInclusive<u64>)] {
        std::slice::from_ref(&self.mmio.fixed_mmio_region)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Barrier;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use vmcore::line_interrupt::LineSetTarget;

    #[derive(Default)]
    struct CountingInterruptTarget {
        high: AtomicBool,
        pulses: AtomicUsize,
    }

    impl LineSetTarget for CountingInterruptTarget {
        fn set_irq(&self, _vector: u32, high: bool) {
            self.high.store(high, Ordering::SeqCst);
            if high {
                self.pulses.fetch_add(1, Ordering::SeqCst);
            }
        }
    }

    fn shared_interrupt_state(
        guest_memory: &GuestMemory,
        target: Arc<CountingInterruptTarget>,
    ) -> InterruptState {
        InterruptState {
            interrupt: LineInterrupt::new_with_target("shared-status-test", target, 0),
            status: 0,
            used_buffer_generation: 0,
            observed_used_buffer_generation: None,
            shared_status: Some(SharedInterruptStatus {
                guest_memory: guest_memory.clone(),
                gpa: 0,
            }),
        }
    }

    #[test]
    fn shared_status_coalesces_bits_and_pulses_on_zero_transition() {
        let guest_memory = GuestMemory::allocate(0x1000);
        let target = Arc::new(CountingInterruptTarget::default());
        let mut state = shared_interrupt_state(&guest_memory, target.clone());

        state.update(true, VIRTIO_MMIO_INTERRUPT_STATUS_USED_BUFFER);
        state.update(true, VIRTIO_MMIO_INTERRUPT_STATUS_CONFIG_CHANGE);
        state.update(true, VIRTIO_MMIO_INTERRUPT_STATUS_USED_BUFFER);

        assert_eq!(state.read_status(), 3);
        assert_eq!(target.pulses.load(Ordering::SeqCst), 1);
        assert!(!target.high.load(Ordering::SeqCst));

        let consumed = state.shared_status.as_ref().unwrap().store(0).unwrap();
        assert_eq!(consumed, 3);
        state.update(true, VIRTIO_MMIO_INTERRUPT_STATUS_CONFIG_CHANGE);
        assert_eq!(target.pulses.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn shared_status_completion_racing_exchange_is_not_lost() {
        const ITERATIONS: usize = 1000;

        let guest_memory = GuestMemory::allocate(0x1000);
        let shared_status = Arc::new(SharedInterruptStatus {
            guest_memory,
            gpa: 0,
        });
        let barrier = Arc::new(Barrier::new(2));
        let publisher = {
            let shared_status = shared_status.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                for _ in 0..ITERATIONS {
                    barrier.wait();
                    shared_status
                        .fetch_or(VIRTIO_MMIO_INTERRUPT_STATUS_USED_BUFFER)
                        .unwrap();
                    barrier.wait();
                }
            })
        };

        for _ in 0..ITERATIONS {
            shared_status.store(0).unwrap();
            barrier.wait();
            let consumed = shared_status.store(0).unwrap();
            barrier.wait();
            let pending = shared_status.load().unwrap();
            assert_eq!(
                (consumed | pending) & VIRTIO_MMIO_INTERRUPT_STATUS_USED_BUFFER,
                VIRTIO_MMIO_INTERRUPT_STATUS_USED_BUFFER
            );
        }
        publisher.join().unwrap();
    }

    #[test]
    fn shared_status_reset_clears_pending_state_without_interrupt() {
        let guest_memory = GuestMemory::allocate(0x1000);
        let target = Arc::new(CountingInterruptTarget::default());
        let state = MmioTransport {
            fixed_mmio_region: ("test", 0..=0xfff),
            device_id: 0,
            vendor_id: 0,
            interrupt_state: Arc::new(Mutex::new(shared_interrupt_state(
                &guest_memory,
                target.clone(),
            ))),
            #[cfg(target_os = "linux")]
            _interrupt_ack_task: None,
            #[cfg(target_os = "linux")]
            _interrupt_ack_doorbell: None,
            #[cfg(target_os = "linux")]
            interrupt_ack_event: None,
        };
        state
            .interrupt_state
            .lock()
            .update(true, VIRTIO_MMIO_INTERRUPT_STATUS_USED_BUFFER);
        let pulses = target.pulses.load(Ordering::SeqCst);

        state.reset_interrupt_state();

        assert_eq!(state.read_interrupt_status(), 0);
        assert_eq!(target.pulses.load(Ordering::SeqCst), pulses);
        assert!(!target.high.load(Ordering::SeqCst));
    }

    #[test]
    fn shared_status_teardown_clears_pending_state() {
        let guest_memory = GuestMemory::allocate(0x1000);
        let target = Arc::new(CountingInterruptTarget::default());
        let transport = MmioTransport {
            fixed_mmio_region: ("test", 0..=0xfff),
            device_id: 0,
            vendor_id: 0,
            interrupt_state: Arc::new(Mutex::new(shared_interrupt_state(
                &guest_memory,
                target.clone(),
            ))),
            #[cfg(target_os = "linux")]
            _interrupt_ack_task: None,
            #[cfg(target_os = "linux")]
            _interrupt_ack_doorbell: None,
            #[cfg(target_os = "linux")]
            interrupt_ack_event: None,
        };
        transport
            .interrupt_state
            .lock()
            .update(true, VIRTIO_MMIO_INTERRUPT_STATUS_USED_BUFFER);

        drop(transport);

        assert_eq!(guest_memory.read_plain::<u32>(0).unwrap(), 0);
        assert!(!target.high.load(Ordering::SeqCst));
    }
}
