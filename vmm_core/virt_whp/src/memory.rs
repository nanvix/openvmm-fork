// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

pub mod vtl2_mapper;

use super::VtlPartition;
use super::WhpPartition;
use super::WhpPartitionAndVtl;
use anyhow::Context;
use hvdef::HV_PAGE_SIZE;
use inspect::Inspect;
use memory_range::MemoryRange;
use parking_lot::Mutex;
use sparse_mmap::alloc::SharedMem;
use std::fmt::Debug;
use std::os::windows::prelude::*;
use std::sync::Arc;
use virt::PageVisibility;

#[derive(Debug, Inspect)]
pub(super) struct MappedRange {
    range: MemoryRange,
    writable: bool,
    exec: bool,
}

/// Trait implemented by underlying partition implementations for mapping and
/// unmapping ranges.
pub trait SimpleMemoryMap: Send + Sync {
    unsafe fn map_range(
        &self,
        process: Option<BorrowedHandle<'_>>,
        data: *mut u8,
        size: usize,
        addr: u64,
        writable: bool,
        exec: bool,
    ) -> anyhow::Result<()>;

    fn unmap_range(&self, addr: u64, size: u64) -> anyhow::Result<()>;
}

impl SimpleMemoryMap for whp::Partition {
    unsafe fn map_range(
        &self,
        process: Option<BorrowedHandle<'_>>,
        data: *mut u8,
        size: usize,
        addr: u64,
        writable: bool,
        exec: bool,
    ) -> anyhow::Result<()> {
        tracing::trace!(addr, size, ?data, writable, exec, "map range");
        let mut flags = whp::abi::WHvMapGpaRangeFlagRead;
        if writable {
            flags |= whp::abi::WHvMapGpaRangeFlagWrite;
        }
        if exec {
            flags |= whp::abi::WHvMapGpaRangeFlagExecute;
        }

        // SAFETY: Caller is required to pass valid parameters
        unsafe {
            whp::Partition::map_range(self, process, data, size, addr, flags)
                .context("whp map_range failed")
        }
    }

    fn unmap_range(&self, addr: u64, size: u64) -> anyhow::Result<()> {
        tracing::trace!(addr, size, "unmap range");
        whp::Partition::unmap_range(self, addr, size).context("whp unmap_range failed")
    }
}

/// Trait for different partition memory mapper implementations.
pub(crate) trait MemoryMapper: Inspect + Send + Sync {
    /// Map a range into the partition.
    ///
    /// # Safety
    /// The caller must guarantee that `data` is a valid pointer to access of
    /// the given `size` until [`Self::unmap_range`] is called.
    unsafe fn map_range(
        &self,
        partition: &dyn SimpleMemoryMap,
        process: Option<BorrowedHandle<'_>>,
        data: *mut u8,
        size: usize,
        addr: u64,
        writable: bool,
        exec: bool,
    ) -> anyhow::Result<()>;

    /// Unmap a given range from the partition.
    ///
    /// `addr` and `size` should only ever cover full ranges. This means that
    /// only full ranges from `map_range` should become unmapped, not partial
    /// ranges.
    fn unmap_range(
        &self,
        partition: &dyn SimpleMemoryMap,
        addr: u64,
        size: u64,
    ) -> anyhow::Result<()>;

    /// If overlays are supported in this partition mapper or not.
    fn overlays_supported(&self) -> bool;

    /// Add an overlay page.
    ///
    /// # Panics
    /// Panics if overlays are not supported.
    fn add_overlay_page(
        &self,
        partition: &dyn SimpleMemoryMap,
        gpa: u64,
        mem: Arc<SharedMem>,
        writable: bool,
        executable: bool,
    ) -> bool;

    /// Remove an overlay page.
    ///
    /// # Panics
    /// Panics if overlays are not supported.
    fn remove_overlay_page(&self, partition: &dyn SimpleMemoryMap, gpa: u64);

    /// Add an allowed range, which allows [`Self::map_range`] calls to map
    /// rather than defer the mapping, if the mapper is in a deferred state.
    fn add_allowed_range(&self, range: MemoryRange);

    /// Returns true if a `gpa` is in a deferred range.
    #[cfg_attr(guest_arch = "aarch64", expect(dead_code))]
    fn in_deferred_range(&self, gpa: u64) -> bool;

    /// Registers the deferred range containing `gpa`, if any.
    fn map_deferred_on_fault(
        &self,
        partition: &dyn SimpleMemoryMap,
        gpa: u64,
    ) -> anyhow::Result<bool>;

    /// Map all deferred ranges and put the mapper into a mapped state, where
    /// future map calls are no longer deferred.
    fn map_deferred(&self, partition: &dyn SimpleMemoryMap) -> anyhow::Result<()>;

