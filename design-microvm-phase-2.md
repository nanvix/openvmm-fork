# OpenVMM microVM migration phase 2: snapshot and restore

**Status:** Proposed

**OpenVMM baseline:** `ed2a1a274d60d31572ce6c7df444a8b59721211c`

**NVX baseline:** [`2ebc48690f6cf8c22ce2e58e7f5b7e23cf327a86`](https://github.com/nanvix/nvx/tree/2ebc48690f6cf8c22ce2e58e7f5b7e23cf327a86)

**Depends on:** [Phase 1: base machine](design-microvm-phase-1.md)

## Outcome

Phase 2 adds guest-requested capture, source-process termination, and restore
in a new process for the complete phase-1 microVM machine on Linux/KVM and
Windows/WHP.

The snapshot is an OpenVMM format, not an import or export of standalone NVX
`MVMSNAP*` or `WHPSNAP*` files. Initial restore requires the same hypervisor
kind as capture. The same committed snapshot can be restored repeatedly
without a restored VM modifying its memory artifact.

## Parity contract

Both backends must provide:

- a write to PMIO `0x605` that completes before capture;
- a capture boundary at the instruction immediately after that `out`;
- quiesced device, processor, interrupt, and clock state;
- source-process exit after a successful commit;
- authoritative reconstruction in a new process;
- immutable snapshot artifacts;
- private copy-on-write guest RAM;
- explicit rejection of incompatible CPU, backend, device, and attachment
  state; and
- equivalent failure behavior before and after the publication commit point.

The OpenVMM design matches pinned NVX's downtime behavior while strengthening
several persistence and compatibility checks:

| Area | Pinned NVX | OpenVMM phase-2 contract |
|---|---|---|
| Manifest | Backend-local magic/version only | Authoritative machine, CPU, device, attachment, and artifact manifest |
| Integrity | No artifact digests | Strict structural and semantic validation; no embedded payload checksums |
| Publication | KVM writes final files directly; WHP renames individual temp files | Same-filesystem staging directory and atomic directory rename |
| CPU portability | Destination-derived CPUID | Recorded and reproducible effective CPU contract |
| Downtime | WHP advances TSC and guest time by elapsed host time | Apply pinned NVX's advance-by-downtime policy consistently on KVM and WHP |
| Block device | Not present | Optional immutable read-only OpenVMM extension |

These are OpenVMM microVM ABI guarantees, not claims about standalone NVX
snapshot compatibility.

## Scope

### Included

- asynchronous routing of guest snapshot requests;
- bounded and fallible quiescing;
- exact saved-state inventory;
- manifest-authoritative restore;
- staged checksum-free publication;
- immutable/private memory restore;
- same-backend CPU and TSC compatibility;
- coherent TSC, paravirtual clock, RTC, PIT, LAPIC timer, and interrupt state
  across host downtime;
- saved state for every phase-1 device;
- immutable read-only virtio-blk media;
- stable host attachment identities;
- new-process restore and capture-and-exit;
- negative and corruption tests on KVM and WHP.

### Deferred

- cross-KVM/WHP restore;
- writable block snapshots;
- mutable external filesystem snapshots;
- capture-and-continue;
- live migration;
- importing standalone NVX snapshots;
- cryptographic authenticity or encryption of snapshot artifacts.

## Baseline investigation

### Current OpenVMM request and lifecycle behavior

`VmRpc` supports pause, save, and resume but has no guest-generated snapshot
notification or transactional capture operation
(`openvmm/openvmm_defs/src/rpc.rs:21-30`).

The current REPL/controller save flow:

1. pauses the VM;
2. calls `VmRpc::Save`;
3. syncs the memory backing;
4. writes a snapshot; and
5. leaves the original VM paused.

This path is in `openvmm/openvmm_entry/src/vm_controller.rs:462-509`.
Resume prevention is a REPL-local Boolean
(`openvmm/openvmm_entry/src/repl.rs:843-886`), not a worker invariant. There
is no TTRPC snapshot surface.

Pinned NVX coordinates snapshot capture in each backend runtime loop. OpenVMM
should instead route a small nonblocking device event to the controller; the
observable guest boundary remains the same.

### Current state-unit behavior

`StateUnits` already starts in dependency order, stops in reverse dependency
order, restores in dependency order, and saves only stopped units
(`vmm_core/state_unit/src/lib.rs:472-616`).

Important gaps:

- stop cannot report a device-drain failure;
- communication failures in state transitions can panic;
- cancelling a partially completed global stop is not a safe rollback;
- a newly registered unit with no matching saved state remains at its default;
- current dependencies do not fully order timer/RTC consumers around VM time;
- some device drains, including virtio-blk, have no timeout.

A microVM capture therefore needs a fallible quiesce transaction rather than a
timeout wrapped around `StateUnits::stop()`.

### Existing saved state

Reusable state includes:

- normalized partition and vCPU state;
- PIC, IOAPIC, LAPIC, PIT, and VM reference time;
- virtio-mmio device status, feature selectors, negotiated features, queue
  configuration/progress, configuration generation, and interrupt status;
- virtio-blk queue drain and completion.

The normalized x86 VP state includes general registers, activity, XSAVE, APIC,
XCR0, XSS, MTRRs, PAT, architectural MSRs, debug registers, TSC, CET, and
TSC_AUX. It does not establish a portable CPU contract by itself.

Confirmed gaps include:

- effective CPUID is not serialized;
- the source hypervisor and effective TSC frequency are not saved;
- `IA32_TSC_DEADLINE` is not represented;
- KVM cannot recover one class of pending non-injected external interrupt;
- KVM CET shadow-stack restore is currently a no-op;
- partition halt state is incomplete;
- restore capability mismatches can reach assertions;
- queue prevalidation does not check all alignment, overlap, and GPA coverage
  before workers start.

### Current on-disk snapshot safety

The warning in the original design remains valid at the pinned OpenVMM
baseline.

`SnapshotManifest` currently contains only version, creation time, OpenVMM
version, memory size, VP count, page size, and architecture
(`openvmm/openvmm_helpers/src/snapshot.rs:17-38`).

`write_snapshot()`:

- creates or reuses the final directory;
- writes final `manifest.bin` and `state.bin` directly;
- replaces `memory.bin`; and
- hard-links the live writable guest-memory file
  (`openvmm/openvmm_helpers/src/snapshot.rs:47-100`).

There is no staging directory, artifact digest, bounded read, file/directory
flush protocol, or exclusive final destination. Restore opens `memory.bin`
read-write (`openvmm/openvmm_entry/src/lib.rs:2422-2464`).

Low-level file mappings are shared:

- Unix uses `MAP_SHARED` in `support/sparse_mmap/src/unix.rs`;
- Windows selects writable shared section/view access in
  `support/sparse_mmap/src/windows.rs`;
- `openvmm/membacking` has no file-backed copy-on-write mode.

The current VMM snapshot test verifies save files and resumes the same process;
it does not prove new-process restore or immutability.

## Required architecture

### 1. Guest request route

The PMIO `0x605` device must:

1. return all ones on reads;
2. complete any write width/value immediately;
3. latch at most one outstanding request;
4. send a nonblocking notification to the VM worker/controller; and
5. clear or retire the latch only after the transaction outcome is known.

A bounded channel or atomic pending bit prevents a guest from allocating
unbounded work. Repeated writes while one capture is pending are coalesced and
logged only through a rate-limited event.

The vCPU I/O callback must not pause processors, drain devices, hash memory, or
write files.

If no destination is configured, the request is ignored, a rate-limited host
warning is emitted, and guest execution continues.

### 2. Capture transaction and commit point

Preflight while the VM is still running:

- verify the microVM profile and ABI;
- require KVM or WHP and a configured destination;
- require that the final destination does not exist;
- prove same-filesystem staging is possible;
- reject writable disks;
- resolve immutable attachment identities;
- bound expected state and memory sizes;
- capture the effective CPU/TSC contract; and
- reject another capture already in progress.

After preflight:

1. gate host-input producers;
2. pause processors so no vCPU remains in a run call;
3. quiesce and drain device workers with bounded deadlines;
4. stop timer consumers and VM time in dependency order;
5. save all state units;
6. capture RAM and the backend compatibility contract;
7. write and verify staged artifacts;
8. atomically rename the staging directory to the final destination;
9. report success; and
10. tear down the worker and source process.

The directory rename is the commit point. The source must never resume after
commit.

Before commit, a failure may resume only if every stopped unit confirms a
valid rollback/start transition. If any unit has uncertain state, terminate
the VM with an error and expose no final snapshot.

### 3. Fallible quiesce protocol

Add a bounded lifecycle operation such as:

```text
QuiesceForSave(deadline)
ResumeAfterFailedSave
```

It must distinguish:

- cleanly stopped;
- cleanly drained;
- retryable timeout before state ownership changed;
- non-recoverable partial transition.

Use an explicit dependency chain:

```text
host-input producers
        depends on
partition/processors
        depends on
chipset and virtio workers
        depends on
VM time
```

This produces:

```text
capture stop: input -> processors -> devices -> VM time
restore start: VM time -> devices -> processors -> input
```

Device-specific state must represent every guest-visible operation that was
accepted but not completed. A declaration of `supports_save_restore()` is not
evidence that the drain contract is met.

### 4. Exact state and device inventory

The manifest is authoritative for:

- machine profile and ABI;
- RAM layout;
- vCPU topology;
- effective boot command line;
- device presence, order, address, IRQ, transport, feature mask, and queue
  limits;
- state-unit names, including units with no mutable state; and
- required host attachments.

Before creating a partition, compare the complete manifest inventory with the
candidate machine composition. Reject missing, extra, duplicated, reordered,
or default-added devices.

This closes a current `StateUnits::restore()` behavior where a newly registered
unit without saved state silently remains initialized.

### 5. Snapshot publication

Publish:

```text
parent/
  .<snapshot>.staging-<unique>/
    state.bin
    memory.bin
    manifest.bin
```

Requirements:

1. create the staging directory and files exclusively;
2. stream bounded state and memory without computing payload checksums;
3. use a filesystem COW/reflink clone when available, otherwise make a sparse
   copy from stopped guest memory;
4. never hard-link a shared-writable inode;
5. flush state and memory;
6. write and flush the manifest last;
7. flush the staging directory where supported;
8. rename the complete staging directory to the previously nonexistent final
   path; and
9. flush the parent directory where supported.

On Windows, use a same-volume directory rename and document the platform's
directory-flush guarantee. Do not emulate atomicity by deleting an existing
destination or renaming artifacts one by one.

The manifest contains fixed relative artifact names. It must reject absolute
paths, parent traversal, symlinks/reparse-point escapes, oversized files, and
unexpected artifacts.

The v3 manifest does not embed checksums for `state.bin` or `memory.bin`.
Regular-file, exact-length, bounded-decoding, and machine-contract validation
remain mandatory, but same-length payload changes are not detected.
Authenticated export or transport integrity is separate work.

### 6. Private copy-on-write RAM

Add a file mapping mode through `SharedMemoryBacking`, `RamBackingRequest`,
`MappingBacking`, `MappingParams`, and `VaMapper`:

```rust
enum FileMappingMode {
    Shared,
    CopyOnWrite,
}
```

Restore opens the verified memory artifact read-only, then gives the guest a
writable private view:

- KVM/Linux: `PROT_READ | PROT_WRITE` with `MAP_PRIVATE`;
- WHP/Windows: a write-copy section/view using `PAGE_WRITECOPY` and
  `FILE_MAP_COPY`.

Mark the result as private so it cannot later be exported as a shared restart
backing. Validate the file length and each RAM-range-to-file-offset mapping
before mapping.

A conformance test must restore the same snapshot, dirty every RAM range,
terminate it, and restore the original snapshot again while proving with a
test-only payload comparison that `memory.bin` never changed.

### 7. CPU and TSC compatibility

Record a canonical effective CPU contract, not merely the physical-host
identity:

- source backend;
- CPU vendor;
- effective indexed CPUID leaves, including topology and XSAVE leaves;
- supported XCR0 and XSS masks;
- XSAVE component layout, sizes, offsets, and alignment;
- required architectural state elements and MSRs;
- APIC mode and IDs;
- effective TSC frequency and accepted tolerance.

Required backend work:

- expose the effective CPUID after KVM/WHP policy filtering;
- allow restore to request the saved contract rather than regenerate defaults;
- add KVM TSC-frequency get/set plumbing;
- capture and restore the KVM paravirtual clock and its guest MSR
  configuration;
- expose WHP processor-clock configuration before partition creation;
- add TSC-deadline saved state;
- reject unsupported CET/XSTATE/MSR elements rather than ignoring or
  asserting;
- restore frequency, a common reference TSC, and timer deadlines in the
  correct order.

Initial restore requires the same backend. Backend equality is necessary but
not sufficient: a destination host must reproduce the entire saved contract.
Mask poorly portable features such as nested virtualization, SGX, and CET from
ABI version 1 unless they are explicitly supported and tested.

### 8. Clock and interrupt semantics

`VmTimeKeeper` already saves stopped monotonic VM time and reanchors it to a
new host instant on start. Extend restore to add measured host downtime before
reanchoring, matching pinned NVX's guest-visible time policy.

PIT saves timer state and the last VM-time tick. Add explicit dependencies so
VM time restores before PIT/RTC and stops after them.

The RTC currently saves CMOS bytes and selector state but derives calendar
time from its live real-time source. Add a saved epoch/source offset so restore
resumes from the captured guest instant plus measured host downtime rather
than resetting independently from the destination clock.

On KVM, also save and restore the paravirtual KVM clock
(`KVM_GET_CLOCK`/`KVM_SET_CLOCK` and the guest's system-time/wall-clock MSR
configuration). Restoring only TSC and `VmTimeKeeper` is insufficient when
Linux selects `kvm-clock`.

Save and validate:

- pending and masked PIC/IOAPIC/LAPIC interrupt state;
- PIT remaining duration;
- LAPIC timer/deadline state;
- RTC guest epoch;
- VM monotonic time;
- TSC value and frequency;
- KVM paravirtual clock state when exposed; and
- capture wall-clock metadata used to compute downtime.

Apply one explicit policy on KVM, MSHV, and WHP: advance guest time by
nonnegative elapsed host wall time since capture, shorten or expire timer
deadlines by the same duration, and keep TSC, RTC, paravirtual clock, and VM
time coherent.
Reject a destination clock that moves backwards or produces an elapsed value
outside the supported bound rather than silently creating a time rollback.

An elapsed one-shot deadline produces at most one pending interrupt when it was
unmasked at expiry. An elapsed periodic timer preserves its phase at the
destination time and coalesces any missed periods into at most one pending
interrupt. A masked timer advances without manufacturing a pending interrupt.
Interrupts already pending at capture remain pending and are not duplicated.
PIT evaluates its restored counter state once against the advanced VM time;
PIC's pending bit provides the same coalescing rule for repeated IRQ0 edges.

Guest monotonic, boottime, and realtime clocks advance by the accepted downtime.
Process and thread CPU clocks do not advance because no guest instructions ran.
TSC, deadline, backend clock, and timer arithmetic is checked; overflow is a
restore error rather than a saturated or wrapped clock value.

Repeatable restore guarantees immutable memory and device state; it does not
mean that two restores performed at different wall times observe identical
clocks.

### 9. Phase-1 devices

The exact phase-1 inventory is:

- partition and vCPU;
- PIC, IOAPIC, and LAPIC;
- PIT, RTC, and VM time;
- microVM portb;
- shutdown and snapshot-request devices;
- optional virtio-mmio/virtio-blk.

Portb saved state includes bounded pending input and any output accepted by the
device but not yet acknowledged by the host backend. Capture must drain output
to a documented boundary with a timeout.

Shutdown and snapshot request have no mutable device state, but remain in the
manifest inventory.

Virtio-mmio already saves transport state. Extend restore prevalidation to
reject, before workers start:

- feature masks different from the saved effective mask;
- packed-ring state;
- wrong queue count or maximum;
- zero/non-power-of-two split-ring sizes;
- index overflow;
- bad alignment;
- descriptor/available/used overlap;
- arithmetic overflow; and
- ranges not wholly covered by guest RAM.

Virtio-blk currently drains active operations before returning queue state,
but its drain is unbounded. Add a deadline and an explicit failure result.

### 10. External block media

The snapshot does not contain external disk contents. A path, file ID, size,
timestamp, read-only flag, or virtio disk ID is not immutable content
identity.

Phase 2 permits:

- no block device; or
- read-only media identified by a SHA-256 content digest and length; or
- read-only media supplied by a provider-guaranteed immutable generation.

Define an attachment identity:

```text
ContentDigest { sha256, length }
ProviderGeneration { provider_id, generation, length }
```

Alternatively, copy the complete read-only image into snapshot-owned immutable
storage and record its digest.

Reject writable media during preflight before pausing. Writable snapshots are
deferred until the disk provider can create an immutable point-in-time source
and a private writable clone for each restore.

### 11. Authoritative restore ordering

Restore-time CLI/API input may select the matching host backend and provide
attachments keyed by stable IDs. It may not override RAM, vCPU count, ABI,
devices, features, placement, IRQs, or the effective command line.

Required order:

1. open the snapshot directory without following escape paths;
2. read a bounded manifest;
3. validate format, ABI, backend, architecture, topology, and path fields;
4. validate artifact types and exact lengths;
5. validate CPU/TSC reproducibility;
6. resolve and validate immutable attachments;
7. compare the exact machine/device/state-unit inventory;
8. decode bounded saved state;
9. prevalidate all device state and guest-memory ranges;
10. create private COW memory mappings;
11. create the partition and devices from the manifest;
12. restore VM time, devices, interrupt state, partition, and vCPU state;
13. reconnect host attachments;
14. start vCPUs only after every prior step succeeds.

No current CLI default is applied during restore. Missing attachments or
conflicting guest-visible options fail before partition creation.

## Proposed manifest

At minimum, encode:

```text
format_magic
manifest_version
machine_profile = MICROVM
microvm_abi_version
openvmm_saved_state_schema_version
saved_state_root_type
source_hypervisor
architecture
page_size

memory:
  total_bytes
  ranges[] { gpa_start, length, file_offset }
  artifact { relative_path, length, sha256 }

topology:
  vp_count
  sockets, dies, cores, threads
  apic_ids[]

cpu:
  vendor
  cpuid[] { function, index, eax, ebx, ecx, edx }
  xcr0_supported
  xss_supported
  xsave_layout[]
  required_state_elements[]
  required_msrs[]
  tsc_frequency_hz
  tsc_tolerance_ppm
  clock_policy = advance_by_host_downtime
  capture_wall_clock

boot:
  pvh_layout_version
  effective_command_line
  command_line_sha256

devices[]:
  stable_id
  state_unit_name
  kind and order
  PMIO/MMIO ranges
  IRQ
  transport
  effective_feature_banks[]
  queue_count and limits[]

attachments[]:
  stable_id
  kind
  required
  reconnect_policy
  immutable_identity

artifacts[]:
  kind
  relative_path
  length
  sha256
```

Manifest decoding and every repeated field need explicit size/count limits.
Create snapshot files with restrictive host permissions because they contain
guest memory and secrets.

## Replay entropy

Replaying a snapshot clones the guest's in-memory cryptographic RNG state.
Copy-on-write RAM does not make post-restore keys, nonces, UUIDs, or tokens
unique.

Provide an out-of-band restore attachment or guest-agent protocol that injects
fresh host entropy and forces a guest CRNG reseed before security-sensitive
work starts. Fresh entropy must never be stored in the reusable snapshot.
Guests without a cooperating reseed path must receive an explicit warning and
must not be advertised as safe for cloned cryptographic workloads.

## Failure contract

| Failure | Required outcome |
|---|---|
| No configured destination | Rate-limited warning; guest continues |
| Duplicate guest request | Coalesce; never run concurrent captures |
| Existing final destination | Fail before pausing |
| Writable or unidentified media | Fail before pausing |
| Missing/changed attachment | Fail before partition creation |
| Quiesce/drain timeout | No publication; resume only if rollback is proven |
| State or memory write failure | Remove the exact staging directory; expose no final snapshot |
| Atomic rename failure | Expose no final snapshot |
| Failure after commit rename | Treat capture as committed and terminate source |
| Oversized/truncated artifact | Reject before decode/allocation |
| Wrong artifact type or length | Reject before state decode or writable mapping |
| Added/removed/reordered device | Reject before partition creation |
| CPU/XSTATE/MSR/TSC mismatch | Explicit pre-start incompatibility error |
| Cross-backend restore | Explicit pre-start incompatibility error |
| Device restore failure | Tear down without running a vCPU |
| Unsupported saved capability | Typed error, never panic/assert |

Cleanup may delete only the uniquely named staging directory created by the
failed transaction. Never recursively remove an ambiguous destination.

## Ordered implementation plan

1. Finalize phase-1 stable device and state-unit identities.
2. Add bounded manifest decoding and exact inventory validation.
3. Add staged publication, exact artifact lengths, and restrictive
   permissions.
4. Add file-backed COW mapping modes to `sparse_mmap` and `membacking`.
5. Add backend CPU-contract discovery/reproduction and TSC-frequency control.
6. Add missing TSC-deadline, interrupt, RTC-epoch, and capability validation.
7. Add fallible bounded quiesce and the correct VM-time dependency chain.
8. Complete portb output barriers and pending-input saved state.
9. When the optional block extension is configured, add immutable media
  identity and writable rejection; do not make it gate no-block microVM
  parity.
10. Add PMIO notification, one controller transaction, rollback, and
    capture-and-exit.
11. Make restore manifest-authoritative before constructing resources.
12. Add CLI/TTRPC attachment and restore surfaces.
13. Replace same-process save-only tests with new-process restore tests.
14. Document the snapshot format, host limitations, and security model.

## Expected code areas

| Area | Primary paths |
|---|---|
| PMIO request and controller event | New microVM device module, `openvmm_defs`, `openvmm_entry/src/vm_controller.rs` |
| Lifecycle | `vmm_core/state_unit`, `openvmm_core/src/worker/dispatch.rs` |
| Snapshot manifest/publication | `openvmm/openvmm_helpers/src/snapshot.rs` |
| Restore CLI/API | `openvmm/openvmm_entry/src` and TTRPC definitions |
| COW memory | `openvmm/membacking` and `support/sparse_mmap` |
| CPU/TSC state | `virt_kvm`, `virt_whp`, `virt`, and partition-unit code |
| Clock/RTC | `vmcore/src/vmtime.rs`, KVM clock state, PIT, CMOS RTC |
| Virtio validation | `vm/devices/virtio/virtio/src/transport` |
| Block identity | disk backend/resource abstractions |
| Integration tests | Petri and `vmm_tests` |

## Acceptance gates

Run each applicable test on Linux/KVM, Linux/MSHV, and Windows/WHP:

| Test | KVM | MSHV | WHP |
|---|:---:|:---:|:---:|
| Guest `out 0x605` captures and source PID exits | Required | Required | Required |
| New PID resumes at the instruction after `out` | Required | Required | Required |
| No destination leaves the guest running | Required | Required | Required |
| Repeated requests produce one transaction | Required | Required | Required |
| Portb output drains exactly once | Required | Required | Required |
| Pending portb input survives; host handles do not | Required | Required | Required |
| TSC, VM time, RTC, and timer deadlines advance coherently over downtime | Required | Required | Required |
| KVM paravirtual clock remains coherent with TSC and RTC | Required | N/A | N/A |
| Pending/masked interrupt state remains coherent | Required | Required | Required |
| Active immutable block I/O completes exactly once, when configured | Required | Required | Required |
| Writable or changed configured media is rejected before capture/start | Required | Required | Required |
| Same snapshot restores twice after the first VM dirties all RAM | Required | Required | Required |
| Test-only payload comparison proves `memory.bin` remains unchanged after both restores | Required | Required | Required |
| Restore-time entropy injection makes cloned guest RNG output diverge | Required | Required | Required |
| Malformed, truncated, oversized, symlinked, and wrong-type artifacts are rejected | Required | Required | Required |
| Invalid virtio queue state is rejected before workers run | Required | Required | Required |
| Added, removed, or reordered devices are rejected | Required | Required | Required |
| ABI, topology, and command-line mismatch are rejected | Required | Required | Required |
| CPU, XSTATE, MSR, and TSC mismatch are rejected | Required | Required | Required |
| Cross-backend restore fails before partition creation | Required | Required | Required |
| Capture failure never exposes a final directory | Required | Required | Required |

The sequencing test should have the guest write a marker immediately after
`out 0x605`. The marker must not be present in captured source state and must
appear exactly once after restore.

## Completion criterion

Phase 2 is complete only when a guest-triggered snapshot on each host is
published atomically, the source process exits, a separate process restores
from structurally validated immutable artifacts, and every incompatibility fails before
guest execution. Saving and resuming the original process is not an acceptable
proxy.
