// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(missing_docs)]
#![cfg(any(windows, target_os = "linux"))]

mod aggregate;
mod file;
mod inode;
#[cfg(test)]
mod integration_tests;
pub mod profile;
pub mod resolver;
mod saved_state;
#[cfg(windows)]
mod section;
mod util;
pub mod virtio;
mod virtio_util;

#[cfg(windows)]
pub use section::SectionFs;

use aggregate::AggregateState;
use aggregate::SYNTHETIC_ROOT_FH;
use anyhow::Context;
use file::VirtioFsFile;
use fuse::protocol::*;
use fuse::*;
use inode::DedupKey;
use inode::VirtioFsInode;
use inode::VirtioFsVolume;
pub use lxutil::LxVolumeOptions;
use parking_lot::RwLock;
use profile::MICROVM_FUSE_MAJOR;
use profile::MICROVM_FUSE_MAX_WRITE;
use profile::MICROVM_FUSE_MIN_MINOR;
use profile::MICROVM_REQUEST_QUEUES;
use profile::MicroVmAccessMode;
use profile::MicroVmVirtioFsProfile;
use saved_state::MAX_ALIAS_BYTES;
use saved_state::MAX_ALIASES;
use saved_state::MAX_ALIASES_PER_INODE;
use saved_state::MAX_DIRECTORY_BYTES;
use saved_state::MAX_DIRECTORY_ENTRIES;
use saved_state::MAX_DIRECTORY_ENTRIES_PER_HANDLE;
use saved_state::MAX_HANDLES;
use saved_state::MAX_INODES;
use saved_state::MAX_PATH_BYTES;
use saved_state::PREVIOUS_SCHEMA_VERSION;
use saved_state::SCHEMA_VERSION;
use saved_state::SavedHandle;
use saved_state::SavedInode;
use saved_state::SavedNegotiation;
use saved_state::SavedObjectIdentity;
use saved_state::SavedState;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::hash_map::Entry;
use std::ffi::OsString;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use virtio_util::MAX_FUSE_REQUEST_BYTES;

// FUSE likes to spam getattr a lot, so having a small timeout on the attributes avoids excessive
// calls. It also means that a lookup/stat sequence can use the attributes returned by lookup
// rather than having to call getattr.
const DEFAULT_ATTRIBUTE_TIMEOUT: Duration = Duration::from_millis(1);

// Entry timeout must be zero, because on rename existing entries for the child being renamed do
// not get updated and would stop working. Having a zero timeout forces a new lookup which will
// update the path.
const DEFAULT_ENTRY_TIMEOUT: Duration = Duration::ZERO;

const MAX_GUEST_BUFFER_SIZE: usize = 1024 * 1024;

const MICROVM_SESSION_DEFAULT_WANT: u32 = FUSE_ASYNC_READ
    | FUSE_PARALLEL_DIROPS
    | FUSE_AUTO_INVAL_DATA
    | FUSE_HANDLE_KILLPRIV
    | FUSE_ASYNC_DIO
    | FUSE_ATOMIC_O_TRUNC
    | FUSE_BIG_WRITES
    | FUSE_MAX_PAGES
    | FUSE_INIT_EXT;

#[derive(Clone, Copy)]
struct CacheAndIoPolicy {
    attribute_timeout: Duration,
    entry_timeout: Duration,
    direct_io: bool,
}

impl CacheAndIoPolicy {
    const fn normal() -> Self {
        Self {
            attribute_timeout: DEFAULT_ATTRIBUTE_TIMEOUT,
            entry_timeout: DEFAULT_ENTRY_TIMEOUT,
            direct_io: true,
        }
    }

    const fn microvm(profile: &MicroVmVirtioFsProfile) -> Self {
        Self {
            attribute_timeout: profile.attribute_cache_timeout(),
            entry_timeout: profile.entry_cache_timeout(),
            direct_io: profile.direct_io(),
        }
    }
}

#[derive(Clone, Copy, Default, Eq, PartialEq)]
struct FuseNegotiation {
    initialized: bool,
    major: u32,
    minor: u32,
    capable: u32,
    capable2: u32,
    want: u32,
    want2: u32,
    max_readahead: u32,
    max_write: u32,
    max_background: u16,
    congestion_threshold: u16,
    time_gran: u32,
}

/// Shared mutable state behind a [`VirtioFs`] handle.
struct VirtioFsInner {
    inodes: RwLock<InodeMap>,
    files: RwLock<HandleMap<Arc<VirtioFsFile>>>,
    mode: VirtioFsMode,
    policy: CacheAndIoPolicy,
    microvm_profile: Option<MicroVmVirtioFsProfile>,
    negotiation: RwLock<FuseNegotiation>,
}

/// Distinguishes a single-share device from a multi-share aggregate.
///
/// The read-only setting lives on each volume's inodes, not here, so aggregate
/// children can differ (see [`AggregateState`]).
enum VirtioFsMode {
    /// Single share: node 1 is a real inode at the volume root.
    Direct,
    /// Multi-share: node 1 is a synthetic directory whose children are
    /// independent host folders.
    Aggregate(AggregateState),
}

impl VirtioFsInner {
    /// The aggregate state, or `None` for a direct (single-share) device.
    fn aggregate(&self) -> Option<&AggregateState> {
        match &self.mode {
            VirtioFsMode::Aggregate(state) => Some(state),
            VirtioFsMode::Direct => None,
        }
    }
}

fn build_volume(
    root_path: impl AsRef<Path>,
    mount_options: Option<&LxVolumeOptions>,
) -> lx::Result<(lxutil::LxVolume, bool)> {
    let readonly = mount_options.is_some_and(|options| options.is_readonly());
    let volume = if let Some(mount_options) = mount_options {
        mount_options.new_volume(root_path)
    } else {
        lxutil::LxVolume::new(root_path)
    }?;
    Ok((volume, readonly))
}

/// Implementation of the virtio-fs file system.
#[derive(Clone)]
pub struct VirtioFs {
    inner: Arc<VirtioFsInner>,
}

impl Fuse for VirtioFs {
    fn init(&self, info: &mut SessionInfo) {
        if let Some(profile) = self.inner.microvm_profile.as_ref() {
            let policy = profile.fuse_negotiation();
            // Session has already selected its supported protocol version;
            // make the profile-controlled portion of the response explicit.
            info.max_write = policy.maximum_write();
        }

        // Indicate we support both readdir and readdirplus.
        if info.capable() & FUSE_DO_READDIRPLUS != 0 {
            info.want |= FUSE_DO_READDIRPLUS;
        }

        // Using "auto" lets FUSE pick whether to use readdir or readdirplus, which can be
        // beneficial since readdirplus needs to query every file and is therefore more expensive.
        if info.capable() & FUSE_READDIRPLUS_AUTO != 0 {
            info.want |= FUSE_READDIRPLUS_AUTO;
        }

        // Allow shared mmap on files opened with FOPEN_DIRECT_IO. This is
        // relevant for virtiofs where direct-I/O is used to avoid page-cache
        // coherency issues with the host, but applications still need mmap.
        if info.capable2() & FUSE_DIRECT_IO_ALLOW_MMAP_FLAG2 != 0 {
            info.want2 |= FUSE_DIRECT_IO_ALLOW_MMAP_FLAG2;
        }

        // The session owns the wire handshake. Keep the complete negotiated
        // contract here so device-private state can validate it before a
        // restore ever starts guest execution.
        *self.inner.negotiation.write() = FuseNegotiation {
            initialized: true,
            major: info.major(),
            minor: info.minor(),
            capable: info.capable(),
            capable2: info.capable2(),
            want: info.want,
            want2: info.want2,
            max_readahead: info.max_readahead,
            max_write: info.max_write,
            max_background: info.max_background,
            congestion_threshold: info.congestion_threshold,
            time_gran: info.time_gran,
        };
    }

    fn get_attr(&self, request: &Request, flags: u32, fh: u64) -> lx::Result<fuse_attr_out> {
        let node_id = request.node_id();
        // If a file handle is specified, get the attributes from the open file. This is faster on
        // Windows and works if the file was deleted. The synthetic root's directory handle has no
        // backing file, so fall through to the node-based branch for it.
        let attr = if flags & FUSE_GETATTR_FH != 0 && !self.is_synthetic_root_handle(node_id, fh) {
            let file = self.get_file(fh)?;
            file.get_attr()?
        } else if self.is_synthetic_root(node_id) {
            self.synthetic_root_attr()
        } else {
            let inode = self.get_inode(node_id)?;
            inode.get_attr()?
        };

        Ok(fuse_attr_out::new(self.attribute_timeout(), attr))
    }

    fn get_statx(
        &self,
        request: &Request,
        fh: u64,
        getattr_flags: u32,
        flags: StatxFlags,
        mask: lx::StatExMask,
    ) -> lx::Result<fuse_statx_out> {
        let node_id = request.node_id();
        // If a file handle is specified, get the attributes from the open file. This is faster on
        // Windows and works if the file was deleted. The synthetic root's directory handle has no
        // backing file, so fall through to the node-based branch for it.
        let statx = if getattr_flags & FUSE_GETATTR_FH != 0
            && !self.is_synthetic_root_handle(node_id, fh)
        {
            let file = self.get_file(fh)?;
            file.get_statx()?
        } else if self.is_synthetic_root(node_id) {
            self.synthetic_root_statx(mask)
        } else {
            let inode = self.get_inode(node_id)?;
            inode.get_statx()?
        };

        Ok(fuse_statx_out::new(self.attribute_timeout(), flags, statx))
    }

    fn set_attr(&self, request: &Request, arg: &fuse_setattr_in) -> lx::Result<fuse_attr_out> {
        let node_id = request.node_id();

        if self.is_synthetic_root(node_id) {
            return Err(lx::Error::EROFS);
        }

        // If a file handle is specified, set the attributes on the open file. This is faster on
        // Windows and works if the file was deleted.
        let attr = if arg.valid & FATTR_FH != 0 {
            let file = self.get_file(arg.fh)?;
            // Block truncation and other modifications on readonly filesystems
            if arg.valid & !(FATTR_FH | FATTR_LOCKOWNER) != 0 {
                self.check_writable(file.inode())?;
            }
            file.set_attr(arg, request.uid())?;
            file.get_attr()?
        } else {
            let inode = self.get_inode(node_id)?;
            // Block truncation and other modifications on readonly filesystems
            if arg.valid & !(FATTR_FH | FATTR_LOCKOWNER) != 0 {
                self.check_writable(&inode)?;
            }
            inode.set_attr(arg, request.uid())?
        };

        Ok(fuse_attr_out::new(self.attribute_timeout(), attr))
    }

    fn lookup(&self, request: &Request, name: &lx::LxStr) -> lx::Result<fuse_entry_out> {
        if self.is_synthetic_root(request.node_id()) {
            return self.lookup_synthetic_root(name);
        }
        let inode = self.get_inode(request.node_id())?;
        self.lookup_helper(&inode, name)
    }