    /// Apply the specified VTL protection to the specified `addr`, `size`.
    ///
    /// These protections are removed on [`Self::reset_mappings`].
    fn apply_vtl_protection(
        &self,
        partition: &dyn SimpleMemoryMap,
        addr: u64,
        size: u64,
        access: VtlAccess,
    ) -> anyhow::Result<()>;

    /// Return this partition to the reset state. If this partition was
    /// previously in a deferred mapping state at start, each range will become
    /// unmapped and deferred until mapped again, unless previously allowed.
    ///
    /// Additionally, VTL protections are removed.
    fn reset_mappings(&self, partition: &dyn SimpleMemoryMap) -> anyhow::Result<()>;

    /// If page acceptance and visibility is supported by this mapper.
    #[cfg_attr(guest_arch = "aarch64", expect(dead_code))]
    fn page_acceptance_supported(&self) -> bool;

    /// The page visibility for a given page. None means this page is not
    /// accepted. Only supported on mappers that support page acceptance.
    fn gpa_visibility(&self, gpa: u64) -> Option<PageVisibility>;

    /// Accept the given gpa range on behalf of the guest, with the given
    /// visibility. This page must be backed by ram.
    ///
    /// An error is returned if any of the given range was already accepted, or
    /// if the page is not backed by ram.
    ///
    /// TODO: Page visibility is currently not enforced with host virtstack
    /// components. Since the hypervisor is not providing isolation, this is a
    /// best-effort emulation only.
    fn accept_range(
        &self,
        partition: &dyn SimpleMemoryMap,
        range: &MemoryRange,
        visibility: PageVisibility,
    ) -> anyhow::Result<()>;

    /// Modify the visibility of an accepted range.
    ///
    /// TODO: This flow follows VBS where the range does not need to be
    /// unaccepted first before modifying visibility.
    fn modify_visibility(
        &self,
        partition: &dyn SimpleMemoryMap,
        range: &MemoryRange,
        visibility: PageVisibility,
    ) -> anyhow::Result<()>;
}

/// Memory mapper implementation that does not support VTLs, but does support
/// overlays and optional on-demand GPA registration.
#[derive(Debug, Inspect)]
pub(crate) struct WhpMemoryMapper {
    state: Option<Mutex<EmulatedOverlayState>>,
    overlays_supported: bool,
}

impl WhpMemoryMapper {
    pub(crate) fn new(with_overlays: bool, lazy_registration: bool) -> Self {
        Self {
            state: (with_overlays || lazy_registration)
                .then(|| Mutex::new(EmulatedOverlayState::new(lazy_registration))),
            overlays_supported: with_overlays,
        }
    }
}

impl MemoryMapper for WhpMemoryMapper {
    unsafe fn map_range(
        &self,
        partition: &dyn SimpleMemoryMap,
        process: Option<BorrowedHandle<'_>>,
        data: *mut u8,
        size: usize,
        addr: u64,
        writable: bool,
        exec: bool,
    ) -> anyhow::Result<()> {
        if let Some(state) = self.state.as_ref() {
            if process.is_some() {
                todo!();
            }
            let mapping = Mapping {
                range: MemoryRange::new(addr..addr + size as u64),
                data,
                writable,
                executable: exec,
            };
            state.lock().map_range(partition, mapping)?;
        } else {
            // SAFETY: caller guarantees `data` is a valid pointer
            // describing `size` bytes until this range is unmapped.
            unsafe { partition.map_range(process, data, size, addr, writable, exec)? };
        }

        Ok(())
    }

    fn unmap_range(
        &self,
        partition: &dyn SimpleMemoryMap,
        addr: u64,
        size: u64,
    ) -> anyhow::Result<()> {
        if let Some(state) = self.state.as_ref() {
            state.lock().unmap_range(partition, addr, size);
            Ok(())
        } else {
            Ok(partition.unmap_range(addr, size)?)
        }
    }

    fn overlays_supported(&self) -> bool {
        self.overlays_supported
    }

    fn add_overlay_page(
        &self,
        partition: &dyn SimpleMemoryMap,
        gpa: u64,
        mem: Arc<SharedMem>,
        writable: bool,
        executable: bool,
    ) -> bool {
        assert!(
            self.overlays_supported,
            "cannot add overlays if not supported"
        );
        self.state
            .as_ref()
            .expect("cannot add overlays if not supported")
            .lock()
            .add_overlay_page(
                partition,
                Overlay {
                    gpa,
                    mem,
                    writable,
                    executable,
                },
            )
    }

