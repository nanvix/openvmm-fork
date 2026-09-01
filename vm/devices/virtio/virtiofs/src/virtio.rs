// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::VirtioFs;
use crate::profile::MICROVM_ATTACHMENT_ID;
use crate::profile::MICROVM_MOUNT_TAG;
use crate::profile::MICROVM_REQUEST_QUEUES;
use crate::profile::MicroVmVirtioFsProfile;
use crate::save_dormant_microvm_state;
use crate::saved_state::SavedState;
use crate::validate_dormant_microvm_state;
use crate::validate_microvm_state;
use crate::virtio_util::MAX_FUSE_REQUEST_BYTES;
use crate::virtio_util::VirtioPayloadReader;
use crate::virtio_util::VirtioPayloadWriter;
use anyhow::Context as _;
use futures::StreamExt;
use guestmem::GuestMemory;
use guestmem::MappedMemoryRegion;
use inspect::InspectMut;
use pal_async::wait::PolledWait;
use parking_lot::Mutex;
use std::io;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use task_control::AsyncRun;
use task_control::Cancelled;
use task_control::StopTask;
use task_control::TaskControl;
use virtio::DeviceQueueState;
use virtio::DeviceStateValidator;
use virtio::DeviceTraits;
use virtio::DeviceTraitsSharedMemory;
use virtio::QueueResources;
use virtio::VirtioDevice;
use virtio::VirtioQueue;
use virtio::VirtioQueueCallbackWork;
use virtio::queue::QueueState;
use virtio::spec::VirtioDeviceFeatures;
use vmcore::save_restore::RestoreError;
use vmcore::save_restore::SaveError;
use vmcore::save_restore::SavedStateBlob;
use vmcore::vm_task::VmTaskDriver;
use vmcore::vm_task::VmTaskDriverSource;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

/// Default request queue count when the caller does not specify one. Two
/// queues let the guest's FUSE-request hashing send concurrent operations
/// on different inodes to different host workers, which is the whole
/// point of having more than one queue, while keeping the per-device
/// footprint (MSI-X vectors, kernel worker threads, host tasks) modest.
///
/// Callers that know the appropriate concurrency for their environment
/// (e.g., guest vCPU count for an in-VMM device, or host parallelism for
/// a host service like `wsldevicehost`) should pass an explicit value via
/// [`VirtioFsDevice::with_num_request_queues`].
const DEFAULT_NUM_REQUEST_QUEUES: u32 = 2;

/// Upper bound for the request queue count. Past this, virtio-fs's
/// hash-based queue selection has diminishing returns, and each extra queue
/// costs a guest MSI-X vector plus a kernel worker thread.
const MAX_REQUEST_QUEUES: u32 = 8;

struct DormantMicrovmFs;

impl fuse::Fuse for DormantMicrovmFs {}

/// PCI configuration space values for virtio-fs devices.
#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout)]
struct VirtioFsDeviceConfig {
    tag: [u8; 36],
    num_request_queues: u32,
}

/// A virtio-fs PCI device.
#[derive(InspectMut)]
pub struct VirtioFsDevice {
    task_name: Box<str>,
    driver: VmTaskDriver,
    #[inspect(skip)]
    config: VirtioFsDeviceConfig,
    #[inspect(skip)]
    fs: Arc<fuse::Session>,
    #[inspect(skip)]
    workers: Vec<TaskControl<VirtioFsWorker, VirtioFsQueue>>,
    shmem_size: u64,
    #[inspect(skip)]
    shared_memory_region: Option<Arc<dyn MappedMemoryRegion>>,
    #[inspect(skip)]
    notify_corruption: Arc<dyn Fn() + Sync + Send>,
    num_request_queues: u32,
    #[inspect(skip)]
    microvm_attachment_id: Option<String>,
    #[inspect(skip)]
    microvm_profile: Option<MicroVmVirtioFsProfile>,
    #[inspect(skip)]
    stateful_fs: Option<VirtioFs>,
    #[inspect(skip)]
    admission: Arc<RequestAdmission>,
    #[inspect(skip)]
    save_error: Option<anyhow::Error>,
}

