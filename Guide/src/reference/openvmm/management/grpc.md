# gRPC / ttrpc

To enable a gRPC or ttrpc management interface, pass `--rpc`. This spawns an
OpenVMM process acting as an RPC server on the given Unix socket:

```bash
--rpc path=/path/to/openvmm.sock[,transport=<TRANSPORT>]
```

`transport` selects which wire protocol the server accepts:

* `auto` (default) — auto-detect ttrpc vs. gRPC per connection
* `ttrpc` — accept ttrpc clients only
* `grpc` — accept gRPC clients only

For example, to accept ttrpc clients only:

```bash
--rpc path=/path/to/openvmm.sock,transport=ttrpc
```

Here is a list of supported RPCs:

```admonish danger title="Disclaimer"
The following list is not exhaustive, and may be out of date. The most up to
date reference is the [`vmservice.proto`] file.

Moreover, many APIs defined in the `.proto` file may not be fully wired up yet.

In other words: This API is _very_ WIP, and user discretion is advised.
```

* CreateVM
* TeardownVM
* PauseVM
* ResumeVM
* WaitVM
* CapabilitiesVM
* PropertiesVM
* ModifyResource
* AddPcieDevice
* RemovePcieDevice
* Quit

## microVM snapshots

`CreateVMRequest.microvm_snapshot` exposes the microVM capture and
restore flow over both transports:

* `destination_path` configures guest-requested capture. Supply an ABI-v1 or
  ABI-v2 microVM configuration, including PVH boot files, memory, and the
  processor count. The path
	must not exist. `quiesce_timeout_ms` defaults to five seconds when zero.
* `restore_path` selects manifest-authoritative restore. `config` may be
  absent, or may contain only the matching microVM profile, an optional exact
  processor-count assertion, serial port 0 host
  attachment, matching `DevicesConfig.virtio_console` and
  `DevicesConfig.virtiofs_config` attachments, and guest power actions.
  Guest-visible boot, memory, processor topology, other device, NUMA, and PCIe
  fields are rejected. A saved listener is reconstructed from the manifest; a saved
  client requires the matching path configuration.
* `restore_entropy` requests fresh entropy and is valid only with
	`restore_path`.
* `restore_ready_path` names an existing Unix domain socket on Linux or a
  `//./pipe/...` named pipe on Windows. `ResumeVM` writes and flushes exactly
  `OPENVMM_RESTORE_READY_V1\n` after all fatal restore startup work completes
  and before the restored vCPU is released. Signaling failure makes
  `ResumeVM` fail and tears down the managed VM. The peer must accept and read
  concurrently with `ResumeVM`; Windows flush completion waits until the
  complete frame has been consumed.

On cold boot, `DevicesConfig.virtio_console` may configure one microVM Unix
socket or named-pipe endpoint. Listener mode recreates the path on restore.
Client mode is required and uses the ABI-v1 five-second connection timeout
before vCPUs start. The device uses MMIO `0xd0002000`, IRQ 7, and selects
`hvc1`; its canonical path and policy become the stable restore attachment.

`DevicesConfig.virtiofs_config` may configure one microVM HostFs attachment.
Set `tag` to `microvm`, supply `root_path`, and set
`guest_mount_target` to an absolute Linux path. `read_write=false` selects the
default read-only policy. Restore requires the same tag, guest target, and
access mode with a freshly supplied live root whose identity matches the
snapshot. The fixed device uses MMIO `0xd0001000`, IRQ 6, one request queue,
and no DAX window.

Capture and restore paths are mutually exclusive. A successful capture halts
the managed source VM at the committed boundary and terminates the OpenVMM
source process; clients observe the transport closing. Restore creates a private
copy-on-write RAM view and leaves the VM paused until `ResumeVM`.
The readiness endpoint is a process-local orchestration attachment and is not
part of saved state. Each successful restore publishes one event; validation,
attachment, or worker-start failure publishes none.

`VMConfig.MICROVM` remains ABI v1. `VMConfig.MICROVM_V2` selects ABI v2 and
accepts exactly 1, 2, 4, or 8 processors. TTRPC ABI-v2 construction is
currently no-block; role-bearing sandbox blocks remain CLI-only.

The API has the same KVM/MSHV/WHP backend, no-block device, artifact integrity,
and security restrictions documented under [`--snapshot-destination`].

[`vmservice.proto`]: https://github.com/microsoft/openvmm/blob/main/openvmm/openvmm_ttrpc_vmservice/src/vmservice.proto
[`--snapshot-destination`]: ./cli.md