    fn remove_overlay_page(&self, partition: &dyn SimpleMemoryMap, gpa: u64) {
        assert!(
            self.overlays_supported,
            "cannot remove overlays if not supported"
        );
        self.state
            .as_ref()
            .expect("cannot remove overlays if not supported")
            .lock()
            .remove_overlay_page(partition, gpa)
    }

    fn add_allowed_range(&self, _range: MemoryRange) {}

    fn in_deferred_range(&self, _gpa: u64) -> bool {
        false
    }

    fn map_deferred_on_fault(
        &self,
        partition: &dyn SimpleMemoryMap,
        gpa: u64,
    ) -> anyhow::Result<bool> {
        match self.state.as_ref() {
            Some(state) => state.lock().map_deferred_on_fault(partition, gpa),
            None => Ok(false),
        }
    }

    fn map_deferred(&self, _partition: &dyn SimpleMemoryMap) -> anyhow::Result<()> {
        Ok(())
    }

    fn apply_vtl_protection(
        &self,
        _partition: &dyn SimpleMemoryMap,
        _addr: u64,
        _size: u64,
        _access: VtlAccess,
    ) -> anyhow::Result<()> {
        unimplemented!()
    }

    fn reset_mappings(&self, _partition: &dyn SimpleMemoryMap) -> anyhow::Result<()> {
        Ok(())
    }

    fn page_acceptance_supported(&self) -> bool {
        false
    }

    fn gpa_visibility(&self, _gpa: u64) -> Option<PageVisibility> {
        unimplemented!()
    }

    fn accept_range(
        &self,
        _partition: &dyn SimpleMemoryMap,
        _range: &MemoryRange,
        _visibility: PageVisibility,
    ) -> anyhow::Result<()> {
        unimplemented!()
    }

    fn modify_visibility(
        &self,
        _partition: &dyn SimpleMemoryMap,
        _range: &MemoryRange,
        _visibility: PageVisibility,
    ) -> anyhow::Result<()> {
        unimplemented!()
    }
}

/// What VTL access to apply to a page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Inspect)]
pub enum VtlAccess {
    /// No access
    NoAccess,
    /// Read only
    ReadOnly,
    /// Restore full access
    FullAccess,
}

#[derive(thiserror::Error, Debug)]
#[error("invalid HvMapGpaFlags {0} to VtlAccess conversion")]
pub struct InvalidMapGpaFlags(u32);

impl TryFrom<hvdef::HvMapGpaFlags> for VtlAccess {
    type Error = InvalidMapGpaFlags;

    fn try_from(value: hvdef::HvMapGpaFlags) -> Result<Self, Self::Error> {
        const HV_MAP_GPA_READ_ONLY: hvdef::HvMapGpaFlags =
            hvdef::HvMapGpaFlags::new().with_readable(true);

        // TODO: Only support HV_MAP_GPA_PERMISSIONS_NONE, read, or all.
        match value {
            hvdef::HV_MAP_GPA_PERMISSIONS_NONE => Ok(VtlAccess::NoAccess),
            HV_MAP_GPA_READ_ONLY => Ok(VtlAccess::ReadOnly),
            hvdef::HV_MAP_GPA_PERMISSIONS_ALL => Ok(VtlAccess::FullAccess),
            _ => Err(InvalidMapGpaFlags(value.into())),
        }
    }
}

impl VtlPartition {
    unsafe fn map_range(
        &self,
        process: Option<BorrowedHandle<'_>>,
        data: *mut u8,
        size: usize,
        addr: u64,
        writable: bool,
        exec: bool,
    ) -> anyhow::Result<()> {
        let range = MemoryRange::new(addr..addr + size as u64);
        let mut ranges = self.ranges.write();
        assert!(
            ranges
                .iter()
                .all(|r| range.contains(&r.range) || !range.overlaps(&r.range))
        );

        // SAFETY: Caller must past valid arguments.
        unsafe {
            self.mapper
                .map_range(&self.whp, process, data, size, addr, writable, exec)?;
        }

        ranges.push(MappedRange {
            range,
            writable,
            exec,
        });

        Ok(())
    }

    pub fn map_deferred(&self) -> anyhow::Result<()> {
        self.mapper.map_deferred(&self.whp)
    }

    pub fn map_deferred_on_fault(&self, gpa: u64) -> anyhow::Result<bool> {
        self.mapper.map_deferred_on_fault(&self.whp, gpa)
    }

    fn unmap_range(&self, addr: u64, size: u64) -> anyhow::Result<()> {
        let range = MemoryRange::new(addr..addr + size);
        let mut ranges = self.ranges.write();

        self.mapper.unmap_range(&self.whp, addr, size)?;
        ranges.retain(|r| {
            assert!(range.contains(&r.range) || !range.overlaps(&r.range));
            !range.contains(&r.range)
        });
        Ok(())
    }