struct AdmissionState {
    accepting: bool,
    in_flight: u32,
}

struct RequestAdmission {
    state: Mutex<AdmissionState>,
}

struct RequestGuard {
    admission: Arc<RequestAdmission>,
}

impl RequestAdmission {
    fn new() -> Self {
        Self {
            state: Mutex::new(AdmissionState {
                accepting: true,
                in_flight: 0,
            }),
        }
    }

    fn accept(self: &Arc<Self>) -> Option<RequestGuard> {
        let mut state = self.state.lock();
        if !state.accepting {
            return None;
        }
        state.in_flight = state.in_flight.checked_add(1)?;
        Some(RequestGuard {
            admission: Arc::clone(self),
        })
    }

    fn quiesce(&self) {
        let mut state = self.state.lock();
        state.accepting = false;
    }

    fn resume(&self) {
        let mut state = self.state.lock();
        state.accepting = true;
    }

    fn verify_drained(&self) -> anyhow::Result<()> {
        let state = self.state.lock();
        anyhow::ensure!(
            !state.accepting,
            "virtio-fs save was not preceded by input quiesce"
        );
        anyhow::ensure!(
            state.in_flight == 0,
            "virtio-fs has {} unrepresented in-flight request(s)",
            state.in_flight
        );
        Ok(())
    }
}

impl Drop for RequestGuard {
    fn drop(&mut self) {
        let mut state = self.admission.state.lock();
        state.in_flight = state.in_flight.saturating_sub(1);
    }
}

impl VirtioFsDevice {
    /// Creates a new `VirtioFsDevice` with the specified mount tag.
    ///
    /// The number of FUSE request queues defaults to
    /// `DEFAULT_NUM_REQUEST_QUEUES`. Callers that know the appropriate
    /// concurrency for their environment (e.g., guest vCPU count for an
    /// in-VMM device, or host parallelism for a host service) should use
    /// [`Self::with_num_request_queues`] instead.
    pub fn new<Fs>(
        driver_source: &VmTaskDriverSource,
        tag: &str,
        fs: Fs,
        shmem_size: u64,
        notify_corruption: Option<Arc<dyn Fn() + Sync + Send>>,
    ) -> Self
    where
        Fs: 'static + fuse::Fuse + Send + Sync,
    {
        Self::with_num_request_queues(
            driver_source,
            tag,
            fs,
            shmem_size,
            notify_corruption,
            DEFAULT_NUM_REQUEST_QUEUES,
        )
    }

    /// Creates a new `VirtioFsDevice` with an explicit number of FUSE
    /// request queues. The value is clamped to `[1, MAX_REQUEST_QUEUES]`.
    pub fn with_num_request_queues<Fs>(
        driver_source: &VmTaskDriverSource,
        tag: &str,
        fs: Fs,
        shmem_size: u64,
        notify_corruption: Option<Arc<dyn Fn() + Sync + Send>>,
        num_request_queues: u32,
    ) -> Self
    where
        Fs: 'static + fuse::Fuse + Send + Sync,
    {
        let num_request_queues = num_request_queues.clamp(1, MAX_REQUEST_QUEUES);
        Self::from_parts(
            driver_source,
            tag,
            fs,
            shmem_size,
            notify_corruption,
            num_request_queues,
            None,
            None,
            None,
        )
    }

