// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::cleanup_socket;
use anyhow::Context;
use pal_async::driver::Driver;
#[cfg(windows)]
use pal_async::pipe::PolledPipe;
use serial_socket::net::OpenSocketSerialConfig;
use std::fs::File;
use std::io;
#[cfg(unix)]
use std::io::Read;
use std::net::SocketAddr;
use std::net::TcpStream;
use std::path::Path;
use unix_socket::UnixListener;
use vm_resource::IntoResource;
use vm_resource::Resource;
use vm_resource::kind::SerialBackendHandle;

#[cfg(unix)]
pub fn anonymous_serial_pair(
    driver: &(impl Driver + ?Sized),
) -> io::Result<(
    Resource<SerialBackendHandle>,
    pal_async::socket::PolledSocket<unix_socket::UnixStream>,
)> {
    let (left, right) = unix_socket::UnixStream::pair()?;
    let right = pal_async::socket::PolledSocket::new(driver, right)?;
    Ok((OpenSocketSerialConfig::from(left).into_resource(), right))
}

#[cfg(windows)]
pub fn anonymous_serial_pair(
    driver: &(impl Driver + ?Sized),
) -> io::Result<(Resource<SerialBackendHandle>, PolledPipe)> {
    use serial_socket::windows::OpenWindowsPipeSerialConfig;

    // Use named pipes on Windows even though we also support Unix sockets
    // there. This avoids an unnecessary winsock dependency.
    let (server, client) = pal::windows::pipe::bidirectional_pair(false)?;
    let server = PolledPipe::new(driver, server)?;
    // Use the client for the VM side so that it does not try to reconnect
    // (which isn't possible via pal_async for pipes opened in non-overlapped
    // mode, anyway).
    Ok((
        OpenWindowsPipeSerialConfig::from(client).into_resource(),
        server,
    ))
}

pub fn bind_serial(path: &Path) -> io::Result<Resource<SerialBackendHandle>> {
    bind_serial_inner(path, true)
}

/// Binds a listener without removing an existing socket path.
pub fn bind_serial_without_cleanup(path: &Path) -> io::Result<Resource<SerialBackendHandle>> {
    bind_serial_inner(path, false)
}

#[cfg(unix)]
pub fn bind_control_serial(path: &Path) -> io::Result<Resource<SerialBackendHandle>> {
    use std::os::unix::fs::FileTypeExt;
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::fs::PermissionsExt;

    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "control endpoint has no parent directory",
        )
    })?;
    let parent_metadata = fs_err::symlink_metadata(parent)?;
    if parent_metadata.file_type().is_symlink()
        || !parent_metadata.is_dir()
        || parent_metadata.uid() != pal::unix::effective_user_id()
        || parent_metadata.mode() & 0o7777 != 0o700
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "control endpoint parent must be a non-symlink directory owned by OpenVMM with mode 0700",
        ));
    }

    let control_listener = openvmm_defs::profile::ProfileSpan::start();
    let listener = UnixListener::bind(path)?;
    let bound_metadata = fs_err::symlink_metadata(path)?;
    let prepare_result = (|| {
        fs_err::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        let socket_metadata = fs_err::symlink_metadata(path)?;
        if !socket_metadata.file_type().is_socket()
            || socket_metadata.dev() != bound_metadata.dev()
            || socket_metadata.ino() != bound_metadata.ino()
            || socket_metadata.uid() != parent_metadata.uid()
            || socket_metadata.mode() & 0o7777 != 0o600
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "control endpoint must be an owned Unix socket with mode 0600",
            ));
        }
        Ok(())
    })();
    if let Err(error) = prepare_result {
        drop(listener);
        if let Ok(current) = fs_err::symlink_metadata(path)
            && current.file_type().is_socket()
            && current.dev() == bound_metadata.dev()
            && current.ino() == bound_metadata.ino()
        {
            let _ = fs_err::remove_file(path);
        }
        return Err(error);
    }
    control_listener.complete_milestone("startup", "control_listener_ready", Default::default());
    Ok(OpenSocketSerialConfig::from(listener).into_resource())
}

#[cfg(not(unix))]
pub fn bind_control_serial(_path: &Path) -> io::Result<Resource<SerialBackendHandle>> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "secure control-console named pipes are not implemented",
    ))
}

#[cfg(unix)]
pub fn read_control_capability(raw_handle: u64) -> io::Result<[u8; 32]> {
    use std::os::unix::fs::FileTypeExt;

    let mut file = pal::take_inherited_file(raw_handle)?;
    if !file.metadata()?.file_type().is_fifo() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "control authentication handle is not a one-way pipe",
        ));
    }
    pal::unix::pipe::set_nonblocking(&file, true)?;

    let mut bytes = [0u8; 33];
    let mut count = 0;
    loop {
        match file.read(&mut bytes[count..]) {
            Ok(0) => break,
            Ok(read) => {
                count += read;
                if count == bytes.len() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "control authentication payload has an invalid length",
                    ));
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "control authentication writer was not closed",
                ));
            }
            Err(error) => return Err(error),
        }
    }
    bytes[..count].try_into().map_err(|_| {
        io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "control authentication payload has an invalid length",
        )
    })
}