    fn forget(&self, node_id: u64, lookup_count: u64) {
        // This must be done under lock so an inode can't be resurrected between the lookup count
        // reaching zero and removing it from the list.
        let mut inodes = self.inner.inodes.write();
        if let Some(inode) = inodes.get(node_id) {
            if inode.forget(node_id, lookup_count) == 0 {
                tracing::trace!(node_id, "Removing inode");
                inodes.remove(node_id);
            }
        }
    }

    fn open(&self, request: &Request, flags: u32) -> lx::Result<fuse_open_out> {
        let inode = self.get_inode(request.node_id())?;
        self.check_open_readonly(&inode, flags)?;
        self.preflight_file_insert()?;
        let file = inode.open(flags)?;
        let fh = self.insert_file(file)?;

        // TODO: Optionally allow caching.
        Ok(fuse_open_out::new(fh, self.open_flags()))
    }

    fn create(
        &self,
        request: &Request,
        name: &lx::LxStr,
        arg: &fuse_create_in,
    ) -> lx::Result<CreateOut> {
        if self.is_synthetic_root(request.node_id()) {
            return Err(lx::Error::EROFS);
        }
        let inode = self.get_inode(request.node_id())?;
        self.check_writable(&inode)?;
        let path = inode.child_path(name)?;
        self.preflight_create_inode(&inode, name, &path)?;
        self.preflight_file_insert()?;
        let (new_inode, attr, file) =
            inode.create(name, arg.flags, arg.mode, request.uid(), request.gid())?;

        // Insert the newly created inode; this can return an existing inode if it found a match
        // on the inode number (if this is a non-exclusive create), so make sure to associate the
        // file with the returned inode.
        let (new_inode, node_id) = self.insert_inode(new_inode)?;
        let file = VirtioFsFile::new(file, new_inode, arg.flags);
        let fh = self.insert_file(file)?;
        Ok(CreateOut {
            entry: fuse_entry_out::new(
                node_id,
                self.entry_timeout(),
                self.attribute_timeout(),
                attr,
            ),
            open: fuse_open_out::new(fh, self.open_flags()),
        })
    }

    fn mkdir(
        &self,
        request: &Request,
        name: &lx::LxStr,
        arg: &fuse_mkdir_in,
    ) -> lx::Result<fuse_entry_out> {
        if self.is_synthetic_root(request.node_id()) {
            return Err(lx::Error::EROFS);
        }
        let inode = self.get_inode(request.node_id())?;
        self.check_writable(&inode)?;
        let path = inode.child_path(name)?;
        self.preflight_new_inode_path(&path)?;
        let (new_inode, attr) = inode.mkdir(name, arg.mode, request.uid(), request.gid())?;
        let (_, node_id) = self.insert_inode(new_inode)?;
        Ok(fuse_entry_out::new(
            node_id,
            self.entry_timeout(),
            self.attribute_timeout(),
            attr,
        ))
    }

    fn mknod(
        &self,
        request: &Request,
        name: &lx::LxStr,
        arg: &fuse_mknod_in,
    ) -> lx::Result<fuse_entry_out> {
        if self.is_synthetic_root(request.node_id()) {
            return Err(lx::Error::EROFS);
        }
        let inode = self.get_inode(request.node_id())?;
        self.check_writable(&inode)?;
        let path = inode.child_path(name)?;
        self.preflight_new_inode_path(&path)?;
        let (new_inode, attr) =
            inode.mknod(name, arg.mode, request.uid(), request.gid(), arg.rdev)?;

        let (_, node_id) = self.insert_inode(new_inode)?;
        Ok(fuse_entry_out::new(
            node_id,
            self.entry_timeout(),
            self.attribute_timeout(),
            attr,
        ))
    }

    fn symlink(
        &self,
        request: &Request,
        name: &lx::LxStr,
        target: &lx::LxStr,
    ) -> lx::Result<fuse_entry_out> {
        if self.is_synthetic_root(request.node_id()) {
            return Err(lx::Error::EROFS);
        }
        // The generic LxVolume API cannot pin every ancestor while resolving
        // a symlink. The microVM profile therefore does not create links that
        // could later turn a checked relative lookup into an escape.
        if self.is_microvm() {
            return Err(lx::Error::ENOTSUP);
        }
        let inode = self.get_inode(request.node_id())?;
        self.check_writable(&inode)?;
        let (new_inode, attr) = inode.symlink(name, target, request.uid(), request.gid())?;

        let (_, node_id) = self.insert_inode(new_inode)?;
        Ok(fuse_entry_out::new(
            node_id,
            self.entry_timeout(),
            self.attribute_timeout(),
            attr,
        ))
    }

    fn link(&self, request: &Request, name: &lx::LxStr, target: u64) -> lx::Result<fuse_entry_out> {
        if self.is_synthetic_root(request.node_id()) {
            return Err(lx::Error::EROFS);
        }
        let inode = self.get_inode(request.node_id())?;
        let target_inode = self.get_inode(target)?;
        self.check_writable(&inode)?;
        let alias = inode.child_path(name)?;
        self.preflight_alias_add(&target_inode, &alias)?;
        let attr = inode.link(name, &target_inode)?;
        target_inode.add_alias(alias);

        // Increment the lookup count since we're returning an entry for this inode.
        // The kernel will send a forget for this entry later.
        target_inode.inc_lookup();

        // Use the target inode as the reply, with refreshed attributes.
        Ok(fuse_entry_out::new(
            target,
            self.entry_timeout(),
            self.attribute_timeout(),
            attr,
        ))
    }

    fn read_link(&self, request: &Request) -> lx::Result<lx::LxString> {
        let inode = self.get_inode(request.node_id())?;
        inode.read_link()
    }

    fn read(&self, _request: &Request, arg: &fuse_read_in) -> lx::Result<Vec<u8>> {
        let file = self.get_file(arg.fh)?;
        let mut buffer = guest_buffer(arg.size)?;
        let size = file.read(&mut buffer, arg.offset)?;
        buffer.truncate(size);
        Ok(buffer)
    }

    fn write(&self, request: &Request, arg: &fuse_write_in, data: &[u8]) -> lx::Result<usize> {
        if data.len() > MAX_GUEST_BUFFER_SIZE {
            return Err(lx::Error::E2BIG);
        }
        let file = self.get_file(arg.fh)?;
        self.check_writable(file.inode())?;
        file.write(data, arg.offset, request.uid())
    }

    fn release(&self, _request: &Request, arg: &fuse_release_in) -> lx::Result<()> {
        self.remove_file(arg.fh);
        Ok(())
    }

    fn open_dir(&self, request: &Request, flags: u32) -> lx::Result<fuse_open_out> {
        if self.is_synthetic_root(request.node_id()) {
            // The synthetic root has no backing handle; hand out a sentinel that
            // read_dir/read_dir_plus/release_dir recognize.
            return Ok(fuse_open_out::new(SYNTHETIC_ROOT_FH, 0));
        }
        // There is no special handling for directories, so just call open.
        self.open(request, flags)
    }

    fn read_dir(&self, request: &Request, arg: &fuse_read_in) -> lx::Result<Vec<u8>> {
        if self.is_synthetic_root_handle(request.node_id(), arg.fh) {
            return self.read_synthetic_root_dir(arg.offset, arg.size, false);
        }
        let file = self.get_file(arg.fh)?;
        file.read_dir(self, arg.offset, arg.size, false)
    }

    fn read_dir_plus(&self, request: &Request, arg: &fuse_read_in) -> lx::Result<Vec<u8>> {
        if self.is_synthetic_root_handle(request.node_id(), arg.fh) {
            return self.read_synthetic_root_dir(arg.offset, arg.size, true);
        }
        let file = self.get_file(arg.fh)?;
        file.read_dir(self, arg.offset, arg.size, true)
    }

    fn release_dir(&self, request: &Request, arg: &fuse_release_in) -> lx::Result<()> {
        if self.is_synthetic_root_handle(request.node_id(), arg.fh) {
            return Ok(());
        }
        self.release(request, arg)
    }

    fn unlink(&self, request: &Request, name: &lx::LxStr) -> lx::Result<()> {
        self.unlink_helper(request, name, 0)
    }

    fn rmdir(&self, request: &Request, name: &lx::LxStr) -> lx::Result<()> {
        self.unlink_helper(request, name, lx::AT_REMOVEDIR)
    }

    fn rename(
        &self,
        request: &Request,
        name: &lx::LxStr,
        new_dir: u64,
        new_name: &lx::LxStr,
        flags: u32,
    ) -> lx::Result<()> {
        if self.is_synthetic_root(request.node_id()) || self.is_synthetic_root(new_dir) {
            return Err(lx::Error::EROFS);
        }
        let inode = self.get_inode(request.node_id())?;
        let new_inode = self.get_inode(new_dir)?;
        // A rename cannot cross aggregated volume boundaries.
        if inode.volume_id() != new_inode.volume_id() {
            return Err(lx::Error::EXDEV);
        }
        self.check_writable(&inode)?;
        let old_path = inode.child_path(name)?;
        let new_path = new_inode.child_path(new_name)?;
        self.preflight_rename_aliases(inode.volume_id(), &old_path, &new_path)?;
        inode.rename(name, &new_inode, new_name, flags)?;
        let mut inodes = self.inner.inodes.write();
        inodes.remove_alias_prefix(inode.volume_id(), &new_path);
        inodes.rename_alias_prefix(inode.volume_id(), &old_path, &new_path);
        Ok(())
    }

    fn statfs(&self, request: &Request) -> lx::Result<fuse_kstatfs> {
        if self.is_synthetic_root(request.node_id()) {
            return Ok(fuse_kstatfs::new(0, 0, 0, 0, 0, 512, 255, 512));
        }
        let inode = self.get_inode(request.node_id())?;
        inode.stat_fs()
    }

    fn fsync(&self, _request: &Request, fh: u64, flags: u32) -> lx::Result<()> {
        let file = self.get_file(fh)?;
        let data_only = flags & FUSE_FSYNC_FDATASYNC != 0;
        file.fsync(data_only)
    }

    fn fsync_dir(&self, request: &Request, fh: u64, flags: u32) -> lx::Result<()> {
        self.fsync(request, fh, flags)
    }

    fn get_xattr(&self, request: &Request, name: &lx::LxStr, size: u32) -> lx::Result<Vec<u8>> {
        if self.is_synthetic_root(request.node_id()) {
            return Err(lx::Error::ENODATA);
        }
        let inode = self.get_inode(request.node_id())?;
        let mut value = guest_buffer(size)?;
        let size = inode.get_xattr(name, Some(&mut value))?;
        value.truncate(size);
        Ok(value)
    }

    fn get_xattr_size(&self, request: &Request, name: &lx::LxStr) -> lx::Result<u32> {
        if self.is_synthetic_root(request.node_id()) {
            return Err(lx::Error::ENODATA);
        }
        let inode = self.get_inode(request.node_id())?;
        let size = inode.get_xattr(name, None)?;
        let size = size.try_into().map_err(|_| lx::Error::E2BIG)?;
        Ok(size)
    }

