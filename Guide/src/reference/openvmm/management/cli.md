# CLI

```admonish danger title="Disclaimer"
The following list is not exhaustive, and may be out of date.

The most up to date reference is always the [code itself](https://openvmm.dev/rustdoc/linux/openvmm_entry/struct.Options.html),
as well as the generated CLI help (via `cargo run -- --help`).
```

* `--version`, `-V`: Print the OpenVMM build identity and exit. `-V` prints the concise identity. `--version` also prints the upstream product version, build kind, full Git revision when available, and build target. An ordinary checkout reports `MAJOR.MINOR.PATCH+g<SHORT_REVISION>`. This includes an exact checkout of an `openvmm-vMAJOR.MINOR.PATCH` release tag. A checkout detected with tracked changes appends `.dirty`; staged changes refresh this reliably, while an unstaged-only transition may remain cached until another build-script input changes. A Git-free source tree reports `MAJOR.MINOR.PATCH`. On Windows, the executable's `VERSIONINFO` uses the product version as `MAJOR.MINOR.PATCH.0`.
* `--processors <COUNT>`: The number of processors. Defaults to 1.
* `--machine <PROFILE>`: Select the guest-visible machine contract. The
  default is `standard`. `microvm` selects the x86-64 Xen PVH microVM, which
  runs on KVM, MSHV, or WHP with exactly 1, 2, 4, or 8 vCPUs. On
  Linux, auto-detection prefers MSHV when `/dev/mshv` is available and falls
  back to KVM:

  ```bash
  openvmm --machine microvm --hypervisor kvm \
    --kernel vmlinux --initrd initramfs.cpio.gz
  openvmm --machine microvm --hypervisor mshv \
    --kernel vmlinux --initrd initramfs.cpio.gz
  openvmm --machine microvm --hypervisor whp \
    --kernel vmlinux --initrd initramfs.cpio.gz
  openvmm --machine microvm --processors 8 --hypervisor whp \
    --kernel vmlinux --initrd initramfs.cpio.gz
  ```

  The kernel must be an uncompressed ELF64 image containing
  `XEN_ELFNOTE_PHYS32_ENTRY`. The profile owns the base command line
  (`earlycon=xe9 console=hvc0 reboot=t panic=-1`) and switches the primary
  console to `hvc1` when `--virtio-console` is present. It reserves a 1-GiB
  MMIO gap from 3 to 4 GiB and exposes only PIC/IOAPIC, PIT, a CMOS RTC
  anchored to UTC,
  the microVM portb console, lifecycle ports, and the optional fixed virtio
  devices described below. User arguments cannot override `earlycon=`,
  `console=`, `virtio_mmio.device=`, `virtnet_*=`, or `virtfs_*=`.

  One virtio-fs slot is exposed at MMIO `0xd0001000`, IRQ 6 and remains
  dormant when `--mount` is omitted; an optional `--mount` binds HostFs to it;
  one optional `--virtio-console <BACKEND>` is exposed at MMIO `0xd0002000`,
  IRQ 7; and `--microvm-sandbox-block` exposes fixed distro, runtime, custom,
  and scratch slots starting at MMIO `0xd0003000`. Ordinary `--virtio-blk`
  is rejected. All use split rings. Firmware, ACPI, SMBIOS, PCI,
  VMBus, UARTs, graphics, isolation, nested virtualization, and other devices
  are rejected. Host-driven save/restore, pulse-save/restore, and worker
  restart remain unavailable.

  `microvm` uses one socket and one die,
  with one core per vCPU, no SMT, xAPIC mode, and contiguous APIC IDs from 0.
  Guest-requested snapshot capture and new-process restore are available for
  blockless and fixed-block machines on Linux/KVM, Linux/MSHV, and Windows/WHP.

  ```admonish warning title="microVM migration"
  The canonical `microvm` spelling now selects the contract formerly exposed
  as `microvm-v2`; the `microvm-v2` selector and the former ABI-v1 behavior are
  removed. Snapshot ABI and PVH layout fields remain numeric value 2. ABI or
  layout value 1 snapshots are rejected and must be run with OpenVMM commit
  `1b70365613517a10718e00284a62bdffbd80e41c` or an earlier compatible build.
  ```