    pub fn apply_vtl_protection(
        &self,
        addr: u64,
        size: u64,
        access: VtlAccess,
    ) -> anyhow::Result<()> {
        self.mapper
            .apply_vtl_protection(&self.whp, addr, size, access)
    }

    pub fn reset_mappings(&self) -> anyhow::Result<()> {
        self.mapper.reset_mappings(&self.whp)
    }

    pub fn accept_pages(
        &self,
        range: &MemoryRange,
        visibility: PageVisibility,
    ) -> anyhow::Result<()> {
        self.mapper.accept_range(&self.whp, range, visibility)
    }

    pub fn gpa_visibility(&self, gpa: u64) -> Option<PageVisibility> {
        self.mapper.gpa_visibility(gpa)
    }

    pub fn modify_visibility(
        &self,
        range: &MemoryRange,
        visibility: PageVisibility,
    ) -> anyhow::Result<()> {
        self.mapper.modify_visibility(&self.whp, range, visibility)
    }
}

impl virt::PartitionMemoryMapper for WhpPartition {
    fn memory_mapper(&self, vtl: hvdef::Vtl) -> Arc<dyn virt::PartitionMemoryMap> {
        self.with_vtl(vtl).clone()
    }
}

impl virt::PartitionMemoryMap for WhpPartitionAndVtl {
    fn unmap_range(&self, addr: u64, size: u64) -> anyhow::Result<()> {
        self.vtlp().unmap_range(addr, size)
    }

    unsafe fn map_range(
        &self,
        data: *mut u8,
        size: usize,
        addr: u64,
        writable: bool,
        exec: bool,
    ) -> anyhow::Result<()> {
        // SAFETY: guaranteed by caller
        unsafe {
            self.vtlp()
                .map_range(None, data, size, addr, writable, exec)
        }
    }

    unsafe fn map_remote_range(
        &self,
        process: BorrowedHandle<'_>,
        data: *mut u8,
        size: usize,
        addr: u64,
        writable: bool,
        exec: bool,
    ) -> anyhow::Result<()> {
        // SAFETY: guaranteed by caller
        unsafe {
            self.vtlp()
                .map_range(Some(process), data, size, addr, writable, exec)
        }
    }

    fn prefetch_range(&self, addr: u64, size: u64) -> anyhow::Result<()> {
        self.vtlp().whp.populate_ranges(
            &[whp::abi::WHV_MEMORY_RANGE_ENTRY {
                GuestAddress: addr,
                SizeInBytes: size,
            }],
            whp::abi::WHvMemoryAccessWrite,
            Default::default(),
        )?;
        Ok(())
    }

    fn pin_range(&self, addr: u64, size: u64) -> anyhow::Result<()> {
        self.vtlp()
            .whp
            .pin_ranges(&[whp::abi::WHV_MEMORY_RANGE_ENTRY {
                GuestAddress: addr,
                SizeInBytes: size,
            }])?;
        Ok(())
    }
}

#[derive(Inspect)]
struct MappingInspect {
    #[inspect(hex)]
    length: u64,
    writable: bool,
    executable: bool,
}

#[derive(Clone, Copy, Debug)]
struct Mapping {
    range: MemoryRange,
    data: *mut u8,
    writable: bool,
    executable: bool,
}

unsafe impl Send for Mapping {}
unsafe impl Sync for Mapping {}

impl Mapping {
    fn as_inspect_kv(&self) -> (MemoryRange, MappingInspect) {
        let &Self {
            range,
            data: _,
            writable,
            executable,
        } = self;
        (
            range,
            MappingInspect {
                length: range.len(),
                writable,
                executable,
            },
        )
    }
}

#[derive(Debug)]
struct Overlay {
    gpa: u64,
    mem: Arc<SharedMem>,
    writable: bool,
    executable: bool,
}

impl Overlay {
    fn as_inspect_kv(&self) -> (MemoryRange, MappingInspect) {
        let &Self {
            gpa,
            mem: _,
            writable,
            executable,
        } = self;
        (
            MemoryRange::new(gpa..gpa + HV_PAGE_SIZE),
            MappingInspect {
                length: HV_PAGE_SIZE,
                writable,
                executable,
            },
        )
    }
}

