// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Worker for the prototype gRPC/ttrpc management endpoint.

#![cfg(any(feature = "ttrpc", feature = "grpc"))]

// The fd-passing protocol relies on `SCM_RIGHTS` and so exists only on unix.
#[cfg(unix)]
mod fd_passing;

#[cfg(unix)]
use fd_passing::FdRegistry;

/// On non-unix platforms the fd-passing protocol does not exist. The registry
/// is still threaded through the shared NIC configuration code, so provide an
/// empty placeholder there; it is never populated or resolved.
#[cfg(not(unix))]
#[derive(Clone, Default)]
struct FdRegistry {}

use crate::cli_args::GuestPowerAction;
use crate::cli_args::SerialConfigCli;
use crate::meshworker::VmmMesh;
use crate::serial_io::bind_serial;
use crate::serial_io::bind_serial_without_cleanup;
use crate::serial_io::connect_serial;
use crate::vm_controller::GuestPowerActions;
use crate::vm_controller::InspectTarget;
use crate::vm_controller::VmController;
use crate::vm_controller::VmControllerEvent;
use crate::vm_controller::VmControllerRpc;
use anyhow::Context;
use anyhow::anyhow;
use anyhow::bail;
use chipset_resources::microvm::MicrovmPortbHandle;
use chipset_resources::microvm::MicrovmShutdownHandle;
use chipset_resources::microvm::MicrovmSnapshotRequestHandle;
use futures::FutureExt;
use futures::StreamExt;
use guid::Guid;
use inspect::InspectionBuilder;
use inspect_proto::InspectResponse2;
use inspect_proto::InspectService;
use inspect_proto::UpdateResponse2;
use memory_range::MemoryRange;
use mesh::CancelReason;
use mesh::MeshPayload;
use mesh::error::RemoteError;
use mesh::rpc::RpcSend;
use mesh_rpc::service::Code;
use mesh_rpc::service::Status;
use mesh_worker::Worker;
use mesh_worker::WorkerId;
use mesh_worker::WorkerRpc;
use net_backend_resources::consomme::ConsommeRequest;
use net_backend_resources::consomme::HostPort;
use net_backend_resources::consomme::HostPortConfig;
use net_backend_resources::consomme::HostPortProtocol;
use net_backend_resources::mac_address::MacAddress;
use netvsp_resources::NetvspHandle;
use openvmm_defs::config::ArchTopologyConfig;
use openvmm_defs::config::Config;
use openvmm_defs::config::DeviceVtl;
use openvmm_defs::config::HypervisorConfig;
use openvmm_defs::config::LoadMode;
use openvmm_defs::config::MICROVM_ABI_VERSION_1;
use openvmm_defs::config::MachineProfile as OpenvmmMachineProfile;
use openvmm_defs::config::MemoryConfig;
use openvmm_defs::config::NumaDistance;
use openvmm_defs::config::NumaNode;
use openvmm_defs::config::NumaTopology;
use openvmm_defs::config::PcieDeviceConfig;
use openvmm_defs::config::PcieGenericInitiatorConfig;
use openvmm_defs::config::PcieMmioRangeConfig;
use openvmm_defs::config::PciePortConfig;
use openvmm_defs::config::PcieRootComplexConfig;
use openvmm_defs::config::PcieSwitchConfig;
use openvmm_defs::config::ProcessorTopologyConfig;
use openvmm_defs::config::UefiConsoleMode;
use openvmm_defs::config::VirtioBus;
use openvmm_defs::config::VmbusConfig;
use openvmm_defs::config::VpAssignment;
use openvmm_defs::config::VpciDeviceConfig;
use openvmm_defs::config::X86TopologyConfig;
use openvmm_defs::config::build_microvm_command_line;
use openvmm_defs::rpc::VmRpc;
use openvmm_defs::worker::VM_WORKER;
use openvmm_defs::worker::VmWorkerParameters;
use openvmm_helpers::disk::OpenDiskOptions;
use openvmm_helpers::disk::open_disk_type;
use openvmm_ttrpc_vmservice as vmservice;
use pal_async::DefaultDriver;
use pal_async::DefaultPool;
use pal_async::task::Spawn;
use pal_async::task::Task;
use scsidisk_resources::SimpleScsiDiskHandle;
use serial_core::resources::DisconnectedSerialBackendHandle;
use std::fs::File;
use std::future::Future;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use storvsp_resources::ScsiControllerHandle;
use storvsp_resources::ScsiControllerRequest;
use storvsp_resources::ScsiDeviceAndPath;
use unix_socket::UnixListener;
use virtio_resources::VirtioPciDeviceHandle;
use vm_manifest_builder::VmManifestBuilder;
use vm_resource::IntoResource;
use vm_resource::Resource;
use vm_resource::ResourceId;
use vm_resource::kind::DiskHandleKind;
use vm_resource::kind::NetEndpointHandleKind;
use vm_resource::kind::PciDeviceHandleKind;
use vm_resource::kind::SerialBackendHandle;
use vm_resource::kind::VirtioDeviceHandle;
use vm_resource::kind::VmbusDeviceHandleKind;
use vmcore::non_volatile_store::resources::EphemeralNonVolatileStoreHandle;
use vmotherboard::ChipsetDeviceHandle;

#[derive(mesh::MeshPayload)]
pub struct Parameters {
    pub listener: UnixListener,
    pub transport: RpcTransport,
}

#[derive(Copy, Clone, mesh::MeshPayload)]
pub enum RpcTransport {
    Ttrpc,
    Grpc,
    /// Auto-detect ttrpc vs. gRPC per connection, based on the first byte of
    /// the stream.
    Auto,
}

impl std::fmt::Display for RpcTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.pad(match self {
            RpcTransport::Ttrpc => "ttrpc",
            RpcTransport::Grpc => "grpc",
            RpcTransport::Auto => "auto",
        })
    }
}

#[derive(Copy, Clone)]
enum ResolvedTransport {
    #[cfg(feature = "ttrpc")]
    Ttrpc,
    #[cfg(feature = "grpc")]
    Grpc,
    Auto,
}

impl ResolvedTransport {
    /// Returns whether ttrpc connections are permitted in this mode.
    #[cfg(feature = "ttrpc")]
    fn allows_ttrpc(self) -> bool {
        match self {
            ResolvedTransport::Ttrpc => true,
            #[cfg(feature = "grpc")]
            ResolvedTransport::Grpc => false,
            ResolvedTransport::Auto => true,
        }
    }

    /// Returns whether gRPC connections are permitted in this mode.
    #[cfg(feature = "grpc")]
    fn allows_grpc(self) -> bool {
        match self {
            #[cfg(feature = "ttrpc")]
            ResolvedTransport::Ttrpc => false,
            ResolvedTransport::Grpc => true,
            ResolvedTransport::Auto => true,
        }
    }
}

/// The RPC server accept loop.
///
/// This owns the accept loop rather than delegating to
/// [`mesh_rpc::Server::run`], so that the protocol used for each connection can
/// be chosen based on the compiled-in features and the configured transport,
/// including auto-detecting ttrpc vs. gRPC from the first byte on the wire.
///
/// Neither ttrpc nor gRPC has an explicit negotiation phase, but their first
/// byte on the wire is distinct, so a single non-consuming peek is enough to
/// classify a connection:
///
/// * ttrpc frames begin with a big-endian `u32` length field whose most
///   significant byte is always zero (messages are capped well under 16 MiB),
///   so the first byte is `0x00`.
/// * gRPC uses HTTP/2 cleartext, whose client connection preface begins with
///   the ASCII bytes `"PRI "`, so the first byte is `b'P'`.
/// * The OpenVMM fd-passing protocol (UNIX only) begins with a handshake whose
///   first byte is `0xFD`, distinct from both of the above.
mod dispatch {
    use super::FdRegistry;
    use super::ResolvedTransport;
    use futures::FutureExt;
    use pal_async::driver::Driver;
    use pal_async::socket::AsSockRef;
    use pal_async::socket::PolledSocket;
    use std::io::Read;
    use std::io::Write;
    use unicycle::FuturesUnordered;
    use unix_socket::UnixListener;
    use unix_socket::UnixStream;

    /// Runs the RPC server, listening on `listener` and servicing connections
    /// until `cancel`, dispatching each connection according to `transport`.
    pub(super) async fn run(
        server: &mesh_rpc::Server,
        driver: &(impl Driver + ?Sized),
        listener: UnixListener,
        cancel: mesh::OneshotReceiver<()>,
        transport: ResolvedTransport,
        registry: FdRegistry,
    ) -> anyhow::Result<()> {
        let mut listener = PolledSocket::new(driver, listener)?;
        let mut tasks = FuturesUnordered::new();
        let mut cancel = cancel.fuse();
        loop {
            let conn = futures::select! { // merge semantics
                r = listener.accept().fuse() => r,
                _ = tasks.next() => continue,
                _ = cancel => break,
            };
            if let Ok(conn) = conn.and_then(|(conn, _)| PolledSocket::new(driver, conn)) {
                let registry = registry.clone();
                tasks.push(async move {
                    let _ = serve(server, conn, transport, &registry)
                        .await
                        .map_err(|err| {
                            tracing::error!(
                                error = err.as_ref() as &dyn std::error::Error,
                                "connection error"
                            )
                        });
                });
            }
        }
        Ok(())
    }

    /// Services a single connection.
    ///
    /// The protocol is always determined by peeking the first byte of the
    /// stream; the configured `transport` only restricts which protocols are
    /// permitted (e.g. a ttrpc-only server rejects a gRPC client).
    async fn serve(
        server: &mesh_rpc::Server,
        mut conn: PolledSocket<UnixStream>,
        transport: ResolvedTransport,
        registry: &FdRegistry,
    ) -> anyhow::Result<()> {
        // Wait for the client to send data (returning early if it hangs up
        // first) and classify the protocol from its first byte.
        let Some(first_byte) = peek_first_byte(&mut conn).await? else {
            return Ok(());
        };

        // The fd-passing protocol (UNIX only) is allowed in every transport
        // mode; it shares the socket with ttrpc/gRPC and is selected by its
        // distinct magic first byte.
        #[cfg(not(unix))]
        let _ = registry;

        match first_byte {
            #[cfg(feature = "ttrpc")]
            0x00 if transport.allows_ttrpc() => server.serve_connection(conn).await,
            #[cfg(feature = "grpc")]
            b'P' if transport.allows_grpc() => server.serve_connection_grpc(conn).await,
            #[cfg(unix)]
            super::fd_passing::MAGIC_FIRST_BYTE => super::fd_passing::serve(conn, registry).await,
            byte => {
                anyhow::bail!("unrecognized or disallowed rpc protocol (first byte {byte:#04x})")
            }
        }
    }

    /// Waits for the first byte of the stream to be available and returns it
    /// without consuming it, so the chosen protocol handler sees a pristine
    /// stream.
    ///
    /// Returns `None` if the peer closed the connection before sending any data.
    async fn peek_first_byte(
        conn: &mut PolledSocket<impl AsSockRef + Read + Write>,
    ) -> std::io::Result<Option<u8>> {
        let mut buf = [0u8; 1];
        let n = conn.peek(&mut buf).await?;
        Ok((n != 0).then_some(buf[0]))
    }
}

pub struct TtrpcWorker {
    listener: UnixListener,
    transport: ResolvedTransport,
}

pub const TTRPC_WORKER: WorkerId<Parameters> = WorkerId::new("TtrpcWorker");

impl Worker for TtrpcWorker {
    type Parameters = Parameters;
    type State = ();
    const ID: WorkerId<Self::Parameters> = TTRPC_WORKER;

    fn new(parameters: Self::Parameters) -> anyhow::Result<Self> {
        Ok(Self {
            listener: parameters.listener,
            transport: match parameters.transport {
                #[cfg(feature = "ttrpc")]
                RpcTransport::Ttrpc => ResolvedTransport::Ttrpc,
                #[cfg(feature = "grpc")]
                RpcTransport::Grpc => ResolvedTransport::Grpc,
                RpcTransport::Auto => ResolvedTransport::Auto,
                #[expect(clippy::allow_attributes)]
                #[allow(unreachable_patterns)]
                transport => bail!("unsupported transport {transport}"),
            },
        })
    }

    fn restart(_state: Self::State) -> anyhow::Result<Self> {
        bail!("not yet supported");
    }

    fn run(self, recv: mesh::Receiver<WorkerRpc<Self::State>>) -> anyhow::Result<()> {
        DefaultPool::run_with(async |driver| {
            let mut service = VmService {
                driver,
                vm: None,
                vm_controller: None,
                vm_controller_events: None,
                controller_task: None,
                wait_vm_response: None,
                lifecycle: VmLifecycle::Uninitialized,
                rpc_tasks: Vec::new(),
                transport: self.transport,
                registry: FdRegistry::default(),
            };
            service.run(self.listener, recv).await?;
            Ok(())
        })
    }
}