* `--net <IPv4/PREFIX>`: With `--machine microvm`, attach one virtio-net NIC
  at MMIO `0xd0000000`. KVM and MSHV use IRQ 10; WHP uses IRQ 5. Prefixes
  `/1` through `/30` are accepted. The first usable subnet address becomes
  the gateway; network, broadcast, and gateway addresses cannot be assigned
  to the guest. Guest and gateway MAC addresses are derived as
  `52:54:00:<second>:<third>:<fourth>` from their IPv4 addresses. Networking
  requires the only supported capability profile, `--network-profile portable`;
  omitting it rejects the command before OpenVMM opens host resources.

  ```bash
  openvmm --machine microvm --hypervisor whp \
    --kernel vmlinux --initrd initramfs.cpio.gz \
    --net 10.0.0.2/24 --network-profile portable
  ```

  `portable` uses an in-process Consomme endpoint on Linux/KVM, Linux/MSHV,
  and Windows/WHP. It needs no TAP, root access, driver, or host network
  configuration. `--net-tap` is incompatible and is rejected before any
  endpoint or host resource is created. The gateway provides DNS over UDP and
  TCP, ICMP echo, and outbound TCP/UDP through ordinary host sockets. Consomme
  rejects IPv4 fragments deterministically; policy filtering remains before
  host socket creation. Its per-connection TCP buffers start at 16 KiB and
  are bounded at 4 MiB; UDP bindings expire after five minutes; and at most
  256 DNS requests are pending at once. At most 128 TCP, 256 UDP, and 16 ICMP
  guest flows are active at once; excess flows are deterministically rejected
  before a host socket is created.

  `--allow-host <IPv4[/PREFIX]>`, `--block-host <IPv4[/PREFIX]>`, and
  `--allow-endpoint <IPv4:TCP-PORT>` are repeatable, mutually exclusive
  egress modes. Filtering runs before host socket creation. Active policy
  fails closed for malformed packets, non-IPv4 traffic, and IPv4 options.
  Exact endpoint mode also rejects UDP, ICMP, VLAN, fragments, and every TCP
  destination not listed. Endpoint addresses must be usable unicast identities;
  unspecified, current-network, loopback, link-local, multicast, reserved,
  guest-self, subnet-network, and subnet-broadcast addresses are rejected before
  host resources are opened. For each endpoint, ARP may resolve the endpoint
  itself when it is on-link, or the gateway otherwise. Duplicate endpoint
  addresses share one canonical next hop. This layer-2 permission does not relax
  the independent destination, TCP, or port check. No implicit DNS exception is
  added.

  Networked snapshots record the `portable` profile, drain accepted TX and
  endpoint-ready RX at the capture boundary, rewind unused guest RX
  descriptors, and recreate a fresh Consomme endpoint generation on restore.
  Restore of a networked snapshot requires `--network-profile portable` and
  the same active egress policy rules. The policy digest binds the saved static
  identity and its derived ARP next hops, which are reconstructed before vCPUs
  start. Native sockets and NAT flow tables are not serialized. The capture
  protocol does not retain pre-capture endpoint completions; restored guest
  software must establish new host-side flows.
* `--mount <GUEST_TARGET,HOST_PATH[,ro|rw]>`: With `--machine microvm`, attach
  one no-DAX HostFs device at MMIO `0xd0001000`, IRQ 6, with tag `microvm`.
  The default mode is read-only; `rw` must be explicit. The guest target must
  be an absolute, non-root Linux path without dot, parent, empty, whitespace,
  backslash, or `=` components.

  ```bash
  openvmm --machine microvm --hypervisor kvm \
    --kernel path/to/vmlinux --initrd path/to/initramfs.cpio.gz \
    --mount /mnt/share,path/to/share,ro
  ```

  The device has one high-priority queue, one request queue, direct-I/O file
  behavior, zero entry and attribute cache lifetimes, and no shared-memory
  window. `--mount` conflicts with `--virtio-fs` and
  `--virtio-fs-shmem`; those standard-machine options cannot select the
  microVM filesystem profile.

  Filesystem snapshots contain guest-visible FUSE and queue state, not host
  directory contents or native handles. An active snapshot requires
  `--mount` again with the exact canonical host path, guest target, and access
  mode; the live root and every saved object identity are also revalidated
  before vCPUs start. A snapshot captured without `--mount` may remain dormant
  or bind a new attachment. The resumed guest must then explicitly run
  `mount -t virtiofs microvm <GUEST_TARGET>` because its cold-boot mount hook
  has already completed.
  See [virtio-fs](../../devices/virtio/virtio-fs.md).
* `--snapshot-destination <DIR>`: Publish a microVM snapshot when the guest
  writes to PMIO port `0x605`. The destination must not exist and its parent
  must already be a directory. OpenVMM automatically creates file-backed RAM
  in that parent when no memory backing file was supplied, quiesces the VM,
  writes and flushes a sibling staging directory, and atomically renames it to
  `DIR`. After a successful commit, the source VM terminates without executing
  the instruction after the snapshot `out`.

  `--snapshot-quiesce-timeout-ms <MILLISECONDS>` sets the bounded quiesce
  timeout and defaults to 5000. A request with no configured destination is
  ignored and the guest continues. Capture requires 1, 2, 4, or 8 vCPUs,
  KVM, MSHV, or WHP, and shared file-backed RAM. Sandbox block media must
  be cached regular raw files with nonzero 512-byte-aligned geometry. An
  attached virtio console saves accepted but undelivered input and the offset
  of a partially forwarded guest transmit descriptor. An attached microVM
  virtio-net device saves its static identity, queue progress, drained packet
  ownership, endpoint generation, and policy requirement.
  The fixed microVM virtio-fs slot saves either an explicit dormant state or,
  when attached, its negotiated FUSE policy, namespace and handle identifiers,
  aliases, and directory cookies. The host tree remains external live state.

  `--memory-capacity <SIZE>` opts the snapshot into restore-time memory
  expansion. `SIZE` is an immutable 128-MiB-aligned upper bound, must be at
  least the base `--memory` size, and reserves the complete canonical GPA
  aperture without adding it to the initial PVH usable-RAM map or
  `memory.bin`.

  ```bash
  openvmm --machine microvm --hypervisor kvm --memory 128M \
    --kernel vmlinux --initrd initramfs.cpio.gz \
    --snapshot-destination snapshot
  ```

  `/sbin/nvx-snapshot` requests a paired capture by default. When scratch is
  mounted, it freezes the workload cgroup with a bounded wait, syncs, freezes
  the scratch filesystem, and asks OpenVMM to drain queues and atomically
  publish `scratch.img`. `/sbin/nvx-snapshot --fresh-scratch` is for a
  pre-mount boundary and records that restore must supply a fresh scratch.
