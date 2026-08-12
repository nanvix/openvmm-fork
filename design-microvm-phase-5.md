# OpenVMM microVM migration phase 5: virtio-fs

**Status:** Proposed

**OpenVMM baseline:** `ed2a1a274d60d31572ce6c7df444a8b59721211c`

**NVX baseline:** [`2ebc48690f6cf8c22ce2e58e7f5b7e23cf327a86`](https://github.com/nanvix/nvx/tree/2ebc48690f6cf8c22ce2e58e7f5b7e23cf327a86)

**Depends on:** [Phase 1: base machine](design-microvm-phase-1.md),
[phase 2: snapshot and restore](design-microvm-phase-2.md), and the generic
device-private virtio state introduced by
[phase 3](design-microvm-phase-3.md)

## Outcome

Phase 5 adds one no-DAX virtio-fs device at MMIO `0xd0001000`, IRQ 6, with
mount tag `microvm`, on Linux/KVM and Windows/WHP. Cold boot supports live
read-only and explicitly configured read-write host directories. Snapshot
support preserves FUSE namespace and handle identity by reopening and
revalidating them against a freshly supplied host attachment. An optional
immutable-generation provider adds deterministic replay but is not required
for pinned NVX feature parity.

The guest uses ordinary RAM-backed descriptors and payloads. The profile does
not allocate a DAX window or expose Windows `SectionFs`.

## Parity contract

Both backends expose:

- virtio device ID 26;
- one high-priority queue and one request queue;
- a standard 36-byte tag containing `microvm`;
- FUSE 7.31-compatible negotiation;
- no DAX shared-memory region;
- direct-I/O file behavior and zero/near-zero cache lifetimes;
- equivalent guest-visible node, handle, path, cookie, and error semantics;
- read-only enforcement in the host device, not only guest mount flags;
- bounded quiesce and exact virtqueue/FUSE progress across restore;
- host-object reopening relative to a validated export root;
- no serialization of POSIX descriptors or Windows handles.

Host filesystem semantics cannot be identical where Windows cannot represent a
POSIX operation. The common ABI must return an explicit Linux error such as
`EOPNOTSUPP` or `ENOSYS`; it must not report false success.

## Snapshot policy

The parity baseline follows pinned NVX:

- the host directory is external live state, not snapshot content;
- capture saves guest-visible FUSE identities, aliases, handles, and policy;
- restore requires a fresh host-directory attachment;
- every saved identity is reopened and revalidated before vCPUs start;
- missing, replaced, ambiguous, or no-longer-reopenable objects fail closed;
- read-only/read-write policy comes from the snapshot, not restore CLI.

Host changes after capture can therefore change the result of restore or make
restore fail. A read-write restored VM also modifies the shared external
directory, so restoring the same VM snapshot again does not roll that
directory back. Phase 2's immutable-memory replay guarantee remains true, but
does not extend to external host filesystem contents.

An optional hardened mode may require a provider-guaranteed immutable
generation. That mode enables deterministic read-only replay; read-write
isolation additionally requires a private writable clone per restore. It is a
stronger OpenVMM capability, not the minimum phase-5 parity gate.

## Guest-visible ABI

The device is:

```text
stable ID: fs:microvm0
kind: virtio-fs
transport: virtio-mmio
base: 0xd0001000
length: 0x1000
IRQ: 6
tag: microvm
high-priority queues: 1
request queues: 1
DAX/shared memory: none
packed ring: masked
```

The profile appends tokens equivalent to:

```text
virtio_mmio.device=0x1000@0xd0001000:6
virtfs_dir=<guest-target>
virtfs_tag=microvm
virtfs_mode=<ro|rw>
```

The target path and access mode are guest-visible configuration and become
snapshot-authoritative. The host export path is a restore attachment and may
change only when all saved root, alias, and object-identity checks still pass.

## Baseline investigation

### Reusable OpenVMM implementation

`vm/devices/virtio/virtiofs` already contains:

- `VirtioFsDevice` and standard virtio-fs config space;
- a FUSE session and request decoder;
- `HostFs` backed by `LxVolume`;
- inode maps and guest-visible node IDs;
- open file-handle maps;
- relative paths and inode-based deduplication;
- read-only enforcement;
- direct I/O;
- Linux/Windows host-operation translation;
- queue worker start/stop;
- integration and Petri performance tests.

Resource and resolver plumbing exists in:

- `vm/devices/virtio/virtio_resources/src/lib.rs`;
- `vm/devices/virtio/virtiofs/src/resolver.rs`.

For `HostFs`, the resolver already passes shared-memory size zero. On Windows,
`SectionFs` instead allocates an 8-GiB shared-memory window and is therefore
incompatible with the microVM no-DAX contract.

### Device-shape differences

Current `VirtioFsDevice`:

- defaults to two request queues;
- supports up to eight request queues;
- exposes one high-priority queue plus those request queues;
- advertises packed rings;
- uses a fixed 1-ms attribute timeout and zero entry timeout.

Pinned NVX uses one request queue, no DAX, and zero cache lifetimes. The microVM
resolver must call `with_num_request_queues(..., 1)`, rely on the machine
profile to mask packed rings, and make cache timing a device/profile setting
rather than inheriting current global constants.

The existing `Aggregate` backend exposes multiple roots through one device.
It is not part of the pinned NVX ABI and should be rejected for ABI version 1.

### Missing save/restore

`VirtioFsDevice` does not override `supports_save_restore()`. Queue stop
returns only `QueueState`; it does not save the FUSE session, inode map, handle
map, or directory enumeration state.

Current in-memory state includes:

- inode volume, relative path, lookup count, host inode number, and
  guest-visible inode number;
- open `LxFile` objects tied to inodes;
- handle-map numeric IDs;
- FUSE negotiation/session state.

Native `LxFile` objects cannot cross a process boundary and must be represented
by reopenable identities.

### Quiesce gaps

Queue workers can be stopped, but there is no filesystem-wide admission gate,
in-flight request counter, bounded drain, or rollback result. FUSE interrupt
requests currently return `ENOSYS`
(`vm/devices/support/fs/fuse/src/session.rs:316`), so capture cannot rely on
FUSE cancellation to interrupt host operations.

The implementation performs host filesystem work while processing a request.
Snapshot must wait for each accepted mutation/read to reach a well-defined
response boundary or fail capture. Cancelling a worker future is not evidence
that a host mutation did not happen.

### Namespace and directory-state gaps

The current implementation tracks relative paths and uses `O_NOFOLLOW` for
opens, providing useful primitives. A complete restore still requires:

- a pinned root identity;
- proof that every relative alias resolves under that root;
- object identity validation after reopen;
- preservation of numeric FUSE node and handle IDs;
- stable directory-entry snapshots/cookies;
- a defined policy for unlinked-but-open objects;
- explicit cross-platform checks for symlink/reparse-point traversal.

Current directory reads use the live host directory handle. A stable
name/cookie snapshot, as required by pinned NVX semantics, is not represented
as serializable device state.

### Attachment-identity gap

`VirtioFsHandle` currently contains only a tag and backend/path/mount options.
There is no stable attachment ID, saved root identity, reopen policy, or
portable object-identity contract. Those are required for parity restore.
There is also no optional generation/freeze/private-clone provider for
deterministic replay.

## Required migration

### 1. microVM resource and resolver

Add a microVM-specific filesystem attachment or extend `VirtioFsHandle` with
explicit profile settings:

```text
stable attachment ID
backend = HostFs
tag = microvm
request queues = 1
shared memory size = 0
access mode = read-only | read-write
guest mount target
restore mode = live-revalidate | immutable-generation
optional host-tree provider/generation
```

The microVM resolver must reject:

- `SectionFs`;
- any DAX/shared-memory region;
- `Aggregate`;
- a tag other than `microvm`;
- more than one request queue;
- PCI transport;
- multiple filesystem devices.

Cold-boot use of an ordinary `HostFs` path remains allowed. Snapshot preflight
requires a reopenable live attachment; immutable-generation checks apply only
when that stronger mode is selected.

### 2. Cache and FUSE protocol contract

Make entry and attribute cache lifetimes configurable and set both to zero for
microVM. Preserve direct I/O and the common FUSE negotiation flags needed by the
pinned guest.

Record and validate:

- negotiated FUSE major/minor version;
- requested/capable feature flags;
- maximum read/write and request sizes;
- request-queue count;
- direct-I/O/cache policy;
- read-only/read-write mode.

Do not accept a restored negotiation state that the destination implementation
cannot reproduce exactly.

### 3. Live attachment baseline and optional generations

For the parity baseline, define a declarative live attachment:

```text
stable attachment ID
root path supplied on cold boot/restore
root filesystem and object identity
access mode
live-revalidate policy
```

Capture records the pinned root identity and all reopenable object identities
after accepted guest requests drain. Restore requires a fresh `--mount` or API
attachment, pins that root, and revalidates every saved alias and handle.

An ordinary live directory remains mutable by the host. Deterministic capture
requires the operator to quiesce external host mutation; otherwise restore may
observe later content and namespace changes or fail identity validation. This
limitation must be explicit in the CLI and snapshot manifest.

Optionally define a provider API separate from the virtio device:

```text
open_live(root, access_mode)
freeze_or_snapshot() -> immutable generation
open_generation(provider_id, generation)
clone_writable(generation) -> private generation
root_identity(generation)
validate_object(generation, identity)
```

An immutable generation prevents host namespace/content mutation for its
lifetime. A path, timestamp, inode number, or guest read-only flag alone is
not such a generation. For read-write deterministic replay, the provider must
freeze a point-in-time source and create a private writable clone for each
restore.

### 4. Device-private saved state

Use phase 3's generic virtio device-private blob. Save:

```text
schema_version
FUSE negotiated version and flags
access/cache/direct-I/O policy
next/free node and handle allocation state
inode table:
  node ID
  volume ID
  relative aliases
  lookup count
  guest inode number
  attachment object identity
open handle table:
  FUSE handle ID
  node ID
  open flags and file/directory kind
  attachment object identity
directory enumeration:
  stable entry snapshot
  cookies and current position
lock/flush state that is guest-visible
```

Transport state remains responsible for queue configuration/progress,
negotiated virtio features, and IRQ status.

Never serialize `LxFile`, POSIX FDs, Windows handles, task objects, locks, or
absolute host paths.

Bound every table, alias, path length, directory snapshot, and aggregate blob.
Phase 2's overall state bound is a second line of defense, not a substitute
for per-component limits.

### 5. Reopenable identity

Define a portable attachment object identity strong enough to distinguish
object replacement:

```text
attachment/provider object ID
object kind
optional immutable generation
relative alias set
stable metadata needed for validation
```

Restore:

1. opens and pins the freshly supplied live root or immutable generation;
2. validates every alias lexically as a relative guest path;
3. resolves component-by-component without following symlinks/reparse points;
4. reopens the object beneath the pinned root;
5. compares attachment identity and object kind;
6. reconstructs inode aliases and lookup counts;
7. reopens handles with saved access flags;
8. restores FUSE numeric IDs and directory cookies.

Fail closed if an alias escapes, an object is missing/replaced, permissions no
longer satisfy the saved access, or an open object has no supported reopen
identity.

For unlinked-but-open files, either require an attachment/provider object
handle that survives by identity or reject snapshot while such a handle
exists. Do not silently reopen a different path.

### 6. Stable directory enumeration

To preserve FUSE cookies while the host tree changes, capture a bounded
directory-entry snapshot per open directory:

```text
entry name
guest-visible inode/type
next cookie
attachment object identity where needed
```

Subsequent metadata/file operations resolve against the newly pinned live root
or immutable generation. The directory snapshot stabilizes enumeration order
and cookies; it does not authorize paths outside the pinned root.

Validate unique/monotonic cookies, entry count, name encoding, and total byte
size before rebuilding a directory handle.

### 7. Quiesce and request completion

Add a filesystem-wide admission gate and accepted-request accounting.

Capture:

1. stop accepting new host filesystem requests after vCPUs pause;
2. allow already accepted operations to finish with a bounded deadline;
3. ensure mutating host calls and their guest responses have one clear owner;
4. complete response descriptors and update used indices;
5. flush completed host operations and any provider state required by the
   selected attachment mode;
6. verify no worker owns an unrepresented request;
7. save FUSE and transport state.

Do not serialize arbitrary in-flight host-operation futures. A timed-out
mutation may already have changed the host tree, so failed capture can resume
only when the attachment/device can prove a consistent state. Otherwise
terminate without publishing.

Long-term FUSE interrupt support may improve cancellation, but does not remove
the need to know whether a host mutation committed.

### 8. Read-only and read-write enforcement

Read-only mode must reject mutations in the shared FUSE layer before invoking
host operations. Guest remount flags cannot bypass it.

Read-write mode exposes only operations with a well-defined common contract.
On Windows:

- report root-style uid/gid where POSIX ownership is unrepresentable;
- return `EOPNOTSUPP` for unsupported `chown`/exact mode changes;
- map rename/delete atomically where host APIs support the required semantics;
- reject unsupported special files, extended attributes, and locks
  explicitly.

The same guest operation may have different host metadata details, but it must
never falsely succeed on one backend.

### 9. Path-confinement audit

Treat every guest FUSE name and every saved alias as untrusted.

Required invariants:

- no absolute paths or parent components;
- reject platform-specific separators/alternate streams where relevant;
- never follow symlinks or Windows reparse points during namespace resolution;
- hold/pin parent or root handles across mutation so a checked ancestor cannot
  be replaced;
- perform namespace operations relative to the pinned root;
- revalidate after restore before vCPUs start;
- rate-limit guest-triggered path and protocol errors.

Existing `LxVolume`, relative paths, and `O_NOFOLLOW` are useful foundations,
but the complete Linux `*at`/Windows handle-relative behavior must be audited
and covered by adversarial tests.

### 10. Restore attachment

The phase-5 attachment manifest should contain:

```text
stable_id = fs:microvm0
restore_mode = live-revalidate | immutable-generation
provider_id (optional)
generation (optional)
root_identity
access_mode
guest_mount_target
reopen_policy
```

Restore-time CLI/API supplies provider credentials or a mount attachment keyed
by `fs:microvm0`. It may not change tag, queue count, DAX policy, access mode, or
guest target.

Pin and validate the supplied live root or immutable generation and every
saved object before constructing workers or starting vCPUs.

## Ordered implementation plan

1. Add the fixed phase-5 manifest entry, tag, queue count, and discovery
   tokens.
2. Add a microVM HostFs resolver that rejects DAX, SectionFs, Aggregate, PCI, and
   extra queues.
3. Make cache/FUSE negotiation policy explicit and reproducible.
4. Add stable live attachments, pinned-root identity, and reopen validation.
5. Add filesystem-wide admission, in-flight accounting, and bounded drain.
6. Add generic typed FUSE/device-private state.
7. Serialize inode aliases, lookup counts, handles, directory snapshots, and
   allocation state.
8. Implement root-relative reopen and object-identity validation.
9. Implement failed-capture rollback/termination behavior.
10. Add adversarial path-confinement and malformed-state tests.
11. Add active-I/O new-process VMM tests on KVM and WHP.
12. Optionally add immutable generation/private-clone providers for
    deterministic replay.
13. Document live external-state limits and optional hardened generations.

## Expected code areas

| Area | Primary paths |
|---|---|
| Device and queue policy | `vm/devices/virtio/virtiofs/src/virtio.rs` |
| FUSE state | `vm/devices/virtio/virtiofs/src/lib.rs`, `inode.rs`, `file.rs`, `aggregate.rs` |
| FUSE session | `vm/devices/support/fs/fuse` |
| Resource/resolver | `virtio_resources` and `virtiofs/src/resolver.rs` |
| Host attachment/provider | New live-attachment identity abstraction and optional generation provider |
| Platform host operations | `lxutil` and virtio-fs host operation code |
| microVM manifest/CLI | `openvmm_core/src/worker`, `openvmm_entry`, `openvmm_defs` |
| Integration | Petri and `vmm_tests` |

## Security and failure requirements

- Bound all FUSE messages, tables, aliases, names, directory snapshots, and
  saved payloads.
- Never panic on guest requests or malformed saved state.
- Keep all resolution beneath the freshly pinned live or generation root.
- Reject symlink/reparse races and object replacement.
- Do not reopen by unchecked absolute path.
- Do not serialize native handles.
- Do not claim that an ordinary mutable directory is captured by the VM
  snapshot or provides deterministic replay.
- Do not resume after a timed-out mutation unless consistency is proven.
- Preserve restrictive host permissions on snapshot metadata containing path
  and object identities.

## Acceptance gates

| Test | Linux/KVM | Windows/WHP |
|---|:---:|:---:|
| Device enumerates at `0xd0001000`, IRQ 6, tag `microvm` | Required | Required |
| Exactly one request queue plus high-priority queue | Required | Required |
| No DAX/shared-memory region and no packed rings | Required | Required |
| Read-only mount rejects every mutation in the host layer | Required | Required |
| Configured read-write mount performs supported operations | Required | Required |
| Host changes are visible according to zero-cache/direct-I/O policy | Required | Required |
| SectionFs, Aggregate, alternate tag, or DAX is rejected | Required | Required |
| Unchanged live directory snapshot restores open files/directories | Required | Required |
| Restore requires a fresh mount attachment | Required | Required |
| Changed/missing live object fails identity validation pre-start | Required | Required |
| Optional immutable generation provides deterministic replay | Optional hardening | Optional hardening |
| Active read/write request reaches exactly one completion | Required | Required |
| Node IDs, lookup counts, handles, and directory cookies survive | Required | Required |
| Missing/replaced object or wrong optional generation fails pre-start | Required | Required |
| Symlink/reparse escape and rename race fail closed | Required | Required |
| Unlinked-open object without attachment identity blocks capture | Required | Required |
| Same snapshot restores twice when the live attachment remains compatible | Required | Required |
| Unsupported Windows/POSIX operation returns explicit error | Required | Required |

Tests must include concurrent rename/delete attempts from the host while the
guest performs lookup/open, malformed FUSE descriptor chains, large directory
enumeration bounds, and capture with active file and directory handles.

## Completion criterion

Phase 5 is complete only when both hosts expose the same no-DAX microVM
filesystem contract and a new-process restore can revalidate and reopen every
saved guest-visible identity against a freshly supplied live attachment.
Saving only virtqueue indices is not snapshot parity. Immutable generations
remain an optional stronger replay mode.
