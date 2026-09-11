// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(unix)]
// UNSAFETY: Calls to libc functions to interact with low level primitives.
#![expect(unsafe_code)]

pub mod affinity;
pub mod fs;
pub mod pipe;
pub mod process;
pub mod pthread;

use std::fs::File;
use std::io;
use std::io::Error;
use std::os::unix::prelude::*;

#[cfg(target_os = "linux")]
const FIRST_DYNAMIC_FD: RawFd = usize::BITS as RawFd;

/// Returns the effective user ID of the current process.
pub fn effective_user_id() -> u32 {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() }
}

#[cfg(target_os = "linux")]
pub fn unix_socket_peer_user_id(socket: BorrowedFd<'_>) -> io::Result<u32> {
    let mut credentials = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut length = size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `socket` is valid for the call and the output pointers refer to
    // writable objects with the supplied sizes.
    let result = unsafe {
        libc::getsockopt(
            socket.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            std::ptr::from_mut(&mut credentials).cast(),
            &mut length,
        )
    };
    if result != 0 {
        return Err(Error::last_os_error());
    }
    if length as usize != size_of::<libc::ucred>() {
        return Err(Error::new(
            io::ErrorKind::InvalidData,
            "SO_PEERCRED returned an invalid credential length",
        ));
    }
    Ok(credentials.uid)
}

/// Takes a numeric inherited descriptor and returns an owned duplicate.
///
/// The launcher transfers ownership of a valid descriptor to the new process
/// and must not use it there. This function immediately creates a close-on-exec
/// duplicate and closes the inherited descriptor.
pub fn take_inherited_file(raw: u64) -> io::Result<File> {
    let raw = RawFd::try_from(raw)
        .map_err(|_| Error::new(io::ErrorKind::InvalidInput, "invalid inherited descriptor"))?;
    // SAFETY: `fcntl` accepts an integer descriptor and reports `EBADF` for an
    // invalid value. On success, ownership of the new descriptor is unique.
    let duplicate = unsafe { libc::fcntl(raw, libc::F_DUPFD_CLOEXEC, 0) };
    if duplicate < 0 {
        return Err(Error::last_os_error());
    }
    // SAFETY: the validity and ownership contract above permits consuming the
    // inherited descriptor. Do not retry close after EINTR.
    if unsafe { libc::close(raw) } != 0 {
        let error = Error::last_os_error();
        // SAFETY: `duplicate` is uniquely owned here.
        unsafe { libc::close(duplicate) };
        return Err(error);
    }
    // SAFETY: successful F_DUPFD_CLOEXEC returns a fresh owned descriptor.
    Ok(unsafe { File::from_raw_fd(duplicate) })
}

/// A Linux error value.
#[derive(Copy, Clone, PartialEq, Eq)]
pub struct Errno(pub i32);

impl std::fmt::Debug for Errno {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&Error::from(*self), f)
    }
}

impl From<Errno> for Error {
    fn from(code: Errno) -> Self {
        Self::from_raw_os_error(code.0)
    }
}

/// Trait for extracting a Unix error value from an error type.
pub trait TryAsErrno {
    /// Gets the Unix error value if there is one.
    fn try_as_errno(&self) -> Option<Errno>;
}

impl TryAsErrno for Errno {
    fn try_as_errno(&self) -> Option<Errno> {
        Some(*self)
    }
}

impl TryAsErrno for Error {
    fn try_as_errno(&self) -> Option<Errno> {
        self.raw_os_error().map(Errno)
    }
}

/// Returns the value of errno.
pub(crate) fn errno() -> Errno {
    Errno(Error::last_os_error().raw_os_error().unwrap())
}

/// A helper trait to convert from a libc return value to a `Result<_, Errno>`.
pub trait SyscallResult: Sized {
    /// Returns `Ok(self)` if `self >= 0`, otherwise `Err(errno())`.
    fn syscall_result(self) -> Result<Self, Errno>;
}

impl SyscallResult for i32 {
    fn syscall_result(self) -> Result<Self, Errno> {
        if self >= 0 { Ok(self) } else { Err(errno()) }
    }
}