* `--restore-snapshot <DIR>`: Restore a microVM from a committed snapshot.
  The manifest supplies the authoritative RAM size, topology, ABI,
  fixed device inventory, effective kernel command line, source backend, CPU
  contract, and TSC frequency. Kernel, initrd, command-line, ordinary
  `--memory`, processor, device, and topology overrides are not accepted;
  expansion-capable snapshots use only `--restore-memory`. Repeat the
  snapshot's exact `--processors` count; a mismatch is rejected before any VP
  starts. Restore requires the same backend kind as capture.

  When the snapshot contains a virtio console, its attachment policy comes
  from the manifest. OpenVMM recreates listeners, reconnects required clients,
  or requires an inherited replacement before creating the partition. Restore
  fails before any vCPU starts when a required attachment cannot be rebuilt.
  A listener peer may connect after restore; guest transmit descriptors remain
  pending while no peer is connected.

  When the snapshot contains an active virtio-fs attachment, restore requires
  a fresh `--mount`. The argument must reproduce the manifest's exact
  canonical host path, guest target, and `ro`/`rw` mode while also supplying a
  live root with the same saved identity. A snapshot advertising the dormant
  slot may instead accept a new attachment; snapshots without that capability
  reject additive attachment.

  When the snapshot contains virtio-net, restore also requires
  `--network-profile portable`; the snapshot's profile and canonical egress
  policy must match the supplied portable configuration.

  For sandbox-block snapshots, restore repeats each read-only
  `--microvm-sandbox-block` argument. Its role, access, geometry, and SHA-256
  must match the manifest. A paired snapshot supplies scratch internally from
  a verified process-private copy of `scratch.img`; passing another scratch is
  rejected. A fresh-scratch snapshot instead requires a writable scratch
  argument with matching geometry.

  `--restore-memory <SIZE>` selects the total RAM for this launch. It requires
  an expansion-capable snapshot and a 128-MiB-aligned value from the exact
  captured base through the immutable capacity. Base RAM remains a private
  copy-on-write mapping of `memory.bin`; selected expansion ranges use fresh
  zeroed private backing. Expansion implies the post-restore repair gate.

  ```bash
  openvmm --machine microvm --hypervisor kvm \
    --restore-snapshot snapshot --restore-entropy
  ```
* `--restore-ready-path <PATH>`: Connect to an existing Unix domain socket on
  Linux or a `//./pipe/...` named pipe on Windows and write exactly
  `OPENVMM_RESTORE_READY_V1\n` once all restored state, required attachments,
  and execution-owned workers are ready. Ungated restores flush the event
  before releasing the restored vCPU. Gated microVM restores flush it after the
  guest acknowledges post-restore repair and external input is re-enabled,
  while the restored vCPU remains stopped.
  It is valid only with `--restore-snapshot` and is process-local; it is not
  saved in the snapshot. A connection, write, or flush failure aborts startup
  and stops the VM. The peer must accept and read the event while startup is
  in progress; Windows flush completion waits for the named-pipe peer to
  consume the complete frame.
* `--restore-entropy`: Make a fresh `OPENVMM_ENTROPY_V1` packet available on
  the private portb restore channel. The guest must consume the packet and
  explicitly reseed its RNG. Restoring cloned RNG state without this option is
  unsafe for cryptographic workloads and emits a warning.
  Processor activation uses `OPENVMM_ENTROPY_V2`. Memory expansion uses the
  backward-compatible `OPENVMM_ENTROPY_V3` packet. Its exact format is the
  19-byte `OPENVMM_ENTROPY_V3\0` header, a one-byte online-VP target (zero
  means none), a one-byte expansion-range count, that many little-endian
  `(u64 GPA start, u64 byte length)` pairs, and 64 bytes of fresh entropy.
  Explicitly selecting the snapshot base size with `--restore-memory` still
  emits V3 with an expansion-range count of zero; omitting the option preserves
  V1/V2 behavior. Private portb status bit 3 reports a V3 memory target, while
  bit 4 additionally reports that the packet contains one or more expansion
  ranges, allowing a zero-range target to avoid post-restore repair.
* `--restore-processors <COUNT>`: For an opt-in microVM snapshot, bring the
  contiguous VP prefix `0..COUNT-1` online before restore readiness. The
  snapshot's manifest VP count remains immutable capacity and must still match
  `--processors`. The target must be 1, 2, 4, or 8 and satisfy
  `boot-online <= target <= capacity`. This option implies a version-2 private
  restore packet and the post-restore gate. Snapshots without activation
  metadata reject it. An
  explicit MSHV restore instantiates and binds only the requested prefix while
  validating the full saved VP inventory; that reduced-prefix process cannot
  be saved again. MSHV restores without this option, and KVM and WHP restores,
  instantiate the full VP capacity.
* `--restore-gate-timeout-ms <MILLISECONDS>`: Bound microVM guest repair and
  gate acknowledgement after restore. The default is 60000 milliseconds.
* `--snapshot-tier <TIER>`: Required for snapshot capture with sandbox blocks. Choose
  `platform`, `workload-start`, or `instance-checkpoint`. The first two are
  reusable clone policies; instance checkpoints use single-use resume policy.

A committed snapshot contains `manifest.bin`, `state.bin`, `memory.bin`, and
optionally the manifest-declared `scratch.img`. Restore rejects unknown files,
symlinks, malformed or oversized data, length or scratch-digest mismatches, and
incompatible machine contracts before starting a vCPU. `memory.bin` uses a
private writable copy-on-write mapping and paired scratch is privately copied,
so clone-policy snapshots can be restored repeatedly without modifying
artifacts. Instance-checkpoint snapshots permit one restore attempt.

