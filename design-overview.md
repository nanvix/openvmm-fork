# OpenVMM microVM backend migration overview

**Status:** Proposed

**OpenVMM baseline:** `ed2a1a274d60d31572ce6c7df444a8b59721211c`

**NVX baseline:** [`2ebc48690f6cf8c22ce2e58e7f5b7e23cf327a86`](https://github.com/nanvix/nvx/tree/2ebc48690f6cf8c22ce2e58e7f5b7e23cf327a86)

## Goal

Implement an NVX-compatible, versioned `microvm` profile that runs on the
existing KVM and WHP hypervisor backends. The work is divided into five phases
so that boot, snapshot infrastructure, and each stateful virtio device can be
reviewed and validated independently on Linux/KVM and Windows/WHP.

The target is feature parity between the two OpenVMM host backends:

- the same Xen PVH guest artifacts boot on both hosts;
- both expose the versioned microVM guest ABI;
- each delivered device works on both hosts;
- snapshots capture and restore active guest-visible state coherently; and
- host-specific resources are reconstructed without leaking KVM or WHP types
  into the machine profile.

## Architecture

Machine selection remains independent from hypervisor selection:

```text
openvmm --machine microvm --hypervisor kvm --kernel kernel.elf --initrd rootfs.cpio
openvmm --machine microvm --hypervisor whp --kernel kernel.elf --initrd rootfs.cpio
```

The microVM profile owns:

- Xen PVH boot and initial CPU state;
- the RAM and MMIO layout;
- fixed PMIO and virtio-mmio devices;
- deterministic Linux command-line generation;
- lifecycle and snapshot-request behavior;
- device feature masks and queue limits; and
- snapshot compatibility and attachment requirements.

KVM and WHP continue to provide partition creation, processor execution,
interrupt injection, and backend-specific host integration. Existing OpenVMM
chipset, virtio, resource-resolution, and state-unit implementations are reused
where their behavior satisfies the microVM ABI.

## Delivery phases

| Phase | Deliverable | Primary result |
|---|---|---|
| [1. Base machine](design-microvm-phase-1.md) | Machine identity, PVH loader, memory layout, chipset, PMIO devices, fixed virtio-mmio, and optional virtio-blk | The same kernel and initramfs boot to userspace on KVM and WHP without firmware, ACPI, PCI, or a device tree. |
| [2. Snapshot and restore](design-microvm-phase-2.md) | Guest-requested capture, atomic immutable artifacts, private RAM restore, CPU/time compatibility, and manifest-authoritative reconstruction | A guest snapshot exits the source process and restores in a new process on the same backend. |
| [3. virtio-console](design-microvm-phase-3.md) | Fixed single-port console, generic device-private virtio state, active RX/TX capture, and endpoint reconstruction | Bidirectional console traffic survives new-process restore without replaying or losing VMM-owned bytes. |
| [4. virtio-net](design-microvm-phase-4.md) | Static IPv4 model, deterministic MACs, egress policy, KVM TAP, WHP user-mode networking, and packet-progress state | Both hosts provide the same guest network contract while recreating their native host data planes. |
| [5. virtio-fs](design-microvm-phase-5.md) | No-DAX HostFs, FUSE namespace/handle state, bounded quiesce, and live attachment revalidation | Open files, directories, aliases, and cookies restore against a freshly validated host-directory attachment. |

The dependency chain is:

```text
Phase 1: base machine
        |
Phase 2: snapshot foundation
        |
Phase 3: generic device-private virtio state and console
        |
        +---- Phase 4: network
        |
        +---- Phase 5: filesystem
```

The numbered delivery remains sequential so every merged phase has complete
KVM and WHP coverage before the next device expands the snapshot contract.

## Versioned guest ABI

ABI version 1 reserves all device locations from phase 1 so later phases do
not renumber existing devices:

| Device | Address | Interrupt | Phase |
|---|---:|---:|---:|
| microVM portb data/status | PMIO `0xe9` / `0xea` | None | 1 |
| CMOS RTC | PMIO `0x70` / `0x71` | Existing RTC route | 1 |
| Shutdown | PMIO `0x604` | None | 1 |
| Snapshot request | PMIO `0x605` | None | 1/2 |
| PIT | Standard PIT ports | IRQ 0 | 1 |
| virtio-net | MMIO `0xd0000000` | KVM 10, WHP 5 | 4 |
| virtio-fs | MMIO `0xd0001000` | 6 | 5 |
| virtio-console | MMIO `0xd0002000` | 7 | 3 |
| virtio-blk extension | MMIO `0xd0003000` | 4 | 1 |

All microVM virtio devices use deterministic discovery tokens and split rings;
`VIRTIO_F_RING_PACKED` is masked. Guest-visible configuration, device order,
feature masks, placement, IRQs, and the effective command line become
snapshot-authoritative after phase 2.

## Cross-cutting migration work

### Configuration and composition

Add a versioned `MachineProfile::Microvm` to public configuration, CLI, TTRPC,
worker validation, Petri, and snapshot manifests. Build the microVM machine from
an allowlist rather than removing devices from a standard PC manifest.

### Boot and memory

Add a checked x86-64 Xen PVH ELF loader and initial `RBX` support. RAM ends at
3 GiB, resumes at 4 GiB, and leaves a fixed 1-GiB MMIO gap. Loader and device
addresses must be range-checked without panicking on malformed input.

### Fixed device transport

Extend virtio-mmio construction with explicit address, IRQ, feature-mask, and
publication metadata. microVM devices do not use dynamic slot allocation and are
not described through ACPI.

### Saved-state model

Phase 2 establishes bounded, fallible quiescing and exact state-unit
inventory. Phase 3 extends virtio saved state with typed device-private
payloads needed by console, network, and filesystem devices.

Every device must preserve:

- negotiated features and queue configuration;
- queue and interrupt progress;
- accepted but incomplete guest-visible work;
- bounded buffered data; and
- stable attachment identities.

Native handles, threads, sockets, TAP descriptors, filesystem handles, and
other process-local objects are recreated rather than serialized.

### Snapshot persistence

Snapshots use staged, checksummed artifacts and an atomic directory rename.
RAM restores through private copy-on-write mappings so one restore cannot
modify the reusable memory artifact. Restore verifies the manifest, artifacts,
CPU contract, device inventory, and host attachments before creating workers
or running a vCPU.

Initial snapshots are backend-local: KVM snapshots restore on compatible KVM
hosts and WHP snapshots restore on compatible WHP hosts. Cross-backend
conversion is deferred.

### Host attachments

Snapshot state contains guest-visible device state, not external resources.
Restore resolves stable attachment IDs for serial endpoints, network data
planes, block media, and host directories.

- virtio-blk snapshots require immutable read-only media when the optional
  block extension is configured;
- virtio-console recreates listeners, clients, or supplied handles according
  to an explicit reconnect policy;
- virtio-net recreates TAP or user-mode networking and reapplies egress
  policy; and
- virtio-fs reopens and revalidates saved identities against a fresh live
  attachment, with immutable generations available as optional hardening.

## Shared safety requirements

- Treat ELF files, snapshots, guest addresses, virtio descriptors, packets,
  FUSE requests, paths, and restore attachments as untrusted.
- Use checked arithmetic and bounded collections throughout parsing, capture,
  and restore.
- Never panic on malformed guest or snapshot input.
- Rate-limit guest-triggerable diagnostics.
- Do not perform snapshot I/O synchronously from a vCPU PMIO callback.
- Fail before guest execution when CPU state, device state, or attachments
  cannot be reproduced.
- Preserve path confinement and least privilege while reopening host
  resources.
- Inject fresh entropy before cloned guests perform security-sensitive work,
  because snapshot replay duplicates in-memory RNG state.

## Validation strategy

Each phase adds focused unit tests plus end-to-end VMM tests on Linux/KVM and
Windows/WHP. After phase 2, acceptance requires process teardown and restore
in a separate process; saving and resuming the original VM is not sufficient.

The final matrix must demonstrate:

- identical PVH boot artifacts on both hosts;
- the same Alpine Linux guest image booting to userspace on both hosts;
- measured performance matching pinned NVX on the same physical host under
  the same documented workload and host configuration;
- exact fixed device layout and feature masks;
- interrupt, timer, RTC, and command-line behavior;
- active-I/O snapshot coverage for every delivered device;
- no lost, duplicated, or falsely completed guest-visible work;
- explicit rejection of corrupt or incompatible snapshots;
- repeated private-memory restore; and
- attachment reconstruction and failure behavior on both hosts.

## Deferred work

- SMP;
- cross-KVM/WHP snapshot conversion;
- standalone NVX snapshot import/export;
- capture-and-continue and live migration;
- non-x86 guests;
- virtio-fs DAX and `SectionFs`;
- writable block snapshots without an external point-in-time provider; and
- additional devices outside the version-1 microVM profile.

## Completion criterion

The migration is complete when all five phases pass their KVM and WHP
acceptance gates using the same guest artifacts and one documented,
versioned microVM ABI. Separate host-specific implementations that merely
boot similar guests do not satisfy the parity target.
