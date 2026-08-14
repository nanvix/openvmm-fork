# Chunked SHA-256 Snapshot Memory Integrity

Status: proposed

## Summary

OpenVMM currently verifies `memory.bin` by computing one SHA-256 digest over the
entire logical guest RAM image before partition creation. This is serial work and
was the dominant measured restore cost for a 128 MiB microVM:

| Backend | Full memory verification |
| --- | ---: |
| WHP | approximately 82 ms |
| KVM | approximately 68 ms |

A chunked SHA-256 tree would preserve cryptographic integrity while allowing
independent chunks to be verified concurrently. A four-worker, zero-copy
prototype over the measured image was approximately 2.05 times faster than a
single linear SHA-256 pass.

## Goals

- Preserve detection of modified, reordered, truncated, and substituted memory.
- Verify the exact file handle later used for the COW mapping.
- Use the existing `sha2` workspace dependency.
- Bound worker count and memory consumption.
- Continue restoring existing version 2 snapshots.
- Establish a format that can later authenticate sparse zero chunks.

## Non-goals

- This does not authenticate who created a snapshot. A signature or trusted
  transport is still required for provenance.
- This does not permit guest execution before verification succeeds.
- This does not change `state.bin` verification.

## Proposed format

Add a `SnapshotMemoryIntegrity` message to `SnapshotManifest` using the next
available mesh tag. It should contain:

- algorithm version;
- fixed chunk size;
- logical memory length;
- root SHA-256 digest;
- optional per-chunk descriptors needed by sparse or lazy verification.

Keep `memory_sha256` for version 2 compatibility. New captures should use a new
manifest version, while restore should dispatch to either the version 2 linear
verifier or the new tree verifier.

Use a fixed chunk size, initially 2 MiB. It is large enough to keep manifest and
task overhead small while providing enough independent work for typical host
CPU counts. The value belongs to the snapshot format and should not initially
be exposed as a CLI tuning option.

## Hash construction

Every hash input must be domain-separated and encode its boundaries. One
possible construction is:

```text
leaf[i] = SHA256(
    "OPENVMM_MEMORY_LEAF_V1" ||
    little_endian(i) ||
    little_endian(chunk_length) ||
    chunk_bytes
)

root = SHA256(
    "OPENVMM_MEMORY_ROOT_V1" ||
    little_endian(memory_length) ||
    little_endian(chunk_size) ||
    leaf[0] || ... || leaf[n]
)
```

Including the index and length prevents chunk reordering, duplication, and
last-chunk ambiguity. The implementation should reject zero chunk size,
overflow, a chunk count inconsistent with memory length, and digest fields of
an unexpected size before allocating worker state.

## Capture implementation

Refactor `copy_and_hash` in
`openvmm/openvmm_helpers/src/snapshot.rs` into a bounded pipeline:

1. Open and validate the exact source memory handle.
2. Read at most a small fixed number of chunks ahead.
3. Send owned chunk buffers to a bounded hash worker pool.
4. Write chunks to the staging `memory.bin` in file order.
5. Collect leaf hashes by index and calculate the root.
6. Flush `memory.bin`, write the completed manifest, and publish atomically.

The producer must not permit unbounded buffering. Four workers and no more than
two queued chunks per worker is a reasonable initial limit. Worker failure must
abort publication and remove the staging directory.

## Restore implementation

Add a versioned verifier beside `open_and_verify_file`:

1. Validate the manifest and expected logical memory length.
2. Open `memory.bin` once with the existing no-follow protections.
3. Divide that exact handle into the format-defined chunks.
4. Hash chunks concurrently with positional reads.
5. Reassemble leaf digests in index order and calculate the root.
6. Compare the root before exposing the handle to the memory mapper.
7. Rewind or duplicate the same verified handle for COW mapping.

Use platform positional-read APIs so workers do not contend on one shared file
offset. Cap concurrency to the smaller of the configured maximum, available
parallelism, and chunk count.

## Compatibility and rollout

- Version 2 snapshots continue using their existing whole-file SHA-256 field.
- New snapshots use the tree format and a new manifest version.
- Readers should produce a specific unsupported-integrity-version error.
- Writers must never populate both formats with conflicting values.
- Update `Guide/src/dev_guide/snapshot_format.md` with the exact byte-level
  construction once finalized.

## Tests

Add tests for:

- one full chunk, multiple chunks, and a short final chunk;
- modified bytes at the beginning, middle, and end;
- reordered and duplicated leaves;
- changed chunk size or logical memory length;
- truncated and extended files;
- malformed digest and excessive chunk counts;
- verification of the exact opened handle across pathname replacement;
- compatibility with existing version 2 snapshots;
- deterministic roots across Windows and Linux.

## Performance acceptance

Extend `phase2_snapshot_bench` to report linear and tree verification. At 128
MiB, the tree verifier should reduce warm-cache verification by at least 40
percent without regressing snapshot correctness. Also test 64, 256, 512, and
1024 MiB images and report CPU utilization so speedup is not obtained through
unbounded host contention.