impl VmService {
    async fn run(
        &mut self,
        listener: UnixListener,
        mut recv: mesh::Receiver<WorkerRpc<()>>,
    ) -> anyhow::Result<()> {
        let mut server = mesh_rpc::Server::new();
        let mut vm_service_recv = server.add_service::<vmservice::Vm>();
        let mut inspect_service_recv = server.add_service::<InspectService>();

        let transport = self.transport;
        let registry = self.registry.clone();
        let (cancel_send, cancel_recv) = mesh::oneshot();
        let server_task = self.driver.spawn("ttrpc-server", {
            let driver = self.driver.clone();
            async move {
                let r = dispatch::run(&server, &driver, listener, cancel_recv, transport, registry)
                    .await;
                match &r {
                    Ok(()) => tracing::debug!("ttrpc server shutting down"),
                    Err(err) => tracing::error!(
                        error = err.as_ref() as &dyn std::error::Error,
                        "ttrpc server error"
                    ),
                }
                r
            }
        });

        let quit = loop {
            // Take the controller events receiver out of self so it can be
            // polled in the select without borrowing self.
            let mut ctrl_events = self.vm_controller_events.take();
            let ctrl_fut = async {
                match &mut ctrl_events {
                    Some(recv) => recv.next().await,
                    None => std::future::pending().await,
                }
            };

            // Clone the WaitVm cancel context so we can poll it without
            // borrowing self.
            let mut wait_cancel_ctx = self.wait_vm_response.as_mut().map(|(ctx, _)| ctx.clone());
            let wait_cancel_fut = async {
                match &mut wait_cancel_ctx {
                    Some(ctx) => Some(ctx.cancelled().await),
                    None => std::future::pending().await,
                }
            };

            enum Action {
                VmService(Box<Option<(mesh::CancelContext, vmservice::Vm)>>),
                InspectService(Option<(mesh::CancelContext, InspectService)>),
                WorkerRpc(Result<WorkerRpc<()>, mesh::RecvError>),
                ControllerEvent(Option<VmControllerEvent>),
                WaitVmCancelled(CancelReason),
            }

            let action = futures::select! { // merge semantics
                m = vm_service_recv.next() => Action::VmService(Box::new(m)),
                m = inspect_service_recv.next() => Action::InspectService(m),
                r = recv.recv().fuse() => Action::WorkerRpc(r),
                e = ctrl_fut.fuse() => Action::ControllerEvent(e),
                reason = wait_cancel_fut.fuse() => Action::WaitVmCancelled(reason.unwrap()),
            };

            // Restore controller events (unless the channel closed).
            if let Action::ControllerEvent(None) = &action {
                tracing::debug!("controller event channel closed");
            } else {
                self.vm_controller_events = ctrl_events;
            }

            match action {
                Action::VmService(message) => match *message {
                    Some((ctx, message)) => match self.handle(ctx, message).await {
                        HandleAction::None => (),
                        HandleAction::Quit => break true,
                    },
                    None => {
                        tracing::debug!("no more ttrpc requests");
                        break false;
                    }
                },
                Action::InspectService(Some((ctx, message))) => {
                    self.handle_inspect(ctx, message).await;
                }
                Action::InspectService(None) => {
                    tracing::debug!("no more ttrpc requests");
                    break false;
                }
                Action::WorkerRpc(Ok(WorkerRpc::Restart(rpc))) => {
                    rpc.complete(Err(RemoteError::new(anyhow::anyhow!("not supported"))));
                }
                Action::WorkerRpc(Ok(WorkerRpc::Inspect(_))) => (),
                Action::WorkerRpc(Ok(WorkerRpc::Stop)) => {
                    tracing::info!("ttrpc worker stopping");
                    break false;
                }
                Action::WorkerRpc(Err(err)) => {
                    tracing::info!(
                        error = &err as &dyn std::error::Error,
                        "ttrpc worker tearing down"
                    );
                    break false;
                }
                Action::ControllerEvent(Some(event)) => {
                    if self.handle_controller_event(event) {
                        break true;
                    }
                }
                Action::ControllerEvent(None) => {} // handled above
                Action::WaitVmCancelled(reason) => {
                    tracing::debug!("WaitVm client cancelled");
                    if let Some((_, response)) = self.wait_vm_response.take() {
                        response.send(Err(grpc_error(anyhow::Error::new(reason))));
                    }
                }
            }
        };

        // If the controller is still alive (non-Quit exit), shut it down.
        if !quit {
            if let Some(controller) = self.vm_controller.take() {
                controller.send(VmControllerRpc::Quit);
            }
        }
        if let Some(task) = self.controller_task.take() {
            task.await;
        }

        // Complete any pending WaitVm with an error.
        if let Some((_, response)) = self.wait_vm_response.take() {
            response.send(Err(grpc_error(anyhow!("server shutting down"))));
        }

        // Drain any remaining RPCs.
        futures::future::join_all(self.rpc_tasks.drain(..)).await;
        if let Some(vm) = self.vm.take() {
            let _ = Arc::try_unwrap(vm).ok().expect("no more VM references");
        }
        drop(cancel_send);
        server_task.await
    }

    fn start_rpc<F, R>(
        &mut self,
        response: mesh::OneshotSender<Result<R, Status>>,
        r: anyhow::Result<F>,
    ) where
        F: 'static + Future<Output = anyhow::Result<R>> + Send,
        R: 'static + MeshPayload + Send,
    {
        match r {
            Ok(fut) => {
                let task = self.driver.spawn("ttrpc-rpc", async move {
                    response.send(map_grpc(fut.await));
                });
                self.rpc_tasks.push(task);
            }
            Err(err) => response.send(Err(grpc_error(err))),
        }
    }
}

struct Vm {
    worker_rpc: mesh::Sender<VmRpc>,
    scsi_rpc: Option<mesh::Sender<ScsiControllerRequest>>,
    consomme_rpc: Option<mesh::Sender<ConsommeRequest>>,
}

struct AuthoritativeMicrovmRestore {
    path: PathBuf,
    memory_size: u64,
    vp_count: u32,
    machine_contract: openvmm_helpers::snapshot::SnapshotMachineContract,
}

enum VmLifecycle {
    Uninitialized,
    Running,
    Paused,
    Halted(String),
}

impl From<&VmLifecycle> for vmservice::VmState {
    fn from(lifecycle: &VmLifecycle) -> Self {
        match lifecycle {
            VmLifecycle::Uninitialized => vmservice::VmState::Uninitialized,
            VmLifecycle::Running => vmservice::VmState::Running,
            VmLifecycle::Paused => vmservice::VmState::Paused,
            VmLifecycle::Halted(_) => vmservice::VmState::Halted,
        }
    }
}

struct VmService {
    driver: DefaultDriver,
    vm: Option<Arc<Vm>>,
    vm_controller: Option<mesh::Sender<VmControllerRpc>>,
    vm_controller_events: Option<mesh::Receiver<VmControllerEvent>>,
    controller_task: Option<Task<()>>,
    wait_vm_response: Option<(mesh::CancelContext, mesh::OneshotSender<Result<(), Status>>)>,
    lifecycle: VmLifecycle,
    rpc_tasks: Vec<Task<()>>,
    transport: ResolvedTransport,
    /// Registry of file descriptors passed in over the fd-passing protocol,
    /// resolvable by name (e.g. for tap NIC backends).
    registry: FdRegistry,
}

fn grpc_error(err: anyhow::Error) -> Status {
    let root_cause = err.root_cause();
    let code = if let Some(code) = root_cause.downcast_ref::<Code>() {
        *code
    } else if let Some(reason) = root_cause.downcast_ref::<CancelReason>() {
        match reason {
            CancelReason::Cancelled => Code::Cancelled,
            CancelReason::DeadlineExceeded => Code::DeadlineExceeded,
        }
    } else {
        Code::Unknown
    };
    Status {
        code: code.into(),
        message: format!("{:#}", err),
        details: vec![],
    }
}

fn map_grpc<T>(r: anyhow::Result<T>) -> Result<T, Status> {
    r.map_err(grpc_error)
}

enum HandleAction {
    None,
    Quit,
}

impl VmService {
    async fn handle(&mut self, ctx: mesh::CancelContext, request: vmservice::Vm) -> HandleAction {
        tracing::debug!(?request, "request");
        match request {
            vmservice::Vm::CreateVm(request, response) => {
                response.send(map_grpc(self.create_vm(request).await))
            }
            vmservice::Vm::TeardownVm((), response) => {
                response.send(map_grpc(self.teardown_vm().await))
            }
            vmservice::Vm::Quit((), response) => {
                // Shut down the controller (which stops and joins the worker).
                // Drop the VM's device RPC channels first; see `teardown_vm`.
                self.vm.take();
                if let Some(controller) = self.vm_controller.take() {
                    controller.send(VmControllerRpc::Quit);
                }
                if let Some(task) = self.controller_task.take() {
                    task.await;
                }
                self.vm_controller_events.take();
                if let Some((_, wait_response)) = self.wait_vm_response.take() {
                    wait_response.send(Err(grpc_error(anyhow!("VM quit"))));
                }
                response.send(Ok(()));
                return HandleAction::Quit;
            }
            vmservice::Vm::CapabilitiesVm((), response) => {
                response.send(Ok(self.build_capabilities()));
            }
            vmservice::Vm::PropertiesVm(_request, response) => {
                response.send(Ok(self.build_properties()));
            }
            vmservice::Vm::PauseVm((), response) => {
                response.send(map_grpc(self.pause_vm().await));
            }
            vmservice::Vm::ResumeVm((), response) => {
                response.send(map_grpc(self.resume_vm().await));
            }
            vmservice::Vm::WaitVm((), response) => {
                if self.vm.is_none() {
                    response.send(Err(grpc_error(anyhow!("VM not created yet"))));
                } else if self.wait_vm_response.is_some() {
                    response.send(Err(grpc_error(anyhow!("wait VM already in flight"))));
                } else if matches!(self.lifecycle, VmLifecycle::Halted(_)) {
                    response.send(Ok(()));
                } else {
                    self.wait_vm_response = Some((ctx.clone(), response));
                }
            }
            vmservice::Vm::ModifyResource(request, response) => {
                let r = self.modify_resource(request);
                self.start_rpc(response, r);
            }
            vmservice::Vm::AddPcieDevice(request, response) => {
                let r = self.add_pcie_device(request);
                self.start_rpc(response, r);
            }
            vmservice::Vm::RemovePcieDevice(request, response) => {
                let r = self.remove_pcie_device(request);
                self.start_rpc(response, r);
            }
        }
        HandleAction::None
    }

    async fn handle_inspect(&mut self, ctx: mesh::CancelContext, request: InspectService) {
        match request {
            InspectService::Inspect(request, response) => {
                self.start_rpc(response, Ok(self.inspect(ctx, request)))
            }
            InspectService::Update(request, response) => {
                self.start_rpc(response, Ok(self.update(ctx, request)))
            }
        }
    }

    fn inspect(
        &self,
        ctx: mesh::CancelContext,
        request: inspect_proto::InspectRequest,
    ) -> impl Future<Output = anyhow::Result<InspectResponse2>> + use<> {
        let mut inspection = InspectionBuilder::new(&request.path)
            .depth(Some(request.depth as usize))
            .inspect(inspect::adhoc(|req| {
                if let Some(controller) = &self.vm_controller {
                    controller.send(VmControllerRpc::Inspect(InspectTarget::Host, req.defer()));
                }
            }));
        async move {
            let _ = ctx
                .with_timeout(Duration::from_secs(1))
                .until_cancelled(inspection.resolve())
                .await;
            let result = inspection.results();
            let response = InspectResponse2 { result };
            Ok(response)
        }
    }

    fn update(
        &self,
        ctx: mesh::CancelContext,
        request: inspect_proto::UpdateRequest,
    ) -> impl Future<Output = anyhow::Result<UpdateResponse2>> + use<> {
        let update = inspect::update(
            &request.path,
            &request.value,
            inspect::adhoc(|req| {
                if let Some(controller) = &self.vm_controller {
                    controller.send(VmControllerRpc::Inspect(InspectTarget::Host, req.defer()));
                }
            }),
        );
        async move {
            let new_value = ctx
                .with_timeout(Duration::from_secs(1))
                .until_cancelled(update)
                .await??;
            let response = UpdateResponse2 { new_value };
            Ok(response)
        }
    }