Versions 3 through 5 do not embed or validate checksums for `state.bin` or `memory.bin`;
legacy version 2 checksum fields are accepted without re-hashing their
payloads. This format does not detect same-length payload changes,
authenticate, or encrypt a snapshot. Treat all three artifacts as sensitive
guest state and protect the directory with host access controls.
* `--memory <SPEC>`: Configure guest RAM. Defaults to `size=1G`.
  `SPEC` can be a size-only shorthand, such as `--memory 4G`, or a
  comma-separated key/value list:

  ```bash
  --memory size=4G,shared=on,prefetch=off
  ```

  The keys below select the guest RAM **memory backing**. For an explanation
  of shared vs. private memory, prefetch, huge pages, and file-backed RAM —
  and how to choose between them — see
  [Memory Backing](../../architecture/openvmm/memory-backing.md).

  Supported keys:
  * `size=<SIZE>` - guest RAM size. Sizes accept `K`, `M`, `G`, and
    `T` suffixes, optionally followed by `B`.
  * `shared[=on|off]` - use shared file-backed guest RAM. The default is
    `on`; `off` uses private anonymous memory.
  * `prefetch[=on|off]` - pre-populate guest RAM mappings up front.
    Only has an effect under WHP; a no-op on KVM/mshv.
  * `thp[=on|off]` - mark guest RAM (shared or private) as Transparent Huge
    Page eligible. Linux-only, best-effort, and on by default; pass `thp=off` to
    opt out.
  * `hugepages[=on|off]` - allocate guest RAM from explicit large/huge pages
    (Linux hugetlb pages or a Windows `SEC_LARGE_PAGES` section). Requires
    shared memory.
  * `hugepage_size=<SIZE>` - request a specific large-page size, such
    as `2MB` or `1GB`. Requires `hugepages=on`; defaults to 2 MB. On
    Windows only 2 MB is supported.
  * `file=<PATH>` - use an existing file as the guest RAM backing file.
    This is used by snapshots.

  Examples:

  ```bash
  --memory 4G
  --memory size=64GB,hugepages=on,hugepage_size=2MB
  --memory size=4G,file=path/to/memory.bin
  --memory size=4G,thp=off
  ```
* `--hv`: Exposes Hyper-V enlightenments. VMBus is enabled by default
  when `--hv` is active; pass `--no-vmbus` to suppress VMBus while keeping
  enlightenments.
* `--no-hv`: Boots AArch64 UEFI without exposing Hyper-V enlightenments.
  By default, UEFI exposes the enlightenments. This option requires
  `--no-vmbus`, is not supported for x86_64 UEFI, and conflicts with `--hv`,
  `--vtl2`, `--get`, and `--pcat`.
* `--no-vmbus`: Disables the VMBus server and all VMBus devices, even when
  `--hv` or `--uefi` is active. The guest boots using only standard PCIe
  devices and virtio transports. Incompatible with `--disk`, `--pcat`,
  `--vtl2`, and VMBus serial options.
* `--hypervisor <SPEC>`: Select a specific hypervisor backend, optionally with
  backend-specific parameters. The format is `name` or `name:key=val,key,...`.
  Available backends: `whp` (Windows), `kvm` (Linux), `mshv` (Linux,
  `x86_64` guests only), `hvf` (macOS). When omitted, OpenVMM
  auto-detects the best available backend.

  WHP accepts the following parameters (x86_64 guests only):
  * `user_mode_apic` — use the user-mode APIC emulator instead of WHP's
    in-hypervisor APIC
  * `no_enlightenments` — disable in-hypervisor Hyper-V enlightenment support

  Examples:
  ```bash
  --hypervisor whp
  --hypervisor whp:user_mode_apic
  --hypervisor whp:user_mode_apic,no_enlightenments
  --hypervisor kvm
  ```
* `--nested-virt`: Expose hardware virtualization (VMX/SVM) to the guest so it
  can run its own hypervisor (Hyper-V, KVM, etc.). Only supported on `x86_64`,
  and only by backends that support nested virtualization (currently WHP and
  KVM); requesting it with a backend that does not support it fails early. The
  host must expose virtualization extensions to the VM running OpenVMM. When
  enabled, a guest may detect nested virtualization and turn on features such
  as Virtual Secure Mode (VSM), which can hurt performance and interfere with
  VMBus devices; nested virt cannot currently be combined with `--hv`/VMBus or
  `--hypervisor whp:user_mode_apic`.
* `--uefi`: Boot using `mu_msvm` UEFI
* `--uefi-firmware <FILE>`: Path to the UEFI firmware file (`MSVM.fd`). When `--uefi` is specified, this option is required only if you do not set the environment variable `OPENVMM_UEFI_FIRMWARE` (or the architecture-specific variants `X86_64_OPENVMM_UEFI_FIRMWARE`, or `AARCH64_OPENVMM_UEFI_FIRMWARE`). If omitted, the default is read from `OPENVMM_UEFI_FIRMWARE` first, then falls back to the architecture-specific variables.
* `--pcat`: Boot using the Microsoft Hyper-V PCAT BIOS
* `--vmbus-scsi id=<name>[,sub_channels=<N>][,vtl2]`: Creates a
  named VMBus SCSI controller. Use with `--disk ...,on=<name>` to
  attach disks.