/// Tracks active mappings, overlay pages, and optional on-demand GPA
/// registration. Without overlays or lazy registration, mapping requests pass
/// straight through to WHP.
#[derive(Debug, Inspect)]
pub(crate) struct EmulatedOverlayState {
    lazy_registration: bool,
    /// Active memory mappings. Non-overlapping, sorted by GPA.
    #[inspect(with = "|x| inspect::iter_by_key(x.iter().map(Mapping::as_inspect_kv))")]
    mappings: Vec<Mapping>,
    /// Active overlay pages. Non-overlapping, sorted by GPA.
    #[inspect(with = "|x| inspect::iter_by_key(x.iter().map(Overlay::as_inspect_kv))")]
    overlays: Vec<Overlay>,
    /// Underlying GPA ranges already registered with WHP.
    #[inspect(iter_by_index)]
    registered_ranges: Vec<MemoryRange>,
}

impl EmulatedOverlayState {
    // Bound synchronous restore registration to the smallest canonical guest
    // size, then amortize later registration in large-page-sized chunks.
    const INITIAL_REGISTRATION_BYTES: u64 = 64 * 1024 * 1024;
    const REGISTRATION_CHUNK_BYTES: u64 = 2 * 1024 * 1024;

    fn new(lazy_registration: bool) -> Self {
        Self {
            lazy_registration,
            mappings: Vec::new(),
            overlays: Vec::new(),
            registered_ranges: Vec::new(),
        }
    }

    fn map_range(&mut self, p: &dyn SimpleMemoryMap, mapping: Mapping) -> anyhow::Result<()> {
        let index = self
            .mappings
            .binary_search_by_key(&mapping.range.start(), |m| m.range.end() - 1)
            .unwrap_err();
        if let Some(old_mapping) = self.mappings.get(index) {
            assert!(old_mapping.range.start() >= mapping.range.end());
        }

        let registered_end = if self.lazy_registration {
            mapping
                .range
                .start()
                .saturating_add(Self::INITIAL_REGISTRATION_BYTES)
                .min(mapping.range.end())
        } else {
            mapping.range.end()
        };
        let registered = MemoryRange::new(mapping.range.start()..registered_end);
        self.map_underlay_range(p, &mapping, registered)?;
        if self.lazy_registration {
            self.record_registered_range(registered);
        }
        self.mappings.insert(index, mapping);
        Ok(())
    }

    fn map_underlay_range(
        &self,
        p: &dyn SimpleMemoryMap,
        mapping: &Mapping,
        range: MemoryRange,
    ) -> anyhow::Result<()> {
        let mut gpa = range.start();
        let end = range.end();
        while gpa < end {
            let o_index = self
                .overlays
                .binary_search_by_key(&gpa, |o| o.gpa)
                .unwrap_or_else(|e| e);
            let this_end = self
                .overlays
                .get(o_index)
                .map(|o| std::cmp::min(o.gpa, end))
                .unwrap_or(end);
            if this_end > gpa {
                // SAFETY: `range` is contained by `mapping.range`, whose data
                // pointer remains valid until the matching unmap.
                unsafe {
                    p.map_range(
                        None,
                        mapping
                            .data
                            .wrapping_add((gpa - mapping.range.start()) as usize),
                        (this_end - gpa) as usize,
                        gpa,
                        mapping.writable,
                        mapping.executable,
                    )?;
                }
            }

            if this_end == end {
                break;
            }
            // Skip the overlay page.
            gpa = this_end + HV_PAGE_SIZE;
        }
        Ok(())
    }

    fn unmap_range(&mut self, p: &dyn SimpleMemoryMap, gpa: u64, len: u64) {
        let range = MemoryRange::new(gpa..gpa + len);
        if range.is_empty() {
            return;
        }
        let start = self
            .mappings
            .binary_search_by_key(&range.start(), |m| m.range.end() - 1)
            .unwrap_err();

        let end = start
            + self.mappings[start..]
                .binary_search_by_key(&(range.end() - 1), |m| m.range.start())
                .unwrap_err();

        for mapping in self.mappings.drain(start..end) {
            assert!(range.contains(&mapping.range));
        }

        if self.lazy_registration {
            let registered_ranges = std::mem::take(&mut self.registered_ranges);
            for registered in registered_ranges {
                if range.contains(&registered) {
                    self.unmap_underlay_range(p, registered);
                } else {
                    assert!(!range.overlaps(&registered));
                    self.registered_ranges.push(registered);
                }
            }
            return;
        }

        self.unmap_underlay_range(p, range);
    }

    fn unmap_underlay_range(&self, p: &dyn SimpleMemoryMap, range: MemoryRange) {
        let mut gpa = range.start();
        let end = range.end();
        while gpa < end {
            let o_index = self
                .overlays
                .binary_search_by_key(&gpa, |o| o.gpa)
                .unwrap_or_else(|e| e);
            let this_end = self
                .overlays
                .get(o_index)
                .map(|o| std::cmp::min(o.gpa, end))
                .unwrap_or(end);

            if this_end > gpa {
                p.unmap_range(gpa, this_end - gpa)
                    .expect("cannot handle unmap failure");
            }

            if this_end == end {
                break;
            }
            // Skip the overlay page.
            gpa = this_end + HV_PAGE_SIZE;
        }
    }