    async fn create_vm(&mut self, request: vmservice::CreateVmRequest) -> anyhow::Result<()> {
        if self.vm.is_some() {
            bail!("VM already created");
        }

        let vmservice::CreateVmRequest {
            config: requested_config,
            microvm_snapshot,
            ..
        } = request;
        let vmservice::MicrovmSnapshotConfig {
            destination_path,
            restore_path,
            restore_entropy,
            quiesce_timeout_ms,
        } = microvm_snapshot.unwrap_or_default();
        let resolve_path = |value: String| -> anyhow::Result<Option<PathBuf>> {
            if value.is_empty() {
                return Ok(None);
            }
            let path = PathBuf::from(value);
            if path.is_absolute() {
                Ok(Some(path))
            } else {
                Ok(Some(
                    std::env::current_dir()
                        .context("failed to resolve current directory")?
                        .join(path),
                ))
            }
        };
        let snapshot_destination = resolve_path(destination_path)?;
        let restore_path = resolve_path(restore_path)?;
        anyhow::ensure!(
            snapshot_destination.is_none() || restore_path.is_none(),
            "snapshot capture and restore paths are mutually exclusive"
        );
        anyhow::ensure!(
            !restore_entropy || restore_path.is_some(),
            "restore_entropy requires restore_path"
        );
        anyhow::ensure!(
            quiesce_timeout_ms == 0 || snapshot_destination.is_some(),
            "quiesce_timeout_ms requires destination_path"
        );
        let snapshot_quiesce_timeout = Duration::from_millis(if quiesce_timeout_ms == 0 {
            5_000
        } else {
            quiesce_timeout_ms
        });

        let authoritative_restore = if let Some(path) = restore_path {
            let manifest = openvmm_helpers::snapshot::read_snapshot_manifest(&path)?;
            openvmm_helpers::snapshot::validate_manifest(
                &manifest,
                crate::GUEST_ARCH,
                manifest.memory_size_bytes,
                1,
                crate::system_page_size(),
            )?;
            let machine_contract = manifest
                .machine_contract
                .context("microVM snapshot is missing its authoritative machine contract")?;
            anyhow::ensure!(
                machine_contract.machine_profile == "microvm"
                    && machine_contract.microvm_abi_version == MICROVM_ABI_VERSION_1,
                "snapshot is not a supported microVM ABI-v1 snapshot"
            );
            Some(AuthoritativeMicrovmRestore {
                path,
                memory_size: manifest.memory_size_bytes,
                vp_count: manifest.vp_count,
                machine_contract,
            })
        } else {
            None
        };

        let mut restore_console_config = None;
        let mut restore_filesystem_config = None;
        let mut req_config = if authoritative_restore.is_some() {
            let requested = requested_config.unwrap_or_else(|| vmservice::VmConfig {
                machine_profile: vmservice::vm_config::MachineProfile::Microvm as i32,
                ..Default::default()
            });
            anyhow::ensure!(
                requested.machine_profile == vmservice::vm_config::MachineProfile::Microvm as i32,
                "microVM snapshot restore requires the microVM machine profile"
            );
            anyhow::ensure!(
                requested.memory_config.is_none()
                    && requested.processor_config.is_none()
                    && requested.boot_config.is_none()
                    && requested.windows_options.is_none()
                    && requested.hvsocket_config.is_none()
                    && requested.numa_config.is_none()
                    && requested.pcie.is_none(),
                "restore configuration may contain only serial attachments and guest power actions"
            );
            if let Some(devices) = &requested.devices_config {
                anyhow::ensure!(
                    devices.scsi_disks.is_empty()
                        && devices.vpmem_disks.is_empty()
                        && devices.nic_config.is_empty()
                        && devices.windows_device.is_empty()
                        && devices.virtiofs_config.len() <= 1
                        && devices.virtio_blk.is_none(),
                    "restore devices_config may contain only one virtio-fs and one virtio-console attachment"
                );
                restore_console_config = devices.virtio_console.clone();
                restore_filesystem_config = devices.virtiofs_config.first().cloned();
            }
            vmservice::VmConfig {
                serial_config: requested.serial_config,
                guest_power_actions: requested.guest_power_actions,
                machine_profile: vmservice::vm_config::MachineProfile::Microvm as i32,
                ..Default::default()
            }
        } else {
            requested_config.context("missing configuration")?
        };

        let requested_profile =
            vmservice::vm_config::MachineProfile::from_i32(req_config.machine_profile)
                .with_context(|| {
                    format!("unknown machine profile {}", req_config.machine_profile)
                })?;
        let machine_profile = match requested_profile {
            vmservice::vm_config::MachineProfile::Standard => OpenvmmMachineProfile::Standard,
            vmservice::vm_config::MachineProfile::Microvm => OpenvmmMachineProfile::Microvm {
                abi_version: MICROVM_ABI_VERSION_1,
            },
        };
        let is_microvm = matches!(machine_profile, OpenvmmMachineProfile::Microvm { .. });
        anyhow::ensure!(
            is_microvm || (snapshot_destination.is_none() && authoritative_restore.is_none()),
            "microVM snapshot options require the microVM machine profile"
        );
        let hypervisor = if is_microvm {
            openvmm_helpers::hypervisor::choose_microvm_hypervisor()?
        } else {
            openvmm_helpers::hypervisor::choose_hypervisor()?
        };
        let source_hypervisor = hypervisor.id().to_owned();
        if let Some(restore) = &authoritative_restore {
            anyhow::ensure!(
                restore.machine_contract.source_hypervisor == source_hypervisor,
                "snapshot source hypervisor '{}' does not match destination '{}'",
                restore.machine_contract.source_hypervisor,
                source_hypervisor
            );
        }
        let restored_microvm_filesystem = if let Some(restore) = &authoritative_restore {
            let has_device = restore
                .machine_contract
                .devices
                .iter()
                .any(|device| device.stable_id == "fs:microvm0");
            let saved_policy = restore.machine_contract.microvm_filesystem.as_ref();
            let saved_attachment = restore
                .machine_contract
                .attachments
                .iter()
                .find(|attachment| attachment.stable_id == "fs:microvm0");
            anyhow::ensure!(
                has_device == saved_policy.is_some() && has_device == saved_attachment.is_some(),
                "snapshot microVM filesystem device, policy, and attachment inventories disagree"
            );
            match (saved_policy, saved_attachment) {
                (Some(saved_policy), Some(saved_attachment)) => {
                    let requested = restore_filesystem_config.as_ref().context(
                        "snapshot restore requires a fresh virtiofs_config attachment for fs:microvm0",
                    )?;
                    let config = crate::microvm_filesystem_from_snapshot(saved_policy)?;
                    anyhow::ensure!(
                        requested.tag == "microvm"
                            && requested.guest_mount_target == config.guest_mount_target
                            && requested.read_write != config.access.is_read_only()
                            && !requested.root_path.is_empty(),
                        "restore-time virtiofs_config does not match the snapshot policy"
                    );
                    let (root_path, attachment) =
                        crate::microvm_filesystem_attachment(Path::new(&requested.root_path))?;
                    anyhow::ensure!(
                        &attachment == saved_attachment,
                        "restore-time filesystem root identity does not match the snapshot attachment"
                    );
                    Some((config, root_path, attachment))
                }
                (None, None) => {
                    anyhow::ensure!(
                        restore_filesystem_config.is_none(),
                        "a restore-time virtiofs_config cannot be added to a snapshot without virtio-fs"
                    );
                    None
                }
                _ => anyhow::bail!(
                    "snapshot microVM filesystem policy and attachment inventories disagree"
                ),
            }
        } else {
            None
        };
        let prepared_restore = if let Some(restore) = &authoritative_restore {
            anyhow::ensure!(
                restore.machine_contract.microvm_network.is_none(),
                "ttrpc restore does not yet expose microVM network attachments"
            );
            let (fd, state, restore_time) = crate::prepare_snapshot_restore_for_config(
                &restore.path,
                restore.memory_size,
                restore.vp_count,
                Some((
                    &source_hypervisor,
                    &restore.machine_contract.effective_command_line,
                    None,
                    restored_microvm_filesystem
                        .as_ref()
                        .map(|(config, _, attachment)| (config, attachment)),
                    restore
                        .machine_contract
                        .attachments
                        .iter()
                        .find(|attachment| attachment.stable_id == "console:microvm-virtio0"),
                )),
                openvmm_helpers::snapshot::SnapshotMemoryVerification::Sha256,
            )?;
            let restore_time =
                restore_time.context("microVM snapshot is missing its restore-time contract")?;
            if !restore_entropy {
                tracing::warn!(
                    "restoring cloned guest RNG state without fresh entropy injection; cryptographic workloads are unsafe"
                );
            }
            Some((fd, state, restore_time))
        } else {
            None
        };
        let (microvm_snapshot_notify, microvm_snapshot_requests) = if is_microvm {
            let (notify, requests) = mesh::channel();
            (Some(notify), Some(requests))
        } else {
            (None, None)
        };
        let (snapshot_ready, snapshot_requests) = if is_microvm {
            let (ready, requests) = mesh::channel();
            (Some(ready), Some(requests))
        } else {
            (None, None)
        };
        if is_microvm {
            anyhow::ensure!(
                cfg!(guest_arch = "x86_64"),
                "microVM requires an x86-64 guest"
            );
            if authoritative_restore.is_none() {
                anyhow::ensure!(
                    matches!(
                        req_config.boot_config.as_ref(),
                        Some(vmservice::vm_config::BootConfig::PvhBoot(_))
                    ),
                    "the microVM profile requires pvh_boot"
                );
            }
            anyhow::ensure!(
                req_config
                    .processor_config
                    .as_ref()
                    .map(|config| config.processor_count)
                    .unwrap_or(1)
                    == 1,
                "microVM ABI version 1 requires exactly one vCPU"
            );
            anyhow::ensure!(
                req_config.numa_config.is_none(),
                "microVM ABI version 1 does not support custom NUMA topology"
            );
            anyhow::ensure!(
                req_config.pcie.is_none(),
                "microVM ABI version 1 does not support PCIe"
            );
            anyhow::ensure!(
                req_config.hvsocket_config.is_none(),
                "microVM ABI version 1 does not support hvsocket"
            );

            let serial_ports = req_config
                .serial_config
                .iter()
                .flat_map(|config| &config.ports)
                .collect::<Vec<_>>();
            anyhow::ensure!(
                serial_ports.len() <= 1 && serial_ports.iter().all(|port| port.port == 0),
                "microVM ABI version 1 accepts only serial port 0 as its portb endpoint"
            );
            if let Some(devices) = &req_config.devices_config {
                anyhow::ensure!(
                    devices.scsi_disks.is_empty()
                        && devices.vpmem_disks.is_empty()
                        && devices.nic_config.is_empty()
                        && devices.windows_device.is_empty()
                        && devices.virtiofs_config.len() <= 1,
                    "microVM ABI version 1 supports only one fixed virtio-fs, one fixed virtio-console, and optional virtio-blk devices"
                );
                if let Some(filesystem) = devices.virtiofs_config.first() {
                    anyhow::ensure!(
                        filesystem.tag == "microvm" && !filesystem.root_path.is_empty(),
                        "microVM virtio-fs requires tag 'microvm' and a host root path"
                    );
                    openvmm_defs::config::MicrovmFilesystemConfig::new(
                        filesystem.guest_mount_target.clone(),
                        if filesystem.read_write {
                            openvmm_defs::config::MicrovmFilesystemAccess::ReadWrite
                        } else {
                            openvmm_defs::config::MicrovmFilesystemAccess::ReadOnly
                        },
                    )?;
                }
                if let Some(console) = &devices.virtio_console {
                    anyhow::ensure!(
                        !console.socket_path.is_empty(),
                        "microVM virtio-console requires a socket path"
                    );
                }
                if snapshot_destination.is_some() {
                    anyhow::ensure!(
                        devices.virtio_blk.is_none(),
                        "microVM snapshot capture with virtio-blk requires immutable media identity"
                    );
                }
            }
        }
        // Snapshot the fd registry so tap NIC backends can resolve descriptors
        // passed in over the fd-passing protocol.
        let registry = self.registry.clone();

        // Serial ports are set up before the boot configuration because UEFI
        // needs to know whether any are present to decide whether to enable its
        // serial console.
        let mut ports = [(); 4].map(|_| None);
        for port in req_config.serial_config.iter().flat_map(|c| &c.ports) {
            let pc = ports
                .get_mut(port.port as usize)
                .context("invalid serial port")?;
            let (serial_fn, action) = open_socket_backend(port.connect);
            *pc = Some(serial_fn(port.socket_path.as_ref()).with_context(|| {
                format!("failed to {} serial socket: {}", action, port.socket_path)
            })?);
        }
        let any_serial_configured = ports.iter().any(|port| port.is_some());
        let com1_configured = ports[0].is_some();
        let has_requested_microvm_console = is_microvm
            && authoritative_restore.is_none()
            && req_config
                .devices_config
                .as_ref()
                .and_then(|devices| devices.virtio_console.as_ref())
                .is_some();

        #[cfg(guest_arch = "aarch64")]
        let arch = vm_manifest_builder::MachineArch::Aarch64;
        #[cfg(guest_arch = "x86_64")]
        let arch = vm_manifest_builder::MachineArch::X86_64;

        // The boot configuration also determines the base chipset, since the
        // firmware and the device model have to agree on the platform.
        let (load_mode, base_chipset_type, uefi_config) = if let Some(restore) =
            &authoritative_restore
        {
            (
                LoadMode::Pvh {
                    kernel: tempfile::tempfile()
                        .context("failed to create inert restore kernel handle")?,
                    initrd: None,
                    cmdline: restore.machine_contract.effective_command_line.clone(),
                },
                vm_manifest_builder::BaseChipsetType::Microvm,
                None,
            )
        } else {
            match req_config
                .boot_config
                .take()
                .context("missing boot configuration")?
            {
                vmservice::vm_config::BootConfig::DirectBoot(boot) => {
                    if matches!(machine_profile, OpenvmmMachineProfile::Microvm { .. }) {
                        bail!("the microVM profile requires pvh_boot");
                    }
                    let kernel = File::open(boot.kernel_path).context("failed to open kernel")?;
                    let initrd = if boot.initrd_path.is_empty() {
                        None
                    } else {
                        Some(File::open(boot.initrd_path).context("failed to open initrd")?)
                    };
                    (
                        LoadMode::Linux {
                            kernel,
                            initrd,
                            cmdline: boot.kernel_cmdline,
                            enable_serial: true,
                            boot_mode: openvmm_defs::config::LinuxDirectBootMode::Acpi,
                        },
                        vm_manifest_builder::BaseChipsetType::HyperVGen2LinuxDirect,
                        None,
                    )
                }
                vmservice::vm_config::BootConfig::PvhBoot(boot) => {
                    if !matches!(machine_profile, OpenvmmMachineProfile::Microvm { .. }) {
                        bail!("pvh_boot requires the microVM profile");
                    }
                    let kernel =
                        File::open(boot.kernel_path).context("failed to open PVH kernel")?;
                    let initrd = if boot.initrd_path.is_empty() {
                        None
                    } else {
                        Some(File::open(boot.initrd_path).context("failed to open PVH initrd")?)
                    };
                    (
                        LoadMode::Pvh {
                            kernel,
                            initrd,
                            cmdline: build_microvm_command_line(
                                &[boot.kernel_cmdline],
                                has_requested_microvm_console,
                            )?,
                        },
                        vm_manifest_builder::BaseChipsetType::Microvm,
                        None,
                    )
                }
                vmservice::vm_config::BootConfig::Uefi(uefi) => {
                    if matches!(machine_profile, OpenvmmMachineProfile::Microvm { .. }) {
                        bail!("the microVM profile requires pvh_boot");
                    }
                    let firmware = File::open(&uefi.firmware_path).with_context(|| {
                        format!("failed to open uefi firmware {}", uefi.firmware_path)
                    })?;
                    let initial_variables = uefi.initial_variables.unwrap_or_default();
                    let base_template_json = match (arch, initial_variables.secure_boot_template()) {
                    (_, vmservice::uefi::initial_variables::SecureBootTemplate::None) => {
                        None
                    }
                    (
                        vm_manifest_builder::MachineArch::X86_64,
                        vmservice::uefi::initial_variables::SecureBootTemplate::MicrosoftWindows,
                    ) => Some(
                        firmware_uefi_resources::x64_secure_boot_templates::microsoft_windows(),
                    ),
                    (
                        vm_manifest_builder::MachineArch::Aarch64,
                        vmservice::uefi::initial_variables::SecureBootTemplate::MicrosoftWindows,
                    ) => Some(
                        firmware_uefi_resources::aarch64_secure_boot_templates::microsoft_windows(),
                    ),
                    (
                        vm_manifest_builder::MachineArch::X86_64,
                        vmservice::uefi::initial_variables::SecureBootTemplate::MicrosoftUefiCertificateAuthority,
                    ) => Some(
                        firmware_uefi_resources::x64_secure_boot_templates::microsoft_uefi_ca(),
                    ),
                    (
                        vm_manifest_builder::MachineArch::Aarch64,
                        vmservice::uefi::initial_variables::SecureBootTemplate::MicrosoftUefiCertificateAuthority,
                    ) => Some(
                        firmware_uefi_resources::aarch64_secure_boot_templates::microsoft_uefi_ca(),
                    ),
                };
                    (
                        LoadMode::Uefi {
                            firmware,
                            enable_serial: any_serial_configured,
                            // Route the firmware console to COM1 when it is
                            // available. The firmware's default console is the
                            // video device, so without this the firmware and
                            // anything it launches would have nowhere to write on a
                            // VM with no graphics adapter.
                            uefi_console_mode: com1_configured.then_some(UefiConsoleMode::Com1),
                            bios_guid: Guid::new_random(),
                            enable_vmbus: true,
                            // Everything below is fixed for now. The proto has no
                            // way to express these yet; fields will be added as
                            // callers need them.
                            //
                            // Note that memory protections match the CLI in
                            // defaulting to off, since Linux currently fails to
                            // boot with them enabled.
                            enable_memory_protections: false,
                            enable_debugging: false,
                            disable_frontpage: false,
                            enable_tpm: false,
                            enable_battery: false,
                            enable_vpci_boot: false,
                            default_boot_always_attempt: false,
                            force_dma_bounce: false,
                            enable_hv: true,
                        },
                        vm_manifest_builder::BaseChipsetType::HypervGen2Uefi,
                        Some((base_template_json, uefi.secure_boot_enabled)),
                    )
                }
            }
        };

        let microvm_portb = if matches!(machine_profile, OpenvmmMachineProfile::Microvm { .. }) {
            if ports.iter().skip(1).any(Option::is_some) {
                bail!("microVM ABI version 1 accepts only serial port 0 as its portb endpoint");
            }
            Some(
                ports[0]
                    .take()
                    .unwrap_or_else(|| DisconnectedSerialBackendHandle.into_resource()),
            )
        } else {
            None
        };
        let mut chipset_builder = VmManifestBuilder::new(base_chipset_type, arch);
        if microvm_portb.is_none() {
            chipset_builder = chipset_builder.with_serial(ports);
        }
        if let Some((base_template_json, secure_boot_enabled)) = uefi_config {
            // The UEFI helper device backs the firmware's variable store and
            // runtime services, so it is required for a UEFI boot. The store is
            // ephemeral: with no VMGS file configured there is nowhere to
            // persist boot entries or secure boot state across reboots.
            chipset_builder = chipset_builder.with_uefi(vm_manifest_builder::UefiManifest::new(
                arch,
                base_template_json,
                None,
                secure_boot_enabled,
                firmware_uefi_resources::LogLevel::make_default(),
                None,
                EphemeralNonVolatileStoreHandle.into_resource(),
                None,
            ));
        }
        let layout_config = chipset_builder.layout_config();
        let mut chipset = chipset_builder
            .build()
            .context("failed to build vm configuration")?;
        if let Some(io) = microvm_portb {
            let restore_entropy = if restore_entropy {
                let mut entropy = [0_u8; 64];
                getrandom::fill(&mut entropy).context("failed to generate restore entropy")?;
                let mut packet = b"OPENVMM_ENTROPY_V1\0".to_vec();
                packet.extend(entropy);
                packet
            } else {
                Vec::new()
            };
            chipset.chipset_devices.extend([
                ChipsetDeviceHandle {
                    name: MicrovmPortbHandle::ID.to_owned(),
                    resource: MicrovmPortbHandle {
                        io,
                        restore_entropy,
                    }
                    .into_resource(),
                },
                ChipsetDeviceHandle {
                    name: MicrovmShutdownHandle::ID.to_owned(),
                    resource: MicrovmShutdownHandle.into_resource(),
                },
                ChipsetDeviceHandle {
                    name: MicrovmSnapshotRequestHandle::ID.to_owned(),
                    resource: MicrovmSnapshotRequestHandle {
                        notify: microvm_snapshot_notify,
                        input_gate_timeout: snapshot_quiesce_timeout,
                    }
                    .into_resource(),
                },
            ]);
        }

        // Build the NUMA topology. A `MemoryConfig` and an explicit
        // `NumaConfig` are mutually exclusive (mirrors the CLI `--memory` vs
        // `--numa` conflict). `config_mem_size` is the total guest memory
        // reported to the `VmController`.
        let (numa, config_mem_size) = if let Some(restore) = &authoritative_restore {
            let mem_size = restore.memory_size;
            let numa = NumaTopology {
                nodes: vec![NumaNode {
                    mem: Some(MemoryConfig {
                        mem_size,
                        prefetch_memory: false,
                        private_memory: false,
                        transparent_hugepages: true,
                        hugepages: false,
                        hugepage_size: None,
                        host_numa_node: None,
                    }),
                    vps: VpAssignment::FromTopology,
                }],
                distances: vec![],
            };
            (numa, mem_size)
        } else if let Some(numa_config) = req_config.numa_config.take() {
            if req_config.memory_config.is_some() {
                bail!("memory_config and numa_config are mutually exclusive");
            }
            build_numa_topology(numa_config)?
        } else {
            let mem_size = req_config
                .memory_config
                .as_ref()
                .context("missing memory configuration")?
                .memory_mb
                .checked_mul(0x100000)
                .context("invalid memory configuration")?;
            let numa = NumaTopology {
                nodes: vec![NumaNode {
                    mem: Some(MemoryConfig {
                        mem_size,
                        prefetch_memory: false,
                        private_memory: false,
                        transparent_hugepages: true,
                        hugepages: false,
                        hugepage_size: None,
                        host_numa_node: None,
                    }),
                    vps: VpAssignment::FromTopology,
                }],
                distances: vec![],
            };
            (numa, mem_size)
        };

        let config_proc_count = authoritative_restore
            .as_ref()
            .map(|restore| restore.vp_count)
            .or_else(|| {
                req_config
                    .processor_config
                    .as_ref()
                    .map(|config| config.processor_count)
            })
            .unwrap_or(1);

        // Build the PCIe topology (root complexes, switches, and the devices
        // attached behind their ports).
        let pcie = if let Some(pcie) = req_config.pcie.take() {
            build_pcie_topology(pcie, &registry).await?
        } else {
            BuiltPcieTopology::default()
        };

        let mut config = Config {
            // TODO: devices, other stuff
            machine_profile,
            load_mode,
            ide_disks: vec![],
            floppy_disks: vec![],
            pcie_root_complexes: pcie.root_complexes,
            pcie_devices: pcie.devices,
            pcie_switches: pcie.switches,
            pcie_generic_initiators: pcie.generic_initiators,
            vpci_devices: vec![],
            numa,
            chipset: chipset.chipset,
            processor_topology: ProcessorTopologyConfig {
                proc_count: config_proc_count,
                vps_per_socket: None,
                enable_smt: None,
                arch: if is_microvm {
                    Some(ArchTopologyConfig::X86(X86TopologyConfig::default()))
                } else {
                    None
                },
            },
            hypervisor: HypervisorConfig {
                with_hv: matches!(machine_profile, OpenvmmMachineProfile::Standard),
                ..Default::default()
            },
            #[cfg(windows)]
            kernel_vmnics: vec![],
            input: mesh::Receiver::new(),
            framebuffer: None,
            vga_firmware: None,
            vtl2_gfx: false,
            virtio_devices: vec![],
            vmbus: matches!(machine_profile, OpenvmmMachineProfile::Standard)
                .then_some(VmbusConfig::default()),
            vtl2_vmbus: None,
            vmbus_devices: vec![],
            #[cfg(windows)]
            vpci_resources: vec![],
            vmgs: None,
            firmware_event_send: None,
            debugger_rpc: None,
            chipset_devices: chipset.chipset_devices,
            pci_chipset_devices: chipset.pci_chipset_devices,
            isa_dma_controller: chipset.isa_dma_controller,
            chipset_capabilities: chipset.capabilities,
            layout: layout_config,
            rtc_delta_milliseconds: 0,
            microvm_network: None,
            microvm_filesystem: None,
        };

        let guest_power_actions = {
            use vmservice::vm_config::GuestPowerAction as ProtoAction;

            let requested = req_config.guest_power_actions.unwrap_or_default();
            let defaults = GuestPowerActions::default();
            let action = |value: i32, default| -> anyhow::Result<GuestPowerAction> {
                Ok(match ProtoAction::from_i32(value) {
                    Some(ProtoAction::Default) => default,
                    Some(ProtoAction::Restart) => GuestPowerAction::Reset,
                    Some(ProtoAction::Halt) => GuestPowerAction::Halt,
                    None => bail!("unknown guest power action {value}"),
                })
            };
            GuestPowerActions {
                shutdown: action(requested.shutdown, defaults.shutdown)?,
                reset: action(requested.reset, defaults.reset)?,
                crash: action(requested.crash, defaults.crash)?,
                watchdog: action(requested.watchdog, defaults.watchdog)?,
            }
        };

        let mut scsi_rpc = None;
        let mut consomme_rpc = None;
        let mut microvm_console_attachment = None;
        let mut microvm_console_socket_cleanup = None;
        let mut microvm_filesystem_attachment = None;
        let mut microvm_filesystem_root_path = None;
        if let Some((filesystem, root_path, attachment)) = restored_microvm_filesystem {
            microvm_filesystem_root_path = Some(PathBuf::from(&root_path));
            config.microvm_filesystem = Some(filesystem.clone());
            config.virtio_devices.push((
                VirtioBus::Mmio,
                virtio_resources::fs::VirtioFsHandle {
                    tag: "microvm".to_owned(),
                    fs: virtio_resources::fs::VirtioFsBackend::HostFs {
                        root_path,
                        mount_options: String::new(),
                    },
                    profile: virtio_resources::fs::VirtioFsProfile::MicrovmV1 {
                        stable_id: "fs:microvm0".to_owned(),
                        root_identity: attachment.identity.clone(),
                        read_only: filesystem.access.is_read_only(),
                    },
                }
                .into_resource(),
            ));
            microvm_filesystem_attachment = Some(attachment);
        }
        if let Some(restore) = &authoritative_restore {
            let has_console = restore
                .machine_contract
                .devices
                .iter()
                .any(|device| device.stable_id == "console:microvm-virtio0");
            let attachment = restore
                .machine_contract
                .attachments
                .iter()
                .find(|attachment| attachment.stable_id == "console:microvm-virtio0");
            anyhow::ensure!(
                has_console == attachment.is_some(),
                "snapshot microVM console device and attachment inventories disagree"
            );
            if let Some(attachment) = attachment {
                let requested_attachment = restore_console_config.as_ref().map(|console| {
                    if console.connect {
                        SerialConfigCli::ConnectPipe(PathBuf::from(&console.socket_path))
                    } else {
                        SerialConfigCli::Pipe(PathBuf::from(&console.socket_path))
                    }
                });
                let (endpoint_config, resource_attachment, snapshot_attachment) =
                    crate::microvm_console_attachment_from_snapshot(
                        attachment,
                        requested_attachment.as_ref(),
                    )?;
                crate::validate_microvm_console_attachment_namespace(
                    &snapshot_attachment,
                    &restore.path,
                )?;
                let (backend, disconnect_policy) = match endpoint_config {
                    SerialConfigCli::Pipe(path) => {
                        let backend = bind_serial_without_cleanup(&path).with_context(|| {
                            format!(
                                "failed to recreate virtio console listener: {}",
                                path.display()
                            )
                        })?;
                        microvm_console_socket_cleanup =
                            crate::microvm_console_socket_cleanup(path)?;
                        (
                            backend,
                            virtio_resources::console::VirtioConsoleDisconnectPolicy::Retain,
                        )
                    }
                    SerialConfigCli::Tcp(address) => (
                        crate::serial_io::bind_tcp_serial(&address)?,
                        virtio_resources::console::VirtioConsoleDisconnectPolicy::Retain,
                    ),
                    SerialConfigCli::ConnectPipe(path) => (
                        crate::serial_io::connect_serial_with_timeout(
                            &path,
                            Duration::from_millis(
                                openvmm_defs::config::MICROVM_CONSOLE_RECONNECT_TIMEOUT_MS,
                            ),
                        )
                        .with_context(|| {
                            format!(
                                "failed to reconnect virtio console client: {}",
                                path.display()
                            )
                        })?,
                        virtio_resources::console::VirtioConsoleDisconnectPolicy::Retain,
                    ),
                    SerialConfigCli::ConnectTcp(address) => (
                        crate::serial_io::connect_tcp_serial(
                            &address,
                            Duration::from_millis(
                                openvmm_defs::config::MICROVM_CONSOLE_RECONNECT_TIMEOUT_MS,
                            ),
                        )?,
                        virtio_resources::console::VirtioConsoleDisconnectPolicy::Retain,
                    ),
                    SerialConfigCli::None => (
                        DisconnectedSerialBackendHandle.into_resource(),
                        virtio_resources::console::VirtioConsoleDisconnectPolicy::Discard,
                    ),
                    _ => unreachable!("saved microVM console was validated as an attachment"),
                };
                config.virtio_devices.push((
                    VirtioBus::Mmio,
                    virtio_resources::console::VirtioConsoleHandle {
                        backend,
                        disconnect_policy,
                        attachment: Some(resource_attachment),
                    }
                    .into_resource(),
                ));
                microvm_console_attachment = Some(snapshot_attachment);
            }
        }
        if let Some(devices_config) = req_config.devices_config {
            if let Some(virtio_blk) = devices_config.virtio_blk {
                anyhow::ensure!(is_microvm, "fixed virtio-blk requires the microVM profile");
                let vmservice::VirtioBlk { backend, read_only } = virtio_blk;
                let disk =
                    build_disk_backend(backend.context("missing blk backend")?, read_only).await?;
                config.virtio_devices.push((
                    VirtioBus::Mmio,
                    virtio_resources::blk::VirtioBlkHandle { disk, read_only }.into_resource(),
                ));
            }
            if !devices_config.scsi_disks.is_empty() {
                let mut devices = Vec::new();
                for disk in devices_config.scsi_disks {
                    devices.push(make_disk_config(disk).await?);
                }
                let (send, recv) = mesh::channel();
                config.vmbus_devices.push((
                    DeviceVtl::Vtl0,
                    ScsiControllerHandle {
                        instance_id: guid::guid!("ba6163d9-04a1-4d29-b605-72e2ffb1dc7f"),
                        max_sub_channel_count: 0,
                        devices,
                        io_queue_depth: None,
                        requests: Some(recv),
                        poll_mode_queue_depth: None,
                    }
                    .into_resource(),
                ));
                scsi_rpc = Some(send);
            }

            for nic in devices_config.nic_config {
                let is_consomme = matches!(
                    &nic.backend,
                    Some(vmservice::nic_config::Backend::Consomme(_))
                );
                // Only wire the bind/unbind RPC channel to the first consomme
                // NIC. Additional consomme NICs work but cannot be targeted by
                // runtime bind/unbind commands.
                let recv = if is_consomme && consomme_rpc.is_none() {
                    let (send, recv) = mesh::channel();
                    consomme_rpc = Some(send);
                    Some(recv)
                } else {
                    None
                };
                config
                    .vmbus_devices
                    .push(parse_nic_config(nic, recv, &registry)?);
            }

            for virtiofs in devices_config.virtiofs_config {
                if is_microvm {
                    anyhow::ensure!(
                        config.microvm_filesystem.is_none()
                            && virtiofs.tag == "microvm"
                            && !virtiofs.root_path.is_empty(),
                        "microVM ABI version 1 permits one virtio-fs attachment with tag 'microvm'"
                    );
                    let filesystem = openvmm_defs::config::MicrovmFilesystemConfig::new(
                        virtiofs.guest_mount_target,
                        if virtiofs.read_write {
                            openvmm_defs::config::MicrovmFilesystemAccess::ReadWrite
                        } else {
                            openvmm_defs::config::MicrovmFilesystemAccess::ReadOnly
                        },
                    )?;
                    let (root_path, attachment) =
                        crate::microvm_filesystem_attachment(Path::new(&virtiofs.root_path))?;
                    microvm_filesystem_root_path = Some(PathBuf::from(&root_path));
                    let resource = virtio_resources::fs::VirtioFsHandle {
                        tag: "microvm".to_owned(),
                        fs: virtio_resources::fs::VirtioFsBackend::HostFs {
                            root_path,
                            mount_options: String::new(),
                        },
                        profile: virtio_resources::fs::VirtioFsProfile::MicrovmV1 {
                            stable_id: "fs:microvm0".to_owned(),
                            root_identity: attachment.identity.clone(),
                            read_only: filesystem.access.is_read_only(),
                        },
                    }
                    .into_resource();
                    if snapshot_destination.is_some() {
                        tracing::warn!(
                            stable_id = "fs:microvm0",
                            access_mode = filesystem.access.as_str(),
                            "microVM snapshot excludes live host filesystem contents; restore revalidates the external directory and may fail after host changes"
                        );
                    }
                    config.microvm_filesystem = Some(filesystem);
                    microvm_filesystem_attachment = Some(attachment);
                    config.virtio_devices.push((VirtioBus::Mmio, resource));
                } else {
                    anyhow::ensure!(
                        virtiofs.guest_mount_target.is_empty() && !virtiofs.read_write,
                        "guest_mount_target and read_write require the microVM profile"
                    );
                    let resource = virtio_resources::fs::VirtioFsHandle {
                        tag: virtiofs.tag,
                        fs: virtio_resources::fs::VirtioFsBackend::HostFs {
                            root_path: virtiofs.root_path,
                            mount_options: String::new(),
                        },
                        profile: virtio_resources::fs::VirtioFsProfile::Standard,
                    }
                    .into_resource();
                    // Use VPCI when possible (currently only on Windows and macOS due
                    // to KVM backend limitations).
                    if cfg!(windows) || cfg!(target_os = "macos") {
                        config.vpci_devices.push(VpciDeviceConfig {
                            vtl: DeviceVtl::Vtl0,
                            instance_id: Guid::new_random(),
                            resource: VirtioPciDeviceHandle(resource).into_resource(),
                            vnode: None,
                        });
                    } else {
                        config.virtio_devices.push((VirtioBus::Mmio, resource));
                    }
                }
            }

            if let Some(virtio_console) = devices_config.virtio_console {
                if !virtio_console.socket_path.is_empty() {
                    let (backend, disconnect_policy, attachment) = if is_microvm {
                        let endpoint_config = if virtio_console.connect {
                            SerialConfigCli::ConnectPipe(PathBuf::from(&virtio_console.socket_path))
                        } else {
                            SerialConfigCli::Pipe(PathBuf::from(&virtio_console.socket_path))
                        };
                        let (endpoint_config, attachment, snapshot_attachment) =
                            crate::microvm_console_attachment_from_cli(&endpoint_config)?;
                        if let Some(snapshot_dir) = &snapshot_destination {
                            crate::validate_microvm_console_attachment_namespace(
                                &snapshot_attachment,
                                snapshot_dir,
                            )?;
                        }
                        let backend = match endpoint_config {
                            SerialConfigCli::Pipe(path) => {
                                let backend =
                                    bind_serial_without_cleanup(&path).with_context(|| {
                                        format!(
                                            "failed to bind virtio console socket: {}",
                                            path.display()
                                        )
                                    })?;
                                microvm_console_socket_cleanup =
                                    crate::microvm_console_socket_cleanup(path)?;
                                backend
                            }
                            SerialConfigCli::ConnectPipe(path) => {
                                crate::serial_io::connect_serial_with_timeout(
                                    &path,
                                    Duration::from_millis(
                                        openvmm_defs::config::MICROVM_CONSOLE_RECONNECT_TIMEOUT_MS,
                                    ),
                                )
                                .with_context(|| {
                                    format!(
                                        "failed to connect virtio console socket: {}",
                                        path.display()
                                    )
                                })?
                            }
                            _ => unreachable!("path input produced a non-path attachment"),
                        };
                        microvm_console_attachment = Some(snapshot_attachment);
                        (
                            backend,
                            virtio_resources::console::VirtioConsoleDisconnectPolicy::Retain,
                            Some(attachment),
                        )
                    } else {
                        let (serial_fn, action) = open_socket_backend(virtio_console.connect);
                        let backend =
                            serial_fn(virtio_console.socket_path.as_ref()).with_context(|| {
                                format!(
                                    "failed to {} virtio console socket: {}",
                                    action, virtio_console.socket_path
                                )
                            })?;
                        (
                            backend,
                            virtio_resources::console::VirtioConsoleDisconnectPolicy::Discard,
                            None,
                        )
                    };
                    let resource: Resource<VirtioDeviceHandle> =
                        virtio_resources::console::VirtioConsoleHandle {
                            backend,
                            disconnect_policy,
                            attachment,
                        }
                        .into_resource();
                    if is_microvm {
                        config.virtio_devices.push((VirtioBus::Mmio, resource));
                    } else if cfg!(windows) || cfg!(target_os = "macos") {
                        config.vpci_devices.push(VpciDeviceConfig {
                            vtl: DeviceVtl::Vtl0,
                            instance_id: Guid::new_random(),
                            resource: VirtioPciDeviceHandle(resource).into_resource(),
                            vnode: None,
                        });
                    } else {
                        config.virtio_devices.push((VirtioBus::Mmio, resource));
                    }
                }
            }
        }

        if is_microvm && authoritative_restore.is_none() {
            let has_console = config
                .virtio_devices
                .iter()
                .any(|(_, device)| device.id() == "virtio-console");
            let has_block = config
                .virtio_devices
                .iter()
                .any(|(_, device)| device.id() == "virtio-blk");
            let LoadMode::Pvh { cmdline, .. } = &mut config.load_mode else {
                unreachable!("microVM was validated with pvh_boot");
            };
            openvmm_defs::config::append_microvm_virtio_discovery(
                cmdline,
                None,
                config.microvm_filesystem.as_ref(),
                has_console,
                has_block,
            )?;
        }

        if let Some(hvsocket_config) = req_config.hvsocket_config {
            if matches!(machine_profile, OpenvmmMachineProfile::Microvm { .. }) {
                bail!("microVM ABI version 1 does not support hvsocket");
            }
            let listener = UnixListener::bind(&hvsocket_config.path).with_context(|| {
                format!("failed to bind hvsocket path: {}", hvsocket_config.path)
            })?;
            config.vmbus.as_mut().unwrap().vsock_listener = Some(listener);
            config.vmbus.as_mut().unwrap().vsock_path = Some(hvsocket_config.path);
        }

        openvmm_defs::config::validate_machine_config(&config, None)?;

        let effective_command_line = match &config.load_mode {
            LoadMode::Pvh { cmdline, .. } => Some(cmdline.clone()),
            _ => None,
        };
        let has_microvm_block = config
            .virtio_devices
            .iter()
            .any(|(_, device)| device.id() == "virtio-blk");
        let microvm_filesystem = config.microvm_filesystem.clone();
        if let Some(root_path) = microvm_filesystem_root_path.as_deref() {
            crate::validate_microvm_filesystem_private_storage(
                root_path,
                snapshot_destination.as_deref(),
                authoritative_restore
                    .as_ref()
                    .map(|restore| restore.path.as_path()),
                None,
            )?;
        }
        let snapshot_memory_file = if let Some(destination) = &snapshot_destination {
            let parent = destination
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            let parent_metadata = fs_err::symlink_metadata(parent).with_context(|| {
                format!("failed to inspect snapshot parent {}", parent.display())
            })?;
            anyhow::ensure!(
                parent_metadata.file_type().is_dir(),
                "snapshot parent is not a directory: {}",
                parent.display()
            );
            anyhow::ensure!(
                fs_err::symlink_metadata(destination)
                    .is_err_and(|error| error.kind() == io::ErrorKind::NotFound),
                "snapshot destination already exists or cannot be inspected: {}",
                destination.display()
            );
            let file = tempfile::Builder::new()
                .prefix(".openvmm-microvm-memory-")
                .tempfile_in(parent)
                .context("failed to create snapshot memory backing")?;
            file.as_file()
                .set_len(config_mem_size)
                .context("failed to size snapshot memory backing")?;
            Some(file)
        } else {
            None
        };
        let snapshot_memory_path = snapshot_memory_file
            .as_ref()
            .map(|file| file.path().to_owned());
        let snapshot_memory_handle = snapshot_memory_file
            .as_ref()
            .map(|file| {
                file.reopen()
                    .context("failed to duplicate automatic snapshot RAM handle")
            })
            .transpose()?;

        let (shared_memory, saved_state, shared_memory_copy_on_write, restore_time) =
            if let Some((fd, state, restore_time)) = prepared_restore {
                (Some(fd), Some(state), true, Some(restore_time))
            } else if let Some(file) = &snapshot_memory_handle {
                let file = file
                    .try_clone()
                    .context("failed to duplicate snapshot RAM handle for worker")?;
                let shared_memory = openvmm_helpers::shared_memory::file_to_shared_memory_fd(file)?;
                (Some(shared_memory), None, false, None)
            } else {
                (None, None, false, None)
            };

        let (send, recv) = mesh::channel();
        let (notify_send, notify_recv) = mesh::channel();

        // Create a VmmMesh for local/in-process workers.
        let mesh = VmmMesh::new(&self.driver, true)?;
        let vm_host = mesh
            .make_host("vm", None)
            .await
            .context("spawning vm process failed")?;

        let worker = vm_host
            .launch_worker(
                VM_WORKER,
                VmWorkerParameters {
                    hypervisor,
                    cfg: config,
                    saved_state,
                    shared_memory,
                    shared_memory_copy_on_write,
                    snapshot_boundary_requests: microvm_snapshot_requests,
                    snapshot_ready,
                    restore_downtime: restore_time.as_ref().map(|(downtime, _, _)| *downtime),
                    restore_tsc_frequency_hz: restore_time
                        .as_ref()
                        .map(|(_, frequency, _)| *frequency),
                    restore_cpu_contract: restore_time.map(|(_, _, cpu_contract)| cpu_contract),
                    rpc: recv,
                    notify: notify_send,
                },
            )
            .await?;

        let memory = config_mem_size;
        let processors = config_proc_count;

        // Create channels for VmController.
        let (vm_controller_send, vm_controller_recv) = mesh::channel();
        let (event_send, event_recv) = mesh::channel();

        // Build VmController with no paravisor-specific fields.
        let controller = VmController {
            machine_profile,
            mesh,
            vm_worker: worker,
            vnc_worker: None,
            gdb_worker: None,
            diag_inspector: None,
            vtl2_settings: None,
            ged_rpc: None,
            vm_rpc: send.clone(),
            paravisor_diag: None,
            igvm_path: None,
            memory_backing_file: snapshot_memory_path,
            snapshot_memory_handle,
            memory,
            processors,
            log_file: None,
            crash_dump_path: None,
            snapshot_requests,
            snapshot_destination,
            snapshot_quiesce_timeout,
            source_hypervisor,
            effective_command_line,
            has_microvm_block,
            microvm_console_attachment,
            microvm_network: None,
            microvm_network_attachment: None,
            microvm_egress_policy: None,
            microvm_filesystem,
            microvm_filesystem_attachment,
            microvm_console_socket_cleanup,
            snapshot_memory_file,
            guest_power_actions,
        };

        // Spawn the controller task.
        let controller_task = self.driver.spawn(
            "vm-controller",
            controller.run(vm_controller_recv, event_send, notify_recv),
        );

        self.vm_controller = Some(vm_controller_send);
        self.vm_controller_events = Some(event_recv);
        self.controller_task = Some(controller_task);
        self.vm = Some(Arc::new(Vm {
            scsi_rpc,
            consomme_rpc,
            worker_rpc: send,
        }));
        self.lifecycle = VmLifecycle::Paused;
        Ok(())
    }

