# Overlap Snapshot Verification with VM Creation

Status: proposed

## Summary

OpenVMM currently performs restore preparation serially:

```text
verify memory -> launch VM worker -> create partition -> attach memory ->
restore state -> resume
```

The measured full-memory SHA-256 took approximately 82 ms on WHP and 68 ms on
KVM. VM worker initialization took approximately 6 ms on WHP and 19-31 ms on
KVM. Most KVM initialization time was `KVM_CREATE_VM`, which varied between 15
and 43 ms under WSL.

Memory verification and empty partition creation do not depend on each other.
Running them concurrently changes their contribution from approximately a sum
to approximately their maximum.

## Goals

- Begin backend partition creation while `memory.bin` is being verified.
- Preserve the rule that no unverified memory is mapped or exposed to a vCPU.
- Work in both single-process and mesh worker configurations.
- Propagate either branch's failure and cancel the other branch promptly.
- Compose with linear, chunked, sparse, and broker-provided verification.

## Required refactoring

Today `prepare_snapshot_restore_for_config` verifies memory before
`VmWorkerParameters` are sent, while `InitializedVm::new` receives the memory
backing as part of construction. Split initialization into two phases.

### Phase 1: partition preparation

This phase may perform:

- hypervisor resource resolution;
- processor topology and memory-layout calculation;
- prototype partition creation;
- backend VM creation;
- capability and CPU-contract discovery;
- construction that cannot access guest RAM.

It must not attach `memory.bin`, restore device or processor state, or start a
vCPU.

### Phase 2: verified memory attachment

This phase receives the verified exact memory handle and may then:

- create the COW mapping;
- attach RAM ranges to the partition;
- finish vCPU and device construction;
- validate the destination CPU contract;
- restore state units and clock state;
- resume the VM.

A useful internal abstraction is `PreparedVm`, consumed by an
`attach_verified_memory` method to produce `InitializedVm`.

## Coordination design

Start memory verification as an independently scheduled task before launching
the VM worker. Pass the worker a mesh receiver for a result containing either:

- the exact verified `SharedMemoryFd`; or
- a structured verification error.

The worker creates `PreparedVm`, then waits at the attachment boundary. The
verification task sends the handle only after every required check succeeds.
Mesh handle transfer must duplicate the already opened generation, not reopen
`memory.bin` by pathname.

Care is needed because `launch_worker` waits for `Worker::new`. Verification
must therefore run independently before worker launch, or worker creation must
become explicitly asynchronous. A controller task waiting for `launch_worker`
cannot also be responsible for completing the verification send.

## Safety invariants

- No vCPU may run before verification succeeds.
- No unverified memory mapping may be registered with WHP or KVM.
- Hash and map the same opened file generation.
- A verification error must tear down the prepared partition.
- A partition-creation error must cancel verification and close its handles.
- Cancellation must not leak a worker, partition, file handle, or temporary
  thread.
- ttrpc restore must retain full verification unless its protocol is
  deliberately extended with an equally explicit policy.

## Error and cancellation behavior

Use a typed result across the coordination channel. Preserve the original
snapshot error context instead of converting it to worker-exited errors. If one
branch fails, signal cancellation to the other branch and await cleanup before
returning.

The verifier should check cancellation between chunks. Partition creation
cannot always be interrupted inside a host ioctl, but its result can be dropped
immediately after the call returns.

## Interaction with other improvements

With current linear SHA-256, overlap can hide approximately 5 ms on WHP and
15-43 ms on KVM. With chunked verification, both branches become similar in
size, making overlap more important.

Authenticated sparse RAM reduces verifier work. A verified snapshot broker may
complete the verification branch immediately, in which case this design adds
little overhead and still provides one attachment boundary.

## Tests

Add deterministic tests with injected barriers and failures:

- verification finishes before partition creation;
- partition creation finishes before verification;
- verification fails while partition creation is active;
- partition creation fails while verification is active;
- controller cancellation at each boundary;
- exact handle identity survives pathname replacement;
- no resume RPC is accepted before attachment and restore complete;
- single-process and mesh worker behavior match;
- repeated failures leave no worker or file-handle growth.

Use fake hypervisor and verifier implementations for ordering tests. Keep one
WHP and one KVM integration test for the actual handle-transfer path.

## Performance acceptance

Add separate timings for verification, partition preparation, attachment,
state restore, and process-to-marker latency. The combined preparation time
should be close to the slower parallel branch plus attachment overhead, rather
than the sum of both branches. No performance result is acceptable without the
failure and cancellation tests above.