impl SyscallResult for isize {
    fn syscall_result(self) -> Result<Self, Errno> {
        if self >= 0 { Ok(self) } else { Err(errno()) }
    }
}

/// Runs f() until it stop failing with EINTR (as indicated by errno).
pub fn while_eintr<F, R, E>(mut f: F) -> Result<R, E>
where
    F: FnMut() -> Result<R, E>,
    E: TryAsErrno,
{
    loop {
        match f() {
            Err(err) if err.try_as_errno() == Some(Errno(libc::EINTR)) => {}
            r => break r,
        }
    }
}

/// Closes stdout, replacing it the null device.
pub fn close_stdout() -> io::Result<()> {
    let new_stdout = File::open("/dev/null")?;
    // SAFETY: replacing stdout with an owned fd
    unsafe { libc::dup2(new_stdout.as_raw_fd(), 1) }.syscall_result()?;
    Ok(())
}

/// Expands the Linux file descriptor table before worker threads are started.
///
/// Growing a full descriptor table after other threads exist may wait for an
/// RCU grace period. Touching a high descriptor while startup is still
/// single-threaded avoids that restore-time latency.
#[cfg(target_os = "linux")]
pub fn expand_fd_table() -> io::Result<()> {
    let mut limits = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };

    // SAFETY: `limits` is valid writable memory for `getrlimit`.
    unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limits) }.syscall_result()?;

    if limits.rlim_cur <= FIRST_DYNAMIC_FD as libc::rlim_t {
        return Ok(());
    }

    // Linux keeps BITS_PER_LONG descriptors inline. Allocate only the first
    // dynamic table: later growth caused the observed stall, while expanding
    // all the way toward RLIMIT_NOFILE adds unnecessary process-exit work.
    expand_fd_table_to(FIRST_DYNAMIC_FD)
}

#[cfg(target_os = "linux")]
fn expand_fd_table_to(target_fd: RawFd) -> io::Result<()> {
    // An existing descriptor proves that the table already reaches the target.
    // SAFETY: `F_GETFD` only inspects the descriptor number.
    if unsafe { libc::fcntl(target_fd, libc::F_GETFD) } >= 0 {
        return Ok(());
    }
    let error = Error::last_os_error();
    if error.raw_os_error() != Some(libc::EBADF) {
        return Err(error);
    }

    let source = File::open("/dev/null")?;
    if source.as_raw_fd() >= target_fd {
        return Ok(());
    }

    // Unlike dup2, F_DUPFD_CLOEXEC never replaces an inherited descriptor if
    // the target becomes occupied.
    // SAFETY: `source` is valid and `target_fd` is a nonnegative lower bound.
    let duplicate = unsafe { libc::fcntl(source.as_raw_fd(), libc::F_DUPFD_CLOEXEC, target_fd) }
        .syscall_result()?;
    // SAFETY: `duplicate` was returned as a new owned descriptor above.
    unsafe { libc::close(duplicate) }.syscall_result()?;
    Ok(())
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::FIRST_DYNAMIC_FD;
    use super::SyscallResult;
    use super::expand_fd_table_to;
    use std::io::Read;
    use std::io::Write;
    use std::os::fd::AsRawFd;
    use std::os::fd::FromRawFd;
    use std::os::unix::net::UnixStream;

    #[test]
    fn expand_fd_table_preserves_occupied_target() {
        let (source, mut peer) = UnixStream::pair().unwrap();
        // SAFETY: `source` is valid and the returned descriptor is checked.
        let inherited_fd =
            unsafe { libc::fcntl(source.as_raw_fd(), libc::F_DUPFD_CLOEXEC, FIRST_DYNAMIC_FD) }
                .syscall_result()
                .unwrap();
        // SAFETY: `inherited_fd` is a new owned descriptor.
        let mut inherited = unsafe { UnixStream::from_raw_fd(inherited_fd) };

        expand_fd_table_to(inherited.as_raw_fd()).unwrap();

        peer.write_all(b"x").unwrap();
        let mut byte = [0];
        inherited.read_exact(&mut byte).unwrap();
        assert_eq!(byte, [b'x']);
    }
}
