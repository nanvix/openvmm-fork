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
