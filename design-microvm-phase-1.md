# OpenVMM microVM migration phase 1: base machine

**Status:** Proposed

**OpenVMM baseline:** `ed2a1a274d60d31572ce6c7df444a8b59721211c`

**NVX baseline:** [`2ebc48690f6cf8c22ce2e58e7f5b7e23cf327a86`](https://github.com/nanvix/nvx/tree/2ebc48690f6cf8c22ce2e58e7f5b7e23cf327a86)

## Outcome

Phase 1 introduces a versioned microVM profile that boots the same
uncompressed Xen PVH Linux kernel and Alpine Linux userspace on Linux/KVM and
Windows/WHP. Its ABI follows the NVX memory and port-I/O contract, with common
interrupt and timer plumbing, fixed virtio-mmio placement, and an optional
OpenVMM-defined virtio-blk extension. Snapshot capture is explicitly
unavailable until phase 2.

This is a machine-profile migration, not a new hypervisor:

```text
openvmm --machine microvm --hypervisor kvm --kernel kernel.elf --initrd rootfs.cpio
openvmm --machine microvm --hypervisor whp --kernel kernel.elf --initrd rootfs.cpio
```

Later phases add [snapshot and restore](design-microvm-phase-2.md),
[virtio-console](design-microvm-phase-3.md),
[virtio-net](design-microvm-phase-4.md), and
[virtio-fs](design-microvm-phase-5.md).

## Parity definition

The target is feature parity between the OpenVMM KVM and WHP implementations:
both hosts expose the same boot, device, lifecycle, and validation
capabilities. The profile preserves the documented NVX ABI where it is
consistent across the pinned backends, but it does not reproduce accidental
backend quirks.

The initial OpenVMM microVM ABI should make these deliberate choices:

| Area | Phase-1 contract |
|---|---|
| CPU topology | One x86-64 vCPU on both backends. Pinned NVX KVM SMP is deferred because pinned NVX WHP is single-vCPU. |
| Interrupts | Common OpenVMM PIC, IOAPIC, and hypervisor LAPIC integration on both hosts. |
| Timer | Common i8253 PIT on IRQ 0. Do not reproduce pinned NVX WHP's separate 10-ms heartbeat. |
| RTC | NVX-compatible binary, 24-hour CMOS behavior, anchored to UTC on both hosts. |
| Command line | The profile owns generated discovery and console tokens and rejects conflicting user tokens. |
| Portb input | Bounded host-input buffering with a documented overflow policy. Pinned NVX's queue is unbounded. |
| Block | One optional standard virtio-blk device at the OpenVMM-defined fixed slot. Pinned NVX has no block device. |
| Snapshot port | Port `0x605` is present, but writes are no-ops and all host snapshot entry points fail explicitly. |

These choices must be encoded in `MachineProfile::Microvm { abi_version }`. A
future change to guest-visible behavior requires an ABI-version change rather
than backend-dependent drift.

## Scope

### Included

- public machine selection independent of hypervisor selection;
- x86-64 Xen PVH ELF loading;
- the microVM RAM and MMIO layout;
- deterministic Linux command-line construction;
- one-vCPU KVM and WHP machine composition;
- PIC, IOAPIC, LAPIC routing, PIT, RTC, and VM time;
- raw bidirectional portb, shutdown, and snapshot-request PMIO devices;
- fixed-address virtio-mmio without ACPI publication;
- one optional virtio-blk device;
- CLI, TTRPC, Petri, unit, and VMM-test plumbing.

### Deferred

- snapshots and restore;
- virtio-console, virtio-net, and virtio-fs;
- SMP;
- firmware, ACPI, SMBIOS, PCI, and device-tree boot;
- non-x86 guests;
- cross-KVM/WHP snapshots;
- compatibility with standalone NVX snapshot files.

## Baseline investigation

### Existing pieces that can be reused

| Capability | Existing OpenVMM implementation |
|---|---|
| Hypervisor selection | `KvmHandle`, `WhpHandle`, and resource probing in `openvmm/hypervisor_resources/src/lib.rs` |
| Public VM configuration | `openvmm_defs::Config` in `openvmm/openvmm_defs/src/config.rs` |
| Worker construction | `openvmm/openvmm_core/src/worker/dispatch.rs` |
| Memory layout policy | `openvmm/openvmm_core/src/worker/memory_layout.rs` |
| Loader imports | `vm/loader/src` and `vmm_core/vm_loader/src` |
| Chipset composition | `vmm_core/vm_manifest_builder/src/lib.rs` |
| PIC, IOAPIC, PIT, RTC | Existing chipset devices and resource resolvers |
| LAPIC | Selected hypervisor partition/vCPU implementation |
| VM time | `VmTimeKeeper` and its state unit |
| Virtio transport | `vm/devices/virtio/virtio/src/transport/mmio.rs` |
| Virtio block | `vm/devices/virtio/virtio_blk` and its resolver |
| Device lifecycle | `vmm_core/state_unit` and motherboard state-unit registration |
| End-to-end tests | Petri and `vmm_tests` |

### Confirmed gaps

No microVM profile exists at the OpenVMM baseline. In particular, there is no
machine-profile identity, PVH load mode, fixed virtio-mmio configuration,
microVM port device, or snapshot request at `0x605`.

`BaseChipsetType::UnenlightenedLinuxDirect` is not a safe starting point. It
adds serial, Hyper-V power management, and missing-port devices in addition to
PIC, PIT, RTC, and IOAPIC
(`vmm_core/vm_manifest_builder/src/lib.rs:494-534`). The microVM manifest must be
constructed from an allowlist.

The current low-MMIO boundary is derived from the caller-provided
`chipset_low_mmio_size`; it does not guarantee NVX's fixed 1-GiB
`0xc0000000..0xffffffff` gap
(`openvmm/openvmm_core/src/worker/memory_layout.rs:180-195`). Current
virtio-mmio construction allocates sequential 4-KiB slots and assigns one
shared IRQ, either PIC IRQ 5 or IOAPIC IRQ 17
(`openvmm/openvmm_core/src/worker/dispatch.rs:2786-2828`).

The current direct-Linux loader constructs ACPI/SMBIOS data, a Linux zero page,
page tables, and 64-bit initial state with `RSI` pointing to the zero page
(`vm/loader/src/linux.rs:595-724`). It cannot implement the PVH contract by
configuration alone.

## Guest-visible ABI

### RAM and fixed reservations

RAM occupies `[0, min(size, 3 GiB))` and resumes at 4 GiB. The range
`0xc0000000..0xffffffff` is the MMIO gap.

| Guest physical address | Contents |
|---|---|
| `0x500..0x51f` | Bootstrap GDT |
| `0x520` | Empty IDT |
| `0x6000` | Xen `hvm_start_info` |
| `0x6040` | Optional initramfs `HvmModlistEntry` |
| `0x7000` | PVH memory map |
| `0x20000` | NUL-terminated command line, at most 64 KiB including terminator |
| `0x9fc00` | Reserved for a future KVM SMP MP table |
| `0x100000` and above | Kernel and ordinary RAM |
| `0xc0000000..0xffffffff` | MMIO gap |
| `0x100000000` and above | RAM displaced by the MMIO gap |

Every loader and DMA range must be checked with overflow-safe arithmetic and
must fit wholly inside one RAM region. No write may cross the MMIO gap or
another reserved boot structure.

### PVH entry state

The loader must:

1. require ELF64, little-endian, `EM_X86_64`;
2. copy checked `PT_LOAD` segments to `p_paddr` and explicitly zero BSS;
3. parse `PT_NOTE` and require Xen note type 18,
   `XEN_ELFNOTE_PHYS32_ENTRY`;
4. place an optional initramfs page-aligned at the top of low RAM;
5. construct version-1 `hvm_start_info` with magic `0x336ec578`;
6. publish a RAM-only PVH memory map;
7. enter flat 32-bit protected mode with paging disabled; and
8. start at the Xen physical entry with `EBX = 0x6000`.

The initial CPU state is:

| Register/state | Value |
|---|---|
| `RIP` | Xen physical-entry note |
| `RBX` | `0x6000` |
| `RSP` | `0` |
| `RFLAGS` | `2` |
| `CR0` | protected-mode enable set |
| `CR3`, `CR4`, `EFER` | `0` |
| CS | flat 32-bit, attribute `0xc09b` |
| Data segments | flat 32-bit, attribute `0xc093` |
| TSS | selector `0x18`, attribute `0x008b` |
| GDTR | base `0x500`, limit `31` |
| IDTR | base `0x520`, empty |

The loader must not emit a Linux zero page, page tables, ACPI, SMBIOS, or
firmware data.

### Device reservations

All slots are reserved in ABI version 1 even before their delivery phase, so
later phases cannot renumber earlier devices.

| Device | Address | Interrupt | Delivery |
|---|---:|---:|---:|
| microVM portb data/status | PMIO `0xe9` / `0xea` | None | Phase 1 |
| CMOS RTC | PMIO `0x70` / `0x71` | Existing RTC route | Phase 1 |
| Shutdown | PMIO `0x604` | None | Phase 1 |
| Snapshot request | PMIO `0x605` | None | Phase 1 device, phase 2 persistence |
| PIT | Standard PIT ports | IRQ 0 | Phase 1 |
| Interrupt controllers | Standard PIC/IOAPIC/LAPIC | Platform routing | Phase 1 |
| virtio-net | MMIO `0xd0000000` | KVM 10, WHP 5 | Phase 4 |
| virtio-fs | MMIO `0xd0001000` | 6 | Phase 5 |
| virtio-console | MMIO `0xd0002000` | 7 | Phase 3 |
| virtio-blk | MMIO `0xd0003000` | 4 | Phase 1 extension |

Every microVM virtio transport masks `VIRTIO_F_RING_PACKED`; ABI version 1 uses
split rings only.

## Required migration

### 1. Configuration, CLI, API, and worker validation

Add a machine identity next to `load_mode` in `openvmm_defs::Config`:

```rust
enum MachineProfile {
    Standard,
    Microvm { abi_version: u32 },
}
```

Add a dedicated load mode carrying an ELF kernel, optional initramfs, and
effective command line. The initramfs is optional in pinned NVX; making it
mandatory would be an OpenVMM product restriction, not parity.

`--machine microvm` must remain independent of `--hypervisor`. Extend:

- CLI parsing and configuration construction;
- `VmWorkerParameters`;
- TTRPC boot configuration, which currently exposes only direct boot and UEFI;
- Petri's OpenVMM backend;
- snapshot manifests in phase 2.

Validate twice: once before launching a worker for a useful user error, and
again inside the worker as the trust boundary. Reject non-x86 guests,
hypervisors other than KVM/WHP, any vCPU count other than one, UEFI, PCAT,
IGVM, device-tree mode, isolation/VTL2, nested virtualization, VMBus,
PCI/VPCI, firmware, graphics, TPM, IDE, floppy, NVMe, UART/debugcon, and
devices not enabled by the active phase.

Restore will eventually derive guest-visible configuration from a snapshot.
Keep machine identity independent from cold-boot CLI defaults now so phase 2
does not need to unwind inferred configuration.

### 2. Dedicated machine composition

Add `BaseChipsetType::Microvm` to
`vmm_core/vm_manifest_builder/src/lib.rs`. Its manifest should instantiate
only:

- generic PIC;
- generic IOAPIC;
- PIT;
- an RTC configured for the microVM mode;
- portb;
- shutdown;
- snapshot request; and
- the normal unknown-PIO fallback.

LAPIC state belongs to the selected hypervisor partition. Do not add an
independent microVM LAPIC implementation.

Do not inherit serial UARTs, debugcon, Hyper-V power management, gameport or
VMware missing-port shims, PCI, firmware, or ACPI helpers. Reserve all four
virtio-mmio windows in the memory layout, but instantiate only configured
phase-1 devices.

### 3. Memory-layout support

Extend the centralized layout builder rather than creating an independent
allocator:

- reserve a 1-GiB low-MMIO gap;
- make high RAM resume at 4 GiB;
- reserve the fixed virtio windows;
- preserve file offsets for discontinuous RAM;
- reject RAM and address arithmetic overflow;
- expose the final ranges to the PVH memory-map builder.

Add unit tests for RAM below and above 3 GiB, a kernel spanning a RAM boundary,
initramfs overlap, command-line overflow, and fixed-device collisions.

### 4. PVH loader and initial registers

Add a checked PVH module under `vm/loader/src` and a worker wrapper under
`openvmm/openvmm_core/src/worker/vm_loaders`.

`vm/loader/src/elf.rs` provides parsing patterns but currently returns the ELF
header entry and does not parse Xen notes. Do not extend its existing contract
with implicit PVH behavior.

Add `X86Register::Rbx` and update every exhaustive consumer:

- loader import conversion in `vm/loader/src/importer.rs`;
- initial-state application in `vmm_core/vm_loader/src/initial_regs.rs`;
- IGVM relocation pass-through;
- SNP VP-context generation; and
- TDX VP-context generation.

Non-PVH consumers must preserve or explicitly reject `Rbx`; no wildcard arm
may silently drop it. All malformed ELF, note, placement, and integer-overflow
conditions return typed errors without panicking.

### 5. Effective command line

Construct the command line once from the machine manifest in stable device-slot
order. The phase-1 generated portion is:

```text
earlycon=xe9 console=hvc0 reboot=t panic=-1
```

Append one `virtio_mmio.device=<size>@<base>:<irq>` token for each instantiated
virtio device. Reserve `earlycon=`, `console=`, and `virtio_mmio.device=` from
user-supplied arguments; reject conflicts rather than relying on last-token
precedence. Reject embedded NUL and enforce the complete 64-KiB bound before
loading.

Pinned NVX lets the user replace its base command line and then appends device
tokens. The stricter ownership model is intentional and must be part of the
OpenVMM ABI.

### 6. Interrupt, timer, RTC, and time behavior

Reuse OpenVMM's common PIC/IOAPIC/LAPIC routing and PIT on both backends. Add a
cross-host test that proves IRQ0 delivery; this deliberately replaces pinned
NVX WHP's calibration PIT plus heartbeat implementation.

The existing OpenVMM CMOS device cannot be reused unchanged. Its default is
BCD/24-hour and it supports a fuller interrupt model, while pinned NVX reports
binary/24-hour mode with status B `0x06`. Add a configuration mode or narrow
adapter that provides:

- binary 24-hour fields;
- UTC on both backends;
- the documented status-register values;
- only the interrupt behavior included in the ABI; and
- state-unit integration with VM time.

This normalization removes pinned NVX's KVM-local-time/WHP-UTC discrepancy.

### 7. microVM PMIO devices

The existing Bochs-compatible debugcon is not reusable. It is output-only,
restricts access width, returns a magic read value, and transforms newlines.

Implement a small bidirectional portb device:

- writes to `0xe9` forward raw bytes without newline conversion;
- reads from `0xe9` return one pending byte or zero in byte zero and clear the
  remainder;
- reads from `0xea` return availability bit 0 in byte zero and clear the
  remainder;
- input has a fixed bound and explicit drop-oldest, drop-newest, or backpressure
  policy;
- host-triggerable errors and overflow logs are rate-limited;
- arbitrary valid PMIO access widths follow the pinned NVX byte ordering.

Shutdown writes at `0x604` use the first output byte as the process exit code.
Existing `HaltReason::PowerOff` discards this value, so add a typed microVM
event or halt reason carrying `u8` through the worker and controller.

At `0x605`, reads return all ones and writes complete without pausing or
terminating the guest. Build the device around a notification resource that
phase 2 can activate, but do not send snapshot work synchronously from the I/O
callback.

Unhandled PMIO reads return all ones and writes are discarded without
faulting the guest.

### 8. Fixed virtio-mmio and virtio-blk

Extend virtio construction with explicit placement metadata:

```text
stable device ID
MMIO base and length
IRQ
effective feature mask
ACPI publication policy
```

The microVM path must not consume the sequential allocator or publish DSDT
entries. It must reserve and validate each exact window before device
resolution.

For virtio-blk:

- permit zero or one device;
- place it at `0xd0003000`, length `0x1000`, IRQ 4;
- route it directly to `VirtioBus::Mmio`;
- mask packed-ring support before feature exposure and negotiation;
- include its discovery token in the effective command line;
- preserve existing read-only and writable operation during phase 1 because
  snapshot capture is unavailable.

The current storage builder routes unqualified virtio-blk through VPCI, so the
microVM configuration path needs an explicit transport override.

### 9. Lifecycle and phase-1 snapshot rejection

Chipset devices added through motherboard helpers automatically receive state
units. Register stable names now because they become snapshot identifiers in
phase 2.

Recommended phase-1 state behavior:

| Unit | State behavior |
|---|---|
| portb | Explicitly not saveable while pending input exists |
| shutdown | No mutable saved state |
| snapshot request | No mutable saved state |
| PIC/PIT/RTC | Existing save path, with RTC mode additions |
| virtio-mmio/blk | Existing save path, not exposed through host APIs yet |

Generic save may otherwise succeed because the reused platform and block
devices already implement saved state. Reject `VmRpc::Save`, disk snapshot,
and pulse-save/reset/restore for the microVM profile before traversing state
units, using a stable typed error.

### 10. Petri and VMM-test integration

Add a Petri machine option that selects microVM without inheriting standard-PC
firmware or PCI setup. The test kernel/initramfs must include the relevant PVH,
`hvc_xe9`, virtio-mmio, and virtio-blk drivers. Without `hvc_xe9`, the
phase-1 `console=hvc0` contract is not exercised.

Every end-to-end test has a Linux/KVM and Windows/WHP variant using the same
kernel and initramfs artifacts. Tests should inspect behavior, not merely
worker construction.

## Ordered implementation plan

1. Add machine identity and ABI version to CLI, API, worker, and Petri types.
2. Define the normalized interrupt, PIT, RTC, command-line, and portb policies
   as ABI-version-1 constants.
3. Add the microVM base-chipset allowlist and stable state-unit names.
4. Add the 3-GiB/4-GiB RAM split and fixed MMIO reservations.
5. Implement checked PVH structures, ELF-note parsing, and placement.
6. Add `Rbx` and update all initial-register consumers.
7. Add deterministic command-line construction and validation.
8. Implement portb, shutdown-exit-code, and inactive snapshot-request devices.
9. Add fixed virtio-mmio placement and feature masking.
10. Route one optional virtio-blk attachment to the fixed slot.
11. Explicitly reject all snapshot/save entry points.
12. Add unit, CLI/TTRPC, Petri, and host-matrix VMM tests.
13. Update the OpenVMM Guide for the new machine and CLI arguments.

## Security and failure requirements

- Treat the ELF image, initramfs, command line, guest addresses, PMIO access,
  virtio descriptors, and resource handles as untrusted.
- Use checked arithmetic for every address, size, alignment, and table count.
- Never panic on malformed guest virtio state or host-provided boot images.
- Rate-limit guest-triggerable logs.
- Reject unsupported devices before resolving host handles or creating a
  partition.
- Do not expose a partially constructed standard-PC machine on validation
  failure.
- Keep native KVM and WHP types below the hypervisor abstraction; microVM device
  code receives only common interrupt, time, memory, and notification
  resources.

## Acceptance gates

| Test | Linux/KVM | Windows/WHP |
|---|:---:|:---:|
| Same uncompressed PVH ELF reaches userspace | Required | Required |
| Same Alpine Linux guest image boots to userspace | Required | Required |
| Optional initramfs is described as a PVH module | Required | Required |
| Exact GDT, IDT, start-info, memory-map, and register state | Required | Required |
| RAM ends at 3 GiB and resumes at 4 GiB | Required | Required |
| No firmware, ACPI, SMBIOS, PCI, UART, or device tree | Required | Required |
| Deterministic command line and reserved-token rejection | Required | Required |
| PIC/IOAPIC/LAPIC interrupt delivery | Required | Required |
| PIT IRQ0 progress and normalized timing behavior | Required | Required |
| Binary 24-hour UTC RTC behavior | Required | Required |
| Raw binary portb output and input/status reads | Required | Required |
| Shutdown returns the guest-provided status byte | Required | Required |
| `out 0x605` completes and execution continues | Required | Required |
| Host snapshot and pulse-save requests fail explicitly | Required | Required |
| Block appears at `0xd0003000`, IRQ 4 | Required | Required |
| Block read/write policy and interrupt completion | Required | Required |
| Packed-ring negotiation is unavailable | Required | Required |
| Measured performance matches pinned NVX on the same host | Required | Required |

Performance comparisons must run the same Alpine Linux guest artifacts and
the same documented boot and workload benchmarks under OpenVMM and the pinned
NVX baseline on the same physical host. CPU affinity, host configuration, and
benchmark parameters must be held constant, and the results must fall within
a documented tolerance chosen before measurement.

Negative unit and API tests must also cover malformed ELF notes, overlapping
segments, BSS zeroing, overflow, command-line NUL/length errors, bad machine
combinations, non-KVM/WHP resources, wrong vCPU count, fixed-layout conflicts,
invalid virtio queues, and unintended device exposure.

## Completion criterion

Phase 1 is complete only when the same guest artifacts pass the full matrix on
both hosts and the observed guest ABI is described by one versioned OpenVMM
microVM profile. Booting separately through ad hoc KVM and WHP configuration
paths is not sufficient.
