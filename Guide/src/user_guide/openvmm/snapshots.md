# Snapshots

OpenVMM supports saving and restoring VM snapshots, allowing you to capture
the complete state of a running VM and resume it later.

## Overview

A snapshot captures three pieces of state:

- **Guest RAM** — the full contents of guest memory
- **Device state** — the saved state of all emulated devices
- **Manifest** — metadata describing the snapshot (architecture, memory size,
  VP count, page size, etc.)

These are stored as three files in a snapshot directory:

| File            | Contents                                    |
|-----------------|---------------------------------------------|
| `manifest.bin`  | Protobuf-encoded snapshot metadata          |
| `state.bin`     | Serialized device state                     |
| `memory.bin`    | Memory backing file                         |

## Prerequisites

Host-driven snapshots require **file-backed guest memory**. Pass `file=<PATH>`
in the `--memory` option when launching a standard VM. A microVM launched with
`--snapshot-destination` automatically creates temporary file-backed RAM in
the destination's parent directory when no backing file was supplied.

```admonish warning
The memory backing file and snapshot destination must be on the **same
filesystem**. OpenVMM copies memory into a uniquely named sibling staging
directory and atomically renames the completed directory into place.
```

## Saving a snapshot

Start a VM with file-backed memory:

```bash
cargo run -- \
  --uefi \
  --vmbus-scsi id=scsi0 \
  --disk memdiff:file:path/to/disk.vhdx,on=scsi0 \
  --memory size=4096M,file=path/to/memory.bin
```

Once the VM is running, open the interactive console and issue a save command,
specifying the output directory:

```text
save-snapshot path/to/snapshot-dir
```

OpenVMM writes and flushes `manifest.bin`, `state.bin`, and an independent
`memory.bin` in a sibling staging directory. The destination must not already
exist. Publishing the completed directory is the commit point.

```admonish warning
After a host-driven save, the VM remains **paused**. Guest-requested microVM
capture instead terminates the source process after publication commits.
```

## Restoring a snapshot

To restore, pass the snapshot directory with `--restore-snapshot`:

```bash
cargo run -- \
  --uefi \
  --vmbus-scsi id=scsi0 \
  --disk memdiff:file:path/to/disk.vhdx,on=scsi0 \
  --memory size=4096M \
  --processors 4 \
  --restore-snapshot path/to/snapshot-dir
```

`--restore-snapshot` verifies and opens `memory.bin` from the snapshot
directory, so `file=...` should not be specified in `--memory` (the two options
are mutually exclusive). Guest writes use a private copy-on-write mapping and
do not modify the snapshot artifact.

```admonish warning
Version 3 snapshots do not contain or validate embedded checksums for
`state.bin` or `memory.bin`. Restore still requires regular files, bounded
manifest and state decoding, exact artifact lengths, and a compatible machine
contract, but same-length payload changes are not detected. Protect snapshot
directories with host access controls. Integrity or authentication for export
and transport must be supplied outside the default snapshot format.
```

```admonish note
The `--memory` and `--processors` values must match the values recorded in
the snapshot manifest. If they do not match, OpenVMM will report a
validation error and refuse to start.
```

## Device configuration on restore

For standard-machine snapshots, device flags must still be supplied on restore
and must reproduce the saved machine. For microVM ABI-v1 snapshots, the
manifest is authoritative for RAM, topology, ABI, fixed devices, placement,
features, interrupts, and the effective PVH command line. Restore-time
guest-visible overrides are rejected.

The ABI-v1 CPU contract records the effective CPUID/XSTATE surface and TSC
frequency. WHP microVMs use a reproducible 1 GHz virtual TSC configured before
partition setup; restore recreates and validates that rate before any vCPU
runs. KVM snapshots likewise require the destination to reproduce their saved
backend CPU and clock contract.

Every snapshot records a complete state-unit inventory. Each emulated device
saves state under a unique name (for example `"pit"`, `"vmbus"`, or `"ide"`),
and restore requires the saved and current inventories to match exactly. A
microVM manifest additionally records and validates the exact device inventory
and order.

For a phase-3 virtio console, the manifest also records its stable attachment
ID, canonical endpoint identity, reconnect policy, requiredness, and timeout.
Native socket, pipe, terminal, and file handles are never serialized. Restore
recreates listeners, reconnects required clients, or requires an inherited
replacement before starting the partition. Accepted host input and a partial
guest transmit offset live in the device-private virtio payload, preserving
their order across a new-process restore. Host input is gated before the vCPU
snapshot boundary and resumed only if capture rolls back.