    fn set_xattr(
        &self,
        request: &Request,
        name: &lx::LxStr,
        value: &[u8],
        flags: u32,
    ) -> lx::Result<()> {
        if self.is_synthetic_root(request.node_id()) {
            return Err(lx::Error::EROFS);
        }
        if value.len() > MAX_GUEST_BUFFER_SIZE {
            return Err(lx::Error::E2BIG);
        }
        let inode = self.get_inode(request.node_id())?;
        self.check_writable(&inode)?;
        inode.set_xattr(name, value, flags)
    }

    fn list_xattr(&self, request: &Request, size: u32) -> lx::Result<Vec<u8>> {
        if self.is_synthetic_root(request.node_id()) {
            return Ok(Vec::new());
        }
        let inode = self.get_inode(request.node_id())?;
        let mut list = guest_buffer(size)?;
        let size = inode.list_xattr(Some(&mut list))?;
        list.truncate(size);
        Ok(list)
    }

    fn list_xattr_size(&self, request: &Request) -> lx::Result<u32> {
        if self.is_synthetic_root(request.node_id()) {
            return Ok(0);
        }
        let inode = self.get_inode(request.node_id())?;
        let size = inode.list_xattr(None)?;
        let size = size.try_into().map_err(|_| lx::Error::E2BIG)?;
        Ok(size)
    }

    fn remove_xattr(&self, request: &Request, name: &lx::LxStr) -> lx::Result<()> {
        if self.is_synthetic_root(request.node_id()) {
            return Err(lx::Error::EROFS);
        }
        let inode = self.get_inode(request.node_id())?;
        self.check_writable(&inode)?;
        inode.remove_xattr(name)
    }

    fn destroy(&self) {
        // To get the file system ready for re-mount, clean out any open files and leaked inodes.
        self.inner.files.write().clear();
        self.inner.inodes.write().clear();
        *self.inner.negotiation.write() = FuseNegotiation::default();
    }
}

impl VirtioFs {
    /// Check if the inode's volume is readonly and return EROFS if so.
    fn check_writable(&self, inode: &VirtioFsInode) -> lx::Result<()> {
        if inode.readonly() {
            Err(lx::Error::EROFS)
        } else {
            Ok(())
        }
    }

    /// Check whether the open flags are permitted on a read-only filesystem.
    fn check_open_readonly(&self, inode: &VirtioFsInode, flags: u32) -> lx::Result<()> {
        if !inode.readonly() {
            return Ok(());
        }

        // This section exists to superceed error codes when various combination of flags
        // are passed to the open() call. This helps maintain POSIX compatibility
        // If O_CREAT | O_EXCL && file_exists => EEXIST
        // If O_CREAT && file_exists => fallthrough to check other checks
        // If O_CREAT && !file_exists => EROFS
        // Other errors that occur while checking file_exists should bubble up
        if flags & lx::O_CREAT as u32 != 0 {
            match inode.get_attr() {
                Ok(_) if flags & lx::O_EXCL as u32 != 0 => return Err(lx::Error::EEXIST),
                Ok(_) => {}
                Err(e) if e == lx::Error::ENOENT => return Err(lx::Error::EROFS),
                Err(e) => return Err(e),
            }
        } else {
            inode.get_attr()?;
        }

        let access_mode = (flags & lx::O_ACCESS_MASK as u32) as i32;
        if matches!(access_mode, lx::O_WRONLY | lx::O_RDWR) || flags & lx::O_TRUNC as u32 != 0 {
            return Err(lx::Error::EROFS);
        }

        Ok(())
    }

    /// Create a new virtio-fs for the specified root path.
    pub fn new(
        root_path: impl AsRef<Path>,
        mount_options: Option<&LxVolumeOptions>,
    ) -> lx::Result<Self> {
        let (volume, readonly) = build_volume(root_path, mount_options)?;
        let mut inodes = InodeMap::new(false);
        let volume = Arc::new(VirtioFsVolume::new(volume, 0, readonly));
        let (root_inode, _) = VirtioFsInode::new(volume, PathBuf::new())?;
        if inodes.insert(root_inode)?.1 != FUSE_ROOT_ID {
            return Err(lx::Error::EINVAL);
        }
        Ok(Self {
            inner: Arc::new(VirtioFsInner {
                inodes: RwLock::new(inodes),
                files: RwLock::new(HandleMap::new()),
                mode: VirtioFsMode::Direct,
                policy: CacheAndIoPolicy::normal(),
                microvm_profile: None,
                negotiation: RwLock::new(FuseNegotiation::default()),
            }),
        })
    }

    /// Creates a filesystem attachment for the fixed microVM profile.
    ///
    /// `root_path` is deliberately consumed only while opening the attachment;
    /// it is not retained in the filesystem state or in a saved-state blob.
    pub fn new_microvm(
        root_path: impl AsRef<Path>,
        profile: MicroVmVirtioFsProfile,
    ) -> anyhow::Result<Self> {
        let root_path = root_path.as_ref();
        profile.validate_root_path(root_path)?;
        let mut mount_options = LxVolumeOptions::new();
        mount_options.readonly(profile.is_readonly()).sandbox(true);
        let volume = mount_options.new_volume(root_path)?;
        let mut inodes = InodeMap::new(false);
        let volume = Arc::new(VirtioFsVolume::new_with_strict_paths(
            volume,
            0,
            profile.is_readonly(),
            true,
        ));
        let (root_inode, root_stat) = VirtioFsInode::new(Arc::clone(&volume), PathBuf::new())?;
        profile.validate_opened_root(root_path, &root_stat)?;
        if inodes.insert(root_inode)?.1 != FUSE_ROOT_ID {
            anyhow::bail!("microVM virtio-fs root received an invalid node ID");
        }
        Ok(Self {
            inner: Arc::new(VirtioFsInner {
                inodes: RwLock::new(inodes),
                files: RwLock::new(HandleMap::new()),
                mode: VirtioFsMode::Direct,
                policy: CacheAndIoPolicy::microvm(&profile),
                microvm_profile: Some(profile),
                negotiation: RwLock::new(FuseNegotiation::default()),
            }),
        })
    }

    /// Create a new, empty aggregate virtio-fs.
    ///
    /// Node 1 is a synthetic, read-only directory; use [`Self::add_child`] to
    /// expose host folders as named children, each with its own read-only
    /// setting. Children share one superblock, with inode numbers namespaced
    /// per volume to avoid cross-volume `st_ino` collisions.
    pub fn new_aggregate() -> Self {
        Self {
            inner: Arc::new(VirtioFsInner {
                // `true` enables aggregate mode: node 1 is synthetic (see `InodeMap`).
                inodes: RwLock::new(InodeMap::new(true)),
                files: RwLock::new(HandleMap::new()),
                mode: VirtioFsMode::Aggregate(AggregateState::new()),
                policy: CacheAndIoPolicy::normal(),
                microvm_profile: None,
                negotiation: RwLock::new(FuseNegotiation::default()),
            }),
        }
    }

    fn lookup_helper(&self, inode: &VirtioFsInode, name: &lx::LxStr) -> lx::Result<fuse_entry_out> {
        let (new_inode, attr) = inode.lookup_child(name)?;
        self.preflight_inode_insert(&new_inode)?;
        let (_, new_inode_nr) = self.insert_inode(new_inode)?;
        Ok(fuse_entry_out::new(
            new_inode_nr,
            self.entry_timeout(),
            self.attribute_timeout(),
            attr,
        ))
    }

    pub(crate) fn microvm_profile(&self) -> Option<&MicroVmVirtioFsProfile> {
        self.inner.microvm_profile.as_ref()
    }

    fn is_microvm(&self) -> bool {
        self.inner.microvm_profile.is_some()
    }

    fn attribute_timeout(&self) -> Duration {
        self.inner.policy.attribute_timeout
    }

    fn entry_timeout(&self) -> Duration {
        self.inner.policy.entry_timeout
    }

    fn open_flags(&self) -> u32 {
        if self.inner.policy.direct_io {
            FOPEN_DIRECT_IO
        } else {
            0
        }
    }

    pub(crate) fn save_microvm_state(
        &self,
        profile: &MicroVmVirtioFsProfile,
        session_state: SessionState,
    ) -> anyhow::Result<SavedState> {
        anyhow::ensure!(
            self.microvm_profile() == Some(profile),
            "virtio-fs attachment does not match the microVM profile"
        );
        anyhow::ensure!(
            self.inner.aggregate().is_none(),
            "aggregate virtio-fs is not part of the microVM ABI"
        );

        let (inodes, node_ids, next_node_id) = {
            let inodes = self.inner.inodes.read();
            anyhow::ensure!(inodes.inodes_by_node_id.values.len() <= MAX_INODES);
            let mut saved = Vec::with_capacity(inodes.inodes_by_node_id.values.len());
            let mut node_ids = HashMap::with_capacity(inodes.inodes_by_node_id.values.len());
            let mut alias_count = 0usize;
            let mut alias_bytes = 0usize;
            for (&node_id, inode) in &inodes.inodes_by_node_id.values {
                let aliases = inode.aliases();
                anyhow::ensure!(
                    !aliases.is_empty() && aliases.len() <= MAX_ALIASES_PER_INODE,
                    "inode has no bounded reopenable aliases"
                );
                alias_count = alias_count
                    .checked_add(aliases.len())
                    .context("saved alias count overflow")?;
                anyhow::ensure!(alias_count <= MAX_ALIASES, "saved alias table is too large");
                let volume = inode.volume();
                let mut relative_aliases = Vec::with_capacity(aliases.len());
                let mut object_identity = None;
                for alias in aliases {
                    validate_relative_path(&alias, true)
                        .map_err(anyhow::Error::from)
                        .context("inode has an unsafe relative alias")?;
                    let stat = validate_reopenable_alias(&volume, &alias)
                        .context("inode alias cannot be revalidated for save")?;
                    let identity = saved_identity(&stat);
                    if let Some(expected) = &object_identity {
                        validate_saved_identity(&identity, expected)
                            .context("inode aliases identify different objects")?;
                    } else {
                        object_identity = Some(identity);
                    }
                    let alias = encode_relative_path(&alias)?;
                    alias_bytes = alias_bytes
                        .checked_add(alias.len())
                        .context("saved alias bytes overflow")?;
                    anyhow::ensure!(
                        alias_bytes <= MAX_ALIAS_BYTES,
                        "saved aliases exceed the aggregate byte bound"
                    );
                    relative_aliases.push(alias);
                }
                saved.push(SavedInode {
                    node_id,
                    volume_id: inode.volume_id(),
                    relative_aliases,
                    lookup_count: inode.lookup_count(),
                    guest_inode_id: inode.guest_inode_nr(),
                    object_identity: object_identity.context("inode has no aliases")?,
                });
                anyhow::ensure!(
                    node_ids
                        .insert(Arc::as_ptr(inode) as usize, node_id)
                        .is_none(),
                    "duplicate inode identity in inode table"
                );
            }
            (saved, node_ids, inodes.inodes_by_node_id.next_handle)
        };

        let handles = {
            let files = self.inner.files.read();
            anyhow::ensure!(files.values.len() <= MAX_HANDLES);
            let mut saved = Vec::with_capacity(files.values.len());
            let mut directory_entry_count = 0usize;
            let mut directory_bytes = 0usize;
            for (&handle_id, file) in &files.values {
                let node_id = *node_ids
                    .get(&(std::ptr::from_ref(file.inode()) as usize))
                    .context("open handle refers to an untracked inode")?;
                let stat = file
                    .object_stat()
                    .map_err(anyhow::Error::from)
                    .context("open handle cannot be revalidated for save")?;
                let snapshot_entries = file.directory_entries();
                let directory_snapshot_built = file.directory_snapshot_built();
                anyhow::ensure!(
                    directory_snapshot_built || snapshot_entries.is_empty(),
                    "uninitialized directory snapshot retained entries"
                );
                VirtioFsFile::validate_directory_entries(&snapshot_entries)
                    .map_err(anyhow::Error::from)
                    .context("directory enumeration state is invalid")?;
                directory_entry_count = directory_entry_count
                    .checked_add(snapshot_entries.len())
                    .context("saved directory entry count overflow")?;
                anyhow::ensure!(
                    directory_entry_count <= MAX_DIRECTORY_ENTRIES,
                    "saved directory entries exceed the aggregate bound"
                );
                for entry in &snapshot_entries {
                    directory_bytes = directory_bytes
                        .checked_add(entry.name.len())
                        .context("saved directory bytes overflow")?;
                }
                anyhow::ensure!(
                    directory_bytes <= MAX_DIRECTORY_BYTES,
                    "saved directory entries exceed the aggregate byte bound"
                );
                saved.push(SavedHandle {
                    handle_id,
                    node_id,
                    open_flags: file.open_flags(),
                    kind: stat.mode & lx::S_IFMT,
                    object_identity: saved_identity(&stat),
                    directory_entries: snapshot_entries,
                    directory_snapshot_built,
                });
            }
            saved
        };

        let negotiation = fuse_negotiation_from_session(session_state);
        anyhow::ensure!(
            *self.inner.negotiation.read() == negotiation,
            "FUSE session and filesystem negotiation state disagree"
        );
        Ok(SavedState {
            schema_version: SCHEMA_VERSION,
            attachment_id: profile.attachment_id().to_owned(),
            access_mode: saved_access_mode(profile.access_mode()),
            request_queues: profile.request_queues(),
            shared_memory_size: 0,
            packed_rings: false,
            direct_io: profile.direct_io(),
            entry_cache_timeout_ns: profile.entry_cache_timeout().as_nanos() as u64,
            attribute_cache_timeout_ns: profile.attribute_cache_timeout().as_nanos() as u64,
            negotiation: saved_negotiation(session_state),
            next_node_id,
            next_handle_id: self.inner.files.read().next_handle,
            inodes,
            handles,
            attachment_root_identity: profile.root_identity().to_vec(),
            maximum_request_size: MAX_FUSE_REQUEST_BYTES as u32,
            dormant: false,
        })
    }