* `--disk file:<DISK>,on=<name>`: Attaches a disk to the named
  controller. The `DISK` argument can be:
  * A flat binary disk image
  * A VHD file with an extension of .vhd (Windows host only)
  * A VHDX file with an extension of .vhdx

  On Linux, raw files and block devices use the `disk_blockdevice` backend
  (io_uring-based async I/O) by default. Append `;direct` to the path to
  bypass the OS page cache, e.g. `--disk file:/dev/sdb;direct,on=scsi0`.
* `--numa <PARAMS>`: Configure a guest NUMA node (repeatable, one per
  node). Mutually exclusive with `--memory`. Each `--numa` specifies one
  guest NUMA node with its own memory backing and optional VP assignment.

  Supported keys (in addition to all `--memory` keys except `file`):
  * `host_numa_node=<N>` - bind memory allocation to host NUMA node N
  * `vps=<LIST>` - explicit VP indices for this node. Uses bracket syntax
    with comma-separated indices and dash ranges: `vps=[0,1]`,
    `vps=[0-3]`, `vps=[0,1,4-5]`. When omitted, VPs are assigned by
    round-robin sockets across nodes. An empty list, `vps=[]`, declares a
    CPU-less node (e.g. a generic-initiator target); unlike a non-empty
    list, it may be combined with nodes that omit `vps`.

  Examples:

  ```bash
  --numa size=2G --numa size=2G
  --numa size=2G,host_numa_node=0 --numa size=2G,host_numa_node=1
  --numa size=2G,hugepages=on,vps=[0,1] --numa size=2G,vps=[2,3]
  --numa size=2G,vps=[0-3] --numa size=2G,vps=[4-7]
  ```

  See [NUMA Topology](../../architecture/openvmm/numa.md) for details.

* `--numa-distance <SRC:DST:DIST>`: Specify inter-node NUMA distance
  (repeatable). `SRC` and `DST` are 0-based node indices, `DIST` is
  10–255 (10 = local, 255 = unreachable). Each direction must be specified
  explicitly.

  ```bash
  --numa-distance 0:1:30 --numa-distance 1:0:30
  ```

* `--private-memory`, `--prefetch`, `--thp`, and
  `--memory-backing-file <PATH>`: Deprecated aliases for `--memory`
  parameters. Prefer `shared=off`, `prefetch=on`, `thp=on`, and
  `file=<PATH>`.
* `--pidfile <PATH>`: Write the process ID to the specified file on startup,
  and remove it on clean exit. If the process is killed with `SIGKILL` or
  crashes, the pidfile is not removed — consumers should verify the PID is
  still alive. No file locking is performed; concurrent launches with the same
  pidfile path will overwrite each other. Not written for short-lived utility
  modes such as `--write-saved-state-proto`.
* `--nic`: Exposes a NIC using the Consomme user-mode NAT.
* `--gfx`: Enable a graphical console over VNC (see below)
* `--vnc-port <PORT>`: VNC server port (default: 5900)
* `--vnc-listen <ADDRESS>`: VNC server bind address (default: `127.0.0.1`).
  Use `0.0.0.0` for all IPv4 interfaces, or `::` for dual-stack IPv4+IPv6.
* `--vnc-max-clients <COUNT>`: Maximum concurrent VNC clients (default: 16).
  Each client uses ~8MB for framebuffer buffers.
* `--vnc-evict-oldest`: When the client limit is reached, disconnect the oldest
  client instead of rejecting the new connection. Useful for admin takeover.
* `--virtio-9p`: Expose a virtio 9p file system. Uses the format `tag,root_path`, e.g. `myfs,C:\\`.
  The file system can be mounted in a Linux guest using `mount -t 9p  -o trans=virtio tag /mnt/point`.
  You can specify this argument multiple times to create multiple file systems.
* `--virtio-fs`: Expose a virtio-fs file system. The format is the same as `--virtio-9p`. The
  file system can be mounted in a Linux guest using `mount -t virtiofs tag /mnt/point`.
  You can specify this argument multiple times to create multiple file systems.
* `--virtio-rng`: Add a virtio entropy (RNG) device, exposing `/dev/hwrng` in the Linux guest.
  The guest kernel must have `CONFIG_HW_RANDOM_VIRTIO` enabled.
* `--virtio-rng-bus <BUS>`: Select the bus for the virtio-rng device (`auto`, `mmio`, `pci`, `vpci`).
  Defaults to `auto`.
* `--virtio-vsock-path <PATH>`: Add a virtio-vsock device using OpenVMM's
  hybrid Unix-socket relay.
* `--virtio-vsock-bus <mmio|pci>`: Select the bus for a virtio-vsock device
  created by `--virtio-vsock-path` or `--virtio-vsock-vhost-cid`. When omitted,
  OpenVMM selects the bus automatically.
* `--virtio-vsock-vhost-cid <CID>`: Add a virtio-vsock device backed by the
  Linux kernel's `vhost_vsock` implementation. This makes the guest reachable
  from host applications through `AF_VSOCK` at `CID`, which must be between 3
  and 4294967294 (CIDs 0-2 are reserved for the hypervisor, loopback, and host,
  respectively, and u32::MAX is the ANY wildcard). This option requires
  `/dev/vhost-vsock`, the `vhost_vsock` kernel module, and shared file-backed
  guest RAM (the default memory backing). It uses identity-mapped DMA and
  does not support a non-identity virtual IOMMU. It conflicts with
  `--virtio-vsock-path`.
