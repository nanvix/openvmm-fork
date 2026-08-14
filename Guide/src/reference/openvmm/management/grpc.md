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

`CreateVMRequest.microvm_snapshot` exposes the phase-2 microVM capture and
restore flow over both transports:

* `destination_path` configures guest-requested capture. Supply the normal
	ABI-v1 microVM configuration, including PVH boot files and memory. The path
	must not exist. `quiesce_timeout_ms` defaults to five seconds when zero.
* `restore_path` selects manifest-authoritative restore. `config` may be absent,
	or may contain only the microVM profile, serial port 0 host attachment, and
	guest power actions. Guest-visible boot, memory, processor, device, NUMA, and
	PCIe fields are rejected.
* `restore_entropy` requests fresh entropy and is valid only with
	`restore_path`.

Capture and restore paths are mutually exclusive. A successful capture halts
the managed source VM at the committed boundary and terminates the OpenVMM
source process; clients observe the transport closing. Restore creates a private
copy-on-write RAM view and leaves the VM paused until `ResumeVM`.

The API has the same KVM/WHP backend, no-block device, artifact integrity, and
security restrictions documented under [`--snapshot-destination`].

[`vmservice.proto`]: https://github.com/microsoft/openvmm/blob/main/openvmm/openvmm_ttrpc_vmservice/src/vmservice.proto
[`--snapshot-destination`]: ./cli.md
