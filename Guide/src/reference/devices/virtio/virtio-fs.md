# virtio-fs

OpenVMM can expose a host directory to a Linux guest through `virtio-fs`.

## Standard machine

For the standard machine, `--virtio-fs` creates a HostFs device with a
caller-selected tag and host path:

```bash
openvmm --virtio-fs myfs,path/to/share
```

Mount it in the guest with the same tag:

```bash
mount -t virtiofs myfs /mnt/share
```

Standard-machine virtio-fs may use the normal PCI, VPCI, or MMIO placement
rules. It does not support snapshot and restore.

## microVM ABI version 1

The microVM profile exposes at most one HostFs device. Configure it with:

```bash
openvmm --machine microvm \
  --mount /mnt/share,path/to/share,ro \
  --kernel path/to/vmlinux --initrd path/to/initramfs.cpio.gz
```

The format is `GUEST_TARGET,HOST_PATH[,ro|rw]`. The default mode is `ro`;
read-write access must be explicit. The profile fixes the remaining
guest-visible configuration:

| Property | Value |
|---|---|
| Stable ID | `fs:microvm0` |
| Tag | `microvm` |
| Transport | virtio-mmio at `0xd0001000` |
| Interrupt | IRQ 6 |
| Queues | One high-priority and one request queue |
| Features | Indirect descriptors, event index, version 1, access-platform |
| DAX window | None |
| Cache policy | Zero entry and attribute lifetimes |
| File policy | Direct I/O |
| Maximum write | 1 MiB payload plus protocol headers |

The profile adds `virtfs_dir`, `virtfs_tag`, and `virtfs_mode` bootstrap
tokens to the kernel command line. These values, the fixed transport, and the
access mode become snapshot-authoritative.

`SectionFs`, aggregate roots, alternate tags, PCI transport, DAX, and extra
queues are not part of microVM ABI version 1.

## Snapshot attachments

A microVM snapshot stores FUSE negotiation, namespace identifiers, lookup
counts, reopenable handles, directory cookies, and virtqueue progress. It
does not copy the host directory or serialize native file descriptors and
Windows handles.

An open directory continues from its bounded captured entry snapshot, so
later host additions do not appear midway through that enumeration. New
lookups and newly opened directories still observe the live host tree.

Restore therefore requires a fresh `--mount` argument. The guest target and
mode must match the snapshot. Before any vCPU starts, OpenVMM pins the supplied
root and validates its saved root and object identities. Missing, replaced,
ambiguous, or no-longer-reopenable objects fail restore. The host path may
change only when it still identifies the same saved root.

```admonish warning
An ordinary host directory is live external state. Host changes after capture
can be visible after restore or make identity validation fail. Restoring the
same read-write VM snapshot does not roll the host directory back.
Quiesce external host writers when deterministic replay is required.
```

The snapshot contract uses the `live-revalidate` policy. Immutable filesystem
generations and private writable clones are not currently exposed.

Snapshot destinations, restore directories, and explicit guest-memory backing
files must be outside the exported host tree. OpenVMM rejects configurations
that would expose guest RAM or snapshot files through virtio-fs.

## Security model

Guest FUSE requests, paths, and saved aliases are untrusted. HostFs rejects
absolute and parent-relative aliases, does not follow symbolic links or
Windows reparse points while resolving saved objects, and enforces read-only
mode before invoking a host mutation.

```admonish warning
The current cross-platform `LxVolume` interface does not provide fully
handle-relative component walking for every operation. Do not allow an
untrusted host process to concurrently replace or rename directories inside
the export; quiesce external namespace mutation during capture and restore.
```

Host filesystem behavior differs where Windows cannot represent a POSIX
operation. Unsupported operations return a Linux error rather than reporting
false success.

## Code references

- Device implementation:
  `vm/devices/virtio/virtiofs/`
- FUSE session implementation:
  `vm/devices/support/fs/fuse/`
- Resource contract:
  `vm/devices/virtio/virtio_resources/src/lib.rs`
- microVM composition and restore attachment validation:
  `openvmm/openvmm_entry/src/lib.rs`
- Snapshot manifest:
  `openvmm/openvmm_helpers/src/snapshot.rs`
- [`virtiofs` rustdoc](https://openvmm.dev/rustdoc/linux/virtiofs/index.html)