* `--vhost-user <SOCKET_PATH>,type=<TYPE>[,tag=<NAME>][,num_queues=<N>][,queue_size=<N>][,pcie_port=<PORT>]`: Attach a
  vhost-user device backed by an external process over a Unix socket (Linux
  only). The backend process must already be listening on `SOCKET_PATH`.
  Supported `type` values: `blk`, `fs`. For `type=fs`, `tag=<NAME>` is required
  and specifies the mount tag exposed to the guest (max 36 bytes).
  `num_queues` and `queue_size` control the queue layout (defaults: blk
  num_queues=1/queue_size=128, fs num_queues=1/queue_size=1024).
  Alternatively, use `device_id=<N>` instead of `type=` to specify the numeric
  virtio device ID directly, with `queue_sizes=[N,N,N]` for per-queue sizes.
  Examples:
  ```sh
  --vhost-user /tmp/vhost-blk.sock,type=blk
  --vhost-user /tmp/vhost-blk.sock,type=blk,num_queues=4,queue_size=512
  --vhost-user /tmp/vhost-blk.sock,type=blk,pcie_port=rp0
  --vhost-user /tmp/virtiofsd.sock,type=fs,tag=myfs
  --vhost-user /tmp/virtiofsd.sock,type=fs,tag=myfs,num_queues=2,queue_size=1024
  --vhost-user /tmp/vhost.sock,device_id=26,queue_sizes=[256,256]
  ```

Serial devices can be configured to appear as different devices inside the guest:

* `--com1/com2 <BACKEND>`: Configure a COM port serial device.
* `--com1 debugger-mode:<BACKEND>`: Prefix any COM port binding with
  `debugger-mode:` to run that port in debugger mode for WinDbg kernel
  debugging over serial (KD), e.g. `--com1 debugger-mode:listen=<PATH>` or
  `--com1 debugger-mode:listen=tcp:<IP>:<PORT>`. In this mode OpenVMM keeps that
  port's backend drained and may drop bytes instead of applying backpressure, so
  the KD transport does not deadlock across guest resets or reboots; KD recovers
  dropped bytes with its own retransmission. Debugger mode is chosen
  independently per COM port, so one port can talk to WinDbg while another
  behaves normally.
* `--virtio-console <BACKEND>`: Expose a virtio console device. It normally
  appears as `/dev/hvc0`. Under `--machine microvm`, it occupies fixed MMIO
  `0xd0002000`, IRQ 7, and is selected as `/dev/hvc1`; the raw portb path
  remains available as `hvc0` for early output and recovery.

  A microVM accepts these explicit attachment policies:

  * `listen=PATH` or `listen=tcp:IP:PORT`: save the canonical endpoint and
    recreate the optional listener on restore. Unix sockets must be beside the
    snapshot directory. Windows pipes use the
    `//./pipe/openvmm-microvm-<NAME>` namespace. TCP ports must be nonzero.
    TCP addresses must be loopback addresses.
  * `connect=PATH` or `connect=tcp:IP:PORT`: require a client connection before
    vCPUs start. Cold boot and restore use a five-second timeout. The
    restore command must explicitly resupply the matching client attachment.
  * `console`: require the restore caller to supply the same inherited terminal
    attachment. Portb recovery output moves to stderr while the terminal is
    attached to `hvc1`.
  * `none`: keep the device present and explicitly discard guest TX while
    disconnected.

  A generic byte stream guarantees no replay up to OpenVMM's backend write
  boundary; it cannot prove that the remote application consumed bytes without
  its own acknowledgment protocol.

The `BACKEND` argument is the same for all serial devices:

  * `none`: Serial output is dropped.
  * `console`: Serial input is read and output is written to the console.
  * `stderr`: Serial output is written to stderr.
  * `listen=PATH`: A named pipe (on Windows) or Unix socket (on Linux) is set
      up to listen on the given path. Serial input and output is relayed to this
      pipe/socket.
  * `listen=tcp:IP:PORT`: As with `listen=PATH`, but listen for TCP
      connections on the given IP address and port. A microVM requires a
      loopback IP such as `127.0.0.1` or `::1`.
  * `connect=PATH`: Connect to an existing named pipe or Unix socket.
  * `connect=tcp:IP:PORT`: Connect to an existing TCP listener.

## Guest power events

By default OpenVMM keeps running when the guest powers itself off, hibernates,
or triple-faults: the virtual processors stop, but the VMM process stays up so
you can inspect the VM or restart it from the
[interactive console](./interactive_console.md). A guest-requested reset reboots
the VM in place, as does a guest watchdog timeout when `--guest-watchdog` is
enabled.

Four flags override what happens on each guest power event, so a supervising
process can treat the OpenVMM process lifetime as the VM lifetime. Each takes a
`reset` (reboot in place), `halt` (stop the processors but keep the VMM process,
as above), or `exit` (exit the VMM process) action. The `exit` action may carry a
status code as `exit:<code>` (0-255); a bare `exit` uses 0:

* `--guest-reset-action <reset|halt|exit[:<code>]>` (default `reset`): the guest requested
  a reset.
* `--guest-shutdown-action <reset|halt|exit[:<code>]>` (default `halt`): the guest powered
  off or hibernated.
* `--guest-crash-action <reset|halt|exit[:<code>]>` (default `halt`): the guest
  triple-faulted. The fault registers are written to the trace log.
* `--guest-watchdog-action <reset|halt|exit[:<code>]>` (default `reset`): the guest
  watchdog timer expired without being petted (requires `--guest-watchdog`).

