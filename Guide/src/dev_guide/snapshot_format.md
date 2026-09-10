# Snapshot Format

OpenVMM snapshot I/O validates and publishes one exact local machine-state
generation at a time.

## Directory layout

```text
snapshot-dir/
├── manifest.bin   # Bounded protobuf machine and artifact inventory
├── state.bin      # Protobuf device state
└── memory.bin     # Exact RAM backing or an independent copy
```

## Manifest and publication

`openvmm_helpers::snapshot` records artifact lengths, format and saved-state
schema identifiers, memory ranges, CPU and clock contracts, and complete
state-unit inventory. The initial microVM constructor supports the base
single-vCPU profile without device attachments or memory expansion.
Additional protobuf fields are reserved but are not accepted by this profile.

Publication writes and flushes a private sibling staging directory, then
renames it without replacing an existing destination. `SnapshotWriteError`
distinguishes rollback-safe failures, uncertain live-RAM alias cleanup, and
failures after the publication commit point. An owned RAM handle may be
promoted only when the source is stopped and will terminate after commit.
User-supplied RAM is copied independently.

The local format is not an authenticated container: structural validation
does not authenticate same-length payload modifications. Export and transport
layers must provide any required broader integrity guarantee.

## Private restore memory

`OpenedSnapshot` retains directory and artifact handles for one generation.
Artifact names cannot be replaced between validation and mapping to select
different files. Restore checks exact EOF and observable file generation,
then creates writable private copy-on-write mappings; snapshot bytes remain
unchanged. `SnapshotRestoreGuards` keep the exact handles alive through VM
teardown.

Windows rejects reparse points and retains read-only sharing guards. Unix
retains no-follow descriptors, but descriptors alone do not provide mandatory
write exclusion.

## Device state architecture

Each VM component that participates in save/restore is registered as a
"state unit" with a unique string name via `StateUnits::add("name")`.
During save, every state unit receives a `StateRequest::Save`. Units that
have state return `Ok(Some(blob))`; units with no persistent state (e.g.
the input distributor) return `Ok(None)` and are omitted from `state.bin`.

The resulting `state.bin` contains a `Vec<SavedStateUnit>`, where each
entry pairs a unit name with its opaque protobuf-encoded state blob.

### Restore matching rules

During restore, `StateUnits::restore()` matches saved-state entries to
currently registered units **by name**:

| Scenario | Result |
|---|---|
| Names match exactly | State is dispatched to the unit |
| Saved entry has no matching unit | **Error** — `unknown unit name` |
| Unit exists with no saved entry | Unit is skipped (keeps default state) |
| Duplicate name in saved state | **Error** — `duplicate unit name` |

This means removing a device between save and restore will fail, but
adding a new device is allowed (it initialises to its power-on defaults).

### Unit naming conventions

- **Chipset devices** — registered via `arc_mutex_device("name")` in
  `vmotherboard`, e.g. `"pit"`, `"rtc"`, `"uefi"`, `"ide"`.
- **VMBus devices** — named `"{interface_name}:{instance_id}"`, e.g.
  `"StorageVsp:ba6163d9-..."`. The instance GUID makes each offer
  unique.
- **Infrastructure units** — `"vmtime"`, `"input"`, `"vmbus"`.

### Devices that do not support save/restore

Not all devices implement save/restore. Devices signal this in one of
two ways:

1. **`SaveError::NotSupported`** — the `save()` method returns this error.
   If any state unit does this, the entire save operation fails.
2. **`supports_save_restore() -> false`** (virtio) or
   `supports_save_restore() -> None` (VMBus) — transport-level check
   that causes the transport's `save()` to return
   `SaveError::NotSupported`.

Key unsupported categories:

- **PCIe** — `GenericPcieRootComplex`, `GenericPcieSwitch` return
  `SaveError::NotSupported`.
- **NVMe** — `NvmeController` returns `SaveError::NotSupported`.
- **Pass-through PCI** — `AssignedPciDevice`, `RelayedVpciDevice`.
- **VGA / GDMA** — marked `todo!()` (will panic on save).
- **Virtio devices** — the `VirtioDevice` trait defaults
  `supports_save_restore()` to `false`. Only `virtio-blk`,
  `virtio-net`, `virtio-pmem`, and `virtio-rng` override it to `true`.
  Devices with host-side session state (`virtio-9p`, `virtiofs`,
  `virtio-console`) intentionally leave it `false`.
- **Some VMBus devices** — `GuestCrashDevice`, `GuestEmulationDevice`,
  `VmbusSerialHost`, `Vmbfs` return `None` from
  `supports_save_restore()`.

## Extending the format

When adding new fields to `SnapshotManifest`, use the next available mesh
tag number. The protobuf encoding is forward-compatible: older readers will
ignore unknown fields. However, removing or reordering existing fields is a
breaking change. See [Save State](contrib/save-state.md) for the full set of
compatibility rules.

```admonish warning
Changing the mesh tag numbers of existing fields will break compatibility
with previously saved snapshots.
```