    /// Creates the fixed no-DAX microVM virtio-fs device.
    ///
    /// The filesystem must have been constructed with
    /// [`VirtioFs::new_microvm`], using this exact profile. This keeps the
    /// device shape and host-enforced filesystem policy inseparable.
    pub fn new_microvm(
        driver_source: &VmTaskDriverSource,
        profile: MicroVmVirtioFsProfile,
        fs: VirtioFs,
        notify_corruption: Option<Arc<dyn Fn() + Sync + Send>>,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            fs.microvm_profile() == Some(&profile),
            "microVM device profile does not match the HostFs attachment"
        );
        let attachment_id = profile.attachment_id().to_owned();
        Ok(Self::from_parts(
            driver_source,
            MICROVM_MOUNT_TAG,
            fs.clone(),
            0,
            notify_corruption,
            MICROVM_REQUEST_QUEUES,
            Some(attachment_id),
            Some(profile),
            Some(fs),
        ))
    }

    /// Creates the fixed no-DAX microVM virtio-fs device without a host attachment.
    pub fn new_microvm_dormant(
        driver_source: &VmTaskDriverSource,
        stable_id: String,
        notify_corruption: Option<Arc<dyn Fn() + Sync + Send>>,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            stable_id == MICROVM_ATTACHMENT_ID,
            "microVM virtio-fs attachment ID must be '{MICROVM_ATTACHMENT_ID}'"
        );
        Ok(Self::from_parts(
            driver_source,
            MICROVM_MOUNT_TAG,
            DormantMicrovmFs,
            0,
            notify_corruption,
            MICROVM_REQUEST_QUEUES,
            Some(stable_id),
            None,
            None,
        ))
    }

    /// Creates the fixed no-DAX microVM HostFs device directly from the
    /// `MicrovmV1` resource fields and its process-local host root.
    pub fn new_microvm_hostfs(
        driver_source: &VmTaskDriverSource,
        stable_id: String,
        root_identity: Vec<u8>,
        read_only: bool,
        root_path: impl AsRef<Path>,
        notify_corruption: Option<Arc<dyn Fn() + Sync + Send>>,
    ) -> anyhow::Result<Self> {
        let profile = MicroVmVirtioFsProfile::from_attachment(stable_id, root_identity, read_only)?;
        let fs = VirtioFs::new_microvm(root_path, profile.clone())?;
        Self::new_microvm(driver_source, profile, fs, notify_corruption)
    }

    fn from_parts<Fs>(
        driver_source: &VmTaskDriverSource,
        tag: &str,
        fs: Fs,
        shmem_size: u64,
        notify_corruption: Option<Arc<dyn Fn() + Sync + Send>>,
        num_request_queues: u32,
        microvm_attachment_id: Option<String>,
        microvm_profile: Option<MicroVmVirtioFsProfile>,
        stateful_fs: Option<VirtioFs>,
    ) -> Self
    where
        Fs: 'static + fuse::Fuse + Send + Sync,
    {
        let mut config = VirtioFsDeviceConfig {
            tag: [0; 36],
            num_request_queues,
        };

        let notify_corruption = if let Some(notify) = notify_corruption {
            notify
        } else {
            Arc::new(|| {})
        };

        // Copy the tag into the config space (truncate it for now if too long).
        let length = std::cmp::min(tag.len(), config.tag.len());
        config.tag[..length].copy_from_slice(&tag.as_bytes()[..length]);

        Self {
            task_name: format!("virtiofs-{}", tag).into(),
            driver: driver_source.simple(),
            config,
            fs: Arc::new(fuse::Session::new(fs)),
            workers: Vec::new(),
            shmem_size,
            shared_memory_region: None,
            notify_corruption,
            num_request_queues,
            microvm_attachment_id,
            microvm_profile,
            stateful_fs,
            admission: Arc::new(RequestAdmission::new()),
            save_error: None,
        }
    }
}

impl VirtioDevice for VirtioFsDevice {
    fn traits(&self) -> DeviceTraits {
        let mut device_features = VirtioDeviceFeatures::new()
            .with_ring_event_idx(true)
            .with_ring_indirect_desc(true);
        if self.microvm_attachment_id.is_none() {
            device_features = device_features.with_ring_packed(true);
        }
        DeviceTraits {
            device_id: virtio::spec::VirtioDeviceType::FS,
            device_features,
            max_queues: 1 + self.num_request_queues as u16,
            device_register_length: self.config.as_bytes().len() as u32,
            shared_memory: DeviceTraitsSharedMemory {
                id: 0,
                size: self.shmem_size,
            },
        }
    }

