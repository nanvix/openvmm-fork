# OpenVMM microVM migration phase 4: virtio-net

**Status:** Proposed

**OpenVMM baseline:** `ed2a1a274d60d31572ce6c7df444a8b59721211c`

**NVX baseline:** [`2ebc48690f6cf8c22ce2e58e7f5b7e23cf327a86`](https://github.com/nanvix/nvx/tree/2ebc48690f6cf8c22ce2e58e7f5b7e23cf327a86)

**Depends on:** [Phase 1: base machine](design-microvm-phase-1.md),
[phase 2: snapshot and restore](design-microvm-phase-2.md), and the generic
device-private virtio state introduced by
[phase 3](design-microvm-phase-3.md)

## Outcome

Phase 4 adds one NVX-compatible virtio-net device on Linux/KVM and
Windows/WHP. It preserves the pinned NVX static IPv4 model, deterministic MAC
addresses, egress policy, host-specific interrupt routing, endpoint
reconstruction, and guest-visible queue progress across snapshot.

The device is fixed at MMIO `0xd0000000`. KVM uses IRQ 10 and WHP uses IRQ 5,
matching the pinned NVX ABI. The different IRQ is intentional and becomes part
of each same-backend snapshot manifest.

## Parity contract

Both backends expose the same guest network model:

- one virtio-net device and one RX/TX queue pair;
- split rings only;
- one static IPv4 guest endpoint; IPv6 is not configured by the microVM
  profile;
- gateway derived as the first usable address;
- deterministic guest and gateway MAC addresses;
- equivalent allow-list, block-list, and exact TCP endpoint egress policy;
- backpressure that does not complete an unaccepted TX frame;
- validated restored queues and pending packet ownership;
- recreated host data planes rather than serialized native handles.

The backend binding follows pinned NVX:

| Backend | Default host data plane |
|---|---|
| Linux/KVM | Managed TAP, or a supplied preconfigured TAP |
| Windows/WHP | In-process user-mode NAT/network stack |

Active host TCP/UDP sockets, TAP descriptors, worker threads, and switch-port
handles are outside guest saved state. Connections represented by those
objects may need to reconnect after restore; virtio descriptor and packet
ownership must still remain coherent.

## Guest network ABI

`--net <IP/PREFIX>` identifies the guest. Accept IPv4 prefixes `/1` through
`/30`, derive the gateway as the first usable address, and reject the guest
selecting that address.

For:

```text
--net 10.0.0.2/24
```

the ABI is:

```text
guest address:  10.0.0.2
gateway:        10.0.0.1
netmask:        255.255.255.0
guest MAC:      52:54:00:00:00:02
gateway MAC:    52:54:00:00:00:01
```

The last three IPv4 octets form the last three MAC octets.

The machine appends both the fixed virtio-mmio discovery token and
snapshot-authoritative guest-bootstrap network tokens. Restore takes
addressing and MAC identity from saved state, not a new `--net` value.

## Baseline investigation

### Reusable OpenVMM implementation

`vm/devices/virtio/virtio_net/src/lib.rs` already provides:

- a pluggable `net_backend::Endpoint`;
- one queue pair;
- virtio feature/config-space handling;
- RX/TX descriptor validation;
- offload parsing;
- in-order completion tracking;
- pending RX and TX packet bookkeeping;
- duplicate-descriptor rejection;
- endpoint restart support;
- bounded batching and worker cancellation points.

The generic virtio-mmio transport already saves common queue and interrupt
state.

Existing endpoint resources are:

- null;
- Consomme user-mode networking;
- Windows DirectIO;
- Linux TAP.

They are defined in
`vm/devices/net/net_backend_resources/src/lib.rs`. There is no generic
socket endpoint resource; references to a network "socket" in the old design
must not imply one exists.

Current CLI plumbing can create Consomme, DIO, null, or open a named TAP
(`openvmm/openvmm_entry/src/lib.rs:2085-2153`).

### Confirmed saved-state defect

`VirtioNetDevice::stop_queue()` explicitly discards the active pair and
returns `None`, with a comment that queue save/restore is unsupported
(`virtio_net/src/lib.rs:418-450`). The same implementation returns
`supports_save_restore() == true`.

This is a correctness bug for snapshot claims: transport state alone cannot
preserve pending frames, completion order, or endpoint ownership. Phase 4
must implement real device-private state and should not rely on the current
Boolean.

### Guest ABI gaps

Current OpenVMM:

- dynamically assigns virtio-mmio placement and one shared IRQ;
- chooses a random MAC in CLI construction;
- does not expose the complete pinned NVX static endpoint/gateway contract;
- can open a named TAP but does not provide NVX's managed-TAP lifecycle;
- does not bind Consomme configuration to the NVX deterministic gateway/MAC
  model;
- has no microVM egress-policy surface;
- has PCI-oriented network integration tests rather than fixed-MMIO microVM
  tests.

### Endpoint restart is not snapshot restore

The coordinator can react to `EndpointAction::RestartRequired` and recreate
endpoint queues (`virtio_net/src/lib.rs:732-791`). This is useful prior art,
but it runs in the same process and does not serialize:

- accepted-but-incomplete TX;
- RX buffers owned by the endpoint or device;
- in-order completion cursors;
- static endpoint identity and MAC;
- host attachment/recreation policy.

## Required migration

### 1. microVM manifest entry

Add:

```text
stable ID: net:microvm0
kind: virtio-net
transport: virtio-mmio
base: 0xd0000000
length: 0x1000
IRQ: 10 on KVM, 5 on WHP
queue pairs: 1
packed ring: masked
attachment: net:microvm0
```

Reject multiple NICs, PCI transport, alternate placement, user-selected IRQ,
multiqueue, control queues, and an effective feature mask not included in ABI
version 1.

Preserve the backend-specific IRQs. Normalizing them would change the pinned
guest ABI and complicate restore compatibility without adding a phase-4
capability.

### 2. Effective feature contract

Derive an explicit microVM feature-mask constant from the pinned shared NIC model
and intersect it with OpenVMM device/endpoint capabilities. Do not expose all
features currently available from `VirtioNetDevice` by accident.

At minimum, lock down:

- one queue pair;
- MAC-address feature and exact MAC;
- link-status behavior;
- checksum/GSO capabilities actually implemented on both endpoints;
- merged receive-buffer behavior;
- absence of packed rings;
- absence of control-vq, multiqueue, RSS, and device-specific extensions unless
  verified against pinned NVX.

Save the effective feature banks in the manifest and reject any restore
difference before queue creation.

### 3. Static address and MAC configuration

Add a microVM-specific network configuration type:

```text
guest_ipv4
prefix_length
derived_gateway_ipv4
guest_mac
gateway_mac
```

Validate host bits, prefix range, network/broadcast addresses, gateway
collision, and deterministic MAC derivation with typed errors.

Cold boot generates guest-bootstrap command-line tokens from this type.
Snapshot saves the type with the NIC. Restore rejects a conflicting `--net`
and does not generate a new random MAC.

### 4. Egress policy

Implement the pinned run-scoped policies:

```text
--allow-host <IPv4-or-CIDR>
--block-host <IPv4-or-CIDR>
--allow-endpoint <IPv4>:<TCP-port>
```

The modes are mutually exclusive.

Required semantics:

- allow-list permits only listed IPv4 destinations/CIDRs;
- block-list permits IPv4 except listed destinations/CIDRs;
- endpoint mode permits only exact IPv4 TCP address/port pairs;
- endpoint mode rejects UDP, ICMP, IPv6, VLAN traffic, malformed packets, and
  all other TCP destinations;
- policy executes before TAP transmission or WHP host socket creation;
- malformed IPv4/VLAN traffic fails closed when a policy is active;
- TAP ARP needed to resolve the gateway is handled consistently with policy;
- DNS exceptions, if any, are explicit and identical to the selected mode.

Policy is host security configuration, not guest device state. Require it as a
restore attachment when the snapshot says a policy is mandatory. Record its
mode and a canonical digest in the manifest so restore cannot silently weaken
it; permit a stricter replacement only through an explicit policy rule.

Pinned NVX does not serialize policy and requires it again on restore.
Recording a requirement/digest is a deliberate OpenVMM hardening.

### 5. KVM TAP lifecycle

Support two attachment modes:

| Mode | Required behavior |
|---|---|
| Managed TAP | Create/configure a TAP, assign deterministic gateway IP/MAC, bring it up, and remove it on teardown. Privileged setup failure is explicit. |
| Supplied TAP | Open or receive a preconfigured TAP and validate, where possible, its link state, gateway IP/MAC, and ownership expectations. Leave it in place on teardown. |

Current `TapHandle` carries only an already opened FD. Add declarative
restore-time metadata and a provider that can reacquire it in a new process.

Do not serialize the FD. Restore either creates a new managed TAP or resolves a
supplied TAP attachment under `net:microvm0`.

Forwarding/NAT beyond the host remains operator policy for TAP. Do not report
internet reachability merely because guest-to-host traffic works.

### 6. WHP user-mode networking

Use or adapt Consomme to provide the pinned behavior:

- ARP response for the derived gateway;
- ICMP echo to the gateway;
- guest TCP proxying through host TCP streams;
- guest UDP proxying, including DNS;
- mapping gateway-destination connections to host loopback;
- the exact egress decision before opening a host socket.

Recreate this stack from saved endpoint identity plus restore-time policy.
Native host sockets and NAT connection objects are not serialized.

Windows DirectIO is an existing OpenVMM endpoint but is not the pinned NVX
default. Either reject it for ABI version 1 or define it as a separately
versioned extension with its own attachment and snapshot tests.

### 7. Device-private saved state

Use phase 3's generic virtio device-private state hook. Save:

```text
schema_version
guest IPv4/prefix and derived gateway
guest and gateway MAC addresses
link status
effective device feature banks
effective endpoint/offload capabilities
queue-pair lifecycle state
RX in-order completion state
TX in-order completion state
pending RX packet ownership and bounded payloads
pending TX descriptors/segments and completion ownership
endpoint generation/restart epoch
```

Transport state continues to own queue addresses, negotiated driver features,
indices, device status, and MMIO interrupt status.

Do not serialize native endpoint queues. Serialize only bounded packet data and
ownership needed to reproduce guest-visible completions.

### 8. Quiesce and packet ownership

The core invariant is:

> A guest TX descriptor is completed only after the host backend accepted the
> frame, and every accepted frame has exactly one owner at capture.

Capture order:

1. stop new host RX admission;
2. pause vCPUs so no new descriptors appear;
3. stop endpoint queue polling;
4. drive already reported endpoint completions;
5. wait with a deadline for submitted TX packets, or save a bounded
   VMM-owned representation when recall is supported;
6. retain accepted RX packets not yet copied to the guest;
7. save in-order completion cursors and virtqueue progress;
8. release native endpoint queues only after ownership is represented.

For TAP and DirectIO, packets already accepted by the external network cannot
be recalled. Wait for the endpoint's local completion boundary. For Consomme,
either drain into bounded shared-model state or add an explicit endpoint
quiesce result.

On restore:

1. validate all packet counts, lengths, descriptor IDs, and guest ranges;
2. recreate the endpoint;
3. rebuild completion and pending-packet state;
4. inject saved RX before admitting new host RX;
5. resume TX without replaying frames already accepted before capture.

Endpoint APIs need a fallible quiesce method that reports packet ownership;
`EndpointAction::RestartRequired` alone is insufficient.

### 9. Host connection continuity

The snapshot guarantees virtio-visible descriptor and packet coherence. It
does not serialize:

- host TCP/UDP sockets;
- NAT flow tables unless Consomme gains a separate portable state contract;
- packets still owned solely by the external network;
- remote peer state.

After restore, existing guest TCP flows may reset when their host-side proxy
connection was transient. Document and test this rather than implying that
recreating an endpoint preserves all transport sessions.

If uninterrupted proxy-flow continuity is required for parity, it must be a
separate Consomme saved-state deliverable, not inferred from queue restore.

### 10. Attachment reconstruction

The manifest attachment should contain:

```text
stable_id = net:microvm0
backend_kind = managed_tap | supplied_tap | user_mode_nat
backend_identity/provider
required egress-policy mode and digest
reconnect/recreate policy
```

Restore-time resources may supply a TAP name/FD provider or host policy but
cannot alter guest IP, MAC, features, address, IRQ, or queue shape.

Missing TAP, failed TAP validation, unavailable privilege, Consomme
construction failure, or policy mismatch fails before vCPU start.

## Ordered implementation plan

1. Add the fixed backend-specific manifest entry and discovery token.
2. Define the exact ABI-v1 virtio-net feature mask and one-pair limit.
3. Add validated static IPv4/gateway/deterministic-MAC configuration.
4. Add egress policy parsing, canonicalization, and shared enforcement.
5. Add KVM managed/supplied TAP resource providers.
6. Adapt Consomme to the pinned WHP gateway, loopback, DNS, and policy model.
7. Correct the false `supports_save_restore()` claim.
8. Implement endpoint quiesce and packet-ownership reporting.
9. Save/validate/restore complete device-private queue and packet progress.
10. Add stable endpoint attachments and policy reconstruction.
11. Add fixed-MMIO Petri/VMM tests on both hosts.
12. Document network addressing, privilege, policy, and restore limits.

## Expected code areas

| Area | Primary paths |
|---|---|
| Device and saved state | `vm/devices/virtio/virtio_net` |
| Endpoint lifecycle API | `vm/devices/net/net_backend` |
| Endpoint resources | `vm/devices/net/net_backend_resources` |
| TAP | Linux TAP resolver/provider code |
| User-mode NAT | Consomme implementation and resolver |
| CLI/configuration | `openvmm/openvmm_entry` and `openvmm_defs` |
| microVM manifest | `openvmm/openvmm_core/src/worker` |
| Integration | Petri and `vmm_tests` |

## Security and failure requirements

- Parse IP prefixes, CIDRs, ports, and packet headers without unchecked
  arithmetic.
- Apply egress policy before externally visible transmission on both hosts.
- Bound saved packet counts and total payload bytes.
- Validate descriptor IDs, completion cursors, queue sizes, and every GPA
  before endpoint workers start.
- Reject duplicate descriptor ownership.
- Never serialize or trust a raw native handle from snapshot data.
- Rate-limit malformed guest packet/descriptor logs.
- Fail closed on malformed packets when policy is active.
- Do not claim restore success if required policy or endpoint recreation
  failed.

## Acceptance gates

Run a common Consomme/user-mode test where practical and the native backend
path for each host:

| Test | Linux/KVM | Windows/WHP |
|---|:---:|:---:|
| Device enumerates at `0xd0000000` | Required | Required |
| Interrupt is IRQ 10 on KVM and IRQ 5 on WHP | Required | Required |
| Exactly one split-ring queue pair is exposed | Required | Required |
| Static IPv4/gateway and deterministic MAC match the ABI | Required | Required |
| ARP and host-gateway reachability work | Required | Required |
| TCP and UDP behavior matches the supported backend contract | Required | Required |
| Allow-list, block-list, and endpoint policy fail closed | Required | Required |
| Sustained bidirectional traffic handles backpressure | Required | Required |
| Snapshot under RX/TX load preserves each guest completion exactly once | Required | Required |
| Saved RX packets precede new endpoint RX after restore | Required | Required |
| Address/MAC/feature/queue mismatch is rejected pre-start | Required | Required |
| Missing TAP, NAT, or policy attachment is rejected pre-start | Required | Required |
| Managed/supplied TAP lifecycle behaves as documented | Required | N/A |
| User-mode NAT, DNS, and gateway-loopback behavior works | Optional common path | Required |
| Same snapshot restores twice without stale endpoint handles | Required | Required |

Add an HTTP round trip before and after new-process restore. Also test a
deliberately saturated endpoint so a TX descriptor remains pending at capture;
an idle-link snapshot is not sufficient.

## Completion criterion

Phase 4 is complete only when KVM/TAP and WHP/user-mode networking present the
same microVM guest contract, enforce equivalent policy, and restore active
virtqueue ownership without serializing host endpoints or falsely completing
unaccepted packets.