    async fn teardown_vm(&mut self) -> anyhow::Result<()> {
        let controller = self.vm_controller.take().context("vm not created")?;
        // Drop the VM's device RPC channels before waiting on the controller.
        // A live `ScsiControllerRequest` sender keeps the detached storvsp task
        // running, which in turn stops the VM worker from ever finishing its
        // stop, so waiting for the controller first would hang forever. The
        // REPL's quit path works around the same bug.
        self.vm.take();
        controller.send(VmControllerRpc::Quit);
        drop(controller);
        if let Some(task) = self.controller_task.take() {
            task.await;
        }
        self.vm_controller_events.take();
        self.lifecycle = VmLifecycle::Uninitialized;
        if let Some((_, response)) = self.wait_vm_response.take() {
            response.send(Err(grpc_error(anyhow!("VM torn down"))));
        }
        Ok(())
    }

    fn build_properties(&self) -> vmservice::PropertiesVmResponse {
        let halt_reason = match &self.lifecycle {
            VmLifecycle::Halted(reason) => Some(reason.clone()),
            _ => None,
        };
        vmservice::PropertiesVmResponse {
            memory_stats: None,
            processor_stats: None,
            state: vmservice::VmState::from(&self.lifecycle) as i32,
            halt_reason,
        }
    }