    pub(crate) fn restore_microvm_state(
        &self,
        profile: &MicroVmVirtioFsProfile,
        state: SavedState,
        session: &Session,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.microvm_profile() == Some(profile),
            "virtio-fs attachment does not match the microVM profile"
        );
        validate_microvm_state(&state, profile)?;
        let session_state = session_state_from_saved(&state.negotiation)?;

        let current_root = self.get_inode(FUSE_ROOT_ID).map_err(anyhow::Error::from)?;
        let current_root_stat = current_root
            .object_stat()
            .map_err(anyhow::Error::from)
            .context("restore attachment root cannot be inspected")?;
        let saved_root = state
            .inodes
            .iter()
            .find(|inode| inode.node_id == FUSE_ROOT_ID)
            .context("saved state does not contain a root inode")?;
        validate_identity(&current_root_stat, &saved_root.object_identity)
            .context("restore attachment root identity does not match")?;
        let volume = current_root.volume();

        let mut restored_inodes = InodeMap::new(false);
        let mut node_ids = HashSet::with_capacity(state.inodes.len());
        let mut alias_paths = HashSet::new();
        for saved in &state.inodes {
            anyhow::ensure!(node_ids.insert(saved.node_id), "duplicate saved inode ID");
            anyhow::ensure!(saved.volume_id == 0, "unexpected saved volume ID");
            let mut aliases = Vec::with_capacity(saved.relative_aliases.len());
            let mut first_stat = None;
            for saved_alias in &saved.relative_aliases {
                let path = decode_relative_path(saved_alias)?;
                validate_relative_path(&path, true).map_err(anyhow::Error::from)?;
                if saved.node_id == FUSE_ROOT_ID {
                    anyhow::ensure!(
                        path.as_os_str().is_empty(),
                        "root inode has a non-root alias"
                    );
                } else {
                    anyhow::ensure!(
                        !path.as_os_str().is_empty(),
                        "non-root inode has a root alias"
                    );
                }
                anyhow::ensure!(
                    alias_paths.insert(path.clone()),
                    "saved aliases are duplicated or ambiguous"
                );
                let stat = validate_reopenable_alias(&volume, &path)
                    .context("saved inode alias cannot be reopened")?;
                validate_identity(&stat, &saved.object_identity)
                    .context("saved inode identity does not match the attachment")?;
                if first_stat.is_none() {
                    first_stat = Some(stat);
                }
                aliases.push(path);
            }
            let stat = first_stat.context("saved inode has no reopenable aliases")?;
            let inode =
                VirtioFsInode::from_saved(Arc::clone(&volume), aliases, saved.lookup_count, &stat)
                    .map_err(anyhow::Error::from)?;
            anyhow::ensure!(
                inode.guest_inode_nr() == saved.guest_inode_id,
                "saved guest inode ID does not match the attachment"
            );
            let key = inode.dedup_key();
            anyhow::ensure!(
                !restored_inodes.inodes_by_key.contains_key(&key),
                "saved inode aliases are ambiguous"
            );
            let inode = Arc::new(inode);
            restored_inodes
                .inodes_by_node_id
                .values
                .insert(saved.node_id, Arc::clone(&inode));
            restored_inodes
                .inodes_by_key
                .insert(key, (inode, saved.node_id));
        }
        restored_inodes.inodes_by_node_id.next_handle = state.next_node_id;

        let mut restored_files = HandleMap::starting_at(state.next_handle_id);
        let mut handle_ids = HashSet::with_capacity(state.handles.len());
        for saved in &state.handles {
            anyhow::ensure!(
                handle_ids.insert(saved.handle_id),
                "duplicate saved handle ID"
            );
            let inode = restored_inodes
                .get(saved.node_id)
                .context("saved handle refers to an unknown inode")?;
            let reopen_flags = reopen_flags(saved.open_flags)?;
            let file = inode
                .open(reopen_flags)
                .map_err(anyhow::Error::from)
                .context("saved handle cannot be reopened")?;
            let stat = file
                .object_stat()
                .map_err(anyhow::Error::from)
                .context("reopened handle cannot be inspected")?;
            validate_identity(&stat, &saved.object_identity)
                .context("reopened handle identity does not match")?;
            anyhow::ensure!(
                stat.mode & lx::S_IFMT == saved.kind,
                "reopened handle kind does not match"
            );
            file.restore_directory_snapshot(
                saved.directory_snapshot_built,
                saved.directory_entries.clone(),
            )
            .map_err(anyhow::Error::from)
            .context("saved directory snapshot is invalid")?;
            restored_files
                .values
                .insert(saved.handle_id, Arc::new(file));
        }

        session
            .restore_state(session_state)
            .map_err(anyhow::Error::from)
            .context("saved FUSE negotiation cannot be restored")?;
        *self.inner.inodes.write() = restored_inodes;
        *self.inner.files.write() = restored_files;
        *self.inner.negotiation.write() = fuse_negotiation_from_session(session_state);
        Ok(())
    }

    /// Removes a file or directory.
    fn unlink_helper(&self, request: &Request, name: &lx::LxStr, flags: i32) -> lx::Result<()> {
        if self.is_synthetic_root(request.node_id()) {
            return Err(lx::Error::EROFS);
        }
        let inode = self.get_inode(request.node_id())?;
        self.check_writable(&inode)?;
        let path = inode.child_path(name)?;
        inode.unlink(name, flags)?;
        self.inner
            .inodes
            .write()
            .remove_alias_prefix(inode.volume_id(), &path);
        Ok(())
    }

    /// Retrieve the inode with the specified node ID.
    fn get_inode(&self, node_id: u64) -> lx::Result<Arc<VirtioFsInode>> {
        let inode = self.inner.inodes.read().get(node_id).ok_or_else(|| {
            tracelimit::warn_ratelimited!(node_id, "request for unknown inode");
            lx::Error::EINVAL
        })?;
        inode.validate_confined()?;
        Ok(inode)
    }

    /// Insert a new inode, and returns the assigned node ID as well as a reference to the inode.
    ///
    /// If the file system supports stable inode numbers and an inode already existed with this
    /// number, the existing inode is returned, not the passed in one.
    fn insert_inode(&self, inode: VirtioFsInode) -> lx::Result<(Arc<VirtioFsInode>, u64)> {
        let mut inodes = self.inner.inodes.write();
        if self.is_microvm() {
            inodes.insert_microvm(inode)
        } else {
            inodes.insert(inode)
        }
    }

    /// Retrieve the file object with the specified file handle.
    fn get_file(&self, fh: u64) -> lx::Result<Arc<VirtioFsFile>> {
        let files = self.inner.files.read();
        let file = files.get(fh).ok_or_else(|| {
            tracelimit::warn_ratelimited!(fh, "Request for unknown file");
            lx::Error::EBADF
        })?;

        Ok(Arc::clone(file))
    }

    /// Insert a new file object, and return the assigned file handle.
    fn insert_file(&self, file: VirtioFsFile) -> lx::Result<u64> {
        let mut files = self.inner.files.write();
        if self.is_microvm() && (files.values.len() >= MAX_HANDLES || !files.can_insert()) {
            return Err(lx::Error::ENOSPC);
        }
        files.insert(Arc::new(file)).ok_or(lx::Error::ENOSPC)
    }

    fn preflight_inode_insert(&self, inode: &VirtioFsInode) -> lx::Result<()> {
        if self.is_microvm() {
            self.inner.inodes.read().preflight_microvm_insert(inode)
        } else {
            Ok(())
        }
    }

    fn preflight_new_inode_path(&self, path: &Path) -> lx::Result<()> {
        if self.is_microvm() {
            self.inner.inodes.read().preflight_microvm_new_inode(path)
        } else {
            Ok(())
        }
    }

    fn preflight_create_inode(
        &self,
        parent: &VirtioFsInode,
        name: &lx::LxStr,
        path: &Path,
    ) -> lx::Result<()> {
        if !self.is_microvm() {
            return Ok(());
        }
        match parent.lookup_child(name) {
            Ok((inode, _)) => self.preflight_inode_insert(&inode),
            Err(error) if error == lx::Error::ENOENT => self.preflight_new_inode_path(path),
            Err(error) => Err(error),
        }
    }

    fn preflight_alias_add(&self, inode: &VirtioFsInode, path: &Path) -> lx::Result<()> {
        if self.is_microvm() {
            self.inner
                .inodes
                .read()
                .preflight_microvm_alias_add(inode, path)
        } else {
            Ok(())
        }
    }

    fn preflight_rename_aliases(&self, volume_id: u32, old: &Path, new: &Path) -> lx::Result<()> {
        if self.is_microvm() {
            self.inner
                .inodes
                .read()
                .preflight_microvm_rename(volume_id, old, new)
        } else {
            Ok(())
        }
    }

    fn preflight_file_insert(&self) -> lx::Result<()> {
        if self.is_microvm() {
            let files = self.inner.files.read();
            if files.values.len() >= MAX_HANDLES || !files.can_insert() {
                return Err(lx::Error::ENOSPC);
            }
        }
        Ok(())
    }

    /// Remove the file with the specified node ID.
    fn remove_file(&self, fh: u64) {
        self.inner.files.write().remove(fh);
    }
}

