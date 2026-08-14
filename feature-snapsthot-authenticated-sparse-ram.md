# Authenticated Sparse Snapshot RAM

Status: proposed

## Summary

The measured 128 MiB Windows `memory.bin` was fully allocated even though a
shell-ready microVM leaves most guest RAM untouched or zero. OpenVMM currently
copies and hashes every logical byte during capture and reads every logical byte
again during restore verification.

Authenticated sparse RAM would represent all-zero chunks as filesystem holes
and cover that representation with the snapshot integrity tree. Restore could
then validate zero chunks without reading and hashing their full logical byte
range.

## Goals

- Preserve the guest-visible logical RAM image exactly.
- Reduce snapshot disk allocation and publication I/O.
- Make verification cost scale with populated data rather than configured RAM.
- Detect conversion between zero and data chunks.
- Work on native Windows and Linux filesystems with a safe fallback.

## Dependency

This feature should build on the chunked SHA-256 format described in
`feature-snapsthot-chunked-sha256-tree.md`. Making `memory.bin` sparse while
retaining one whole-file SHA-256 does not materially improve restore latency,
because SHA-256 still processes every logical zero byte.

## Format representation

Each integrity-tree leaf should have an authenticated kind:

- `Data`: the leaf hashes the chunk's bytes;
- `Zero`: the leaf hashes a domain-separated zero descriptor containing the
  chunk index and logical length.

For example:

```text
zero_leaf[i] = SHA256(
    "OPENVMM_MEMORY_ZERO_LEAF_V1" ||
    little_endian(i) ||
    little_endian(chunk_length)
)
```

The root commits to the ordered leaf list, so replacing data with a hole or a
hole with data changes the root. A compact bitmap can record zero/data kinds in
the manifest. Its length must be derived from the memory length and chunk size,
not trusted independently.

Initially, only completely zero integrity chunks should become holes. Partial
chunk hole punching can be considered later; it adds extent complexity for a
smaller initial benefit.

## Capture implementation

Extend the chunked capture pipeline:

1. Create `memory.bin` as a sparse-capable staging file.
2. Read one logical chunk from the exact source memory handle.
3. Check whether the complete chunk is zero.
4. For a zero chunk, advance the destination offset without writing and emit a
   `Zero` leaf.
5. For a data chunk, write its bytes and emit a `Data` leaf.
6. Set the final logical file length even when the last chunks are holes.
7. Flush and publish the file with the manifest as one snapshot generation.

The zero scan is not additional asymptotic capture work because capture already
reads every source byte. Use vectorized or word-sized zero detection where the
standard library and compiler can optimize it, but keep a simple implementation
until profiling shows this scan is significant.

## Platform support

On Windows, mark the destination sparse through the appropriate filesystem
control operation before seeking over holes. Query allocated ranges during
restore through the opened file handle.

On Linux, create holes by seeking over zero chunks and setting the final length.
Use `SEEK_DATA` and `SEEK_HOLE`, or an equivalent supported API, to validate
extent coverage.

Filesystems differ. If sparse creation or extent queries are unsupported, fall
back to writing and reading the logical bytes. Correctness must never depend on
an optimization being available.

## Restore verification

A `Zero` descriptor does not by itself prove that mapped file bytes are zero.
Before skipping reads, restore must establish through the exact opened handle
that no allocated data extent intersects the zero chunk. If the filesystem
cannot provide reliable extent information, read the chunk and verify that all
bytes are zero.

For `Data` chunks, read and hash the complete logical chunk. After all leaves are
validated, map the same handle copy-on-write. Do not recreate the file from its
pathname after verification.

## Memory mapping

No guest-memory ABI change is required. Sparse holes read as zeros through the
existing file-backed COW mapping. Guest writes create private pages and must not
allocate or modify the reusable snapshot artifact.

The memory manager should continue receiving the full logical size and existing
GPA-to-file-offset ranges. Sparse allocation is a storage property, not a guest
physical layout change.

## Failure behavior

Reject restore when:

- a zero chunk overlaps allocated data;
- a data chunk is missing or has the wrong digest;
- the zero/data bitmap has an invalid length;
- the file's logical length differs from the manifest;
- extent arithmetic overflows or exceeds `memory.bin`;
- filesystem metadata changes while the exact generation is being validated.

When extent validation is ambiguous, fall back to byte validation rather than
accepting the artifact.

## Tests

Add tests for:

- an entirely zero image;
- alternating zero and data chunks;
- a short final zero chunk;
- data inserted into an authenticated hole;
- data removed from an authenticated data chunk;
- unsupported sparse and extent APIs using the fallback path;
- exact logical length and COW behavior;
- native NTFS and Linux filesystem allocation size;
- path replacement after the memory handle is opened.

## Performance acceptance

Report logical size, allocated size, capture time, verification time, and
restore-to-marker time. Use shell-ready snapshots at 64, 128, 256, and 512 MiB.
The allocated size should track touched guest memory, and warm restore
verification should avoid reading authenticated zero chunks.