    fn build_capabilities(&self) -> vmservice::CapabilitiesVmResponse {
        use vmservice::capabilities_vm_response::Resource;
        use vmservice::capabilities_vm_response::SupportedGuestOs;
        use vmservice::capabilities_vm_response::SupportedResource;

        vmservice::CapabilitiesVmResponse {
            supported_resources: vec![
                SupportedResource {
                    resource: Resource::Scsi as i32,
                    add: true,
                    remove: true,
                    update: false,
                },
                SupportedResource {
                    resource: Resource::Vpci as i32,
                    add: true,
                    remove: true,
                    update: false,
                },
                SupportedResource {
                    resource: Resource::VmNic as i32,
                    add: true,
                    remove: true,
                    update: true,
                },
            ],
            supported_guest_os: vec![SupportedGuestOs::Linux as i32],
        }
    }

    async fn pause_vm(&mut self) -> anyhow::Result<()> {
        let vm = self.vm.clone().context("VM not created yet")?;
        vm.worker_rpc
            .call(VmRpc::Pause, ())
            .await
            .map(drop)
            .context("pause failed")?;
        if !matches!(self.lifecycle, VmLifecycle::Halted(_)) {
            self.lifecycle = VmLifecycle::Paused;
        }
        Ok(())
    }

    async fn resume_vm(&mut self) -> anyhow::Result<()> {
        let vm = self.vm.clone().context("VM not created yet")?;
        let resumed = vm
            .worker_rpc
            .call(VmRpc::Resume, ())
            .await
            .context("resume failed")?;
        anyhow::ensure!(
            resumed,
            "VM did not resume; a state unit failed to start or the VM was already running"
        );
        if !matches!(self.lifecycle, VmLifecycle::Halted(_)) {
            self.lifecycle = VmLifecycle::Running;
        }
        Ok(())
    }