A bare `exit` exits with status 0; `exit:<code>` exits with that code instead, so
a supervisor can tell the exit reasons apart.

* `--crash-dump-path <PATH>`: when the guest triple-faults, write a
  WinDbg-compatible `.vmrs` dump of the VM's processor state and guest memory to
  `PATH` before the `--guest-crash-action` is applied (see
  [VM Memory Dumps](../../../user_guide/openvmm/vm_memory_dumps.md)). This is a
  host-side, whole-VM dump, distinct from `--openhcl-dump-path` (OpenHCL's
  in-guest crash dump device driven by the guest OS).

`--disable-frontpage`: when booting UEFI, power the VM off instead of showing the
firmware frontpage (the menu shown when there is no bootable device). Combined
with `--guest-shutdown-action exit`, a guest with no boot device exits the VMM.
Requires `--uefi`.

## PCIe Device Support

OpenVMM can emulate a PCI Express topology using `--pcie-root-complex` and
`--pcie-root-port`. Devices that support the `pcie_port=` option can be
attached to a root port to appear as PCIe devices in the guest.

### Setting up a PCIe topology

```sh
# Create a root complex and root port
--pcie-root-complex rc0 --pcie-root-port rc0:rp0
```

`--pcie-root-complex` accepts optional comma-separated options after the root
complex name:

```sh
--pcie-root-complex rc0,segment=0,start_bus=0,end_bus=255
```

- `segment=<N>`: PCIe segment number for the root complex.
- `start_bus=<N>` and `end_bus=<N>`: inclusive bus range assigned to that
  root complex.
- `low_mmio=<SIZE>` and `high_mmio=<SIZE>`: low/high MMIO window sizes.
- `low_mmio_base=<ADDR>` and `high_mmio_base=<ADDR>`: pin the low/high
  MMIO window to a fixed base address instead of letting the VM topology
  allocate it dynamically. Used with `preserve_bars` for P2P DMA.
- `preserve_bars`: treat non-zero BAR values found during PCI probing as
  pinned addresses (GPA = HPA). Required for peer-to-peer DMA between
  VFIO passthrough devices without ATS.
- `hdm=<SIZE>`: CXL HDM decoder MMIO window size (CFMWS window). Default
  is `1G`.
- `hdm_window_restrictions=<MASK>`: CFMWS window restrictions bitmask
  (`u16`, decimal or `0x`-prefixed hex). Default is `0x1`
  (`DEVICE_COHERENT`, bit 0 set).
  Defined bits:
  0: device coherent
  1: host-only coherent
  2: volatile
  3: persistent
  4: fixed device configuration
  5: BI
  Bits 15:6 are reserved and rejected.
- `node=<N>`: NUMA node affinity for this root complex. The guest sees
  this via the ACPI `_PXM` object. When omitted, no `_PXM` is emitted
  and the guest uses its default allocation policy.

### Root port and switch options

`--pcie-root-port` accepts optional comma-separated options after the port
name:

```sh
--pcie-root-port rc0:rp0,hotplug,acs=0x005f,cxl
```

- `addr=<dev>[.<fn>]`: places the root port at a fixed device/function on
  its bus. `dev` is 0-31 and the optional `fn` is 0-7 (both decimal or
  `0x`-prefixed hex). When omitted, the port is assigned the lowest
  available devfn. Ports are assigned in order, so an explicit `addr` that
  collides with an already-assigned port is an error.
- `hotplug`: enables hotplug support for that root port.
- `acs=<mask>`: sets the Access Control Services capability mask for the
  root port. The value can be decimal or hexadecimal. Default is `0x005f`.
  Use `acs=0` to disable ACS for a root port.
- `cxl`: marks the root port as CXL-capable.
- `pasid`: advertises support for TLP prefixing (such as for guest PASID
  behind a virtual IOMMU)

`--pcie-switch` accepts optional comma-separated options as well:

```sh
--pcie-switch rp0:switch0,num_downstream_ports=4,acs=0x005f
```

- `num_downstream_ports=<N>`: number of downstream ports for the switch.
- `hotplug`: enables hotplug support on all downstream switch ports.
- `acs=<mask>`: ACS capability mask requested for downstream switch ports.
  The upstream switch port does not expose ACS. Default is `0x005f`.
  Use `acs=0` to disable ACS for switch downstream ports.
- `pasid`: advertises support for TLP prefixing (such as for guest PASID
  behind a virtual IOMMU)

### Generic initiators

A generic initiator is a device that originates memory accesses but has no
CPUs of its own — for example a GPU or accelerator with its own coherent
memory. Declaring one emits an SRAT Generic Initiator Affinity structure that
tells the guest which NUMA node the device belongs to, so the guest can
account for access latency and online the device's memory on the right
proximity domain.

Use `--pcie-generic-initiator` to mark the device directly behind a PCIe port
as a generic initiator for a NUMA node:

```sh
# Create a CPU-less, memory-less NUMA node and a root port, then declare the
# device behind the root port as a generic initiator for that node.
--numa size=2G --numa size=0,vps=[] \
  --pcie-root-complex rc0 --pcie-root-port rc0:rp0 \
  --pcie-generic-initiator port=rp0,node=1
```

- Syntax: `port=<port_name>,node=<node>`.
- `port=<port_name>` may be a root port name or a switch downstream port name
  (e.g. `switch0-downstream-1`); it is resolved against the live topology.