    async fn read_registers_u32(&mut self, offset: u16) -> u32 {
        let offset = offset as usize;
        let config = self.config.as_bytes();
        config
            .get(offset..offset.saturating_add(4))
            .and_then(|bytes| bytes.try_into().ok())
            .map(u32::from_le_bytes)
            .unwrap_or(0)
    }

    async fn write_registers_u32(&mut self, offset: u16, val: u32) {
        tracelimit::warn_ratelimited!(offset, val, "[virtiofs] Unknown write",);
    }

    fn set_shared_memory_region(
        &mut self,
        region: &Arc<dyn MappedMemoryRegion>,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.microvm_attachment_id.is_none(),
            "the microVM virtio-fs profile does not expose shared memory"
        );
        self.shared_memory_region = Some(region.clone());
        Ok(())
    }

    async fn start_queue(
        &mut self,
        idx: u16,
        resources: QueueResources,
        features: &VirtioDeviceFeatures,
        initial_state: Option<QueueState>,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.microvm_attachment_id.is_none() || !features.ring_packed(),
            "the microVM virtio-fs profile forbids packed virtqueues"
        );
        let mut tc = TaskControl::new(VirtioFsWorker {
            fs: self.fs.clone(),
            shared_memory_region: self.shared_memory_region.clone(),
            shared_memory_size: self.shmem_size,
            notify_corruption: self.notify_corruption.clone(),
            admission: Arc::clone(&self.admission),
        });

        let queue_event = PolledWait::new(&self.driver, resources.event)
            .context("failed to create polled wait")?;
        let queue = VirtioQueue::new(
            *features,
            resources.params,
            resources.guest_memory.clone(),
            resources.notify,
            queue_event,
            initial_state,
        )
        .context("failed to create virtio queue")?;

        tc.insert(
            self.driver.clone(),
            &*self.task_name,
            VirtioFsQueue {
                queue,
                mem: resources.guest_memory,
            },
        );
        tc.start();

        let idx = idx as usize;
        if idx >= self.workers.len() {
            self.workers.resize_with(idx + 1, || {
                TaskControl::new(VirtioFsWorker {
                    fs: self.fs.clone(),
                    shared_memory_region: None,
                    shared_memory_size: 0,
                    notify_corruption: self.notify_corruption.clone(),
                    admission: Arc::clone(&self.admission),
                })
            });
        }
        self.workers[idx] = tc;
        Ok(())
    }

    async fn stop_queue(&mut self, idx: u16) -> Option<QueueState> {
        let idx = idx as usize;
        if idx >= self.workers.len() || !self.workers[idx].has_state() {
            return None;
        }
        self.workers[idx].stop().await;
        let state = self.workers[idx].remove().queue.queue_state();
        Some(state)
    }

    async fn reset(&mut self) {
        self.workers.clear();
        self.admission.resume();
        self.save_error = None;
        if let Some(region) = &self.shared_memory_region {
            if let Err(e) = region.unmap(0, self.shmem_size as usize) {
                tracing::error!(
                    error = &e as &dyn std::error::Error,
                    "failed to unmap DAX region on reset"
                );
            }
        }
        self.shared_memory_region = None;
        self.fs.destroy();
    }

    async fn quiesce_input(&mut self) -> anyhow::Result<()> {
        if self.microvm_attachment_id.is_some() {
            self.admission.quiesce();
        }
        Ok(())
    }

    async fn resume_input(&mut self) -> anyhow::Result<()> {
        if self.microvm_attachment_id.is_some() {
            self.admission.resume();
            self.save_error = None;
        }
        Ok(())
    }

    fn supports_save_restore(&self) -> bool {
        self.microvm_attachment_id.is_some()
    }

    fn save_device(&mut self) -> Result<Option<SavedStateBlob>, SaveError> {
        let Some(attachment_id) = self.microvm_attachment_id.as_deref() else {
            return Err(SaveError::NotSupported);
        };
        if let Some(error) = self.save_error.take() {
            return Err(SaveError::Other(error));
        }
        if self.workers.iter().any(TaskControl::has_state) {
            return Err(SaveError::Other(anyhow::anyhow!(
                "virtio-fs queues are still running"
            )));
        }
        self.admission.verify_drained().map_err(SaveError::Other)?;
        let state = match (&self.microvm_profile, &self.stateful_fs) {
            (Some(profile), Some(fs)) => fs
                .save_microvm_state(profile, self.fs.save_state())
                .map_err(SaveError::Other)?,
            (None, None) => save_dormant_microvm_state(attachment_id, self.fs.save_state())
                .map_err(SaveError::Other)?,
            _ => {
                return Err(SaveError::Other(anyhow::anyhow!(
                    "microVM virtio-fs profile and attachment state disagree"
                )));
            }
        };
        Ok(Some(SavedStateBlob::new(state)))
    }

    fn restore_device(&mut self, state: Option<SavedStateBlob>) -> Result<(), RestoreError> {
        let attachment_id = self
            .microvm_attachment_id
            .as_deref()
            .ok_or(RestoreError::SavedStateNotSupported)?;
        let state = state.ok_or_else(|| {
            RestoreError::InvalidSavedState(anyhow::anyhow!(
                "microVM virtio-fs is missing device-private saved state"
            ))
        })?;
        let state: SavedState = state.parse().map_err(RestoreError::ProtobufDecode)?;
        if state.dormant {
            let session_state = validate_dormant_microvm_state(&state, attachment_id)
                .map_err(RestoreError::InvalidSavedState)?;
            return self
                .fs
                .restore_state(session_state)
                .map_err(|error| RestoreError::InvalidSavedState(error.into()));
        }
        let profile = self.microvm_profile.as_ref().ok_or_else(|| {
            RestoreError::InvalidSavedState(anyhow::anyhow!(
                "active microVM virtio-fs state requires a host attachment"
            ))
        })?;
        validate_microvm_state(&state, profile).map_err(RestoreError::InvalidSavedState)?;
        let fs = self.stateful_fs.as_ref().ok_or_else(|| {
            RestoreError::InvalidSavedState(anyhow::anyhow!(
                "microVM virtio-fs filesystem attachment is unavailable"
            ))
        })?;
        fs.restore_microvm_state(profile, state, &self.fs)
            .map_err(RestoreError::InvalidSavedState)
    }

    fn device_state_validator(&self) -> DeviceStateValidator {
        let attachment_id = self.microvm_attachment_id.clone();
        let profile = self.microvm_profile.clone();
        Box::new(
            move |state, features, queues: &[DeviceQueueState], _guest_memory| {
                let attachment_id = attachment_id
                    .as_deref()
                    .ok_or(RestoreError::SavedStateNotSupported)?;
                if features.ring_packed() {
                    return Err(RestoreError::InvalidSavedState(anyhow::anyhow!(
                        "microVM virtio-fs restored packed virtqueue"
                    )));
                }
                if queues.len() != 2 {
                    return Err(RestoreError::InvalidSavedState(anyhow::anyhow!(
                        "microVM virtio-fs requires one hiprio queue and one request queue"
                    )));
                }
                let state = state.ok_or_else(|| {
                    RestoreError::InvalidSavedState(anyhow::anyhow!(
                        "microVM virtio-fs is missing device-private saved state"
                    ))
                })?;
                let state: SavedState = state.parse().map_err(RestoreError::ProtobufDecode)?;
                if state.dormant {
                    validate_dormant_microvm_state(&state, attachment_id)
                        .map(|_| ())
                        .map_err(RestoreError::InvalidSavedState)
                } else {
                    let profile = profile.as_ref().ok_or_else(|| {
                        RestoreError::InvalidSavedState(anyhow::anyhow!(
                            "active microVM virtio-fs state requires a host attachment"
                        ))
                    })?;
                    validate_microvm_state(&state, profile).map_err(RestoreError::InvalidSavedState)
                }
            },
        )
    }
}