fn bind_serial_inner(
    path: &Path,
    cleanup_existing: bool,
) -> io::Result<Resource<SerialBackendHandle>> {
    #[cfg(windows)]
    {
        use serial_socket::windows::OpenWindowsPipeSerialConfig;

        if path.starts_with("//./pipe") {
            let pipe = pal::windows::pipe::new_named_pipe(
                path,
                windows_sys::Win32::Foundation::GENERIC_READ
                    | windows_sys::Win32::Foundation::GENERIC_WRITE,
                pal::windows::pipe::Disposition::Create,
                pal::windows::pipe::PipeMode::Byte,
            )?;
            return Ok(OpenWindowsPipeSerialConfig::from(pipe).into_resource());
        }
    }

    if cleanup_existing {
        cleanup_socket(path);
    }
    Ok(OpenSocketSerialConfig::from(UnixListener::bind(path)?).into_resource())
}

/// Connect to an existing named pipe or Unix domain socket as a client.
///
/// Unlike [`bind_serial`], which creates a new server, this function connects
/// to a pipe or socket that already exists.
pub fn connect_serial(path: &Path) -> io::Result<Resource<SerialBackendHandle>> {
    #[cfg(windows)]
    {
        use serial_socket::windows::OpenWindowsPipeSerialConfig;

        if path.starts_with("//./pipe") {
            let pipe = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(path)?;
            return Ok(OpenWindowsPipeSerialConfig::from(pipe).into_resource());
        }
    }

    Ok(OpenSocketSerialConfig::from(unix_socket::UnixStream::connect(path)?).into_resource())
}

pub fn connect_serial_with_timeout(
    path: &Path,
    timeout: std::time::Duration,
) -> io::Result<Resource<SerialBackendHandle>> {
    let path = path.to_owned();
    let (send, recv) = std::sync::mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("serial-connect".to_owned())
        .spawn(move || {
            let _ = send.send(connect_serial(&path));
        })?;
    match recv.recv_timeout(timeout) {
        Ok(result) => result,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!("serial endpoint did not connect within {timeout:?}"),
        )),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err(io::Error::other(
            "serial connection worker terminated without a result",
        )),
    }
}

/// Connects a single-use restore-readiness event sink.
pub fn connect_restore_ready_sink(path: &Path) -> io::Result<File> {
    #[cfg(unix)]
    {
        use std::os::fd::OwnedFd;

        let socket = unix_socket::UnixStream::connect(path)?;
        Ok(File::from(OwnedFd::from(socket)))
    }

    #[cfg(windows)]
    {
        const NAMED_PIPE_PREFIX: &str = "//./pipe/";

        let normalized = path.to_string_lossy().replace('\\', "/");
        if !normalized.starts_with(NAMED_PIPE_PREFIX) || normalized.len() == NAMED_PIPE_PREFIX.len()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "restore readiness path must name a Windows //./pipe/... endpoint",
            ));
        }
        std::fs::OpenOptions::new().write(true).open(path)
    }
}

pub fn bind_tcp_serial(addr: &SocketAddr) -> anyhow::Result<Resource<SerialBackendHandle>> {
    let listener = std::net::TcpListener::bind(addr)
        .with_context(|| format!("failed to bind tcp address {addr}"))?;
    Ok(OpenSocketSerialConfig::from(listener).into_resource())
}

pub fn connect_tcp_serial(
    addr: &SocketAddr,
    timeout: std::time::Duration,
) -> anyhow::Result<Resource<SerialBackendHandle>> {
    let stream = TcpStream::connect_timeout(addr, timeout)
        .with_context(|| format!("failed to connect to tcp address {addr}"))?;
    Ok(OpenSocketSerialConfig::from(stream).into_resource())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::DirBuilderExt;
    use std::os::unix::fs::FileTypeExt;
    use std::os::unix::fs::MetadataExt;
    use test_with_tracing::test;

    #[test]
    fn control_listener_has_private_permissions_and_exclusive_path() {
        let mut nonce = [0u8; 8];
        getrandom::fill(&mut nonce).unwrap();
        let directory = std::env::current_dir().unwrap().join(format!(
            ".control-endpoint-test-{:016x}",
            u64::from_ne_bytes(nonce)
        ));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&directory)
            .unwrap();
        let path = directory.join("control.sock");
        let cleanup_path = path.clone();
        let cleanup_directory = directory.clone();
        let _cleanup = pal::ScopeExit::new(move || {
            let _ = fs_err::remove_file(cleanup_path);
            let _ = fs_err::remove_dir(cleanup_directory);
        });

        let listener = bind_control_serial(&path).unwrap();
        let metadata = fs_err::symlink_metadata(&path).unwrap();
        assert!(metadata.file_type().is_socket());
        assert_eq!(metadata.mode() & 0o777, 0o600);
        assert_eq!(metadata.uid(), pal::unix::effective_user_id());
        assert!(bind_control_serial(&path).is_err());
        drop(listener);
    }
}
