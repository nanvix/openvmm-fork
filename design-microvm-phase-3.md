# OpenVMM microVM migration phase 3: virtio-console

**Status:** Proposed

**OpenVMM baseline:** `ed2a1a274d60d31572ce6c7df444a8b59721211c`

**NVX baseline:** [`2ebc48690f6cf8c22ce2e58e7f5b7e23cf327a86`](https://github.com/nanvix/nvx/tree/2ebc48690f6cf8c22ce2e58e7f5b7e23cf327a86)

**Depends on:** [Phase 1: base machine](design-microvm-phase-1.md) and
[phase 2: snapshot and restore](design-microvm-phase-2.md)

## Outcome

Phase 3 adds the standard single-port virtio-console device at MMIO
`0xd0002000`, IRQ 7, on Linux/KVM and Windows/WHP. The device supports
bidirectional traffic, deterministic Linux console selection, active-I/O
snapshot, and explicit reconstruction of its host endpoint in a new process.

The raw microVM portb remains present for early output and recovery. The
effective console command line changes from:

```text
earlycon=xe9 console=hvc0 reboot=t panic=-1
```

to:

```text
earlycon=xe9 console=hvc1 reboot=t panic=-1
```

The complete effective command line remains snapshot-authoritative.

## Parity contract

Both backends expose:

- one standard non-multiport virtio-console device;
- two split virtqueues: receive queue 0 and transmit queue 1;
- fixed address `0xd0002000` and IRQ 7;
- identical effective feature masks;
- raw bidirectional byte transport;
- the same disconnect and reconnect policy;
- saved transport and device-private progress;
- no serialization of native pipe, socket, terminal, or file handles; and
- explicit restore failure when a required attachment cannot be recreated.

`VIRTIO_F_RING_PACKED` remains masked by the microVM profile even though
the current device advertises it.

## Baseline investigation

### Reusable device implementation

`vm/devices/virtio/virtio_console/src/lib.rs` already implements the correct
basic virtio model:

- virtio device ID 3;
- one port;
- receive queue 0 and transmit queue 1;
- `VIRTIO_CONSOLE_F_SIZE`;
- no `VIRTIO_CONSOLE_F_MULTIPORT`;
- event-index and indirect-descriptor support;
- a `SerialIo` backend;
- runtime disconnect/reconnect handling.

The worker keeps `partial_transmit`, the number of bytes already written from
the current guest TX descriptor. That field is specifically needed to avoid
re-sending data after cancellation/restart
(`virtio_console/src/lib.rs:181-206`).

The RX path reads at most 4096 bytes from the host and synchronously copies
them into the current guest descriptor. The TX path retains the queue
descriptor until all bytes have been written, then completes it.

Existing unit tests cover reconnect and partial writes, and current TTRPC/VMM
tests cover socket connection paths. There is no active snapshot test.

### Existing transport saved state

Virtio-mmio already saves:

- device status;
- negotiated feature banks and selectors;
- queue selection and configuration generation;
- queue sizes, addresses, enabled state, and progress; and
- MMIO interrupt status.

This state lives in
`vm/devices/virtio/virtio/src/transport/mmio.rs:393-451` and
`transport/saved_state.rs`.

### Confirmed gaps

`VirtioConsoleDevice` does not override `supports_save_restore()`, so the
default is false and transport save fails
(`vm/devices/virtio/virtio/src/device.rs:110-114`).

Stopping both console queues removes the worker and its
`partial_transmit`. The generic virtio saved-state schema has no opaque
device-private payload, so simply returning true from
`supports_save_restore()` would produce an incomplete snapshot.

The resolver converts `VirtioConsoleHandle` into a live `SerialIo` object and
then drops the declarative backend resource
(`virtio_console/src/resolver.rs:24-47`). The current handle contains only a
native serial-backend resource. It has no stable attachment ID or reconnect
policy (`virtio_resources/src/lib.rs:145-172`).

Current machine construction dynamically assigns virtio-mmio addresses and a
shared IRQ. Phase 1 must provide the fixed-placement adapter before this phase.

Current CLI behavior may automatically select PCI for virtio-console. The
microVM profile must reject PCI attachment and route its one console to the
reserved MMIO slot.

## Required migration

### 1. Machine manifest and discovery

Add one optional console entry to the microVM device manifest:

```text
stable ID: console:microvm-virtio0
kind: virtio-console
transport: virtio-mmio
base: 0xd0002000
length: 0x1000
IRQ: 7
queues: 2
multiport: false
packed ring: masked
attachment: console:microvm-virtio0
```

Reject multiple virtio consoles, PCI transport, alternate addresses/IRQs,
multiport configuration, and feature overrides.

Generate the console discovery token in fixed-address order. Switch the
primary Linux console to `hvc1` only when this manifest entry is present.
Retain `earlycon=xe9`; the portb path remains available even when the
virtio-console endpoint is disconnected.

On restore, device presence and command-line selection come from the snapshot
manifest. A newly configured console must not appear in an older phase-2
snapshot.

### 2. Generic device-private virtio state

Phase 3 is the first phase that cannot be represented by transport queue state
alone. Extend the virtio device/transport contract with a versioned
device-private blob rather than adding console fields to the MMIO transport.

A suitable object-safe contract must support:

```text
quiesce/save device-private state
validate a device-private state blob before starting workers
restore device-private state
resume after a failed capture when safe
```

The MMIO and PCI saved roots should carry the opaque typed payload without
interpreting it. Devices with no private state can continue to use an empty
payload.

Do not use `supports_save_restore()` as the only contract. The save operation
must be fallible and bounded, and restore must reject an absent, extra, or
wrong-schema device payload.

This generic extension is also required by virtio-net in phase 4 and
virtio-fs in phase 5.

### 3. Console device state

Refactor worker ownership so stopping queues does not discard private state.
The saved console state should contain:

```text
schema_version
config:
  columns
  rows
tx:
  current_descriptor_offset
rx:
  staged_input_bytes
disconnect_policy
```

The common transport state remains responsible for negotiated features,
queue addresses/progress, device status, and interrupt status.

Validate:

- exactly two queues;
- offset not greater than the current readable descriptor length;
- staged input at or below the ABI bound;
- config dimensions in range;
- expected schema and disconnect policy; and
- the exact saved feature mask.

Do not serialize the live connected/disconnected bit as an assertion about the
new host. Connectivity is re-evaluated after endpoint construction.

### 4. RX capture boundary

Define host input as accepted by the VM only after it is owned by a microVM
device buffer or copied into guest memory.

Add a bounded ingress buffer between `SerialIo` and the guest queue, or prove
and test that every supported `SerialIo::poll_read` implementation is
cancel-safe at the existing copy boundary. A bounded device-owned buffer is
preferred because it gives phase 2 a concrete state to save and a uniform
overflow policy.

During capture:

1. stop accepting new host input;
2. finish any synchronous guest-memory copy already in progress;
3. save bytes accepted into the device but not delivered to a guest
   descriptor; and
4. leave bytes still owned by the external peer outside the snapshot boundary.

On restore, saved bytes are delivered before newly received endpoint bytes.
This preserves ordering.

### 5. TX capture boundary

The existing `partial_transmit` offset must survive snapshot. Otherwise a
descriptor partially written before capture is replayed from byte zero after
restore.

Capture must either:

- reach a completed-descriptor boundary by flushing with a bounded deadline;
  or
- save the current descriptor's write offset and resume at that offset.

The second option is necessary for endpoints that cannot complete promptly.
Queue state must retain the same front descriptor until the offset reaches its
length.

The VMM can guarantee exactly-once forwarding to its host-backend boundary. A
generic byte stream cannot prove remote application consumption without an
application-level acknowledgment. Document this limit; do not claim durable
peer delivery from `poll_write` or `flush`.

### 6. Endpoint attachment model

Native handles and live `SerialIo` objects are process-local. Keep a
declarative attachment descriptor outside device saved state:

```text
stable_id
backend_kind
mode = listen | connect | inherited
endpoint_identity
reconnect_policy
required
```

Initial supported policies should be explicit:

| Policy | Restore behavior |
|---|---|
| `RecreateListener` | Rebind the saved Unix socket, named pipe, or TCP listener; restore succeeds once binding succeeds, and a peer may connect later. |
| `ReconnectClient` | Connect to the configured endpoint with a bounded timeout; fail if unavailable. |
| `RequireInheritedAttachment` | Restore-time API must supply a replacement handle under the stable ID. |
| `DiscardWhileDisconnected` | Preserve current runtime drain behavior only when explicitly selected by cold-boot policy. |

Anonymous in-process socket/pipe pairs cannot be recreated in a new process
unless the restore caller supplies the external side through an attachment
API. A path string alone is not a substitute for a required inherited handle.

The resolver should receive both the reconstructed backend resource and the
stable attachment metadata. Missing, wrong-kind, or conflicting attachments
fail before partition creation.

### 7. Disconnect behavior

The current implementation completes and discards guest TX descriptors while
the endpoint is disconnected. Preserve that behavior only when it is part of
the configured policy; otherwise a restore that has not yet reconnected could
silently lose output.

For listener endpoints, restore may start with no peer while retaining guest
TX according to a bounded buffering/backpressure policy. For required client
endpoints, finish reconnection before starting vCPUs.

All wait and buffering limits belong in the machine ABI or attachment policy,
not in backend-specific defaults.

### 8. Lifecycle ordering

Register the console transport/device state unit behind host-input gating and
ahead of VM time, following phase 2's quiesce order:

```text
stop input -> stop vCPUs -> stop console worker -> save transport/device
restore transport/device -> recreate endpoint -> start console -> start vCPUs
```

If endpoint construction fails, no vCPU may run. If capture fails before the
snapshot commit, restart the same endpoint and worker only after proving the
saved private state is still valid.

## Proposed saved and attachment schemas

### Device saved state

| Field | Purpose |
|---|---|
| `schema_version` | Typed compatibility |
| `columns`, `rows` | Guest-visible config |
| `partial_transmit` | Offset into the current uncompleted TX descriptor |
| `staged_rx` | Bounded accepted host input not yet delivered |
| `disconnect_policy_id` | Proves restore did not change guest-visible loss/backpressure semantics |

### Manifest attachment

| Field | Purpose |
|---|---|
| `stable_id` | `console:microvm-virtio0` |
| `backend_kind` | Socket, named pipe, TCP, inherited handle, or another supported serial backend |
| `mode` | Listen, connect, or supplied attachment |
| `endpoint_identity` | Canonical path/address or provider identity |
| `reconnect_policy` | Required behavior and bounded timeout |
| `required` | Whether restore can start without a peer/backend |

Host handles, task objects, sockets, and pipe instances are never serialized.

## Ordered implementation plan

1. Add the phase-3 manifest entry and command-line transition.
2. Add generic device-private saved-state hooks to the virtio transport.
3. Refactor console worker state so queue stop preserves TX/RX progress.
4. Add a bounded, device-owned RX staging buffer.
5. Implement typed console save, validation, restore, and failed-capture
   resume.
6. Add stable endpoint descriptors and restore attachment resolution.
7. Implement explicit listener/client/inherited reconnect policies.
8. Gate vCPU start on required endpoint reconstruction.
9. Add console unit tests and active-I/O new-process VMM tests on both hosts.
10. Update Guide documentation for console selection and reconnect behavior.

## Expected code areas

| Area | Primary paths |
|---|---|
| Console runtime/save state | `vm/devices/virtio/virtio_console` |
| Generic device-private state | `vm/devices/virtio/virtio/src/device.rs` and `transport` |
| Resource schema/resolver | `vm/devices/virtio/virtio_resources` and `virtio_console/src/resolver.rs` |
| Serial reconstruction | `openvmm/openvmm_entry/src/serial_io.rs` and serial backend resources |
| microVM manifest/command line | `openvmm_core/src/worker` and machine-profile configuration |
| Integration | Petri and `vmm_tests` |

## Security and failure requirements

- Bound saved RX bytes, endpoint names, paths, addresses, and reconnect waits.
- Validate saved offsets before indexing guest descriptors.
- Treat endpoint paths and inherited handles as untrusted restore input.
- Do not follow filesystem links outside an explicitly allowed socket/pipe
  namespace.
- Rate-limit guest-triggered malformed-descriptor and disconnect logs.
- Never report restore success while a required backend failed to bind or
  connect.
- Never panic on malformed queue or console-private saved state.

## Acceptance gates

| Test | Linux/KVM | Windows/WHP |
|---|:---:|:---:|
| Device enumerates at `0xd0002000`, IRQ 7 | Required | Required |
| Only standard single-port/two-queue behavior is exposed | Required | Required |
| Packed-ring negotiation is unavailable | Required | Required |
| `earlycon=xe9` remains and primary console is `hvc1` | Required | Required |
| Bidirectional binary traffic works | Required | Required |
| Partial host writes resume without replay | Required | Required |
| Accepted RX bytes preserve order across restore | Required | Required |
| Snapshot during concurrent RX/TX loses or duplicates no VMM-owned bytes | Required | Required |
| Listener endpoint is recreated in a new process | Required | Required |
| Required client/inherited endpoint missing at restore fails pre-start | Required | Required |
| Reconnect timeout is bounded and explicit | Required | Required |
| Restoring the same snapshot twice preserves the same console boundary | Required | Required |
| Corrupt private state or offset is rejected before vCPU start | Required | Required |

Tests must include an endpoint that deliberately performs short writes and
disconnects between writes. A test that snapshots only idle queues does not
satisfy phase 3.

## Completion criterion

Phase 3 is complete only when the same console configuration and reconnect
policy work on both hosts, and a new-process restore during active RX and TX
preserves transport progress without serializing process-local handles.