fn guest_buffer(size: u32) -> lx::Result<Vec<u8>> {
    let size = size as usize;
    if size > MAX_GUEST_BUFFER_SIZE {
        return Err(lx::Error::E2BIG);
    }
    let mut buffer = Vec::new();
    buffer
        .try_reserve_exact(size)
        .map_err(|_| lx::Error::ENOMEM)?;
    buffer.resize(size, 0);
    Ok(buffer)
}

fn saved_access_mode(access_mode: MicroVmAccessMode) -> u32 {
    match access_mode {
        MicroVmAccessMode::ReadOnly => 1,
        MicroVmAccessMode::ReadWrite => 2,
    }
}

fn fuse_negotiation_from_session(state: SessionState) -> FuseNegotiation {
    FuseNegotiation {
        initialized: state.initialized,
        major: state.info.major,
        minor: state.info.minor,
        capable: state.info.capable,
        capable2: state.info.capable2,
        want: state.info.want,
        want2: state.info.want2,
        max_readahead: state.info.max_readahead,
        max_write: state.info.max_write,
        max_background: state.info.max_background,
        congestion_threshold: state.info.congestion_threshold,
        time_gran: state.info.time_gran,
    }
}

fn saved_negotiation(state: SessionState) -> SavedNegotiation {
    SavedNegotiation {
        initialized: state.initialized,
        major: state.info.major,
        minor: state.info.minor,
        capable: state.info.capable,
        capable2: state.info.capable2,
        want: state.info.want,
        want2: state.info.want2,
        max_readahead: state.info.max_readahead,
        max_write: state.info.max_write,
        max_background: state.info.max_background.into(),
        congestion_threshold: state.info.congestion_threshold.into(),
        time_gran: state.info.time_gran,
    }
}

fn session_state_from_saved(negotiation: &SavedNegotiation) -> anyhow::Result<SessionState> {
    let state = SessionState {
        initialized: negotiation.initialized,
        info: SessionInfoState {
            major: negotiation.major,
            minor: negotiation.minor,
            max_readahead: negotiation.max_readahead,
            capable: negotiation.capable,
            capable2: negotiation.capable2,
            want: negotiation.want,
            want2: negotiation.want2,
            max_background: negotiation
                .max_background
                .try_into()
                .context("saved FUSE max_background is out of range")?,
            congestion_threshold: negotiation
                .congestion_threshold
                .try_into()
                .context("saved FUSE congestion_threshold is out of range")?,
            max_write: negotiation.max_write,
            time_gran: negotiation.time_gran,
        },
    };
    state
        .validate()
        .map_err(anyhow::Error::from)
        .context("saved FUSE session state is invalid")?;
    Ok(state)
}

fn saved_identity(stat: &lx::Stat) -> SavedObjectIdentity {
    SavedObjectIdentity {
        device_id: stat.device_nr,
        inode_id: stat.inode_nr,
        kind: stat.mode & lx::S_IFMT,
    }
}

fn validate_identity(stat: &lx::Stat, saved: &SavedObjectIdentity) -> anyhow::Result<()> {
    anyhow::ensure!(
        saved.kind == stat.mode & lx::S_IFMT,
        "object kind changed from {:#x} to {:#x}",
        saved.kind,
        stat.mode & lx::S_IFMT
    );
    anyhow::ensure!(
        saved.device_id == stat.device_nr && saved.inode_id == stat.inode_nr,
        "object identity changed"
    );
    Ok(())
}

fn validate_saved_identity(
    actual: &SavedObjectIdentity,
    expected: &SavedObjectIdentity,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        actual.kind == expected.kind
            && actual.device_id == expected.device_id
            && actual.inode_id == expected.inode_id,
        "object identity changed between aliases"
    );
    Ok(())
}

fn validate_reopenable_alias(volume: &VirtioFsVolume, path: &Path) -> anyhow::Result<lx::Stat> {
    validate_relative_path(path, true).map_err(anyhow::Error::from)?;
    let mut prefix = PathBuf::new();
    for component in path.components() {
        let std::path::Component::Normal(component) = component else {
            anyhow::bail!("alias contains a non-normal path component");
        };
        prefix.push(component);
        let stat = volume
            .lstat(&prefix)
            .map_err(anyhow::Error::from)
            .context("alias component cannot be inspected")?;
        anyhow::ensure!(
            stat.mode & lx::S_IFMT != lx::S_IFLNK,
            "alias contains a symbolic-link component"
        );
    }
    volume
        .lstat(path)
        .map_err(anyhow::Error::from)
        .context("alias cannot be inspected")
}

pub(crate) fn save_dormant_microvm_state(
    attachment_id: &str,
    session_state: SessionState,
) -> anyhow::Result<SavedState> {
    anyhow::ensure!(
        session_state == SessionState::default(),
        "dormant microVM virtio-fs has initialized FUSE state"
    );
    Ok(SavedState {
        schema_version: SCHEMA_VERSION,
        attachment_id: attachment_id.to_owned(),
        access_mode: 0,
        request_queues: MICROVM_REQUEST_QUEUES,
        shared_memory_size: 0,
        packed_rings: false,
        direct_io: true,
        entry_cache_timeout_ns: 0,
        attribute_cache_timeout_ns: 0,
        negotiation: saved_negotiation(session_state),
        next_node_id: 0,
        next_handle_id: 0,
        inodes: Vec::new(),
        handles: Vec::new(),
        attachment_root_identity: Vec::new(),
        maximum_request_size: MAX_FUSE_REQUEST_BYTES as u32,
        dormant: true,
    })
}

pub(crate) fn validate_dormant_microvm_state(
    state: &SavedState,
    attachment_id: &str,
) -> anyhow::Result<SessionState> {
    anyhow::ensure!(
        state.schema_version == SCHEMA_VERSION && state.dormant,
        "saved virtio-fs state is not a supported dormant slot"
    );
    anyhow::ensure!(
        state.attachment_id == attachment_id,
        "saved attachment ID does not match the microVM ABI"
    );
    anyhow::ensure!(
        state.access_mode == 0
            && state.request_queues == MICROVM_REQUEST_QUEUES
            && state.shared_memory_size == 0
            && !state.packed_rings
            && state.direct_io
            && state.entry_cache_timeout_ns == 0
            && state.attribute_cache_timeout_ns == 0
            && state.maximum_request_size == MAX_FUSE_REQUEST_BYTES as u32
            && state.next_node_id == 0
            && state.next_handle_id == 0
            && state.inodes.is_empty()
            && state.handles.is_empty()
            && state.attachment_root_identity.is_empty(),
        "dormant virtio-fs state contains active filesystem policy or objects"
    );
    let session_state = session_state_from_saved(&state.negotiation)?;
    anyhow::ensure!(
        session_state == SessionState::default(),
        "dormant microVM virtio-fs contains initialized FUSE state"
    );
    Ok(session_state)
}