struct VirtioFsWorker {
    fs: Arc<fuse::Session>,
    shared_memory_region: Option<Arc<dyn MappedMemoryRegion>>,
    shared_memory_size: u64,
    notify_corruption: Arc<dyn Fn() + Sync + Send>,
    admission: Arc<RequestAdmission>,
}

struct VirtioFsQueue {
    queue: VirtioQueue,
    mem: GuestMemory,
}

impl AsyncRun<VirtioFsQueue> for VirtioFsWorker {
    async fn run(
        &mut self,
        stop: &mut StopTask<'_>,
        state: &mut VirtioFsQueue,
    ) -> Result<(), Cancelled> {
        loop {
            // Admission must precede queue.next(): after save closes the
            // gate, an unowned descriptor must remain on the virtqueue rather
            // than being dequeued and falsely completed with an empty reply.
            let Some(request_guard) = self.admission.accept() else {
                break;
            };
            let work = stop.until_stopped(state.queue.next()).await?;
            let Some(work) = work else { break };
            match work {
                Ok(work) => {
                    let bytes = process_virtiofs_request(self, &state.mem, &work, request_guard);
                    state.queue.complete(work, bytes);
                }
                Err(err) => {
                    tracelimit::error_ratelimited!(
                        error = &err as &dyn std::error::Error,
                        "Failed processing queue"
                    );
                    break;
                }
            }
        }
        Ok(())
    }
}