    fn handle_controller_event(&mut self, event: VmControllerEvent) -> bool {
        match event {
            VmControllerEvent::GuestHalt(reason) => {
                tracing::info!(%reason, "guest halted (via controller)");
                self.lifecycle = VmLifecycle::Halted(reason);
                if let Some((_, response)) = self.wait_vm_response.take() {
                    response.send(Ok(()));
                }
                false
            }
            VmControllerEvent::ExitRequested { code } => {
                let reason = format!("guest exited with status {code}");
                tracing::info!(code, "guest halted with process status");
                self.lifecycle = VmLifecycle::Halted(reason);
                if let Some((_, response)) = self.wait_vm_response.take() {
                    response.send(Ok(()));
                }
                true
            }
            VmControllerEvent::WorkerStopped { error } => {
                if let Some(err) = &error {
                    tracing::error!(error = %err, "VM worker stopped with error");
                } else {
                    tracing::info!("VM worker stopped");
                }
                if let Some((_, response)) = self.wait_vm_response.take() {
                    let status = if let Some(err) = &error {
                        grpc_error(anyhow!("VM worker stopped: {}", err))
                    } else {
                        grpc_error(anyhow!("VM worker stopped"))
                    };
                    response.send(Err(status));
                }
                // Clear VM state since the worker is gone. The controller
                // task will be awaited during final cleanup.
                self.vm.take();
                self.vm_controller.take();
                self.lifecycle = VmLifecycle::Uninitialized;
                false
            }
            VmControllerEvent::VncWorkerStopped { error } => {
                if let Some(err) = &error {
                    tracing::error!(error = %err, "VNC worker stopped unexpectedly");
                }
                false
            }
        }
    }

    fn add_pcie_device(
        &self,
        request: vmservice::AddPcieDeviceRequest,
    ) -> anyhow::Result<impl Future<Output = anyhow::Result<()>> + use<>> {
        let worker_rpc = self
            .vm
            .as_ref()
            .context("VM not created yet")?
            .worker_rpc
            .clone();
        let registry = self.registry.clone();
        Ok(async move {
            let vmservice::AddPcieDeviceRequest { port_name, device } = request;
            let resource = build_pcie_device(device.context("missing device")?, &registry).await?;
            worker_rpc
                .call_failable(VmRpc::AddPcieDevice, (port_name, resource))
                .await
                .map_err(anyhow::Error::from)
        })
    }

    fn remove_pcie_device(
        &self,
        request: vmservice::RemovePcieDeviceRequest,
    ) -> anyhow::Result<impl Future<Output = anyhow::Result<()>> + use<>> {
        let recv = self
            .vm
            .as_ref()
            .context("VM not created yet")?
            .worker_rpc
            .call_failable(VmRpc::RemovePcieDevice, request.port_name);
        Ok(async move { recv.await.map_err(anyhow::Error::from) })
    }

    fn modify_resource(
        &self,
        request: vmservice::ModifyResourceRequest,
    ) -> anyhow::Result<impl Future<Output = anyhow::Result<()>> + use<>> {
        use vmservice::modify_resource_request::Resource;
        let vm = self.vm.as_ref().context("VM not created yet")?;
        match request.resource.context("missing resource")? {
            Resource::ScsiDisk(disk) => {
                let scsi_path = storvsp_resources::ScsiPath {
                    path: 0,
                    target: 0,
                    lun: disk.lun.try_into().ok().context("lun value out of range")?,
                };

                if request.r#type == vmservice::ModifyType::Add as i32 {
                    if disk.controller != 0 {
                        anyhow::bail!("controller must be 0");
                    }
                    let scsi_rpc = vm.scsi_rpc.as_ref().context("no scsi controller")?.clone();
                    Ok(async move {
                        let config = make_disk_config(disk).await?;
                        scsi_rpc
                            .call_failable(ScsiControllerRequest::AddDevice, config)
                            .await
                            .map_err(anyhow::Error::from)
                    }
                    .boxed())
                } else if request.r#type == vmservice::ModifyType::Remove as i32 {
                    let recv = vm
                        .scsi_rpc
                        .as_ref()
                        .context("no scsi controller")?
                        .call_failable(ScsiControllerRequest::RemoveDevice, scsi_path);
                    Ok(async move { recv.await.map_err(anyhow::Error::from) }.boxed())
                } else {
                    anyhow::bail!("unsupported request type {}", request.r#type);
                }
            }
            Resource::NicConfig(nic) => {
                if request.r#type == vmservice::ModifyType::Add as i32 {
                    if matches!(
                        &nic.backend,
                        Some(vmservice::nic_config::Backend::Consomme(_))
                    ) {
                        anyhow::bail!(
                            "adding a consomme NIC via ModifyResource is not supported; \
                             configure it at VM creation time"
                        );
                    }
                    let config = parse_nic_config(nic, None, &self.registry)?;
                    let recv = vm.worker_rpc.call_failable(VmRpc::AddVmbusDevice, config);
                    Ok(async move { recv.await.map_err(anyhow::Error::from) }.boxed())
                } else if request.r#type == vmservice::ModifyType::Update as i32 {
                    let consomme = match nic.backend.context("missing backend")? {
                        vmservice::nic_config::Backend::Consomme(c) => c,
                        _ => anyhow::bail!("port update only supported for consomme backend"),
                    };
                    let consomme_rpc = vm
                        .consomme_rpc
                        .as_ref()
                        .context("no consomme port channel")?
                        .clone();
                    Ok(async move {
                        for port in consomme.ports {
                            let cfg = parse_port_config(port)?;
                            consomme_rpc
                                .call_failable(ConsommeRequest::Bind, cfg)
                                .await
                                .map_err(anyhow::Error::from)?;
                        }
                        Ok(())
                    }
                    .boxed())
                } else if request.r#type == vmservice::ModifyType::Remove as i32 {
                    let consomme = match nic.backend.context("missing backend")? {
                        vmservice::nic_config::Backend::Consomme(c) => c,
                        _ => anyhow::bail!("port remove only supported for consomme backend"),
                    };
                    let consomme_rpc = vm
                        .consomme_rpc
                        .as_ref()
                        .context("no consomme port channel")?
                        .clone();
                    Ok(async move {
                        for port in consomme.ports {
                            let cfg = parse_port_config(port)?;
                            consomme_rpc
                                .call_failable(ConsommeRequest::Unbind, cfg)
                                .await
                                .map_err(anyhow::Error::from)?;
                        }
                        Ok(())
                    }
                    .boxed())
                } else {
                    anyhow::bail!("unsupported NIC modify type {}", request.r#type);
                }
            }
            Resource::VpmemDisk(_) => anyhow::bail!("vpmem not supported"),
            Resource::WindowsDevice(_) => anyhow::bail!("device assignment not supported"),
            Resource::Processor(_) | Resource::ProcessorConfig(_) | Resource::Memory(_) => {
                anyhow::bail!("processor and memory resources not supported")
            }
        }
    }
}