pub(crate) fn validate_microvm_state(
    state: &SavedState,
    profile: &MicroVmVirtioFsProfile,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        matches!(
            state.schema_version,
            PREVIOUS_SCHEMA_VERSION | SCHEMA_VERSION
        ) && !state.dormant,
        "unsupported virtio-fs state schema version {}",
        state.schema_version
    );
    anyhow::ensure!(
        state.attachment_id == profile.attachment_id(),
        "saved attachment ID does not match the microVM ABI"
    );
    anyhow::ensure!(
        state.attachment_root_identity == profile.root_identity(),
        "saved root identity does not match the restore attachment"
    );
    anyhow::ensure!(
        state.access_mode == saved_access_mode(profile.access_mode()),
        "saved access mode does not match the restore profile"
    );
    anyhow::ensure!(
        state.request_queues == profile.request_queues()
            && state.shared_memory_size == 0
            && !state.packed_rings
            && state.direct_io
            && state.entry_cache_timeout_ns == 0
            && state.attribute_cache_timeout_ns == 0
            && state.maximum_request_size == MAX_FUSE_REQUEST_BYTES as u32,
        "saved device policy does not match the fixed microVM ABI"
    );
    anyhow::ensure!(
        state.inodes.len() <= MAX_INODES,
        "saved inode table is too large"
    );
    anyhow::ensure!(
        state.handles.len() <= MAX_HANDLES,
        "saved handle table is too large"
    );
    anyhow::ensure!(
        state.next_node_id != 0 && state.next_handle_id != 0,
        "saved allocation state is invalid"
    );

    let session_state = session_state_from_saved(&state.negotiation)?;
    if session_state.initialized {
        let expected_want = (MICROVM_SESSION_DEFAULT_WANT & state.negotiation.capable)
            | (state.negotiation.capable & (FUSE_DO_READDIRPLUS | FUSE_READDIRPLUS_AUTO));
        let expected_want2 = if expected_want & FUSE_INIT_EXT != 0 {
            state.negotiation.capable2 & FUSE_DIRECT_IO_ALLOW_MMAP_FLAG2
        } else {
            0
        };
        anyhow::ensure!(
            state.negotiation.major == MICROVM_FUSE_MAJOR
                && state.negotiation.minor >= MICROVM_FUSE_MIN_MINOR
                && state.negotiation.minor <= FUSE_KERNEL_MINOR_VERSION
                && state.negotiation.want == expected_want
                && state.negotiation.want2 == expected_want2
                && state.negotiation.max_background == 0
                && state.negotiation.congestion_threshold == 0
                && state.negotiation.time_gran == 1
                && state.negotiation.max_write == MICROVM_FUSE_MAX_WRITE,
            "saved FUSE negotiation is not reproducible by the microVM profile"
        );
    }

    let mut root_count = 0;
    let mut largest_node_id = 0;
    let mut inode_ids = HashSet::with_capacity(state.inodes.len());
    let mut alias_paths = HashSet::new();
    let mut alias_count = 0usize;
    let mut alias_bytes = 0usize;
    for inode in &state.inodes {
        anyhow::ensure!(
            inode.node_id != 0 && inode_ids.insert(inode.node_id),
            "saved inode IDs are invalid or duplicated"
        );
        largest_node_id = largest_node_id.max(inode.node_id);
        if inode.node_id == FUSE_ROOT_ID {
            root_count += 1;
        }
        anyhow::ensure!(
            inode.volume_id == 0,
            "saved aggregate volumes are unsupported"
        );
        anyhow::ensure!(
            inode.lookup_count != 0,
            "saved inode lookup count is invalid"
        );
        anyhow::ensure!(
            inode.object_identity.kind != lx::S_IFLNK,
            "saved symlink identities are unsupported by the microVM profile"
        );
        anyhow::ensure!(
            !inode.relative_aliases.is_empty()
                && inode.relative_aliases.len() <= MAX_ALIASES_PER_INODE,
            "saved inode has no bounded aliases"
        );
        alias_count = alias_count
            .checked_add(inode.relative_aliases.len())
            .context("saved alias count overflow")?;
        anyhow::ensure!(alias_count <= MAX_ALIASES, "saved alias table is too large");
        for alias in &inode.relative_aliases {
            anyhow::ensure!(
                alias.len() <= MAX_PATH_BYTES,
                "saved relative alias is too long"
            );
            alias_bytes = alias_bytes
                .checked_add(alias.len())
                .context("saved alias bytes overflow")?;
            anyhow::ensure!(
                alias_bytes <= MAX_ALIAS_BYTES,
                "saved aliases exceed the aggregate byte bound"
            );
            let path = decode_relative_path(alias)?;
            validate_relative_path(&path, true).map_err(anyhow::Error::from)?;
            if inode.node_id == FUSE_ROOT_ID {
                anyhow::ensure!(
                    path.as_os_str().is_empty(),
                    "root inode has a non-root alias"
                );
            } else {
                anyhow::ensure!(
                    !path.as_os_str().is_empty(),
                    "non-root inode has a root alias"
                );
            }
            anyhow::ensure!(
                alias_paths.insert(path),
                "saved aliases are duplicated or ambiguous"
            );
        }
    }
    anyhow::ensure!(root_count == 1, "saved inode table must contain one root");
    anyhow::ensure!(
        state.next_node_id > largest_node_id,
        "saved next inode ID can reuse an active ID"
    );

    let mut largest_handle_id = 0;
    let mut handle_ids = HashSet::with_capacity(state.handles.len());
    let mut directory_entry_count = 0usize;
    let mut directory_bytes = 0usize;
    for handle in &state.handles {
        anyhow::ensure!(
            handle.handle_id != 0 && handle_ids.insert(handle.handle_id),
            "saved handle IDs are invalid or duplicated"
        );
        largest_handle_id = largest_handle_id.max(handle.handle_id);
        anyhow::ensure!(
            inode_ids.contains(&handle.node_id),
            "saved handle refers to an unknown inode"
        );
        anyhow::ensure!(
            handle.object_identity.kind == handle.kind && handle.kind != lx::S_IFLNK,
            "saved handle has an unsupported object kind"
        );
        let reopen_flags = reopen_flags(handle.open_flags)?;
        if profile.is_readonly() {
            anyhow::ensure!(
                reopen_flags & lx::O_ACCESS_MASK as u32 == lx::O_RDONLY as u32,
                "read-only microVM profile cannot restore a writable handle"
            );
        }
        anyhow::ensure!(
            handle.directory_entries.len() <= MAX_DIRECTORY_ENTRIES_PER_HANDLE,
            "saved directory enumeration is too large"
        );
        anyhow::ensure!(
            handle.directory_snapshot_built || handle.directory_entries.is_empty(),
            "uninitialized directory snapshot retained entries"
        );
        anyhow::ensure!(
            !handle.directory_snapshot_built || handle.kind == lx::S_IFDIR,
            "saved directory snapshot belongs to a non-directory handle"
        );
        VirtioFsFile::validate_directory_entries(&handle.directory_entries)
            .map_err(anyhow::Error::from)?;
        directory_entry_count = directory_entry_count
            .checked_add(handle.directory_entries.len())
            .context("saved directory entry count overflow")?;
        anyhow::ensure!(
            directory_entry_count <= MAX_DIRECTORY_ENTRIES,
            "saved directory entries exceed the aggregate bound"
        );
        for entry in &handle.directory_entries {
            directory_bytes = directory_bytes
                .checked_add(entry.name.len())
                .context("saved directory bytes overflow")?;
        }
        anyhow::ensure!(
            directory_bytes <= MAX_DIRECTORY_BYTES,
            "saved directory entries exceed the aggregate byte bound"
        );
    }
    anyhow::ensure!(
        state.next_handle_id > largest_handle_id,
        "saved next handle ID can reuse an active ID"
    );

    Ok(())
}

fn reopen_flags(saved: u32) -> anyhow::Result<u32> {
    const O_NOCTTY: u32 = 0x100;
    const O_NONBLOCK: u32 = 0x800;
    const O_DSYNC: u32 = 0x1000;
    const O_SYNC: u32 = 0x101000;
    const O_LARGEFILE: u32 = 0x8000;
    const O_CLOEXEC: u32 = 0x80000;
    const O_TMPFILE: u32 = 0x410000;

    anyhow::ensure!(
        saved & O_TMPFILE != O_TMPFILE,
        "saved handle uses unsupported O_TMPFILE semantics"
    );
    let allowed = lx::O_ACCESS_MASK as u32
        | lx::O_CREAT as u32
        | lx::O_EXCL as u32
        | lx::O_TRUNC as u32
        | lx::O_APPEND as u32
        | lx::O_DIRECTORY as u32
        | lx::O_NOFOLLOW as u32
        | lx::O_NOATIME as u32
        | O_NOCTTY
        | O_NONBLOCK
        | O_DSYNC
        | O_SYNC
        | O_LARGEFILE
        | O_CLOEXEC;
    anyhow::ensure!(
        saved & !allowed == 0,
        "saved handle has unsupported open flags {:#x}",
        saved
    );
    anyhow::ensure!(
        saved & lx::O_ACCESS_MASK as u32 != lx::O_NOACCESS as u32,
        "saved handle has invalid access mode"
    );
    Ok(saved & !(lx::O_CREAT as u32 | lx::O_EXCL as u32 | lx::O_TRUNC as u32))
}

fn encode_relative_path(path: &Path) -> anyhow::Result<Vec<u8>> {
    validate_relative_path(path, true).map_err(anyhow::Error::from)?;
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;

        Ok(path.as_os_str().as_bytes().to_vec())
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;

        let mut bytes = Vec::new();
        for unit in path.as_os_str().encode_wide() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        Ok(bytes)
    }
}

pub(crate) fn relative_path_encoded_len(path: &Path) -> lx::Result<usize> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;

        Ok(path.as_os_str().as_bytes().len())
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;

        path.as_os_str()
            .encode_wide()
            .count()
            .checked_mul(size_of::<u16>())
            .ok_or(lx::Error::E2BIG)
    }
}

fn decode_relative_path(bytes: &[u8]) -> anyhow::Result<PathBuf> {
    anyhow::ensure!(
        bytes.len() <= MAX_PATH_BYTES,
        "saved relative alias is too long"
    );
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;

        Ok(PathBuf::from(OsString::from_vec(bytes.to_vec())))
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStringExt;

        anyhow::ensure!(
            bytes.len().is_multiple_of(2),
            "saved UTF-16 path is malformed"
        );
        let units = bytes
            .chunks_exact(2)
            .map(|bytes| u16::from_le_bytes([bytes[0], bytes[1]]))
            .collect::<Vec<_>>();
        Ok(PathBuf::from(OsString::from_wide(&units)))
    }
}

fn validate_relative_path(path: &Path, strict: bool) -> lx::Result<()> {
    for component in path.components() {
        let std::path::Component::Normal(component) = component else {
            return Err(lx::Error::EINVAL);
        };
        if component.is_empty() {
            return Err(lx::Error::EINVAL);
        }
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;

            let component = component.as_bytes();
            if component.contains(&b'\0')
                || (strict && (component.contains(&b'\\') || component.contains(&b':')))
            {
                return Err(lx::Error::EINVAL);
            }
        }
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStrExt;

            let units = component.encode_wide().collect::<Vec<_>>();
            if units.contains(&0)
                || (strict && (units.contains(&(b'\\' as u16)) || units.contains(&(b':' as u16))))
            {
                return Err(lx::Error::EINVAL);
            }
        }
    }
    Ok(())
}

/// A key/value map where the keys are automatically incremented identifiers.
struct HandleMap<T> {
    values: HashMap<u64, T>,
    next_handle: u64,
}

impl<T> HandleMap<T> {
    /// Create a new `HandleMap`.
    pub fn new() -> Self {
        Self::starting_at(1)
    }

    /// Create a new `HandleMap` starting with handle value `next_handle`.
    pub fn starting_at(next_handle: u64) -> Self {
        Self {
            values: HashMap::new(),
            next_handle,
        }
    }

    /// Inserts an item into the map, and returns the assigned handle.
    pub fn insert(&mut self, value: T) -> Option<u64> {
        if !self.can_insert() {
            return None;
        }
        let handle = self.next_handle;
        self.values.insert(handle, value);
        self.next_handle = handle.checked_add(1).unwrap_or(0);
        Some(handle)
    }

    pub fn can_insert(&self) -> bool {
        self.next_handle != 0 && !self.values.contains_key(&self.next_handle)
    }

    /// Retrieves a value from the map.
    pub fn get(&self, handle: u64) -> Option<&T> {
        self.values.get(&handle)
    }

    /// Retrieves a value from the map.
    #[cfg_attr(not(windows), expect(dead_code))]
    pub fn get_mut(&mut self, handle: u64) -> Option<&mut T> {
        self.values.get_mut(&handle)
    }

    /// Removes a value from the map.
    pub fn remove(&mut self, handle: u64) -> Option<T> {
        self.values.remove(&handle)
    }

    /// Clears the map and resets the handle values.
    pub fn clear(&mut self) {
        self.values.clear();
        self.next_handle = 1;
    }
}

/// Assigns node IDs to inodes, and keeps track of in-use inodes by their actual inode number.
///
/// We cannot use the real inode number as the FUSE node ID:
/// - FUSE node ID 1 is reserved for the root, so this would break if a file system used that inode
///   number.
/// - When we want to support multiple volumes in a single file system, node IDs still need to be
///   globally unique, whereas inode numbers are per-volume.
struct InodeMap {
    inodes_by_node_id: HandleMap<Arc<VirtioFsInode>>,
    /// Maps a [`DedupKey`] to the registered inode and its FUSE node id, so
    /// repeated lookups of one host file share a single node id.
    inodes_by_key: HashMap<DedupKey, (Arc<VirtioFsInode>, u64)>,
    /// When true, node 1 is synthetic and not stored in this map, so node IDs
    /// are allocated starting at 2 and `clear` does not preserve a real root.
    aggregate: bool,
}

impl InodeMap {
    /// Create a new `InodeMap`.
    pub fn new(aggregate: bool) -> Self {
        Self {
            inodes_by_node_id: if aggregate {
                HandleMap::starting_at(FUSE_ROOT_ID + 1)
            } else {
                HandleMap::new()
            },
            inodes_by_key: HashMap::new(),
            aggregate,
        }
    }