For microVM virtio-fs, the manifest records the stable attachment ID, pinned
root identity, guest mount target, access mode, no-DAX queue policy, and
`live-revalidate` restore mode. The device-private payload records FUSE
negotiation, namespace IDs and aliases, lookup counts, reopenable handles,
bounded directory-entry snapshots and cookies, and queue progress. Native file
descriptors and Windows handles are never serialized. An already-open
directory continues through its captured entry list; a newly opened directory
observes the current host tree.

Restore requires a fresh
`--mount <GUEST_TARGET,HOST_PATH[,ro|rw]>` attachment. The target and mode must
match the manifest. OpenVMM validates the new root and every saved object
identity before starting a vCPU.

```admonish warning
The host directory is external live state, not snapshot content. Host
mutations after capture can change restored reads or make restore fail. A
read-write restore also changes the shared host directory, and restoring the
same VM snapshot again does not roll those changes back.
```

The rules are:

| Scenario | Result |
|---|---|
| Device set matches exactly | Restore succeeds |
| Snapshot contains a device not in current config | **Restore fails** — unknown unit name |
| Current config has a device not in snapshot | **Restore fails** — inventory mismatch |

In practice this means:

- You must pass the **same device flags** on restore as you did on save.
  Removing a device that was present at save time will cause restore to
  fail.
- Adding a new device that was not present at save time fails inventory
  validation rather than starting an unenumerated device in its default state.

```admonish warning
Inventory errors identify the saved and current state-unit lists. Compare the
restore configuration with the capture configuration when restoring a standard
machine.
```

## Device save/restore support

Not all devices support save/restore. If a VM includes a device that does
not support saving, the `save-snapshot` command will fail with
`SaveError::NotSupported`.

The following table summarises support for the device types relevant to
OpenVMM snapshots:

| Device | Bus | Save/Restore |
|---|---|---|
| PIT, PIC, I/O APIC, DMA | Chipset (ISA) | Yes |
| CMOS RTC, Power Management | Chipset (ISA) | Yes |
| i8042 (PS/2 keyboard/mouse) | Chipset (ISA) | Yes |
| Serial 16550 | Chipset (ISA) / PCI | Yes |
| UEFI firmware | Chipset (MMIO) | Yes |
| Framebuffer | Chipset (MMIO) | Yes |
| TPM | Chipset (MMIO) | Yes |
| IDE controller | PCI | Yes |
| PIIX4 bridges, bus, PM, RTC | PCI | Yes |
| Generic PCI bus | PCI | Yes |
| StorVsp (SCSI) | VMBus | Yes |
| NetVsp (NIC) | VMBus | Yes |
| Shutdown / Timesync / KVP ICs | VMBus | Yes |
| VMBus Keyboard / Mouse / Video | VMBus | Yes |
| Guest Emulation Log | VMBus | Yes |
| virtio-blk | Virtio (PCI/MMIO) | Yes |
| virtio-net | Virtio (PCI/MMIO) | microVM ABI-v1 only |
| virtio-pmem | Virtio (PCI/MMIO) | Yes |
| virtio-rng | Virtio (PCI/MMIO) | Yes |
| virtio-console | Virtio (PCI/MMIO) | Yes |
| NVMe | PCI | **No** |
| VGA | PCI | **No** (`todo!()`) |
| GDMA (MANA network) | PCI | **No** (`todo!()`) |
| PCIe root complex / switch | PCIe | **No** |
| Assigned PCI (pass-through) | PCI | **No** |
| Relayed vPCI | PCI | **No** |
| PCAT BIOS firmware | Chipset (ISA) | **No** (see limitations) |
| virtio-fs | Virtio (MMIO) | microVM ABI-v1 HostFs only |
| virtio-9p | Virtio (PCI/MMIO) | **No** |
| Guest Crash Device | VMBus | **No** |
| Guest Emulation Device (GED) | VMBus | **No** |
| VMBus serial (host) | VMBus | **No** |
| Vmbfs | VMBus | **No** |

```admonish tip
If you are unsure whether your VM configuration supports snapshots, try
issuing `save-snapshot` to a scratch directory. The save will fail
immediately with a clear error if any active device does not support it.
```

## Limitations

- Snapshots are **not portable** across architectures (e.g., you cannot
  restore an x86_64 snapshot on aarch64)
- Restores use private copy-on-write RAM, so a committed snapshot can be
  restored repeatedly without copying it or modifying `memory.bin`.
- VMs using VPCI or PCIe devices do not currently support save/restore
- OpenHCL-based VMs do not currently support this snapshot mechanism
- VMs using PCAT firmware do not support save/restore
- Standard-machine restore still requires matching `--memory` and
  `--processors`. MicroVM ABI-v1 restore reads them authoritatively from the
  manifest and rejects overrides.