/// Returns the appropriate serial backend open function and a human-readable
/// action verb for error messages, based on whether we should connect to an
/// existing socket or bind a new listener.
fn open_socket_backend(
    connect: bool,
) -> (
    fn(&Path) -> io::Result<Resource<SerialBackendHandle>>,
    &'static str,
) {
    if connect {
        (connect_serial, "connect to")
    } else {
        (bind_serial, "bind")
    }
}

/// Convert a ttrpc `PortConfig` (untrusted input) into a `HostPortConfig`,
/// validating the protocol and port ranges. The host port is always treated as
/// a fixed port; the unbind path ignores it.
fn parse_port_config(port: vmservice::PortConfig) -> anyhow::Result<HostPortConfig> {
    let vmservice::PortConfig {
        host_port,
        guest_port,
        protocol,
    } = port;
    let protocol = if protocol == vmservice::IpProtocol::Tcp as i32 {
        HostPortProtocol::Tcp
    } else if protocol == vmservice::IpProtocol::Udp as i32 {
        HostPortProtocol::Udp
    } else {
        anyhow::bail!("invalid protocol {protocol}");
    };
    Ok(HostPortConfig {
        protocol,
        host_address: None,
        host_port: HostPort::Fixed(host_port.try_into().context("host port out of range")?),
        guest_port: guest_port.try_into().context("guest port out of range")?,
    })
}

fn parse_nic_config(
    nic: vmservice::NicConfig,
    recv: Option<mesh::Receiver<ConsommeRequest>>,
    registry: &FdRegistry,
) -> anyhow::Result<(DeviceVtl, Resource<VmbusDeviceHandleKind>)> {
    use self::vmservice::nic_config::Backend;
    #[cfg(not(target_os = "linux"))]
    let _ = registry;
    let endpoint = match nic.backend.context("missing backend")? {
        #[cfg(windows)]
        Backend::LegacyPortId(port_id) => net_backend_resources::dio::WindowsDirectIoHandle {
            switch_port_id: net_backend_resources::dio::SwitchPortId {
                switch: nic.legacy_switch_id.parse().context("invalid switch ID")?,
                port: port_id.parse().context("invalid port ID")?,
            },
        }
        .into_resource(),
        #[cfg(windows)]
        Backend::Dio(dio) => net_backend_resources::dio::WindowsDirectIoHandle {
            switch_port_id: net_backend_resources::dio::SwitchPortId {
                switch: dio.switch_id.parse().context("invalid switch ID")?,
                port: dio.port_id.parse().context("invalid port ID")?,
            },
        }
        .into_resource(),
        #[cfg(target_os = "linux")]
        Backend::Tap(tap) => build_tap_backend(tap, registry)?,
        Backend::Consomme(consomme) => net_backend_resources::consomme::ConsommeHandle {
            cidr: if consomme.cidr.is_empty() {
                None
            } else {
                Some(consomme.cidr)
            },
            static_ipv4: None,
            ports: consomme
                .ports
                .into_iter()
                .map(parse_port_config)
                .collect::<anyhow::Result<_>>()?,
            recv,
        }
        .into_resource(),
        _ => anyhow::bail!("unsupported backend"),
    };
    let cfg = NetvspHandle {
        instance_id: nic.nic_id.parse().context("invalid instance ID")?,
        mac_address: nic
            .mac_address
            .parse::<MacAddress>()
            .context("invalid mac address")?,
        endpoint,
        max_queues: None,
    };
    Ok((DeviceVtl::Vtl0, cfg.into_resource()))
}

async fn make_disk_config(disk: vmservice::ScsiDisk) -> anyhow::Result<ScsiDeviceAndPath> {
    Ok(ScsiDeviceAndPath {
        path: storvsp_resources::ScsiPath {
            path: 0,
            target: 0,
            lun: disk.lun.try_into().ok().context("lun value out of range")?,
        },
        device: SimpleScsiDiskHandle {
            disk: open_disk_type(
                disk.host_path.as_ref(),
                OpenDiskOptions {
                    read_only: disk.read_only,
                    direct: false,
                },
            )
            .await
            .with_context(|| format!("failed to open {}", disk.host_path))?,
            read_only: disk.read_only,
            parameters: Default::default(),
        }
        .into_resource(),
    })
}