    /// Get an inode with the specified FUSE node ID.
    pub fn get(&self, node_id: u64) -> Option<Arc<VirtioFsInode>> {
        let inode = self.inodes_by_node_id.get(node_id)?;
        Some(Arc::clone(inode))
    }

    /// Insert an inode into the map, returning its node ID.
    pub fn insert(&mut self, inode: VirtioFsInode) -> lx::Result<(Arc<VirtioFsInode>, u64)> {
        // Reuse an existing node id for the same host file; see `DedupKey`
        // for how each volume type is keyed.
        match self.inodes_by_key.entry(inode.dedup_key()) {
            Entry::Occupied(entry) => {
                // Inode found; increment its count and return the existing FUSE node ID.
                let new_path = inode.clone_path();
                let (existing, node_id) = entry.get();
                existing.lookup(new_path);
                Ok((Arc::clone(existing), *node_id))
            }
            Entry::Vacant(entry) => {
                // Inode not found, so insert it into both maps.
                let inode = Arc::new(inode);
                let node_id = self
                    .inodes_by_node_id
                    .insert(Arc::clone(&inode))
                    .ok_or(lx::Error::ENOSPC)?;
                entry.insert((Arc::clone(&inode), node_id));
                Ok((inode, node_id))
            }
        }
    }

    fn insert_microvm(&mut self, inode: VirtioFsInode) -> lx::Result<(Arc<VirtioFsInode>, u64)> {
        self.preflight_microvm_insert(&inode)?;
        self.insert(inode)
    }

    fn preflight_microvm_insert(&self, inode: &VirtioFsInode) -> lx::Result<()> {
        let path = inode.clone_path();
        Self::validate_microvm_alias_path(&path)?;
        if let Some((existing, _)) = self.inodes_by_key.get(&inode.dedup_key()) {
            return self.preflight_microvm_alias_add(existing, &path);
        }
        self.preflight_microvm_new_inode(&path)
    }

    fn preflight_microvm_new_inode(&self, path: &Path) -> lx::Result<()> {
        Self::validate_microvm_alias_path(path)?;
        if self.inodes_by_node_id.values.len() >= MAX_INODES || !self.inodes_by_node_id.can_insert()
        {
            return Err(lx::Error::ENOSPC);
        }
        let (alias_count, alias_bytes) = self.microvm_alias_usage()?;
        if alias_count >= MAX_ALIASES {
            return Err(lx::Error::ENOSPC);
        }
        let path_bytes = relative_path_encoded_len(path)?;
        if alias_bytes
            .checked_add(path_bytes)
            .is_none_or(|bytes| bytes > MAX_ALIAS_BYTES)
        {
            return Err(lx::Error::E2BIG);
        }
        Ok(())
    }

    fn preflight_microvm_alias_add(&self, inode: &VirtioFsInode, path: &Path) -> lx::Result<()> {
        Self::validate_microvm_alias_path(path)?;
        let aliases = inode.aliases();
        if aliases.iter().any(|alias| alias == path) {
            return Ok(());
        }
        if aliases.len() >= MAX_ALIASES_PER_INODE {
            return Err(lx::Error::ENOSPC);
        }
        let (alias_count, alias_bytes) = self.microvm_alias_usage()?;
        if alias_count >= MAX_ALIASES {
            return Err(lx::Error::ENOSPC);
        }
        let path_bytes = relative_path_encoded_len(path)?;
        if alias_bytes
            .checked_add(path_bytes)
            .is_none_or(|bytes| bytes > MAX_ALIAS_BYTES)
        {
            return Err(lx::Error::E2BIG);
        }
        Ok(())
    }

    fn preflight_microvm_rename(&self, volume_id: u32, old: &Path, new: &Path) -> lx::Result<()> {
        Self::validate_microvm_alias_path(new)?;
        let mut alias_count = 0usize;
        let mut alias_bytes = 0usize;
        for inode in self.inodes_by_node_id.values.values() {
            let mut aliases: BTreeSet<_> = inode.aliases().into_iter().collect();
            if inode.volume_id() == volume_id {
                aliases.retain(|alias| !alias.starts_with(new));
                let replacements: Vec<_> = aliases
                    .iter()
                    .filter_map(|alias| {
                        alias.strip_prefix(old).ok().map(|suffix| {
                            let mut replacement = new.to_path_buf();
                            replacement.push(suffix);
                            (alias.clone(), replacement)
                        })
                    })
                    .collect();
                for (old_alias, new_alias) in replacements {
                    aliases.remove(&old_alias);
                    aliases.insert(new_alias);
                }
            }
            if aliases.len() > MAX_ALIASES_PER_INODE {
                return Err(lx::Error::ENOSPC);
            }
            alias_count = alias_count
                .checked_add(aliases.len())
                .ok_or(lx::Error::ENOSPC)?;
            if alias_count > MAX_ALIASES {
                return Err(lx::Error::ENOSPC);
            }
            for alias in aliases {
                Self::validate_microvm_alias_path(&alias)?;
                alias_bytes = alias_bytes
                    .checked_add(relative_path_encoded_len(&alias)?)
                    .ok_or(lx::Error::E2BIG)?;
                if alias_bytes > MAX_ALIAS_BYTES {
                    return Err(lx::Error::E2BIG);
                }
            }
        }
        Ok(())
    }

    fn microvm_alias_usage(&self) -> lx::Result<(usize, usize)> {
        let mut count = 0usize;
        let mut bytes = 0usize;
        for inode in self.inodes_by_node_id.values.values() {
            let aliases = inode.aliases();
            count = count.checked_add(aliases.len()).ok_or(lx::Error::ENOSPC)?;
            for alias in aliases {
                bytes = bytes
                    .checked_add(relative_path_encoded_len(&alias)?)
                    .ok_or(lx::Error::E2BIG)?;
            }
        }
        Ok((count, bytes))
    }

    fn validate_microvm_alias_path(path: &Path) -> lx::Result<()> {
        validate_relative_path(path, true)?;
        if relative_path_encoded_len(path)? > MAX_PATH_BYTES {
            return Err(lx::Error::E2BIG);
        }
        Ok(())
    }

    /// Remove an inode with the specified FUSE node ID from the map.
    pub fn remove(&mut self, node_id: u64) {
        let Some(inode) = self.inodes_by_node_id.remove(node_id) else {
            return;
        };
        // Only drop the by-key entry if it still points at THIS node: on
        // path-keyed volumes the path may have been repointed to a newer inode
        // (via delete+recreate or `evict_dedup_key`), which must not be lost.
        if let Entry::Occupied(entry) = self.inodes_by_key.entry(inode.dedup_key()) {
            if entry.get().1 == node_id {
                entry.remove();
            }
        }
    }

    /// Removes aliases at or below an unlinked path and refreshes path-keyed
    /// deduplication for any surviving inodes.
    pub fn remove_alias_prefix(&mut self, volume_id: u32, path: &Path) {
        for inode in self.inodes_by_node_id.values.values() {
            if inode.volume_id() == volume_id {
                inode.remove_alias_prefix(path);
            }
        }
        self.rebuild_dedup_keys();
    }

    /// Rewrites aliases at or below a renamed path and refreshes path-keyed
    /// deduplication for any surviving inodes.
    pub fn rename_alias_prefix(&mut self, volume_id: u32, old: &Path, new: &Path) {
        for inode in self.inodes_by_node_id.values.values() {
            if inode.volume_id() == volume_id {
                inode.rename_alias_prefix(old, new);
            }
        }
        self.rebuild_dedup_keys();
    }

    fn rebuild_dedup_keys(&mut self) {
        self.inodes_by_key.clear();
        for (&node_id, inode) in &self.inodes_by_node_id.values {
            let key = inode.dedup_key();
            if matches!(key, DedupKey::Path(..)) && inode.aliases().is_empty() {
                continue;
            }
            self.inodes_by_key
                .entry(key)
                .or_insert_with(|| (Arc::clone(inode), node_id));
        }
    }

    /// Clears the map, preserving the root inode.
    pub fn clear(&mut self) {
        if self.aggregate {
            // Node 1 is synthetic and not stored here; drop everything and resume
            // allocating node IDs after the reserved root id.
            self.inodes_by_node_id.clear();
            self.inodes_by_node_id.next_handle = FUSE_ROOT_ID + 1;
            self.inodes_by_key.clear();
            return;
        }

        let Some(root_inode) = self.inodes_by_node_id.get(FUSE_ROOT_ID).cloned() else {
            self.inodes_by_node_id.clear();
            self.inodes_by_key.clear();
            return;
        };
        self.inodes_by_node_id.clear();

        // Re-insert the root inode.
        let inserted = self.inodes_by_node_id.insert(Arc::clone(&root_inode));
        if inserted != Some(FUSE_ROOT_ID) {
            self.inodes_by_node_id.clear();
            self.inodes_by_key.clear();
            return;
        }

        // Rebuild the dedup map with just the root.
        self.inodes_by_key.clear();
        let key = root_inode.dedup_key();
        self.inodes_by_key.insert(key, (root_inode, FUSE_ROOT_ID));
    }
}

#[cfg(test)]
mod microvm_tests {
    use super::*;
    use crate::profile::MICROVM_ATTACHMENT_ID;
    use crate::profile::MicroVmVirtioFsProfile;
    use crate::profile::microvm_root_identity;
    use tempfile::tempdir;
    use test_with_tracing::test;

    fn profile(root_path: &Path) -> MicroVmVirtioFsProfile {
        MicroVmVirtioFsProfile::from_attachment(
            MICROVM_ATTACHMENT_ID.to_owned(),
            microvm_root_identity(root_path).unwrap(),
            true,
        )
        .unwrap()
    }

    #[test]
    fn microvm_state_restores_against_the_same_attachment() {
        let temporary_directory = tempdir().unwrap();
        let profile = profile(temporary_directory.path());
        let source = VirtioFs::new_microvm(temporary_directory.path(), profile.clone()).unwrap();
        let state = source
            .save_microvm_state(&profile, SessionState::default())
            .unwrap();
        let destination =
            VirtioFs::new_microvm(temporary_directory.path(), profile.clone()).unwrap();
        let session = Session::new(destination.clone());

        destination
            .restore_microvm_state(&profile, state, &session)
            .unwrap();
        assert!(destination.get_inode(FUSE_ROOT_ID).is_ok());
    }

    #[test]
    fn initialized_microvm_negotiation_restores() {
        let temporary_directory = tempdir().unwrap();
        let profile = profile(temporary_directory.path());
        let source = VirtioFs::new_microvm(temporary_directory.path(), profile.clone()).unwrap();
        let session_state = SessionState {
            initialized: true,
            info: SessionInfoState {
                major: MICROVM_FUSE_MAJOR,
                minor: MICROVM_FUSE_MIN_MINOR,
                capable: FUSE_ASYNC_READ,
                want: FUSE_ASYNC_READ,
                max_write: MICROVM_FUSE_MAX_WRITE,
                time_gran: 1,
                ..Default::default()
            },
        };
        *source.inner.negotiation.write() = fuse_negotiation_from_session(session_state);
        let state = source.save_microvm_state(&profile, session_state).unwrap();

        let destination =
            VirtioFs::new_microvm(temporary_directory.path(), profile.clone()).unwrap();
        let session = Session::new(destination.clone());
        destination
            .restore_microvm_state(&profile, state, &session)
            .unwrap();
        assert_eq!(session.save_state(), session_state);
        assert!(session.is_initialized());
    }