    fn map_deferred_on_fault(&mut self, p: &dyn SimpleMemoryMap, gpa: u64) -> anyhow::Result<bool> {
        if !self.lazy_registration {
            return Ok(false);
        }
        let Some(mapping) = self
            .mappings
            .iter()
            .find(|mapping| mapping.range.contains_addr(gpa))
            .copied()
        else {
            return Ok(false);
        };
        if self
            .registered_ranges
            .iter()
            .any(|range| range.contains_addr(gpa))
        {
            return Ok(true);
        }

        let chunk_start = mapping.range.start()
            + (gpa - mapping.range.start()) / Self::REGISTRATION_CHUNK_BYTES
                * Self::REGISTRATION_CHUNK_BYTES;
        let chunk_end = chunk_start
            .saturating_add(Self::REGISTRATION_CHUNK_BYTES)
            .min(mapping.range.end());
        let range = MemoryRange::new(chunk_start..chunk_end);
        self.map_underlay_range(p, &mapping, range)?;
        self.record_registered_range(range);
        Ok(true)
    }

    fn record_registered_range(&mut self, range: MemoryRange) {
        assert!(!range.is_empty());
        let index = self
            .registered_ranges
            .partition_point(|registered| registered.start() < range.start());
        if let Some(previous) = index
            .checked_sub(1)
            .and_then(|previous| self.registered_ranges.get(previous))
        {
            assert!(previous.end() <= range.start());
        }
        if let Some(next) = self.registered_ranges.get(index) {
            assert!(range.end() <= next.start());
        }
        self.registered_ranges.insert(index, range);
    }

    fn add_overlay_page(&mut self, p: &dyn SimpleMemoryMap, overlay: Overlay) -> bool {
        let index = match self.overlays.binary_search_by_key(&overlay.gpa, |m| m.gpa) {
            Ok(_) => return false, // overlay already exists
            Err(index) => index,
        };

        // N.B. This may atomically replace part of an existing mapping (but not
        //      an existing overlay, which is checked above).
        unsafe {
            p.map_range(
                None,
                overlay.mem.as_ptr() as *mut u8,
                HV_PAGE_SIZE as usize,
                overlay.gpa,
                overlay.writable,
                overlay.executable,
            )
            .expect("cannot handle mapping failure");
        }

        self.overlays.insert(index, overlay);
        true
    }

    fn remove_overlay_page(&mut self, p: &dyn SimpleMemoryMap, gpa: u64) {
        let index = self
            .overlays
            .binary_search_by_key(&gpa, |m| m.gpa)
            .map_err(|_| panic!("overlay must be mapped: {gpa} {:?}", self.overlays))
            .unwrap();

        let _overlay = self.overlays.remove(index);

        // Remap the old page.
        let underlay_registered = !self.lazy_registration
            || self
                .registered_ranges
                .iter()
                .any(|range| range.contains_addr(gpa));
        let mut remapped = false;
        if underlay_registered {
            let mapping_index = self
                .mappings
                .binary_search_by_key(&gpa, |m| m.range.end() - 1)
                .unwrap_err();
            if let Some(mapping) = self.mappings.get(mapping_index)
                && gpa >= mapping.range.start()
                && gpa < mapping.range.end()
            {
                unsafe {
                    p.map_range(
                        None,
                        mapping
                            .data
                            .wrapping_add((gpa - mapping.range.start()) as usize),
                        HV_PAGE_SIZE as usize,
                        gpa,
                        mapping.writable,
                        mapping.executable,
                    )
                    .expect("cannot handle mapping failure");
                }

                remapped = true;
            }
        }

        if !remapped {
            p.unmap_range(gpa, HV_PAGE_SIZE)
                .expect("cannot handle unmap failure");
        }
    }
}

pub(crate) struct OverlayMapper<'a>(&'a VtlPartition);

impl<'a> OverlayMapper<'a> {
    pub fn new(partition: &'a VtlPartition) -> Self {
        Self(partition)
    }

    pub fn add_overlay_page(
        &mut self,
        gpa: u64,
        mem: Arc<SharedMem>,
        writable: bool,
        executable: bool,
    ) -> bool {
        self.0
            .mapper
            .add_overlay_page(&self.0.whp, gpa, mem, writable, executable)
    }