fn process_virtiofs_request(
    worker: &VirtioFsWorker,
    mem: &GuestMemory,
    work: &VirtioQueueCallbackWork,
    _request_guard: RequestGuard,
) -> u32 {
    let readable_len = work.get_payload_length(false) as usize;
    if readable_len > MAX_FUSE_REQUEST_BYTES {
        tracelimit::error_ratelimited!(
            readable_len,
            max_request_bytes = MAX_FUSE_REQUEST_BYTES,
            "virtio-fs request exceeds the fixed maximum size"
        );
        (worker.notify_corruption)();
        return 0;
    }

    // Parse the request.
    let reader = VirtioPayloadReader::new(mem, work);
    let request = match fuse::Request::new(reader) {
        Ok(request) => request,
        Err(e) => {
            tracelimit::error_ratelimited!(
                error = &e as &dyn std::error::Error,
                "[virtiofs] Invalid FUSE message, error"
            );
            // Often this will result in the guest failing the device as there is no response to a request.
            (worker.notify_corruption)();
            // This only happens if even the header couldn't be parsed, so there's no way
            // to send an error reply since the request's unique ID isn't known.
            return 0;
        }
    };

    // Dispatch to the file system. The sender writes the reply into guest
    // memory but does not complete the descriptor—completion happens once,
    // after dispatch returns. For FUSE no-reply operations (Forget,
    // BatchForget, Destroy), send() is never called and bytes_written
    // stays 0.
    let mut sender = VirtioReplySender {
        work,
        mem,
        bytes_written: 0,
    };
    let mapper = worker
        .shared_memory_region
        .as_ref()
        .map(|shared_memory_region| VirtioMapper {
            region: shared_memory_region.as_ref(),
            size: worker.shared_memory_size,
        });
    worker.fs.dispatch(
        request,
        &mut sender,
        mapper.as_ref().map(|x| x as &dyn fuse::Mapper),
    );
    sender.bytes_written
}
/// An implementation of `ReplySender` for virtio payload.
///
/// Writes the FUSE reply into guest memory and records the byte count.
/// Does not complete the descriptor—the caller is responsible for that.
struct VirtioReplySender<'a> {
    work: &'a VirtioQueueCallbackWork,
    mem: &'a GuestMemory,
    bytes_written: u32,
}