    #[test]
    fn malformed_allocation_state_is_rejected() {
        let temporary_directory = tempdir().unwrap();
        let profile = profile(temporary_directory.path());
        let source = VirtioFs::new_microvm(temporary_directory.path(), profile.clone()).unwrap();
        let mut state = source
            .save_microvm_state(&profile, SessionState::default())
            .unwrap();
        state.next_node_id = FUSE_ROOT_ID;

        assert!(validate_microvm_state(&state, &profile).is_err());
    }

    #[test]
    fn saved_request_size_mismatch_is_rejected() {
        let temporary_directory = tempdir().unwrap();
        let profile = profile(temporary_directory.path());
        let source = VirtioFs::new_microvm(temporary_directory.path(), profile.clone()).unwrap();
        let mut state = source
            .save_microvm_state(&profile, SessionState::default())
            .unwrap();
        state.maximum_request_size = 0;

        assert!(validate_microvm_state(&state, &profile).is_err());
    }

    #[test]
    fn microvm_runtime_path_and_map_limits_are_enforced() {
        let temporary_directory = tempdir().unwrap();
        let profile = profile(temporary_directory.path());
        let fs = VirtioFs::new_microvm(temporary_directory.path(), profile).unwrap();
        let root = fs.get_inode(FUSE_ROOT_ID).unwrap();
        let oversized_name = vec![b'x'; MAX_PATH_BYTES + 1];
        assert_eq!(
            root.lookup_child(lx::LxStr::from_bytes(&oversized_name))
                .err()
                .unwrap(),
            lx::Error::E2BIG
        );

        fs.inner.inodes.write().inodes_by_node_id.next_handle = 0;
        assert_eq!(
            fs.preflight_new_inode_path(Path::new("new-entry"))
                .unwrap_err(),
            lx::Error::ENOSPC
        );
        fs.inner.files.write().next_handle = 0;
        assert_eq!(fs.preflight_file_insert().unwrap_err(), lx::Error::ENOSPC);
    }

    #[test]
    fn microvm_alias_limit_is_enforced_before_linking() {
        let temporary_directory = tempdir().unwrap();
        std::fs::write(temporary_directory.path().join("entry"), b"data").unwrap();
        let profile = profile(temporary_directory.path());
        let fs = VirtioFs::new_microvm(temporary_directory.path(), profile).unwrap();
        let root = fs.get_inode(FUSE_ROOT_ID).unwrap();
        let (inode, _) = fs
            .insert_inode(
                root.lookup_child(lx::LxStr::from_bytes(b"entry"))
                    .unwrap()
                    .0,
            )
            .unwrap();
        for index in 1..MAX_ALIASES_PER_INODE {
            inode.add_alias(PathBuf::from(format!("alias-{index}")));
        }
        assert_eq!(
            fs.preflight_alias_add(&inode, Path::new("alias-overflow"))
                .unwrap_err(),
            lx::Error::ENOSPC
        );
    }

    #[test]
    fn saved_negotiation_must_match_the_exact_microvm_policy() {
        let temporary_directory = tempdir().unwrap();
        let profile = profile(temporary_directory.path());
        let source = VirtioFs::new_microvm(temporary_directory.path(), profile.clone()).unwrap();
        let session_state = SessionState {
            initialized: true,
            info: SessionInfoState {
                major: MICROVM_FUSE_MAJOR,
                minor: MICROVM_FUSE_MIN_MINOR,
                capable: FUSE_ASYNC_READ | FUSE_INIT_EXT,
                capable2: FUSE_DIRECT_IO_ALLOW_MMAP_FLAG2,
                want: FUSE_ASYNC_READ | FUSE_INIT_EXT,
                want2: FUSE_DIRECT_IO_ALLOW_MMAP_FLAG2,
                max_write: MICROVM_FUSE_MAX_WRITE,
                time_gran: 1,
                ..Default::default()
            },
        };
        *source.inner.negotiation.write() = fuse_negotiation_from_session(session_state);
        let mut state = source.save_microvm_state(&profile, session_state).unwrap();

        state.negotiation.want2 = 0;
        assert!(validate_microvm_state(&state, &profile).is_err());
        state.negotiation.want2 = FUSE_DIRECT_IO_ALLOW_MMAP_FLAG2;
        state.negotiation.max_background = 1;
        assert!(validate_microvm_state(&state, &profile).is_err());
    }

    #[test]
    fn reopen_flags_preserve_safe_status_bits_and_reject_effects() {
        const O_NONBLOCK: u32 = 0x800;
        const O_DSYNC: u32 = 0x1000;
        const O_CLOEXEC: u32 = 0x80000;
        const O_TMPFILE: u32 = 0x410000;

        let saved = lx::O_RDWR as u32
            | lx::O_APPEND as u32
            | O_NONBLOCK
            | O_DSYNC
            | O_CLOEXEC
            | lx::O_CREAT as u32
            | lx::O_EXCL as u32
            | lx::O_TRUNC as u32;
        let reopened = reopen_flags(saved).unwrap();
        assert_eq!(
            reopened,
            saved & !(lx::O_CREAT as u32 | lx::O_EXCL as u32 | lx::O_TRUNC as u32)
        );
        assert!(reopen_flags(O_TMPFILE).is_err());
        assert!(reopen_flags(0x80000000).is_err());
    }

    #[test]
    fn readonly_profile_rejects_a_writable_saved_handle() {
        let temporary_directory = tempdir().unwrap();
        let profile = profile(temporary_directory.path());
        let source = VirtioFs::new_microvm(temporary_directory.path(), profile.clone()).unwrap();
        let root = source.get_inode(FUSE_ROOT_ID).unwrap();
        source
            .insert_file(Arc::clone(&root).open(lx::O_RDONLY as u32).unwrap())
            .unwrap();
        let mut state = source
            .save_microvm_state(&profile, SessionState::default())
            .unwrap();
        state.handles[0].open_flags = lx::O_RDWR as u32 | lx::O_NOFOLLOW as u32;

        assert!(validate_microvm_state(&state, &profile).is_err());
    }

    #[test]
    fn saved_parent_alias_is_rejected_before_reopen() {
        let temporary_directory = tempdir().unwrap();
        let profile = profile(temporary_directory.path());
        let source = VirtioFs::new_microvm(temporary_directory.path(), profile.clone()).unwrap();
        let mut state = source
            .save_microvm_state(&profile, SessionState::default())
            .unwrap();
        state.inodes[0].relative_aliases = vec![b"..".to_vec()];
        let destination =
            VirtioFs::new_microvm(temporary_directory.path(), profile.clone()).unwrap();
        let session = Session::new(destination.clone());

        assert!(
            destination
                .restore_microvm_state(&profile, state, &session)
                .is_err()
        );
    }

    #[test]
    fn directory_snapshot_survives_host_mutation_after_restore() {
        let temporary_directory = tempdir().unwrap();
        let root_path = temporary_directory.path();
        std::fs::write(root_path.join("alpha"), b"alpha").unwrap();
        std::fs::write(root_path.join("beta"), b"beta").unwrap();
        let profile = profile(root_path);
        let source = VirtioFs::new_microvm(root_path, profile.clone()).unwrap();
        let root = source.get_inode(FUSE_ROOT_ID).unwrap();
        let handle = source
            .insert_file(Arc::clone(&root).open(lx::O_RDONLY as u32).unwrap())
            .unwrap();
        let file = source.get_file(handle).unwrap();

        let first_page = file.read_dir(&source, 0, 32, false).unwrap();
        assert!(!first_page.is_empty());
        let snapshot = file.directory_entries();
        assert!(snapshot.len() > 1);
        let continuation_offset = snapshot[0].next_cookie;
        assert_ne!(continuation_offset, 0);
        let expected_continuation = file
            .read_dir(&source, continuation_offset, 4096, false)
            .unwrap();
        let state = source
            .save_microvm_state(&profile, SessionState::default())
            .unwrap();

        std::fs::rename(root_path.join("beta"), root_path.join("00-beta")).unwrap();
        std::fs::write(root_path.join("later"), b"later").unwrap();

        let destination = VirtioFs::new_microvm(root_path, profile.clone()).unwrap();
        let session = Session::new(destination.clone());
        destination
            .restore_microvm_state(&profile, state, &session)
            .unwrap();
        let continuation = destination
            .get_file(handle)
            .unwrap()
            .read_dir(&destination, continuation_offset, 4096, false)
            .unwrap();
        assert_eq!(continuation, expected_continuation);
    }

    #[test]
    fn hard_link_aliases_restore_and_reject_missing_or_replaced_names() {
        let temporary_directory = tempdir().unwrap();
        let root_path = temporary_directory.path();
        std::fs::write(root_path.join("first"), b"data").unwrap();
        std::fs::hard_link(root_path.join("first"), root_path.join("second")).unwrap();
        let profile = profile(root_path);
        let source = VirtioFs::new_microvm(root_path, profile.clone()).unwrap();
        let root = source.get_inode(FUSE_ROOT_ID).unwrap();
        let (_, first_node_id) = source
            .insert_inode(
                root.lookup_child(lx::LxStr::from_bytes(b"first"))
                    .unwrap()
                    .0,
            )
            .unwrap();
        let (_, second_node_id) = source
            .insert_inode(
                root.lookup_child(lx::LxStr::from_bytes(b"second"))
                    .unwrap()
                    .0,
            )
            .unwrap();
        assert_eq!(first_node_id, second_node_id);

        let missing_state = source
            .save_microvm_state(&profile, SessionState::default())
            .unwrap();
        let inode = missing_state
            .inodes
            .iter()
            .find(|inode| inode.node_id == first_node_id)
            .unwrap();
        assert_eq!(
            inode.relative_aliases,
            vec![
                encode_relative_path(Path::new("first")).unwrap(),
                encode_relative_path(Path::new("second")).unwrap(),
            ]
        );
        let replaced_state = source
            .save_microvm_state(&profile, SessionState::default())
            .unwrap();

        std::fs::remove_file(root_path.join("second")).unwrap();
        let destination = VirtioFs::new_microvm(root_path, profile.clone()).unwrap();
        let session = Session::new(destination.clone());
        assert!(
            destination
                .restore_microvm_state(&profile, missing_state, &session)
                .is_err()
        );

        std::fs::write(root_path.join("second"), b"replacement").unwrap();
        let destination = VirtioFs::new_microvm(root_path, profile.clone()).unwrap();
        let session = Session::new(destination.clone());
        assert!(
            destination
                .restore_microvm_state(&profile, replaced_state, &session)
                .is_err()
        );
    }

    #[test]
    fn relative_path_validation_rejects_platform_escapes() {
        assert!(validate_relative_path(Path::new("../outside"), true).is_err());
        assert!(validate_relative_path(Path::new("/outside"), true).is_err());
        assert!(validate_relative_path(Path::new("alternate:name"), true).is_err());
    }
}
