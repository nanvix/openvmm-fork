# Verified Snapshot Broker

Status: proposed

## Summary

A one-shot OpenVMM process verifies `memory.bin` on every restore, even when a
service repeatedly launches clones from the same committed snapshot. A
long-lived broker can verify one immutable snapshot generation once and provide
duplicated COW-capable handles to subsequent VM launches.

The diagnostic no-memory-hash measurements establish the approximate repeat
launch ceiling for the tested 128 MiB image:

| Backend | Process start to guest marker |
| --- | ---: |
| WHP | approximately 33.5 ms |
| KVM | approximately 48.4 ms |

These figures still include process startup, partition construction, state
restore, and guest continuation.

## Goals

- Amortize memory verification across repeated clones.
- Preserve exact-generation and no-path-reopen guarantees.
- Prevent mutation after verification.
- Support concurrent launches from one snapshot.
- Bound retained handles and memory through explicit eviction.
- Keep authorization and tenant boundaries visible in the API.

## Non-goals

- The broker does not make an unsigned snapshot authentic.
- It does not allow arbitrary clients to claim that a path is already verified.
- It does not replace per-VM COW isolation.
- It does not improve the first import unless combined with chunked or
  overlapped verification.

## Architecture

Introduce an explicit import operation:

```text
ImportSnapshot(path) -> VerifiedSnapshotId
LaunchFromSnapshot(VerifiedSnapshotId, runtime options) -> VM
ReleaseSnapshot(VerifiedSnapshotId)
```

`VerifiedSnapshotId` must be opaque, unguessable, and scoped to the broker
instance and caller authorization context. Launch requests use the ID rather
than a filesystem path.

During import, the broker:

1. validates directory shape, manifest, state, and machine contract;
2. opens `memory.bin` through the existing no-follow path;
3. verifies its full configured integrity policy;
4. makes or proves the backing immutable;
5. retains the exact handle and decoded immutable metadata;
6. records a cache entry only after all steps succeed.

## Immutable backing

Verification remains valid only while bytes cannot change.

On Linux, the most robust portable design is to import memory into a broker-owned
`memfd` and apply write, grow, shrink, and seal seals after verification. The
one-time copy is amortized across launches. Filesystems with a suitable
immutable verified-file facility may support a zero-copy fast path after their
identity and policy are validated.

On Windows, retain a broker-owned file or section handle opened with sharing
that prevents writes, replacement, and deletion for the cache lifetime. Every
launch receives a duplicated handle with only the rights required to create a
private COW mapping.

Do not rely only on pathname, modification time, or file length. They do not
prove that bytes remained unchanged.

## Cache key and generations

Key internal entries by a generated ID plus immutable identity information:

- manifest and state digest;
- memory integrity root;
- logical memory length;
- stable file identity when applicable;
- machine and CPU contract identity.

Atomic publication of a new snapshot at the same pathname creates a new broker
generation. Existing entries continue using their retained old handles until
released or evicted.

## Launch flow

For every clone, the broker:

1. authorizes access to the verified snapshot ID;
2. increments an in-use reference count;
3. duplicates the immutable memory handle;
4. supplies cached state and contract bytes to restore preparation;
5. creates a private COW mapping in the VM;
6. decrements the reference when launch ownership transfers or fails.

The launch path still validates destination CPU and backend compatibility. Only
artifact reading and content hashing are cached.

## API placement

This is most useful in a management service that already launches multiple VMs.
Extend the ttrpc management surface or add an internal broker service rather
than creating a pathname-based global cache inside each one-shot CLI process.

The existing local CLI can continue restoring directly. A future CLI may ask a
configured broker to import and launch, but it should not silently change trust
or cache scope.

## Eviction and resource limits

Track at least:

- logical and physically retained bytes;
- open handle count;
- last use time;
- active launch references;
- importing or failed state.

Use a bounded LRU policy for entries with zero active references. Reject or
block imports when limits cannot be met. Never evict an entry while duplicating
a launch handle, and never expose a partially imported entry.

## Authorization and isolation

A verified snapshot can contain sensitive guest memory. Broker APIs must enforce
caller identity, snapshot ownership, and launch permission. Diagnostics must
not print memory content or secret-bearing state. Cache IDs should not cross
security principals unless an explicit sharing policy permits it.

Each launch must use `CopyOnWrite` mapping mode and must not receive a writable
handle to the broker's immutable backing.

## Failure handling

- Failed imports must close all handles and publish no ID.
- A failed launch must release its reference and duplicated handles.
- Broker restart invalidates IDs unless a secure persistent index is designed.
- Memory pressure and shutdown must await active references or terminate their
  VMs through an explicit policy.
- Corruption discovered during import must never become a negative cache entry
  that masks a later valid generation at the same path.

## Tests

Add tests for:

- first import verifies and subsequent launches do not rehash;
- same pathname with a new generation yields a different ID;
- path replacement cannot affect an imported generation;
- attempted writes or truncation fail while cached;
- concurrent launches receive isolated COW mappings;
- one clone's writes never appear in another clone or the broker backing;
- authorization rejection and ID guessing;
- import, launch, cancellation, eviction, and shutdown races;
- handle and memory accounting under repeated failures;
- Windows and Linux immutable-handle behavior.

## Performance acceptance

Measure first import separately from repeated launch. For repeated 128 MiB
shell restores, target process-to-marker medians close to the measured no-hash
ceilings while preserving all machine-contract, saved-state, and destination
CPU validation. Report broker memory, handle count, and concurrent launch
scaling alongside latency.