/// Builds a [`NumaTopology`] from the proto `NumaConfig`, returning the
/// topology and the total guest memory in bytes (summed across the nodes).
fn build_numa_topology(numa: vmservice::NumaConfig) -> anyhow::Result<(NumaTopology, u64)> {
    let vmservice::NumaConfig {
        nodes: proto_nodes,
        distances: proto_distances,
    } = numa;
    let mut total_mem = 0u64;
    let mut nodes = Vec::new();
    for node in proto_nodes {
        let vmservice::NumaNode { memory, vps } = node;
        let mem = if let Some(mem) = memory {
            let vmservice::NodeMemoryConfig {
                memory_mb,
                host_numa_node,
                prefetch,
                private_memory,
                transparent_hugepages,
                hugepages,
                hugepage_size_bytes,
            } = mem;
            let mem_size = memory_mb
                .checked_mul(0x100000)
                .context("invalid node memory size")?;
            total_mem = total_mem
                .checked_add(mem_size)
                .context("total memory overflow")?;
            Some(MemoryConfig {
                mem_size,
                prefetch_memory: prefetch,
                private_memory,
                transparent_hugepages: transparent_hugepages.unwrap_or(true),
                hugepages,
                hugepage_size: hugepage_size_bytes,
                host_numa_node,
            })
        } else {
            None
        };
        // Absent => `FromTopology`; present-but-empty => `Empty` (CPU-less);
        // present-and-non-empty => explicit VP indices.
        let vps = match vps {
            None => VpAssignment::FromTopology,
            Some(vmservice::VpAssignment { vp_index }) if vp_index.is_empty() => {
                VpAssignment::Empty
            }
            Some(vmservice::VpAssignment { vp_index }) => VpAssignment::Explicit(vp_index),
        };
        nodes.push(NumaNode { mem, vps });
    }

    let distances = proto_distances
        .into_iter()
        .map(|d| {
            let vmservice::NumaDistance { src, dst, distance } = d;
            Ok(NumaDistance {
                src,
                dst,
                distance: distance.try_into().context("distance out of range")?,
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;

    Ok((NumaTopology { nodes, distances }, total_mem))
}

/// Converts a proto MMIO window (a size plus an optional pinned base) into a
/// [`PcieMmioRangeConfig`].
fn pcie_mmio_range_config(size: u64, base: Option<u64>) -> anyhow::Result<PcieMmioRangeConfig> {
    Ok(if let Some(base) = base {
        let end = base
            .checked_add(size)
            .context("MMIO base + size overflows")?;
        PcieMmioRangeConfig::Fixed(MemoryRange::try_new(base..end).context("invalid MMIO range")?)
    } else {
        PcieMmioRangeConfig::Dynamic { size }
    })
}

/// Flattens the nested proto PCIe topology into the flat config representation:
/// a list of root complexes (each carrying its root ports), a list of switches
/// (each referencing its parent port), and a list of devices (each referencing
/// the port it sits behind).
#[derive(Default)]
struct BuiltPcieTopology {
    root_complexes: Vec<PcieRootComplexConfig>,
    switches: Vec<PcieSwitchConfig>,
    devices: Vec<PcieDeviceConfig>,
    generic_initiators: Vec<PcieGenericInitiatorConfig>,
}

async fn build_pcie_topology(
    topology: vmservice::PcieTopologyConfig,
    registry: &FdRegistry,
) -> anyhow::Result<BuiltPcieTopology> {
    let vmservice::PcieTopologyConfig {
        root_complexes: proto_root_complexes,
        generic_initiators,
    } = topology;
    let mut root_complexes = Vec::new();
    let mut switches = Vec::new();
    // Devices are built after the topology walk so that the (async) device
    // construction does not need to recurse.
    let mut pending_devices: Vec<(String, vmservice::PcieDeviceKind)> = Vec::new();

    for (index, rc) in proto_root_complexes.into_iter().enumerate() {
        let vmservice::PcieRootComplex {
            name,
            segment,
            start_bus,
            end_bus,
            low_mmio,
            high_mmio,
            low_mmio_base,
            high_mmio_base,
            preserve_bars,
            node,
            root_ports,
        } = rc;
        let mut ports = Vec::new();
        for root_port in root_ports {
            let vmservice::PciePort {
                name: port_name,
                hotplug,
                attached,
                devfn,
                acs_capabilities_supported,
            } = root_port;
            ports.push(PciePortConfig {
                name: port_name.clone(),
                devfn: devfn
                    .map(|d| d.try_into().context("devfn out of range"))
                    .transpose()?,
                hotplug,
                acs_capabilities_supported: acs_capabilities_supported
                    .map(|acs| acs.try_into().context("ACS capability mask out of range"))
                    .transpose()?,
                cxl: false,
                pasid: false,
            });
            if let Some(attached) = attached {
                walk_pcie_attachment(port_name, attached, &mut switches, &mut pending_devices)?;
            }
        }

        root_complexes.push(PcieRootComplexConfig {
            index: index as u32,
            name,
            segment: segment.try_into().context("segment out of range")?,
            start_bus: start_bus.try_into().context("start_bus out of range")?,
            end_bus: end_bus.try_into().context("end_bus out of range")?,
            low_mmio: pcie_mmio_range_config(low_mmio, low_mmio_base)?,
            high_mmio: pcie_mmio_range_config(high_mmio, high_mmio_base)?,
            ports,
            cxl: None,
            iommu: None,
            vnode: node,
            preserve_bars,
        });
    }

    let mut devices = Vec::new();
    for (port_name, device) in pending_devices {
        let resource = build_pcie_device(device, registry).await?;
        devices.push(PcieDeviceConfig {
            port_name,
            resource,
        });
    }

    let generic_initiators = generic_initiators
        .into_iter()
        .map(|initiator| PcieGenericInitiatorConfig {
            port_name: initiator.port_name,
            node: initiator.node,
        })
        .collect();

    Ok(BuiltPcieTopology {
        root_complexes,
        switches,
        devices,
        generic_initiators,
    })
}

/// Walks a single proto `PcieAttachment` (the thing behind one port): either an
/// endpoint device (queued in `pending_devices`) or a nested switch (appended
/// to `switches`, recursing into its downstream ports).
fn walk_pcie_attachment(
    port_name: String,
    attachment: vmservice::PcieAttachment,
    switches: &mut Vec<PcieSwitchConfig>,
    pending_devices: &mut Vec<(String, vmservice::PcieDeviceKind)>,
) -> anyhow::Result<()> {
    match attachment.kind.context("missing attachment kind")? {
        vmservice::pcie_attachment::Kind::Device(device) => {
            pending_devices.push((port_name, device));
        }
        vmservice::pcie_attachment::Kind::Switch(switch) => {
            let vmservice::PcieSwitch {
                name: switch_name,
                downstream_ports,
            } = switch;
            let mut ports = Vec::new();
            let mut children = Vec::new();
            for downstream in downstream_ports {
                let vmservice::PciePort {
                    name: downstream_name,
                    hotplug,
                    attached,
                    devfn,
                    acs_capabilities_supported,
                } = downstream;
                ports.push(PciePortConfig {
                    name: downstream_name.clone(),
                    devfn: devfn
                        .map(|d| d.try_into().context("devfn out of range"))
                        .transpose()?,
                    hotplug,
                    acs_capabilities_supported: acs_capabilities_supported
                        .map(|acs| acs.try_into().context("ACS capability mask out of range"))
                        .transpose()?,
                    cxl: false,
                    pasid: false,
                });
                if let Some(attached) = attached {
                    children.push((downstream_name, attached));
                }
            }
            switches.push(PcieSwitchConfig {
                name: switch_name,
                parent_port: port_name,
                ports,
            });
            for (downstream_name, attached) in children {
                walk_pcie_attachment(downstream_name, attached, switches, pending_devices)?;
            }
        }
    }
    Ok(())
}

/// Builds the resource for a single endpoint PCIe device function (a virtio
/// function, an NVMe controller, or a VFIO-assigned host device).
async fn build_pcie_device(
    device: vmservice::PcieDeviceKind,
    registry: &FdRegistry,
) -> anyhow::Result<Resource<PciDeviceHandleKind>> {
    use vmservice::pcie_device_kind::Kind;
    let vmservice::PcieDeviceKind { kind } = device;
    Ok(match kind.context("missing PCIe device kind")? {
        Kind::Virtio(virtio) => {
            let resource = build_virtio_device(virtio, registry).await?;
            VirtioPciDeviceHandle(resource).into_resource()
        }
        Kind::Nvme(nvme) => build_nvme_controller(nvme).await?,
        Kind::Vfio(vfio) => build_vfio_device(vfio)?,
    })
}

/// Builds a VFIO-assigned host PCI device resource from the proto `VfioDevice`.
///
/// Uses the legacy VFIO group/container path: the device's IOMMU group is
/// resolved from sysfs and the corresponding `/dev/vfio/<group_id>` file is
/// opened. The device must already be bound to `vfio-pci` on the host.
#[cfg(target_os = "linux")]
fn build_vfio_device(vfio: vmservice::VfioDevice) -> anyhow::Result<Resource<PciDeviceHandleKind>> {
    let vmservice::VfioDevice {
        host_pci_address,
        bar_addresses,
    } = vfio;
    let bar_addresses = parse_vfio_bar_addresses(bar_addresses)?;
    // The address is joined into a sysfs path below; reject path separators so
    // it cannot escape `/sys/bus/pci/devices` (an absolute path or `..` would
    // otherwise redirect the join).
    if host_pci_address.contains('/') || host_pci_address.contains("..") {
        anyhow::bail!("PCI address must not contain path separators");
    }
    let sysfs_path = Path::new("/sys/bus/pci/devices").join(&host_pci_address);
    let iommu_group_link =
        std::fs::read_link(sysfs_path.join("iommu_group")).with_context(|| {
            format!("failed to read IOMMU group for {host_pci_address} (is it bound to vfio-pci?)")
        })?;
    let group_id: u64 = iommu_group_link
        .file_name()
        .and_then(|s| s.to_str())
        .context("invalid iommu_group symlink")?
        .parse()
        .context("failed to parse IOMMU group ID")?;
    let group = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(format!("/dev/vfio/{group_id}"))
        .with_context(|| format!("failed to open /dev/vfio/{group_id}"))?;
    Ok(vfio_assigned_device_resources::VfioDeviceHandle {
        pci_id: host_pci_address,
        group,
        bar_addresses,
    }
    .into_resource())
}

#[cfg(target_os = "linux")]
fn parse_vfio_bar_addresses(
    entries: Vec<vmservice::VfioBarAddress>,
) -> anyhow::Result<[vfio_assigned_device_resources::BarAddressConfig; 6]> {
    use vfio_assigned_device_resources::BarAddressConfig;
    use vmservice::vfio_bar_address::Source;

    let mut bar_addresses = [BarAddressConfig::GuestAssigned; 6];
    for entry in entries {
        let index =
            usize::try_from(entry.bar_index).context("VFIO BAR index does not fit usize")?;
        let config = bar_addresses
            .get_mut(index)
            .with_context(|| format!("VFIO BAR index {} is out of range", entry.bar_index))?;
        anyhow::ensure!(
            *config == BarAddressConfig::GuestAssigned,
            "duplicate VFIO BAR index {}",
            entry.bar_index
        );
        *config = match entry.source.context("missing VFIO BAR address source")? {
            Source::Host(()) => BarAddressConfig::HostAssigned,
            Source::Fixed(address) => {
                anyhow::ensure!(address != 0, "VFIO BAR fixed address must be nonzero");
                BarAddressConfig::Fixed(address)
            }
        };
    }
    Ok(bar_addresses)
}

#[cfg(not(target_os = "linux"))]
fn build_vfio_device(
    _vfio: vmservice::VfioDevice,
) -> anyhow::Result<Resource<PciDeviceHandleKind>> {
    anyhow::bail!("VFIO device assignment is only supported on Linux")
}

/// Builds an NVMe controller resource from the proto `NvmeConfig`.
async fn build_nvme_controller(
    nvme: vmservice::NvmeConfig,
) -> anyhow::Result<Resource<PciDeviceHandleKind>> {
    let vmservice::NvmeConfig {
        controller_id,
        namespaces: proto_namespaces,
    } = nvme;
    let mut namespaces = Vec::new();
    for ns in proto_namespaces {
        let vmservice::NvmeNamespace {
            nsid,
            backend,
            read_only,
        } = ns;
        let disk =
            build_disk_backend(backend.context("missing namespace backend")?, read_only).await?;
        namespaces.push(nvme_resources::NamespaceDefinition {
            nsid,
            read_only,
            disk,
        });
    }
    Ok(nvme_resources::NvmeControllerHandle {
        subsystem_id: crate::storage_builder::deterministic_guid(&controller_id),
        msix_count: 64,
        max_io_queues: 64,
        namespaces,
        requests: None,
    }
    .into_resource())
}

/// Builds a transport-independent virtio device function from the proto
/// `VirtioDevice`.
async fn build_virtio_device(
    device: vmservice::VirtioDevice,
    registry: &FdRegistry,
) -> anyhow::Result<Resource<VirtioDeviceHandle>> {
    use vmservice::virtio_device::Kind;
    let vmservice::VirtioDevice { kind } = device;
    Ok(match kind.context("missing virtio device kind")? {
        Kind::Blk(vmservice::VirtioBlk { backend, read_only }) => {
            let disk =
                build_disk_backend(backend.context("missing blk backend")?, read_only).await?;
            virtio_resources::blk::VirtioBlkHandle { disk, read_only }.into_resource()
        }
        Kind::Net(vmservice::VirtioNet {
            max_queues,
            backend,
            mac_address,
        }) => {
            let endpoint = build_nic_backend(backend.context("missing net backend")?, registry)?;
            virtio_resources::net::VirtioNetHandle {
                max_queues: max_queues
                    .map(|q| q.try_into().context("max_queues out of range"))
                    .transpose()?,
                mac_address: mac_address
                    .parse::<MacAddress>()
                    .context("invalid mac address")?,
                endpoint,
                egress_policy: None,
                save_restore: false,
                static_ipv4: None,
                effective_features: None,
            }
            .into_resource()
        }
        Kind::Rng(vmservice::VirtioRng {}) => {
            virtio_resources::rng::VirtioRngHandle.into_resource()
        }
        Kind::Vsock(vmservice::VirtioVsock { socket_path }) => {
            let listener = UnixListener::bind(&socket_path)
                .with_context(|| format!("failed to bind virtio-vsock socket: {socket_path}"))?;
            virtio_resources::vsock::VirtioVsockHandle {
                // The guest CID does not matter for the UDS relay; it just needs
                // to be a non-reserved value.
                guest_cid: 0x3,
                base_path: socket_path,
                listener,
            }
            .into_resource()
        }
        Kind::Console(vmservice::VirtioConsole { backend }) => {
            let backend = build_serial_backend(backend.context("missing console backend")?)?;
            virtio_resources::console::VirtioConsoleHandle {
                backend,
                disconnect_policy:
                    virtio_resources::console::VirtioConsoleDisconnectPolicy::Discard,
                attachment: None,
            }
            .into_resource()
        }
        Kind::VhostUser(vhost_user) => build_vhost_user_device(vhost_user)?,
    })
}

/// Builds a disk backend resource from the proto `DiskBackend`.
async fn build_disk_backend(
    backend: vmservice::DiskBackend,
    read_only: bool,
) -> anyhow::Result<Resource<DiskHandleKind>> {
    let vmservice::DiskBackend { kind } = backend;
    match kind.context("missing disk backend kind")? {
        vmservice::disk_backend::Kind::File(vmservice::FileDisk { path, direct }) => {
            open_disk_type(path.as_ref(), OpenDiskOptions { read_only, direct })
                .await
                .with_context(|| format!("failed to open {path}"))
        }
    }
}

/// Builds a host network endpoint resource from the proto `NicBackend`.
fn build_nic_backend(
    backend: vmservice::NicBackend,
    registry: &FdRegistry,
) -> anyhow::Result<Resource<NetEndpointHandleKind>> {
    use vmservice::nic_backend::Kind;
    #[cfg(not(target_os = "linux"))]
    let _ = registry;
    let vmservice::NicBackend { kind } = backend;
    Ok(match kind.context("missing network backend")? {
        Kind::Consomme(vmservice::ConsommeBackend { cidr, ports }) => {
            net_backend_resources::consomme::ConsommeHandle {
                cidr: (!cidr.is_empty()).then_some(cidr),
                static_ipv4: None,
                ports: ports
                    .into_iter()
                    .map(parse_port_config)
                    .collect::<anyhow::Result<_>>()?,
                recv: None,
            }
            .into_resource()
        }
        #[cfg(target_os = "linux")]
        Kind::Tap(tap) => build_tap_backend(tap, registry)?,
        #[cfg(windows)]
        Kind::Dio(vmservice::DioBackend { switch_id, port_id }) => {
            net_backend_resources::dio::WindowsDirectIoHandle {
                switch_port_id: net_backend_resources::dio::SwitchPortId {
                    switch: switch_id.parse().context("invalid switch ID")?,
                    port: port_id.parse().context("invalid port ID")?,
                },
            }
            .into_resource()
        }
        _ => anyhow::bail!("unsupported network backend"),
    })
}

/// Resolves a proto `TapBackend` into a tap NIC endpoint resource, either by
/// opening `/dev/net/tun` by device name or by resolving a descriptor
/// registered via the fd-passing protocol.
#[cfg(target_os = "linux")]
fn build_tap_backend(
    tap: vmservice::TapBackend,
    registry: &FdRegistry,
) -> anyhow::Result<Resource<NetEndpointHandleKind>> {
    use vmservice::tap_backend::Source;
    let fd = match tap.source.context("missing tap source")? {
        Source::Name(name) => net_tap::tap::open_tap(&name)
            .with_context(|| format!("failed to open TAP device '{name}'"))?,
        Source::FdName(fd_name) => registry
            .resolve(&fd_name)
            .with_context(|| format!("failed to resolve tap fd '{fd_name}'"))?,
    };
    Ok(net_backend_resources::tap::TapHandle { fd }.into_resource())
}

/// Builds a serial backend resource from the proto `SerialBackend`.
fn build_serial_backend(
    backend: vmservice::SerialBackend,
) -> anyhow::Result<Resource<SerialBackendHandle>> {
    let vmservice::SerialBackend { kind } = backend;
    match kind.context("missing serial backend kind")? {
        vmservice::serial_backend::Kind::Relay(vmservice::SerialRelay {
            socket_path,
            connect,
        }) => {
            let (serial_fn, action) = open_socket_backend(connect);
            serial_fn(socket_path.as_ref())
                .with_context(|| format!("failed to {action} serial socket: {socket_path}"))
        }
    }
}

/// Builds a vhost-user-backed virtio device. Only supported on unix, where the
/// backend is reached over a Unix domain socket.
#[cfg(unix)]
fn build_vhost_user_device(
    vhost_user: vmservice::VhostUser,
) -> anyhow::Result<Resource<VirtioDeviceHandle>> {
    use vmservice::vhost_user_device::Kind;

    let vmservice::VhostUser {
        socket_path,
        device,
    } = vhost_user;
    let stream = unix_socket::UnixStream::connect(&socket_path)
        .with_context(|| format!("failed to connect to vhost-user socket: {socket_path}"))?;
    let vmservice::VhostUserDevice { kind } = device.context("missing vhost-user device")?;
    let to_u16 =
        |v: u32| -> anyhow::Result<u16> { v.try_into().context("queue value out of range") };
    Ok(match kind.context("missing vhost-user device kind")? {
        Kind::Blk(vmservice::VhostUserBlk {
            num_queues,
            queue_size,
        }) => virtio_resources::vhost_user::VhostUserBlkHandle {
            socket: stream.into(),
            num_queues: num_queues.map(to_u16).transpose()?,
            queue_size: queue_size.map(to_u16).transpose()?,
        }
        .into_resource(),
        Kind::Fs(vmservice::VhostUserFs {
            tag,
            num_queues,
            queue_size,
        }) => virtio_resources::vhost_user::VhostUserFsHandle {
            socket: stream.into(),
            tag,
            num_queues: num_queues.map(to_u16).transpose()?,
            queue_size: queue_size.map(to_u16).transpose()?,
        }
        .into_resource(),
        Kind::Other(vmservice::VhostUserGeneric {
            device_id,
            queue_sizes,
        }) => virtio_resources::vhost_user::VhostUserGenericHandle {
            socket: stream.into(),
            device_id: to_u16(device_id)?,
            queue_sizes: queue_sizes
                .into_iter()
                .map(to_u16)
                .collect::<anyhow::Result<Vec<_>>>()?,
        }
        .into_resource(),
    })
}

#[cfg(not(unix))]
fn build_vhost_user_device(
    _vhost_user: vmservice::VhostUser,
) -> anyhow::Result<Resource<VirtioDeviceHandle>> {
    anyhow::bail!("vhost-user is only supported on unix hosts")
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use vfio_assigned_device_resources::BarAddressConfig;
    use vmservice::vfio_bar_address::Source;

    fn vfio_bar_address(bar_index: u32, source: Option<Source>) -> vmservice::VfioBarAddress {
        vmservice::VfioBarAddress { bar_index, source }
    }

    #[test]
    fn parse_vfio_bar_address_config() {
        let bars = parse_vfio_bar_addresses(vec![
            vfio_bar_address(0, Some(Source::Host(()))),
            vfio_bar_address(4, Some(Source::Fixed(0x11_0000_0000))),
        ])
        .unwrap();

        assert_eq!(bars[0], BarAddressConfig::HostAssigned);
        assert_eq!(bars[1], BarAddressConfig::GuestAssigned);
        assert_eq!(bars[4], BarAddressConfig::Fixed(0x11_0000_0000));
    }

    #[test]
    fn reject_invalid_vfio_bar_address_config() {
        assert!(
            parse_vfio_bar_addresses(vec![vfio_bar_address(6, Some(Source::Host(())))]).is_err()
        );
        assert!(parse_vfio_bar_addresses(vec![vfio_bar_address(0, None)]).is_err());
        assert!(
            parse_vfio_bar_addresses(vec![vfio_bar_address(0, Some(Source::Fixed(0)))]).is_err()
        );
        assert!(
            parse_vfio_bar_addresses(vec![
                vfio_bar_address(0, Some(Source::Host(()))),
                vfio_bar_address(0, Some(Source::Fixed(0x1000))),
            ])
            .is_err()
        );
    }
}