- `node=<node>` is the NUMA node the device is a generic initiator for, and
  should typically be a CPU-less and memory-less node created via `--numa`.


### Attaching devices to PCIe

Several device types support the `pcie_port=<name>` option to attach to a
PCIe root port. The syntax varies slightly between device types:

**Disks** (comma-separated option): `--nvme-pci` + `--disk`, `--virtio-blk`

```sh
--virtio-blk file:/path/to/disk.raw,pcie_port=rp0
--nvme-pci id=nvme0,pcie_port=rp0 --disk file:/path/to/disk.raw,on=nvme0
```

**CXL test endpoint** (comma-separated option): `--cxl-test`

```sh
--cxl-test mem:1G,pcie_port=rp0
```

`--cxl-test` creates a CXL Type-3 test endpoint with one component-register
BAR.
The `mem:<len>` value sets the emulated HDM size and allocates backing memory.

**NICs** (colon-prefixed): `--net`, `--virtio-net`, `--mana`

```sh
--virtio-net pcie_port=rp0:tap:tap0  # TAP is Linux-only
--net pcie_port=rp0:consomme
--mana pcie_port=rp0:tap:tap0        # TAP is Linux-only
```

**Filesystems and other virtio devices** (colon-prefixed):
`--virtio-fs`, `--virtio-fs-shmem`, `--virtio-9p`, `--virtio-pmem`

```sh
--virtio-fs pcie_port=rp0:myfs,/path/to/share
--virtio-fs-shmem pcie_port=rp0:myfs,/path/to/share
--virtio-9p pcie_port=rp0:myfs,/path/to/share
--virtio-pmem pcie_port=rp0:/path/to/file
```

For `--virtio-rng` and `--virtio-console`, use their separate PCIe port flags:

```sh
--virtio-rng --virtio-rng-pcie-port rp0
--virtio-console console --virtio-console-pcie-port rp0
```

**vhost-user devices** (comma-separated option, Linux only): `--vhost-user`

```sh
--vhost-user /tmp/vhost-blk.sock,type=blk,pcie_port=rp0
--vhost-user /tmp/virtiofsd.sock,type=fs,tag=myfs,pcie_port=rp0
```

**VFIO device assignment** (Linux only): `--vfio` (and optional `--iommu`)

```sh
# Legacy VFIO group/container path:
--vfio host=0000:01:00.0,port=rp0

# Modern VFIO cdev + iommufd path (Linux >= 6.6):
--iommu id=iommu0 --vfio host=0000:01:00.0,port=rp0,iommu=iommu0

# Pin BAR0 to its physical address for P2P DMA:
--vfio host=0000:01:00.0,port=rp0,bar0=host
```

### SMMU (aarch64 only)

`--smmu` enables an emulated Arm SMMUv3 IOMMU for a named PCIe root
complex. The flag is repeatable — use one `--smmu` per root complex that
should have an SMMU. Devices behind a covered root complex get software
IOVA→GPA translation for DMA and MSI addresses.

The syntax is a comma-separated key/value list:

```sh
--smmu rc=<name>[,accel][,oas=auto|N]
```

- `rc=<name>` (required): the PCIe root complex this SMMU covers.
- `accel` (optional): enable hardware-accelerated (iommufd-nested)
  translation, delegating stage-1 walks to the host IOMMU. See the note
  below — this is not yet wired up and currently fails at startup.
- `oas=auto|N` (optional): the SMMU's output address size (OAS) in bits.
  `auto` (the default) advertises a fixed 48 bits, which covers typical
  configurations. Very large RAM or an explicitly pinned high MMIO/ECAM
  base can exceed this, requiring an explicit larger `oas=` (e.g. `oas=52`).
  A fixed `N` must be one of the SMMUv3-legal encodings: `32`, `36`, `40`,
  `42`, `44`, `48`, or `52`.

```sh
# Enable an emulated SMMU on root complex rc0
--smmu rc=rc0

# Multiple root complexes
--smmu rc=rc0 --smmu rc=rc1

# Pin the output address size to 48 bits
--smmu rc=rc0,oas=48
```

```admonish warning
`accel` is accepted by the parser but the acceleration backend is not yet
implemented: requesting it fails during SMMU setup rather than silently
falling back to emulated translation.

VFIO devices therefore cannot currently be placed behind an SMMU-covered
root complex, because host passthrough requires the (not-yet-available)
iommufd nested translation path.
```

### AMD IOMMU (x86_64 only)

`--amd-iommu <RC_NAME>` enables an emulated AMD-Vi IOMMU for the named
root complex. The flag is repeatable — use one `--amd-iommu` per root
complex that should have an IOMMU. Devices behind a covered root complex
get software IOVA→GPA translation for DMA and interrupt remapping.

```sh
# Enable AMD IOMMU on root complex rc0
--amd-iommu rc0
```

Mutually exclusive with `--intel-vtd` within the same VM (only one x86
IOMMU type can be active).

### Intel VT-d (x86_64 only)

`--intel-vtd <RC_NAME>` enables an emulated Intel VT-d IOMMU for the
named root complex. The flag is repeatable — use one `--intel-vtd` per
root complex that should have an IOMMU. The guest discovers VT-d units
via the ACPI DMAR table (not PCI config space).

```sh
# Enable Intel VT-d on root complex rc0
--intel-vtd rc0

# Multiple root complexes
--intel-vtd rc0 --intel-vtd rc1
```

Mutually exclusive with `--amd-iommu` within the same VM (only one x86
IOMMU type can be active).