    pub fn remove_overlay_page(&mut self, gpa: u64) {
        self.0.mapper.remove_overlay_page(&self.0.whp, gpa)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sparse_mmap::alloc::Allocation;
    use test_with_tracing::test;

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct MapCall {
        range: MemoryRange,
        data: usize,
    }

    #[derive(Debug, Default)]
    struct TestPartition {
        maps: Mutex<Vec<MapCall>>,
        unmaps: Mutex<Vec<MemoryRange>>,
    }

    impl SimpleMemoryMap for TestPartition {
        unsafe fn map_range(
            &self,
            _process: Option<BorrowedHandle<'_>>,
            data: *mut u8,
            size: usize,
            addr: u64,
            _writable: bool,
            _exec: bool,
        ) -> anyhow::Result<()> {
            self.maps.lock().push(MapCall {
                range: MemoryRange::new(addr..addr + size as u64),
                data: data.addr(),
            });
            Ok(())
        }

        fn unmap_range(&self, addr: u64, size: u64) -> anyhow::Result<()> {
            self.unmaps.lock().push(MemoryRange::new(addr..addr + size));
            Ok(())
        }
    }

    #[test]
    fn lazy_registration_maps_a_bounded_prefix_and_fault_chunks() {
        const MIB: u64 = 1024 * 1024;
        let partition = TestPartition::default();
        let mapper = WhpMemoryMapper::new(false, true);
        let data = std::ptr::without_provenance_mut::<u8>(0x10000);

        // SAFETY: TestPartition only records the pointer and never dereferences it.
        unsafe {
            mapper
                .map_range(&partition, None, data, (512 * MIB) as usize, 0, true, true)
                .unwrap();
        }
        assert_eq!(
            *partition.maps.lock(),
            [MapCall {
                range: MemoryRange::new(0..64 * MIB),
                data: data.addr(),
            }]
        );

        assert!(mapper.map_deferred_on_fault(&partition, 257 * MIB).unwrap());
        assert!(
            mapper
                .map_deferred_on_fault(&partition, 257 * MIB + 4096)
                .unwrap()
        );
        assert_eq!(
            *partition.maps.lock(),
            [
                MapCall {
                    range: MemoryRange::new(0..64 * MIB),
                    data: data.addr(),
                },
                MapCall {
                    range: MemoryRange::new(256 * MIB..258 * MIB),
                    data: data.wrapping_add((256 * MIB) as usize).addr(),
                },
            ]
        );

        mapper.unmap_range(&partition, 0, 512 * MIB).unwrap();
        assert_eq!(
            *partition.unmaps.lock(),
            [
                MemoryRange::new(0..64 * MIB),
                MemoryRange::new(256 * MIB..258 * MIB),
            ]
        );
    }

    #[test]
    fn lazy_registration_restores_only_registered_overlay_underlays() {
        const MIB: u64 = 1024 * 1024;
        let partition = TestPartition::default();
        let mapper = WhpMemoryMapper::new(true, true);
        let data = std::ptr::without_provenance_mut::<u8>(0x10000);

        // SAFETY: TestPartition only records the pointer and never dereferences it.
        unsafe {
            mapper
                .map_range(&partition, None, data, (128 * MIB) as usize, 0, true, true)
                .unwrap();
        }

        let registered_gpa = MIB;
        let registered_overlay = Arc::new(SharedMem::new(
            Allocation::new(HV_PAGE_SIZE as usize).unwrap(),
        ));
        assert!(mapper.add_overlay_page(
            &partition,
            registered_gpa,
            registered_overlay,
            false,
            false,
        ));
        mapper.remove_overlay_page(&partition, registered_gpa);
        assert_eq!(
            partition.maps.lock().last().copied(),
            Some(MapCall {
                range: MemoryRange::new(registered_gpa..registered_gpa + HV_PAGE_SIZE),
                data: data.wrapping_add(registered_gpa as usize).addr(),
            })
        );

        let deferred_gpa = 100 * MIB;
        let deferred_overlay = Arc::new(SharedMem::new(
            Allocation::new(HV_PAGE_SIZE as usize).unwrap(),
        ));
        assert!(mapper.add_overlay_page(&partition, deferred_gpa, deferred_overlay, false, false,));
        mapper.remove_overlay_page(&partition, deferred_gpa);
        assert_eq!(
            partition.unmaps.lock().last().copied(),
            Some(MemoryRange::new(deferred_gpa..deferred_gpa + HV_PAGE_SIZE))
        );
    }
}

#[cfg(guest_arch = "x86_64")]
pub(crate) mod x86 {
    use super::VtlAccess;
    use crate::VtlPartition;
    use crate::WhpPartitionInner;
    use hvdef::HV_PAGE_SIZE;
    use hvdef::Vtl;
    use memory_range::MemoryRange;