impl fuse::ReplySender for VirtioReplySender<'_> {
    fn send(&mut self, bufs: &[io::IoSlice<'_>]) -> io::Result<()> {
        let mut writer = VirtioPayloadWriter::new(self.mem, self.work);
        let mut size: usize = 0;

        // Write all the slices to the payload buffers.
        // N.B. write_vectored isn't used because it isn't guaranteed to write all the data.
        for buf in bufs {
            writer.write_all(buf)?;
            size = size
                .checked_add(buf.len())
                .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidData))?;
        }

        self.bytes_written = size
            .try_into()
            .map_err(|_| io::Error::from(io::ErrorKind::InvalidData))?;
        Ok(())
    }
}

struct VirtioMapper<'a> {
    region: &'a dyn MappedMemoryRegion,
    size: u64,
}

impl fuse::Mapper for VirtioMapper<'_> {
    fn map(
        &self,
        offset: u64,
        file: fuse::FileRef<'_>,
        file_offset: u64,
        len: u64,
        writable: bool,
    ) -> lx::Result<()> {
        let offset = offset.try_into().map_err(|_| lx::Error::EINVAL)?;
        let len = len.try_into().map_err(|_| lx::Error::EINVAL)?;
        self.region.map(offset, &file, file_offset, len, writable)?;
        Ok(())
    }

    fn unmap(&self, offset: u64, len: u64) -> lx::Result<()> {
        let offset = offset.try_into().map_err(|_| lx::Error::EINVAL)?;
        let len = len.try_into().map_err(|_| lx::Error::EINVAL)?;
        self.region.unmap(offset, len)?;
        Ok(())
    }

    fn clear(&self) {
        let result = self.region.unmap(0, self.size as usize);
        if let Err(result) = result {
            tracing::error!(
                error = &result as &dyn std::error::Error,
                "Failed to unmap shared memory"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::VirtioFs;
    use crate::profile::MICROVM_ATTACHMENT_ID;
    use crate::profile::MicroVmVirtioFsProfile;
    use crate::profile::microvm_root_identity;
    use pal_async::DefaultDriver;
    use pal_async::async_test;
    use test_with_tracing::test;
    use vmcore::vm_task::SingleDriverBackend;

    fn make_device(
        driver: &DefaultDriver,
        num_request_queues: Option<u32>,
    ) -> (VirtioFsDevice, tempfile::TempDir) {
        let tmpdir = tempfile::tempdir().unwrap();
        let fs = VirtioFs::new(tmpdir.path(), None).unwrap();
        let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone()));
        let device = match num_request_queues {
            Some(n) => {
                VirtioFsDevice::with_num_request_queues(&driver_source, "testfs", fs, 0, None, n)
            }
            None => VirtioFsDevice::new(&driver_source, "testfs", fs, 0, None),
        };
        (device, tmpdir)
    }

    #[async_test]
    async fn new_uses_default_num_request_queues(driver: DefaultDriver) {
        let (device, _tmp) = make_device(&driver, None);
        assert_eq!(device.num_request_queues, DEFAULT_NUM_REQUEST_QUEUES);
        assert_eq!(device.config.num_request_queues, DEFAULT_NUM_REQUEST_QUEUES);
        assert_eq!(
            device.traits().max_queues,
            1 + DEFAULT_NUM_REQUEST_QUEUES as u16
        );
    }

    #[async_test]
    async fn with_num_request_queues_clamps_above_max(driver: DefaultDriver) {
        let (device, _tmp) = make_device(&driver, Some(1000));
        assert_eq!(device.num_request_queues, MAX_REQUEST_QUEUES);
        assert_eq!(device.config.num_request_queues, MAX_REQUEST_QUEUES);
        assert_eq!(device.traits().max_queues, 1 + MAX_REQUEST_QUEUES as u16);
    }

    #[async_test]
    async fn with_num_request_queues_clamps_below_one(driver: DefaultDriver) {
        // A request for zero queues must be clamped up to one so the device
        // always exposes at least one request virtqueue.
        let (device, _tmp) = make_device(&driver, Some(0));
        assert_eq!(device.num_request_queues, 1);
        assert_eq!(device.config.num_request_queues, 1);
        // 1 hiprio queue + 1 request queue.
        assert_eq!(device.traits().max_queues, 2);
    }

    #[async_test]
    async fn with_num_request_queues_accepts_value_in_range(driver: DefaultDriver) {
        let (device, _tmp) = make_device(&driver, Some(3));
        assert_eq!(device.num_request_queues, 3);
        assert_eq!(device.config.num_request_queues, 3);
        assert_eq!(device.traits().max_queues, 4);
    }

    #[async_test]
    async fn microvm_profile_has_fixed_device_shape(driver: DefaultDriver) {
        let temporary_directory = tempfile::tempdir().unwrap();
        let root_identity = microvm_root_identity(temporary_directory.path()).unwrap();
        let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(driver));
        let device = VirtioFsDevice::new_microvm_hostfs(
            &driver_source,
            MICROVM_ATTACHMENT_ID.to_owned(),
            root_identity,
            true,
            temporary_directory.path(),
            None,
        )
        .unwrap();

        assert_eq!(device.config.num_request_queues, MICROVM_REQUEST_QUEUES);
        assert_eq!(device.traits().max_queues, 2);
        assert_eq!(device.traits().shared_memory.size, 0);
        assert!(!device.traits().device_features.ring_packed());
        assert!(device.supports_save_restore());
        assert_eq!(&device.config.tag[..7], b"microvm");
    }

    #[async_test]
    async fn microvm_dormant_state_restores_into_active_attachment(driver: DefaultDriver) {
        let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(driver));
        let mut source = VirtioFsDevice::new_microvm_dormant(
            &driver_source,
            MICROVM_ATTACHMENT_ID.to_owned(),
            None,
        )
        .unwrap();
        source.quiesce_input().await.unwrap();
        let state = source.save_device().unwrap().unwrap();

        let root = tempfile::tempdir().unwrap();
        let mut destination = VirtioFsDevice::new_microvm_hostfs(
            &driver_source,
            MICROVM_ATTACHMENT_ID.to_owned(),
            microvm_root_identity(root.path()).unwrap(),
            false,
            root.path(),
            None,
        )
        .unwrap();
        destination.restore_device(Some(state)).unwrap();
    }

    #[async_test]
    async fn microvm_active_state_rejects_dormant_destination(driver: DefaultDriver) {
        let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(driver));
        let root = tempfile::tempdir().unwrap();
        let mut source = VirtioFsDevice::new_microvm_hostfs(
            &driver_source,
            MICROVM_ATTACHMENT_ID.to_owned(),
            microvm_root_identity(root.path()).unwrap(),
            false,
            root.path(),
            None,
        )
        .unwrap();
        source.quiesce_input().await.unwrap();
        let state = source.save_device().unwrap().unwrap();

        let mut destination = VirtioFsDevice::new_microvm_dormant(
            &driver_source,
            MICROVM_ATTACHMENT_ID.to_owned(),
            None,
        )
        .unwrap();
        assert!(destination.restore_device(Some(state)).is_err());
    }

    #[async_test]
    async fn microvm_device_rejects_a_non_profile_attachment(driver: DefaultDriver) {
        let temporary_directory = tempfile::tempdir().unwrap();
        let profile = MicroVmVirtioFsProfile::from_attachment(
            MICROVM_ATTACHMENT_ID.to_owned(),
            microvm_root_identity(temporary_directory.path()).unwrap(),
            true,
        )
        .unwrap();
        let fs = VirtioFs::new(temporary_directory.path(), None).unwrap();
        let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(driver));

        assert!(VirtioFsDevice::new_microvm(&driver_source, profile, fs, None).is_err());
    }

    #[test]
    fn closed_admission_rejects_new_work_until_all_requests_drain() {
        let admission = Arc::new(RequestAdmission::new());
        let request = admission.accept().unwrap();
        admission.quiesce();
        assert!(admission.accept().is_none());
        assert!(admission.verify_drained().is_err());

        drop(request);
        admission.verify_drained().unwrap();
    }
}
