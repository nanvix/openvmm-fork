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

* `destination_path` configures guest-requested capture. Supply a microVM
  configuration, including PVH boot files, memory, and the
  processor count. The path
	must not exist. `quiesce_timeout_ms` defaults to five seconds when zero.
  `memory_capacity_bytes` optionally reserves an immutable, 128-MiB-aligned
  RAM capacity while keeping the configured base memory as the exact
  `memory.bin` payload and initial PVH RAM map.
* `restore_path` selects manifest-authoritative restore. `config` may be
  absent, or may contain only the matching microVM profile, an optional exact
  processor-count assertion, serial port 0 host
  attachment, matching `DevicesConfig.virtio_console` and
  `DevicesConfig.virtiofs_config` attachments, and guest power actions.
  Guest-visible boot, memory, processor topology, other device, NUMA, and PCIe
  fields are rejected. A saved listener is reconstructed from the manifest; a saved
  client requires the matching path configuration.
* `restore_entropy` requests fresh entropy and is valid only with
  `restore_path`. Every microVM process also receives a fresh, non-serialized
  16-byte generation ID through the fixed portb selector. On restore, the ID is
  the first 16 bytes of the entropy packet.
* `restore_processor_count` requests restore-time activation of the contiguous
  VP prefix `0..count-1`. Zero preserves legacy behavior. A nonzero value is
  valid only for a snapshot that advertises processor activation, implies fresh entropy and the
  post-restore gate, and must satisfy the snapshot's boot-online and immutable
  capacity bounds. `ProcessorConfig.processor_count`, when present, remains an
  exact capacity assertion.
* `restore_memory_bytes` selects a 128-MiB-aligned total RAM target between the
  snapshot base and immutable capacity. Zero selects the base. Expansion uses
  fresh private zeroed backing, implies fresh restore packet delivery and the
  post-restore repair gate, and is rejected for legacy snapshots. An explicit
  base-size value emits restore packet V3 with zero expansion ranges; zero
  preserves V1/V2 packet selection.
* `restore_gate_timeout_ms` bounds gated guest repair. Zero selects the
  60-second default; a nonzero value is valid only with `restore_path`.
* `restore_ready_path` names an existing Unix domain socket on Linux or a
  `//./pipe/...` named pipe on Windows. `ResumeVM` writes and flushes exactly
  `OPENVMM_RESTORE_READY_V1\n` after all fatal restore startup work completes.
  For a gated restore, this occurs after guest repair succeeds and host input
  is re-enabled, while the restored vCPU remains stopped. Signaling failure makes `ResumeVM` fail and tears down the
  managed VM. The peer must accept and read concurrently with `ResumeVM`;
  Windows flush completion waits until the complete frame has been consumed.

On cold boot, `DevicesConfig.virtio_console` may configure one microVM Unix
socket or named-pipe endpoint. Listener mode recreates the path on restore.
Client mode is required and uses a five-second connection timeout
before vCPUs start. The device uses MMIO `0xd0002000`, IRQ 7, and selects
`hvc1`; its canonical path and policy become the stable restore attachment.

`DevicesConfig.virtiofs_config` may bind one HostFs attachment to the fixed
microVM slot. Set `tag` to `microvm`, supply `root_path`, and set
`guest_mount_target` to an absolute Linux path. `read_write=false` selects the
default read-only policy. Restoring an active attachment requires the exact
same canonical root path, tag, guest target, and access mode, and the live root
must retain its saved identity. A snapshot captured with the slot dormant may
omit the attachment or supply a new one; after `ResumeVM`, the guest explicitly
mounts tag `microvm`. The fixed device uses MMIO `0xd0001000`, IRQ 6, one
request queue, and no DAX window.

Capture and restore paths are mutually exclusive. A successful capture halts
the managed source VM at the committed boundary and terminates the OpenVMM
source process; clients observe the transport closing. Restore creates a private
copy-on-write RAM view and leaves the VM paused until `ResumeVM`.
The readiness endpoint is a process-local orchestration attachment and is not
part of saved state. Each successful restore publishes one event; validation,
attachment, or worker-start failure publishes none.

`VMConfig.MICROVM` is numeric value 2 and accepts exactly 1, 2, 4, or 8
processors. Numeric value 1 is reserved and rejected before host resources are
opened. Existing clients that already send value 2 remain wire-compatible;
clients that used the former `MICROVM_V2` source name must regenerate or update
their bindings. RPC construction is currently blockless; role-bearing sandbox
blocks remain CLI-only. Snapshot ABI and PVH layout values remain 2, while
value 1 snapshots are unsupported.

The API has the same KVM/MSHV/WHP backend, no-block device, artifact integrity,
and security restrictions documented under [`--snapshot-destination`].

[`vmservice.proto`]: https://github.com/microsoft/openvmm/blob/main/openvmm/openvmm_ttrpc_vmservice/src/vmservice.proto
[`--snapshot-destination`]: ./cli.md