    /// Different backing types for a given GPA.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum GpaBackingType {
        /// This gpa is a monitor page.
        MonitorPage,
        /// This gpa is ram, with the specified writable or not.
        Ram { writable: bool },
        /// This gpa is unmapped.
        Unmapped,
        /// This gpa is protected by a higher VTL, with the specified access
        /// permissions.
        VtlProtected(VtlAccess),
        /// This gpa is unaccepted. It might be backed by ram but not accepted or
        /// unmapped, but for isolated partitions a page in that state is always
        /// unaccepted.
        Unaccepted,
    }

    impl WhpPartitionInner {
        /// Get the backing type for a given GPA.
        pub(crate) fn gpa_backing_type(&self, vtl: Vtl, gpa: u64) -> GpaBackingType {
            if vtl == Vtl::Vtl0 {
                if let Some(access) = self.vtl2_emulation.as_ref().and_then(|emu| {
                    emu.protected_pages
                        .read()
                        .get(&(gpa / HV_PAGE_SIZE))
                        .cloned()
                }) {
                    return GpaBackingType::VtlProtected(access);
                }
            }

            // The monitor page is mapped read only, but the range list does not
            // reflect this.
            if self.monitor_page.gpa() == Some(gpa & !(HV_PAGE_SIZE - 1)) {
                GpaBackingType::MonitorPage
            } else {
                self.vtlp(vtl).gpa_backing_type(gpa)
            }
        }

        /// Returns whether all of `range` is backed uniformly as RAM (writable
        /// per `writable`) with no per-page exceptions inside it — no monitor
        /// page, no higher-VTL page protections, and (on isolated partitions) no
        /// unaccepted pages. Used to decide whether a fault can be resolved by
        /// mapping the whole range as one soft large page instead of 4 KB.
        ///
        /// This answers the question with a couple of range lookups rather than
        /// probing every page, since the backing is stored as ranges.
        pub(crate) fn is_uniform_ram(&self, vtl: Vtl, range: MemoryRange, writable: bool) -> bool {
            // The monitor page is a single RAM page mapped read-only so that
            // writes keep trapping; never fold it into a large page.
            if self
                .monitor_page
                .gpa()
                .is_some_and(|gpa| range.contains_addr(gpa))
            {
                return false;
            }

            // A higher VTL can protect individual pages within VTL0 RAM.
            if vtl == Vtl::Vtl0 {
                if let Some(emu) = self.vtl2_emulation.as_ref() {
                    let (start, end) = (range.start_4k_gpn(), range.end_4k_gpn());
                    if emu
                        .protected_pages
                        .read()
                        .iter()
                        .any(|(pages, _)| *pages.start() < end && start <= *pages.end())
                    {
                        return false;
                    }
                }
            }

            self.vtlp(vtl).is_uniform_ram(range, writable)
        }
    }

    impl VtlPartition {
        pub fn in_deferred_range(&self, gpa: u64) -> bool {
            self.mapper.in_deferred_range(gpa)
        }

        /// Returns whether all of `range` is covered by a single mapped range
        /// with the required writability and no page-acceptance requirement. See
        /// [`WhpPartitionInner::is_uniform_ram`].
        fn is_uniform_ram(&self, range: MemoryRange, writable: bool) -> bool {
            // Isolated partitions accept pages individually, so uniformity can't
            // be assumed across a large page; fall back to per-page population.
            if self.mapper.page_acceptance_supported() {
                return false;
            }

            // The whole range must sit within one mapped range with the required
            // writability. Adjacent same-writability ranges that happen not to be
            // merged conservatively fail this and fall back to 4 KB.
            self.ranges
                .read()
                .iter()
                .any(|r| r.writable == writable && r.range.contains(&range))
        }

        fn gpa_backing_type(&self, gpa: u64) -> GpaBackingType {
            let is_isolated = self.mapper.page_acceptance_supported();

            let backing_type = self
                .ranges
                .read()
                .iter()
                .find(|r| r.range.contains_addr(gpa))
                .map(|r| GpaBackingType::Ram {
                    writable: r.writable,
                })
                .unwrap_or(GpaBackingType::Unmapped);

            if is_isolated {
                // Check that this page is actually accepted. If it's not, then
                // the page is considered unaccepted regardless if it's backed
                // by ram or not.
                //
                // TODO: In the future, we should probably report the actual
                // page visibility type instead of just ram for accepted
                // pages.
                if self.mapper.gpa_visibility(gpa).is_some() {
                    backing_type
                } else {
                    GpaBackingType::Unaccepted
                }
            } else {
                backing_type
            }
        }
    }
}
