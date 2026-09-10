// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Snapshot manifest types and I/O functions for saving/restoring VM snapshots.

use anyhow::Context;
use mesh::payload::Protobuf;
use mesh::payload::Timestamp;
use sha2::Digest;
use std::collections::HashSet;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

/// Current manifest format version. Bump when making incompatible changes.
pub const MANIFEST_VERSION: u32 = 5;
/// Magic identifying the OpenVMM snapshot manifest format.
pub const SNAPSHOT_FORMAT_MAGIC: &[u8] = b"OPENVMM_SNAPSHOT_V5\0";
const VERSION_4_MANIFEST_VERSION: u32 = 4;
const VERSION_4_SNAPSHOT_FORMAT_MAGIC: &[u8] = b"OPENVMM_SNAPSHOT_V4\0";
const PREVIOUS_MANIFEST_VERSION: u32 = 3;
const PREVIOUS_SNAPSHOT_FORMAT_MAGIC: &[u8] = b"OPENVMM_SNAPSHOT_V3\0";
const LEGACY_MANIFEST_VERSION: u32 = 2;
const LEGACY_SNAPSHOT_FORMAT_MAGIC: &[u8] = b"OPENVMM_SNAPSHOT_V2\0";
/// Saved-state schema version used by the VM worker envelope.
pub const SAVED_STATE_SCHEMA_VERSION: u32 = 1;
/// Protobuf root type stored in `state.bin`.
pub const SAVED_STATE_ROOT_TYPE: &str = "openvmm.SavedState";
/// Capability version for the always-present dormant microVM virtio-fs slot.
pub const MICROVM_FILESYSTEM_SLOT_VERSION: u32 = 1;
/// SMP-safe Xen PVH layout with shared interrupt status used by the microVM.
pub const MICROVM_PVH_LAYOUT_VERSION: u32 = 2;
/// Contract version for one-shot restore-time microVM memory expansion.
pub const MICROVM_MEMORY_EXPANSION_VERSION: u32 = 1;
/// Linux memory-block granularity used by the x86-64 microVM guest.
pub const MICROVM_MEMORY_BLOCK_SIZE_BYTES: u64 = 128 * 1024 * 1024;
/// Snapshot contract name for shared-status edge interrupts.
pub const MICROVM_SHARED_STATUS_INTERRUPT_MODE: &str = "edge-shared-status";
/// Clock policy applied when a snapshot is restored.
pub const ADVANCE_BY_HOST_DOWNTIME: &str = "advance_by_host_downtime";
/// Fleet-wide snapshot captured before image and sandbox configuration is consumed.
pub const SNAPSHOT_TIER_PLATFORM: &str = "platform";
/// Tenant-scoped snapshot captured at a warm workload handoff point.
pub const SNAPSHOT_TIER_WORKLOAD_START: &str = "workload-start";
/// Single-instance checkpoint captured during a live workload.
pub const SNAPSHOT_TIER_INSTANCE_CHECKPOINT: &str = "instance-checkpoint";
/// Reusable restore policy for snapshots that create independent instances.
pub const SNAPSHOT_RESTORE_POLICY_CLONE: &str = "clone";
/// Single-use restore policy for snapshots that continue one instance.
pub const SNAPSHOT_RESTORE_POLICY_RESUME: &str = "resume";
/// The invariant configuration section was consumed before capture.
pub const SNAPSHOT_CONFIG_INVARIANTS: u32 = 1 << 0;
/// The image-binding configuration section was consumed before capture.
pub const SNAPSHOT_CONFIG_IMAGE_BINDING: u32 = 1 << 1;
/// The per-sandbox configuration section was consumed before capture.
pub const SNAPSHOT_CONFIG_SANDBOX: u32 = 1 << 2;
const SNAPSHOT_CONFIG_ALL: u32 =
    SNAPSHOT_CONFIG_INVARIANTS | SNAPSHOT_CONFIG_IMAGE_BINDING | SNAPSHOT_CONFIG_SANDBOX;

const MANIFEST_FILE_NAME: &str = "manifest.bin";
const STATE_FILE_NAME: &str = "state.bin";
const MEMORY_FILE_NAME: &str = "memory.bin";
const RESUME_CLAIM_FILE_NAME: &str = "resume.claim";
/// Fixed snapshot-relative name of a paired scratch image.
pub const SCRATCH_FILE_NAME: &str = "scratch.img";
const MAX_MANIFEST_SIZE_BYTES: u64 = 1024 * 1024;
const MAX_SAVED_STATE_SIZE_BYTES: u64 = 256 * 1024 * 1024;
const COPY_BUFFER_SIZE: usize = 1024 * 1024;
const SHA256_SIZE: usize = 32;
const MAX_MEMORY_RANGES: usize = 128;
const MAX_DEVICES: usize = 64;
const MAX_DEVICE_RANGES: usize = 16;
const MAX_STATE_UNITS: usize = 512;
const MAX_ATTACHMENTS: usize = 64;
const MAX_ATTACHMENT_IDENTITY_BYTES: usize = 4096;
const MAX_COMMAND_LINE_BYTES: usize = 64 * 1024;
const MAX_CPU_CONTRACT_BYTES: usize = 1024 * 1024;

static STAGING_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Error publishing a snapshot.
#[derive(Debug, thiserror::Error)]
pub enum SnapshotWriteError {
    /// Nothing was published at the final destination.
    #[error(transparent)]
    BeforeCommit(#[from] anyhow::Error),
    /// Publication did not commit, but an alias to live source RAM may remain.
    ///
    /// Callers must terminate rather than resume the source. The reported
    /// private staging path may require operator cleanup after termination.
    #[error(
        "snapshot failed before commit at {path:?}, and automatic RAM staging alias cleanup is uncertain: publication error: {error:#}; cleanup error: {cleanup_error:#}"
    )]
    CleanupUncertain {
        /// Staging directory that may retain an alias to live source RAM.
        path: PathBuf,
        /// Original publication failure.
        error: anyhow::Error,
        /// Failure removing or proving absence of the staging alias.
        #[source]
        cleanup_error: anyhow::Error,
    },
    /// The directory rename committed, but its parent could not be flushed.
    #[error("snapshot committed to {path:?}, but flushing its parent directory failed: {error:#}")]
    Committed {
        /// Final committed snapshot path.
        path: PathBuf,
        /// Parent-directory flush failure.
        #[source]
        error: anyhow::Error,
    },
}

impl SnapshotWriteError {
    /// Returns whether the final snapshot directory has been published.
    pub fn is_committed(&self) -> bool {
        matches!(self, Self::Committed { .. })
    }

    /// Returns whether a quiesced source may safely roll back and resume.
    pub fn is_rollback_safe(&self) -> bool {
        matches!(self, Self::BeforeCommit(_))
    }
}

/// A guest RAM range and its corresponding offset in `memory.bin`.
#[derive(Clone, Debug, PartialEq, Eq, Protobuf)]
#[mesh(package = "openvmm.snapshot")]
pub struct SnapshotMemoryRange {
    /// Guest physical base address.
    #[mesh(1)]
    pub gpa_start: u64,
    /// Range length in bytes.
    #[mesh(2)]
    pub length: u64,
    /// Byte offset in `memory.bin`.
    #[mesh(3)]
    pub file_offset: u64,
}

/// A restore-attachable guest RAM range that is absent from `memory.bin`.
#[derive(Clone, Debug, PartialEq, Eq, Protobuf)]
#[mesh(package = "openvmm.snapshot")]
pub struct SnapshotMemoryExpansionRange {
    /// Guest physical base address.
    #[mesh(1)]
    pub gpa_start: u64,
    /// Range length in bytes.
    #[mesh(2)]
    pub length: u64,
}

/// Processor topology that must be reproduced during restore.
#[derive(Clone, Debug, PartialEq, Eq, Protobuf)]
#[mesh(package = "openvmm.snapshot")]
pub struct SnapshotProcessorTopology {
    /// Socket count.
    #[mesh(1)]
    pub sockets: u32,
    /// Dies per socket.
    #[mesh(2)]
    pub dies_per_socket: u32,
    /// Cores per die.
    #[mesh(3)]
    pub cores_per_die: u32,
    /// Threads per core.
    #[mesh(4)]
    pub threads_per_core: u32,
    /// APIC IDs in virtual-processor order.
    #[mesh(5)]
    pub apic_ids: Vec<u32>,
}

/// An address range owned by a saved device.
#[derive(Clone, Debug, PartialEq, Eq, Protobuf)]
#[mesh(package = "openvmm.snapshot")]
pub struct SnapshotDeviceRange {
    /// Address space (`pmio` or `mmio`).
    #[mesh(1)]
    pub address_space: String,
    /// Range base address.
    #[mesh(2)]
    pub start: u64,
    /// Range length in bytes.
    #[mesh(3)]
    pub length: u64,
}

/// Guest-visible device identity and placement.
#[derive(Clone, Debug, PartialEq, Eq, Protobuf)]
#[mesh(package = "openvmm.snapshot")]
pub struct SnapshotDevice {
    /// Stable device identity.
    #[mesh(1)]
    pub stable_id: String,
    /// State-unit identity, including units with no mutable state.
    #[mesh(2)]
    pub state_unit_name: String,
    /// Device kind.
    #[mesh(3)]
    pub kind: String,
    /// Stable device order.
    #[mesh(4)]
    pub order: u32,
    /// PMIO or MMIO ranges.
    #[mesh(5)]
    pub ranges: Vec<SnapshotDeviceRange>,
    /// Interrupt number, or `None` for devices without an interrupt.
    #[mesh(6)]
    pub irq: Option<u32>,
    /// Transport kind, empty for non-transport devices.
    #[mesh(7)]
    pub transport: String,
    /// Effective feature banks in bank order.
    #[mesh(8)]
    pub feature_banks: Vec<u32>,
    /// Queue count.
    #[mesh(9)]
    pub queue_count: u32,
    /// Maximum queue sizes in queue order.
    #[mesh(10)]
    pub queue_max_sizes: Vec<u32>,
}

/// Stable identity for a host resource required by a device.
#[derive(Clone, Debug, PartialEq, Eq, Protobuf)]
#[mesh(package = "openvmm.snapshot")]
pub struct SnapshotAttachment {
    /// Stable attachment ID.
    #[mesh(1)]
    pub stable_id: String,
    /// Attachment kind.
    #[mesh(2)]
    pub kind: String,
    /// Whether restore requires the attachment.
    #[mesh(3)]
    pub required: bool,
    /// Reconnection policy.
    #[mesh(4)]
    pub reconnect_policy: String,
    /// Immutable identity kind.
    #[mesh(5)]
    pub identity_kind: String,
    /// Provider identity or SHA-256 bytes.
    #[mesh(6)]
    pub identity: Vec<u8>,
    /// Attachment length when applicable.
    #[mesh(7)]
    pub length: u64,
    /// Bounded reconnect timeout. Zero for non-client policies.
    #[mesh(8)]
    pub reconnect_timeout_ms: u64,
}

/// Canonical guest-visible identity of the microVM network.
#[derive(Clone, Debug, PartialEq, Eq, Protobuf)]
#[mesh(package = "openvmm.snapshot")]
pub struct SnapshotMicrovmNetwork {
    /// Required cross-platform host-network implementation contract.
    #[mesh(9)]
    pub profile: String,
    /// Guest IPv4 address in network byte order.
    #[mesh(1)]
    pub guest_ipv4: u32,
    /// IPv4 subnet prefix length.
    #[mesh(2)]
    pub prefix_length: u32,
    /// Derived gateway IPv4 address in network byte order.
    #[mesh(3)]
    pub gateway_ipv4: u32,
    /// Deterministic guest MAC address.
    #[mesh(4)]
    pub guest_mac: Vec<u8>,
    /// Deterministic gateway MAC address.
    #[mesh(5)]
    pub gateway_mac: Vec<u8>,
    /// Canonical run-scoped egress policy mode.
    #[mesh(6)]
    pub egress_policy_mode: String,
    /// SHA-256 of the canonical run-scoped egress policy.
    #[mesh(7)]
    pub egress_policy_sha256: Vec<u8>,
    /// Whether restore must supply the same policy contract.
    #[mesh(8)]
    pub egress_policy_required: bool,
    /// Version of the canonical egress-policy digest encoding.
    #[mesh(10)]
    pub egress_policy_encoding_version: u32,
}

/// Canonical guest-visible policy of the microVM filesystem.
#[derive(Clone, Debug, PartialEq, Eq, Protobuf)]
#[mesh(package = "openvmm.snapshot")]
pub struct SnapshotMicrovmFilesystem {
    /// Absolute guest mount target.
    #[mesh(1)]
    pub guest_mount_target: String,
    /// Snapshot-authoritative `ro` or `rw` access mode.
    #[mesh(2)]
    pub access_mode: String,
    /// Host attachment policy.
    #[mesh(3)]
    pub restore_mode: String,
    /// Fixed virtio-fs tag.
    #[mesh(4)]
    pub tag: String,
    /// Number of high-priority queues.
    #[mesh(5)]
    pub high_priority_queue_count: u32,
    /// Number of request queues.
    #[mesh(6)]
    pub request_queue_count: u32,
    /// DAX/shared-memory window size.
    #[mesh(7)]
    pub shared_memory_size: u64,
    /// Whether all opened files use direct I/O.
    #[mesh(8)]
    pub direct_io: bool,
    /// Guest entry-cache lifetime in nanoseconds.
    #[mesh(9)]
    pub entry_cache_timeout_ns: u64,
    /// Guest attribute-cache lifetime in nanoseconds.
    #[mesh(10)]
    pub attribute_cache_timeout_ns: u64,
    /// Canonical absolute host export path.
    #[mesh(11)]
    pub canonical_host_path: String,
}

/// Authoritative identity and snapshot policy for a microVM sandbox block.
#[derive(Clone, Debug, PartialEq, Eq, Protobuf)]
#[mesh(package = "openvmm.snapshot")]
pub struct SnapshotMicrovmSandboxBlock {
    /// Stable role (`distro`, `runtime`, `custom`, or `scratch`).
    #[mesh(1)]
    pub role: String,
    /// Whether the guest sees the device as read-only.
    #[mesh(2)]
    pub read_only: bool,
    /// Logical device length in bytes.
    #[mesh(3)]
    pub length: u64,
    /// Kind of immutable identity carried in `identity`.
    #[mesh(4)]
    pub identity_kind: String,
    /// Immutable layer identity or paired-scratch digest.
    #[mesh(5)]
    pub identity: Vec<u8>,
    /// Fixed snapshot-relative artifact name; empty for external layers.
    #[mesh(6)]
    pub artifact: String,
    /// Guest-visible logical block size in bytes.
    #[mesh(7)]
    pub logical_block_size: u32,
    /// Guest-visible physical block size in bytes.
    #[mesh(8)]
    pub physical_block_size: u32,
}

/// Machine composition that becomes authoritative after capture.
#[derive(Clone, Debug, PartialEq, Eq, Protobuf)]
#[mesh(package = "openvmm.snapshot")]
pub struct SnapshotMachineContract {
    /// Machine profile name.
    #[mesh(1)]
    pub machine_profile: String,
    /// microVM ABI version.
    #[mesh(2)]
    pub microvm_abi_version: u32,
    /// Source hypervisor kind.
    #[mesh(3)]
    pub source_hypervisor: String,
    /// Effective kernel command line.
    #[mesh(4)]
    pub effective_command_line: String,
    /// SHA-256 of the effective command line.
    #[mesh(5)]
    pub effective_command_line_sha256: Vec<u8>,
    /// Guest RAM ranges in stable order.
    #[mesh(6)]
    pub memory_ranges: Vec<SnapshotMemoryRange>,
    /// Processor topology.
    #[mesh(7)]
    pub topology: SnapshotProcessorTopology,
    /// Guest-visible devices in stable order.
    #[mesh(8)]
    pub devices: Vec<SnapshotDevice>,
    /// Complete state-unit inventory in stable order.
    #[mesh(9)]
    pub state_unit_names: Vec<String>,
    /// Required host attachments.
    #[mesh(10)]
    pub attachments: Vec<SnapshotAttachment>,
    /// Host wall time at the stopped capture boundary.
    #[mesh(11)]
    pub capture_wall_clock: Timestamp,
    /// Effective guest TSC frequency.
    #[mesh(12)]
    pub tsc_frequency_hz: u64,
    /// Accepted destination TSC frequency tolerance in parts per million.
    #[mesh(13)]
    pub tsc_tolerance_ppm: u32,
    /// Canonical protobuf-encoded effective CPU contract.
    #[mesh(14)]
    pub cpu_contract: Vec<u8>,
    /// SHA-256 of the canonical CPU contract.
    #[mesh(15)]
    pub cpu_contract_sha256: Vec<u8>,
    /// Version of the fixed Xen PVH boot and memory layout.
    #[mesh(16)]
    pub pvh_layout_version: u32,
    /// Policy used to advance clocks and deadlines over host downtime.
    #[mesh(17)]
    pub clock_policy: String,
    /// Static identity of the optional microVM virtio-net device.
    #[mesh(18)]
    pub microvm_network: Option<SnapshotMicrovmNetwork>,
    /// Guest-visible policy of the optional microVM virtio-fs device.
    #[mesh(19)]
    pub microvm_filesystem: Option<SnapshotMicrovmFilesystem>,
    /// Effective local APIC timer frequency.
    #[mesh(20)]
    pub apic_frequency_hz: Option<u64>,
    /// Fixed-role sandbox blocks in guest-visible order.
    #[mesh(21)]
    pub microvm_sandbox_blocks: Vec<SnapshotMicrovmSandboxBlock>,
    /// Version of the reserved restore-attachable microVM virtio-fs slot.
    #[mesh(22)]
    pub microvm_filesystem_slot_version: u32,
    /// Virtual processors online at boot, or zero when restore activation is disabled.
    #[mesh(23)]
    pub boot_online_vp_count: u32,
    /// Virtio interrupt-delivery mode.
    #[mesh(24)]
    pub virtio_interrupt_mode: String,
    /// Guest-physical base of the shared interrupt-status page, or zero when absent.
    #[mesh(25)]
    pub virtio_shared_status_page_gpa: u64,
    /// Size of the shared interrupt-status page, or zero when absent.
    #[mesh(26)]
    pub virtio_shared_status_page_size: u64,
    /// Restore-time memory expansion capability version, or zero when absent.
    #[mesh(27)]
    pub memory_expansion_version: u32,
    /// Immutable maximum guest RAM size for restore-time expansion.
    #[mesh(28)]
    pub memory_capacity_bytes: u64,
    /// Required alignment of every restore-time memory target and range.
    #[mesh(29)]
    pub memory_block_size_bytes: u64,
    /// Canonical capacity ranges absent from the captured PVH memory map.
    #[mesh(30)]
    pub memory_expansion_ranges: Vec<SnapshotMemoryExpansionRange>,
}

impl SnapshotMachineContract {
    /// Sets the effective command line and its digest together.
    pub fn set_effective_command_line(&mut self, command_line: String) {
        self.effective_command_line_sha256 = sha2::Sha256::digest(command_line.as_bytes()).to_vec();
        self.effective_command_line = command_line;
    }

    /// Sets the canonical CPU compatibility contract and its digest together.
    pub fn set_cpu_compatibility_contract(&mut self, cpu_contract: Vec<u8>) {
        self.cpu_contract_sha256 = sha2::Sha256::digest(&cpu_contract).to_vec();
        self.cpu_contract = cpu_contract;
    }
}

fn microvm_snapshot_topology(processor_count: u32) -> anyhow::Result<SnapshotProcessorTopology> {
    anyhow::ensure!(
        openvmm_defs::config::microvm_processor_count_supported(processor_count),
        "microVM does not support {processor_count} vCPUs"
    );
    Ok(SnapshotProcessorTopology {
        sockets: 1,
        dies_per_socket: 1,
        cores_per_die: processor_count,
        threads_per_core: 1,
        apic_ids: (0..processor_count).collect(),
    })
}

fn microvm_boot_online_vp_count(
    vp_capacity: u32,
    effective_command_line: &str,
) -> anyhow::Result<u32> {
    let mut boot_online_vp_count = None;
    for token in effective_command_line.split_ascii_whitespace() {
        let Some(value) = token.strip_prefix("maxcpus=") else {
            continue;
        };
        anyhow::ensure!(
            boot_online_vp_count.is_none(),
            "microVM command line contains multiple maxcpus values"
        );
        let count = value
            .parse::<u32>()
            .context("microVM maxcpus value is invalid")?;
        anyhow::ensure!(
            openvmm_defs::config::microvm_processor_count_supported(count),
            "microVM does not support a boot-online count of {count}"
        );
        anyhow::ensure!(
            count <= vp_capacity,
            "microVM boot-online count {count} exceeds VP capacity {vp_capacity}"
        );
        boot_online_vp_count = Some(count);
    }
    Ok(boot_online_vp_count.unwrap_or(0))
}

/// Validates a restore-time online VP target against an opt-in snapshot contract.
pub fn validate_restore_online_vp_count(
    manifest: &SnapshotManifest,
    restore_online_vp_count: u32,
) -> anyhow::Result<()> {
    let contract = manifest
        .machine_contract
        .as_ref()
        .context("snapshot is missing the authoritative machine contract")?;
    validate_supported_microvm_contract(contract)?;
    anyhow::ensure!(
        contract.boot_online_vp_count != 0,
        "snapshot does not declare restore-time VP activation support"
    );
    anyhow::ensure!(
        openvmm_defs::config::microvm_processor_count_supported(restore_online_vp_count),
        "restore-online VP count {restore_online_vp_count} is not supported"
    );
    anyhow::ensure!(
        restore_online_vp_count >= contract.boot_online_vp_count,
        "restore-online VP count {restore_online_vp_count} is below boot-online count {}",
        contract.boot_online_vp_count
    );
    anyhow::ensure!(
        restore_online_vp_count <= manifest.vp_count,
        "restore-online VP count {restore_online_vp_count} exceeds VP capacity {}",
        manifest.vp_count
    );
    Ok(())
}

fn canonical_microvm_memory_ranges(memory_size: u64) -> anyhow::Result<Vec<SnapshotMemoryRange>> {
    const LOW_RAM_END: u64 = 3 * 1024 * 1024 * 1024;
    const HIGH_RAM_START: u64 = 4 * 1024 * 1024 * 1024;
    anyhow::ensure!(memory_size != 0, "microVM RAM size must be nonzero");
    let low_length = memory_size.min(LOW_RAM_END);
    let mut ranges = vec![SnapshotMemoryRange {
        gpa_start: 0,
        length: low_length,
        file_offset: 0,
    }];
    if memory_size > LOW_RAM_END {
        let high_length = memory_size - LOW_RAM_END;
        HIGH_RAM_START
            .checked_add(high_length)
            .context("microVM RAM layout overflows GPA space")?;
        ranges.push(SnapshotMemoryRange {
            gpa_start: HIGH_RAM_START,
            length: high_length,
            file_offset: LOW_RAM_END,
        });
    }
    Ok(ranges)
}

impl SnapshotMicrovmNetwork {
    fn new(
        config: &openvmm_defs::config::MicrovmNetworkConfig,
        egress_policy: &net_backend_resources::egress::EgressPolicy,
    ) -> Self {
        Self {
            profile: config.profile.as_str().to_owned(),
            guest_ipv4: u32::from(config.guest_ipv4),
            prefix_length: u32::from(config.prefix_length),
            gateway_ipv4: u32::from(config.derived_gateway_ipv4),
            guest_mac: config.guest_mac.to_bytes().to_vec(),
            gateway_mac: config.gateway_mac.to_bytes().to_vec(),
            egress_policy_mode: egress_policy.mode_name().to_owned(),
            egress_policy_sha256: sha2::Sha256::digest(egress_policy.canonical_bytes()).to_vec(),
            egress_policy_required: egress_policy.is_active(),
            egress_policy_encoding_version:
                net_backend_resources::egress::EGRESS_POLICY_ENCODING_VERSION,
        }
    }
}

/// Validates a restore-time policy against the snapshot's canonical contract.
pub fn validate_microvm_network_policy(
    saved: &SnapshotMicrovmNetwork,
    policy: &net_backend_resources::egress::EgressPolicy,
) -> anyhow::Result<()> {
    let encoding_version = match saved.egress_policy_encoding_version {
        0 => 1,
        version => version,
    };
    let canonical = policy
        .canonical_bytes_for_version(encoding_version)
        .with_context(|| {
            format!("snapshot egress policy encoding version {encoding_version} is unsupported")
        })?;
    let digest = sha2::Sha256::digest(canonical);
    anyhow::ensure!(
        saved.egress_policy_mode == policy.mode_name()
            && saved.egress_policy_sha256 == digest.as_slice()
            && saved.egress_policy_required == policy.is_active(),
        "restore-time egress policy does not match the snapshot contract"
    );
    Ok(())
}

/// Adds a portable network identity to a base microVM snapshot contract.
pub fn add_microvm_network_contract(
    contract: &mut SnapshotMachineContract,
    config: &openvmm_defs::config::MicrovmNetworkConfig,
    policy: &net_backend_resources::egress::EgressPolicy,
    attachment: SnapshotAttachment,
) -> anyhow::Result<()> {
    let source_hypervisor = contract.source_hypervisor.as_str();
    let effective_command_line = &contract.effective_command_line;
    let network = Some((config, policy, attachment));
    let mut devices = Vec::new();
    let mut attachments = Vec::new();
    let mmio = |start, length| SnapshotDeviceRange {
        address_space: "mmio".to_owned(),
        start,
        length,
    };
    let microvm_network = if let Some((network, egress_policy, attachment)) = network {
        let policy_is_valid = matches!(source_hypervisor, "kvm" | "mshv" | "whp")
            && attachment.reconnect_policy == "recreate-endpoint"
            && !attachment.required
            && attachment.identity_kind == "user-mode-nat"
            && attachment.identity == b"consomme";
        anyhow::ensure!(
            attachment.stable_id == "net:microvm0"
                && attachment.kind == "virtio-net"
                && policy_is_valid
                && !attachment.identity.is_empty()
                && attachment.identity.len() <= MAX_ATTACHMENT_IDENTITY_BYTES
                && attachment.length == 0
                && attachment.reconnect_timeout_ms == 0,
            "microVM network attachment has an unsupported endpoint policy"
        );
        let irq = openvmm_defs::config::microvm_virtio_net_irq(Some(source_hypervisor))?;
        let discovery = format!(
            "virtio_mmio.device={:#x}@{:#x}:{irq}",
            openvmm_defs::config::MICROVM_VIRTIO_MMIO_LEN,
            openvmm_defs::config::MICROVM_VIRTIO_NET_MMIO_BASE,
        );
        let tokens = effective_command_line
            .split_ascii_whitespace()
            .collect::<HashSet<_>>();
        anyhow::ensure!(
            tokens.contains(discovery.as_str())
                && network
                    .command_line_fragment()
                    .split_ascii_whitespace()
                    .all(|token| tokens.contains(token)),
            "microVM network command line does not match its saved identity"
        );
        devices.push(SnapshotDevice {
            stable_id: "net:microvm0".to_owned(),
            state_unit_name: format!(
                "virtio-net-{}",
                openvmm_defs::config::MICROVM_VIRTIO_NET_MMIO_BASE
            ),
            kind: "virtio-net".to_owned(),
            order: devices.len() as u32,
            ranges: vec![mmio(
                openvmm_defs::config::MICROVM_VIRTIO_NET_MMIO_BASE,
                openvmm_defs::config::MICROVM_VIRTIO_MMIO_LEN,
            )],
            irq: Some(irq),
            transport: "virtio-mmio".to_owned(),
            feature_banks: vec![
                openvmm_defs::config::MICROVM_VIRTIO_NET_FEATURES as u32,
                (openvmm_defs::config::MICROVM_VIRTIO_NET_FEATURES >> 32) as u32,
            ],
            queue_count: 2,
            queue_max_sizes: vec![256, 256],
        });
        attachments.push(attachment);
        Some(SnapshotMicrovmNetwork::new(network, egress_policy))
    } else {
        None
    };

    let index = contract
        .devices
        .iter()
        .position(|device| device.transport == "virtio-mmio")
        .unwrap_or(contract.devices.len());
    contract.devices.splice(index..index, devices);
    for (index, device) in contract.devices.iter_mut().enumerate() {
        device.order = index as u32;
    }
    contract.attachments.extend(attachments);
    contract.microvm_network = microvm_network;
    Ok(())
}

impl SnapshotMicrovmFilesystem {
    fn new(
        config: &openvmm_defs::config::MicrovmFilesystemConfig,
        canonical_host_path: &str,
    ) -> Self {
        Self {
            guest_mount_target: config.guest_mount_target.clone(),
            access_mode: config.access.as_str().to_owned(),
            restore_mode: "live-revalidate".to_owned(),
            tag: "microvm".to_owned(),
            high_priority_queue_count: 1,
            request_queue_count: 1,
            shared_memory_size: 0,
            direct_io: true,
            entry_cache_timeout_ns: 0,
            attribute_cache_timeout_ns: 0,
            canonical_host_path: canonical_host_path.to_owned(),
        }
    }
}

/// Adds a path-bound active filesystem identity without any network dependency.
pub fn add_microvm_filesystem_contract(
    contract: &mut SnapshotMachineContract,
    config: &openvmm_defs::config::MicrovmFilesystemConfig,
    root_path: &Path,
    attachment: SnapshotAttachment,
) -> anyhow::Result<()> {
    let source_hypervisor = contract.source_hypervisor.as_str();
    let effective_command_line = &contract.effective_command_line;
    let filesystem = Some((config, root_path, attachment));
    let filesystem_slot = true;
    let mut devices = Vec::new();
    let mut attachments = Vec::new();
    let mmio = |start, length| SnapshotDeviceRange {
        address_space: "mmio".to_owned(),
        start,
        length,
    };
    anyhow::ensure!(
        filesystem.is_none() || filesystem_slot,
        "microVM filesystem policy requires the reserved virtio-fs slot"
    );
    if filesystem_slot {
        let discovery = format!(
            "virtio_mmio.device={:#x}@{:#x}:{}",
            openvmm_defs::config::MICROVM_VIRTIO_MMIO_LEN,
            openvmm_defs::config::MICROVM_VIRTIO_FS_MMIO_BASE,
            openvmm_defs::config::MICROVM_VIRTIO_FS_IRQ,
        );
        anyhow::ensure!(
            effective_command_line
                .split_ascii_whitespace()
                .any(|token| token == discovery),
            "microVM virtio-fs slot is missing from the effective command line"
        );
        devices.push(SnapshotDevice {
            stable_id: "fs:microvm0".to_owned(),
            state_unit_name: format!(
                "virtiofs-{}",
                openvmm_defs::config::MICROVM_VIRTIO_FS_MMIO_BASE
            ),
            kind: "virtio-fs".to_owned(),
            order: devices.len() as u32,
            ranges: vec![mmio(
                openvmm_defs::config::MICROVM_VIRTIO_FS_MMIO_BASE,
                openvmm_defs::config::MICROVM_VIRTIO_MMIO_LEN,
            )],
            irq: Some(openvmm_defs::config::MICROVM_VIRTIO_FS_IRQ),
            transport: "virtio-mmio".to_owned(),
            feature_banks: vec![
                openvmm_defs::config::MICROVM_VIRTIO_FS_FEATURES as u32,
                (openvmm_defs::config::MICROVM_VIRTIO_FS_FEATURES >> 32) as u32,
            ],
            queue_count: 2,
            queue_max_sizes: vec![256, 256],
        });
    }
    let microvm_filesystem = if let Some((filesystem, canonical_host_path, attachment)) = filesystem
    {
        let canonical_host_path = canonical_host_path
            .to_str()
            .context("microVM filesystem canonical host path is not valid UTF-8")?;
        anyhow::ensure!(
            !canonical_host_path.is_empty(),
            "microVM filesystem canonical host path is empty"
        );
        anyhow::ensure!(
            attachment.stable_id == "fs:microvm0"
                && attachment.kind == "virtio-fs"
                && attachment.required
                && attachment.reconnect_policy == "live-revalidate"
                && match source_hypervisor {
                    "kvm" | "mshv" => attachment.identity_kind == "unix-device-inode-v1",
                    "whp" => attachment.identity_kind == "windows-volume-file-id-v1",
                    _ => false,
                }
                && !attachment.identity.is_empty()
                && attachment.identity.len() <= MAX_ATTACHMENT_IDENTITY_BYTES
                && attachment.length == 0
                && attachment.reconnect_timeout_ms == 0,
            "microVM filesystem attachment has an unsupported live-revalidation policy"
        );
        let tokens = effective_command_line
            .split_ascii_whitespace()
            .collect::<HashSet<_>>();
        anyhow::ensure!(
            filesystem
                .command_line_fragment()
                .split_ascii_whitespace()
                .all(|token| tokens.contains(token)),
            "microVM filesystem command line does not match its saved policy"
        );
        attachments.push(attachment);
        Some(SnapshotMicrovmFilesystem::new(
            filesystem,
            canonical_host_path,
        ))
    } else {
        None
    };

    let index = contract
        .devices
        .iter()
        .position(|device| matches!(device.kind.as_str(), "virtio-console" | "virtio-blk"))
        .unwrap_or(contract.devices.len());
    contract.devices.splice(index..index, devices);
    for (index, device) in contract.devices.iter_mut().enumerate() {
        device.order = index as u32;
    }
    contract.attachments.extend(attachments);
    contract.microvm_filesystem = microvm_filesystem;
    Ok(())
}

/// Reserves the stable dormant filesystem slot in a microVM contract.
pub fn reserve_microvm_filesystem_slot(contract: &mut SnapshotMachineContract) {
    if !contract
        .devices
        .iter()
        .any(|device| device.stable_id == "fs:microvm0")
    {
        let index = contract
            .devices
            .iter()
            .position(|device| matches!(device.kind.as_str(), "virtio-console" | "virtio-blk"))
            .unwrap_or(contract.devices.len());
        contract.devices.insert(
            index,
            SnapshotDevice {
                stable_id: "fs:microvm0".to_owned(),
                state_unit_name: format!(
                    "virtiofs-{}",
                    openvmm_defs::config::MICROVM_VIRTIO_FS_MMIO_BASE
                ),
                kind: "virtio-fs".to_owned(),
                order: index as u32,
                ranges: vec![SnapshotDeviceRange {
                    address_space: "mmio".to_owned(),
                    start: openvmm_defs::config::MICROVM_VIRTIO_FS_MMIO_BASE,
                    length: openvmm_defs::config::MICROVM_VIRTIO_MMIO_LEN,
                }],
                irq: Some(openvmm_defs::config::MICROVM_VIRTIO_FS_IRQ),
                transport: "virtio-mmio".to_owned(),
                feature_banks: vec![
                    openvmm_defs::config::MICROVM_VIRTIO_FS_FEATURES as u32,
                    (openvmm_defs::config::MICROVM_VIRTIO_FS_FEATURES >> 32) as u32,
                ],
                queue_count: 2,
                queue_max_sizes: vec![256, 256],
            },
        );
        for (index, device) in contract.devices.iter_mut().enumerate() {
            device.order = index as u32;
        }
    }
    contract.microvm_filesystem_slot_version = MICROVM_FILESYSTEM_SLOT_VERSION;
}

/// Builds the authoritative microVM machine contract.
pub fn microvm_machine_contract(
    source_hypervisor: &str,
    effective_command_line: String,
    console_attachment: Option<SnapshotAttachment>,
    processor_count: u32,
    memory_size: u64,
    state_unit_names: Vec<String>,
    capture_wall_clock: Timestamp,
    tsc_frequency_hz: u64,
    apic_frequency_hz: Option<u64>,
    cpu_contract: Vec<u8>,
) -> anyhow::Result<SnapshotMachineContract> {
    anyhow::ensure!(
        matches!(source_hypervisor, "kvm" | "whp"),
        "microVM snapshots require the KVM or WHP hypervisor"
    );
    let topology = microvm_snapshot_topology(processor_count)?;
    let boot_online_vp_count =
        microvm_boot_online_vp_count(processor_count, &effective_command_line)?;
    let memory_ranges = canonical_microvm_memory_ranges(memory_size)?;
    let device = |stable_id: &str,
                  state_unit_name: &str,
                  kind: &str,
                  ranges: Vec<SnapshotDeviceRange>,
                  irq: Option<u32>,
                  order: u32| SnapshotDevice {
        stable_id: stable_id.to_owned(),
        state_unit_name: state_unit_name.to_owned(),
        kind: kind.to_owned(),
        order,
        ranges,
        irq,
        transport: String::new(),
        feature_banks: Vec::new(),
        queue_count: 0,
        queue_max_sizes: Vec::new(),
    };
    let pmio = |start, length| SnapshotDeviceRange {
        address_space: "pmio".to_owned(),
        start,
        length,
    };
    let mmio = |start, length| SnapshotDeviceRange {
        address_space: "mmio".to_owned(),
        start,
        length,
    };

    let mut devices = vec![
        device("partition", "partition", "partition", Vec::new(), None, 0),
        device("vp0", "partition", "vcpu", Vec::new(), None, 1),
        device("vmtime", "vmtime", "clock", Vec::new(), None, 2),
        device(
            "pic",
            "pic",
            "pic",
            vec![pmio(0x20, 2), pmio(0xa0, 2)],
            None,
            3,
        ),
        device(
            "ioapic",
            "ioapic",
            "ioapic",
            vec![mmio(0xfec0_0000, 0x1000)],
            None,
            4,
        ),
        device(
            "lapic",
            "partition",
            "lapic",
            vec![mmio(0xfee0_0000, 0x1000)],
            None,
            5,
        ),
        device("pit", "pit", "pit", vec![pmio(0x40, 4)], Some(0), 6),
        device("rtc", "rtc", "rtc", vec![pmio(0x70, 2)], Some(8), 7),
        device(
            "microvm-portb",
            "microvm-portb",
            "portb",
            vec![pmio(0xe9, 2)],
            None,
            8,
        ),
        device(
            "microvm-shutdown",
            "microvm-shutdown",
            "shutdown",
            vec![pmio(0x604, 1)],
            None,
            9,
        ),
        device(
            "microvm-snapshot-request",
            "microvm-snapshot-request",
            "snapshot-request",
            vec![pmio(0x605, 1)],
            None,
            10,
        ),
    ];
    let mut attachments = Vec::new();
    if let Some(attachment) = console_attachment {
        let policy_is_valid = match attachment.reconnect_policy.as_str() {
            "recreate-listener" => {
                !attachment.required
                    && attachment.reconnect_timeout_ms == 0
                    && matches!(
                        attachment.identity_kind.as_str(),
                        "unix-socket" | "named-pipe" | "tcp"
                    )
            }
            "reconnect-client" => {
                attachment.required
                    && attachment.reconnect_timeout_ms
                        == openvmm_defs::config::MICROVM_CONSOLE_RECONNECT_TIMEOUT_MS
                    && matches!(
                        attachment.identity_kind.as_str(),
                        "unix-socket" | "named-pipe" | "tcp"
                    )
            }
            "require-inherited-attachment" => {
                attachment.required
                    && attachment.reconnect_timeout_ms == 0
                    && attachment.identity_kind == "provider"
                    && attachment.identity == b"console"
            }
            "discard-while-disconnected" => {
                !attachment.required
                    && attachment.reconnect_timeout_ms == 0
                    && attachment.identity_kind == "disconnected"
                    && attachment.identity == b"discard"
            }
            _ => false,
        };
        anyhow::ensure!(
            attachment.stable_id == "console:microvm-virtio0"
                && matches!(
                    attachment.kind.as_str(),
                    "virtio-console" | "virtio-net" | "virtio-fs"
                )
                && policy_is_valid
                && !attachment.identity.is_empty()
                && attachment.identity.len() <= MAX_ATTACHMENT_IDENTITY_BYTES
                && attachment.length == 0,
            "microVM console attachment has an unsupported reconnect policy"
        );
        devices.push(SnapshotDevice {
            stable_id: "console:microvm-virtio0".to_owned(),
            state_unit_name: format!(
                "virtio-console-{}",
                openvmm_defs::config::MICROVM_VIRTIO_CONSOLE_MMIO_BASE
            ),
            kind: "virtio-console".to_owned(),
            order: devices.len() as u32,
            ranges: vec![mmio(
                openvmm_defs::config::MICROVM_VIRTIO_CONSOLE_MMIO_BASE,
                openvmm_defs::config::MICROVM_VIRTIO_MMIO_LEN,
            )],
            irq: Some(openvmm_defs::config::MICROVM_VIRTIO_CONSOLE_IRQ),
            transport: "virtio-mmio".to_owned(),
            feature_banks: vec![0x3000_0001, 0x0000_0003],
            queue_count: 2,
            queue_max_sizes: vec![256, 256],
        });
        attachments.push(attachment);
    }

    let mut contract = SnapshotMachineContract {
        machine_profile: "microvm".to_owned(),
        microvm_abi_version: openvmm_defs::config::MICROVM_ABI_VERSION_2,
        source_hypervisor: source_hypervisor.to_owned(),
        effective_command_line: String::new(),
        effective_command_line_sha256: Vec::new(),
        memory_ranges,
        topology,
        devices,
        state_unit_names,
        attachments,
        capture_wall_clock,
        tsc_frequency_hz,
        tsc_tolerance_ppm: 0,
        cpu_contract: Vec::new(),
        cpu_contract_sha256: Vec::new(),
        pvh_layout_version: MICROVM_PVH_LAYOUT_VERSION,
        clock_policy: ADVANCE_BY_HOST_DOWNTIME.to_owned(),
        microvm_network: None,
        microvm_filesystem: None,
        apic_frequency_hz,
        microvm_sandbox_blocks: Vec::new(),
        microvm_filesystem_slot_version: 0,
        boot_online_vp_count,
        virtio_interrupt_mode: MICROVM_SHARED_STATUS_INTERRUPT_MODE.to_owned(),
        virtio_shared_status_page_gpa: openvmm_defs::config::MICROVM_SHARED_STATUS_PAGE_GPA,
        virtio_shared_status_page_size: openvmm_defs::config::MICROVM_SHARED_STATUS_PAGE_SIZE,
        memory_expansion_version: 0,
        memory_capacity_bytes: 0,
        memory_block_size_bytes: 0,
        memory_expansion_ranges: Vec::new(),
    };
    contract.set_effective_command_line(effective_command_line);
    contract.set_cpu_compatibility_contract(cpu_contract);
    validate_machine_contract_shape(&contract, memory_size, processor_count)?;
    Ok(contract)
}

/// Manifest describing a VM snapshot.
#[derive(Clone, Protobuf)]
#[mesh(package = "openvmm.snapshot")]
pub struct SnapshotManifest {
    /// Manifest format version.
    #[mesh(1)]
    pub version: u32,
    /// When the snapshot was created.
    #[mesh(2)]
    pub created_at: Timestamp,
    /// OpenVMM version that created the snapshot.
    #[mesh(3)]
    pub openvmm_version: String,
    /// Guest RAM size in bytes.
    #[mesh(4)]
    pub memory_size_bytes: u64,
    /// Number of virtual processors.
    #[mesh(5)]
    pub vp_count: u32,
    /// Page size in bytes.
    #[mesh(6)]
    pub page_size: u32,
    /// Architecture string ("x86_64" or "aarch64").
    #[mesh(7)]
    pub architecture: String,
    /// Length of `state.bin` in bytes.
    #[mesh(8)]
    pub state_size_bytes: u64,
    /// Legacy v2 SHA-256 digest of `state.bin`; empty in v3 through v5.
    #[mesh(9)]
    pub state_sha256: Vec<u8>,
    /// Legacy v2 SHA-256 digest of `memory.bin`; empty in v3 through v5.
    #[mesh(10)]
    pub memory_sha256: Vec<u8>,
    /// Authoritative machine composition for versioned machine profiles.
    #[mesh(11)]
    pub machine_contract: Option<SnapshotMachineContract>,
    /// Snapshot format magic.
    #[mesh(12)]
    pub format_magic: Vec<u8>,
    /// Version of the serialized VM saved-state schema.
    #[mesh(13)]
    pub saved_state_schema_version: u32,
    /// Fully qualified protobuf root type stored in `state.bin`.
    #[mesh(14)]
    pub saved_state_root_type: String,
    /// Sandbox capture tier. Empty for blockless microVM snapshots.
    #[mesh(15)]
    pub snapshot_tier: String,
    /// `clone` for reusable artifacts or `resume` for single-use artifacts.
    #[mesh(16)]
    pub restore_policy: String,
    /// Bitmask of configuration sections consumed before capture.
    #[mesh(17)]
    pub consumed_config_sections: u32,
}

/// One structurally validated snapshot generation opened for restore.
///
/// Artifact access is relative to the retained directory handle. The open
/// artifact handles keep pathname replacement from substituting another
/// generation after validation.
pub struct OpenedSnapshot {
    directory: OpenedSnapshotDirectory,
    manifest_file: std::fs::File,
    state_file: std::fs::File,
    memory_file: std::fs::File,
    memory_generation: OpenedFileGeneration,
    manifest: SnapshotManifest,
    state_bytes: Vec<u8>,
}

/// Write a snapshot to the given directory.
///
/// The final directory must not already exist. The snapshot consists of:
/// - `manifest.bin` — protobuf-encoded [`SnapshotManifest`]
/// - `state.bin` — raw device saved-state bytes
/// - `memory.bin` — private copy of the memory backing file
///
/// The files are written and flushed in a unique sibling staging directory.
/// The completed staging directory is then renamed to `dir` in one operation.
pub fn write_snapshot(
    dir: &Path,
    manifest: &SnapshotManifest,
    saved_state_bytes: &[u8],
    memory_file_path: &Path,
) -> Result<(), SnapshotWriteError> {
    let memory_file = open_regular_file(memory_file_path, "memory backing file")?;
    write_snapshot_from_memory_file(dir, manifest, saved_state_bytes, &memory_file)
}

/// Writes a snapshot from the exact open file handle backing guest RAM.
///
/// Capture callers should prefer this over [`write_snapshot`] so replacing the
/// backing pathname cannot substitute different bytes after VM construction.
pub fn write_snapshot_from_memory_file(
    dir: &Path,
    manifest: &SnapshotManifest,
    saved_state_bytes: &[u8],
    memory_file: &std::fs::File,
) -> Result<(), SnapshotWriteError> {
    write_snapshot_from_memory_and_scratch_files(
        dir,
        manifest,
        saved_state_bytes,
        memory_file,
        None,
    )
}

/// Writes a snapshot with an optional scratch image paired to the VM state.
pub fn write_snapshot_from_memory_and_scratch_files(
    dir: &Path,
    manifest: &SnapshotManifest,
    saved_state_bytes: &[u8],
    memory_file: &std::fs::File,
    scratch_file: Option<&std::fs::File>,
) -> Result<(), SnapshotWriteError> {
    write_snapshot_with_memory_publication(
        dir,
        manifest,
        saved_state_bytes,
        memory_file,
        scratch_file,
        MemoryPublication::IndependentCopy,
    )
}

/// Writes a snapshot by promoting OpenVMM-owned guest RAM as an exact file.
///
/// The caller must use this only for automatic backing whose lifetime it owns,
/// and must stop all writers before calling. It must terminate the source after
/// success or [`SnapshotWriteError::Committed`]. A
/// [`SnapshotWriteError::BeforeCommit`] proves that staging was removed and the
/// source may resume. [`SnapshotWriteError::CleanupUncertain`] requires source
/// termination because a private staging alias may remain. Unsupported hard
/// links fall back to an independent sparse-aware copy.
pub fn write_snapshot_from_owned_memory_and_scratch_files(
    dir: &Path,
    manifest: &SnapshotManifest,
    saved_state_bytes: &[u8],
    memory_file: &std::fs::File,
    scratch_file: Option<&std::fs::File>,
) -> Result<(), SnapshotWriteError> {
    write_snapshot_with_memory_publication(
        dir,
        manifest,
        saved_state_bytes,
        memory_file,
        scratch_file,
        MemoryPublication::OwnedExactFile,
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MemoryPublication {
    IndependentCopy,
    OwnedExactFile,
}

fn write_snapshot_with_memory_publication(
    dir: &Path,
    manifest: &SnapshotManifest,
    saved_state_bytes: &[u8],
    memory_file: &std::fs::File,
    scratch_file: Option<&std::fs::File>,
    memory_publication: MemoryPublication,
) -> Result<(), SnapshotWriteError> {
    let mut staging = stage_snapshot(
        dir,
        manifest,
        saved_state_bytes,
        memory_file,
        scratch_file,
        memory_publication,
    )?;
    let commit = openvmm_defs::profile::ProfileSpan::start();
    if let Err(error) =
        ensure_path_absent(dir, "snapshot destination").and_then(|()| staging.publish(dir))
    {
        return Err(staging.rollback(error));
    }
    commit.complete("capture", "publication_commit", Default::default());

    let parent = snapshot_parent(dir);
    let parent_sync = openvmm_defs::profile::ProfileSpan::start();
    sync_directory(parent).map_err(|error| SnapshotWriteError::Committed {
        path: dir.to_owned(),
        error,
    })?;
    parent_sync.complete("capture", "publication_parent_sync", Default::default());
    Ok(())
}

fn stage_snapshot(
    dir: &Path,
    manifest: &SnapshotManifest,
    saved_state_bytes: &[u8],
    memory_file: &std::fs::File,
    scratch_file: Option<&std::fs::File>,
    memory_publication: MemoryPublication,
) -> Result<StagingDirectory, SnapshotWriteError> {
    validate_manifest_header(manifest)?;
    validate_manifest_version(manifest)?;
    if let Some(contract) = &manifest.machine_contract {
        validate_machine_contract_shape(contract, manifest.memory_size_bytes, manifest.vp_count)?;
    }
    if manifest.version != MANIFEST_VERSION {
        return Err(anyhow::anyhow!(
            "snapshot manifest version {} is not supported for writing (expected {})",
            manifest.version,
            MANIFEST_VERSION,
        )
        .into());
    }
    if u64::try_from(saved_state_bytes.len()).unwrap_or(u64::MAX) > MAX_SAVED_STATE_SIZE_BYTES {
        return Err(anyhow::anyhow!(
            "saved state exceeds the maximum size of {MAX_SAVED_STATE_SIZE_BYTES} bytes"
        )
        .into());
    }

    let parent = snapshot_parent(dir);
    validate_directory(parent, "snapshot parent directory")?;
    ensure_path_absent(dir, "snapshot destination")?;

    let mut staging = StagingDirectory::create(parent, dir)?;
    let state_path = staging.path().join(STATE_FILE_NAME);
    let memory_path = staging.path().join(MEMORY_FILE_NAME);
    let manifest_path = staging.path().join(MANIFEST_FILE_NAME);

    let result = (|| -> anyhow::Result<()> {
        let state_write = openvmm_defs::profile::ProfileSpan::start();
        write_bytes(&state_path, saved_state_bytes, "saved state")?;
        state_write.complete(
            "capture",
            "publication_state",
            profile_path_counters(&state_path, saved_state_bytes.len() as u64),
        );
        if memory_publication == MemoryPublication::IndependentCopy {
            let memory_publish = openvmm_defs::profile::ProfileSpan::start();
            copy_exact(
                memory_file,
                &memory_path,
                manifest.memory_size_bytes,
                "memory backing file",
                "snapshot memory",
            )?;
            memory_publish.complete(
                "capture",
                "publication_memory",
                profile_path_counters(&memory_path, manifest.memory_size_bytes),
            );
        }
        match (paired_scratch_block(manifest), scratch_file) {
            (Some(scratch), Some(scratch_file)) => {
                let scratch_path = staging.path().join(SCRATCH_FILE_NAME);
                let scratch_publish = openvmm_defs::profile::ProfileSpan::start();
                copy_exact(
                    scratch_file,
                    &scratch_path,
                    scratch.length,
                    "scratch backing file",
                    "snapshot scratch",
                )?;
                verify_file_digest(
                    &open_file_with_length(&scratch_path, scratch.length, SCRATCH_FILE_NAME)?,
                    scratch.length,
                    &scratch.identity,
                    "scratch.img",
                )?;
                scratch_publish.complete(
                    "capture",
                    "publication_scratch",
                    profile_path_counters(&scratch_path, scratch.length),
                );
            }
            (Some(_), None) => anyhow::bail!("snapshot contract requires a paired scratch image"),
            (None, Some(_)) => {
                anyhow::bail!("snapshot contract does not declare a scratch image")
            }
            (None, None) => {}
        }

        let mut published_manifest = manifest.clone();
        published_manifest.state_size_bytes = saved_state_bytes.len() as u64;
        // Current local snapshots use strict structure, length, generation,
        // and machine-contract checks without RAM-sized in-band hashing.
        published_manifest.state_sha256.clear();
        published_manifest.memory_sha256.clear();

        let manifest_bytes = mesh::payload::encode(published_manifest);
        anyhow::ensure!(
            manifest_bytes.len() as u64 <= MAX_MANIFEST_SIZE_BYTES,
            "snapshot manifest exceeds the maximum size of {MAX_MANIFEST_SIZE_BYTES} bytes",
        );
        let manifest_write = openvmm_defs::profile::ProfileSpan::start();
        write_bytes(&manifest_path, &manifest_bytes, "snapshot manifest")?;
        manifest_write.complete(
            "capture",
            "publication_manifest",
            profile_path_counters(&manifest_path, manifest_bytes.len() as u64),
        );

        if memory_publication == MemoryPublication::OwnedExactFile {
            let memory_publish = openvmm_defs::profile::ProfileSpan::start();
            publish_owned_memory_file(&mut staging, memory_file, manifest.memory_size_bytes)?;
            memory_publish.complete(
                "capture",
                "publication_memory",
                profile_path_counters(&memory_path, manifest.memory_size_bytes),
            );
        }

        let staging_sync = openvmm_defs::profile::ProfileSpan::start();
        sync_directory(staging.path())?;
        staging_sync.complete("capture", "publication_staging_sync", Default::default());
        Ok(())
    })();
    match result {
        Ok(()) => Ok(staging),
        Err(error) => Err(staging.rollback(error)),
    }
}

impl OpenedSnapshot {
    /// Opens and structurally validates one exact snapshot generation.
    pub fn open(dir: &Path) -> anyhow::Result<Self> {
        let directory = OpenedSnapshotDirectory::open(dir)?;
        let manifest_file = directory.open_regular_file(MANIFEST_FILE_NAME, "snapshot manifest")?;
        let state_file = directory.open_regular_file(STATE_FILE_NAME, "saved state")?;
        let memory_file = directory.open_regular_file(MEMORY_FILE_NAME, "snapshot memory")?;
        let manifest = decode_snapshot_manifest(&manifest_file)?;
        validate_snapshot_directory(&directory, &manifest)?;
        anyhow::ensure!(
            manifest.state_size_bytes <= MAX_SAVED_STATE_SIZE_BYTES,
            "state.bin length in the manifest exceeds the maximum size of \
             {MAX_SAVED_STATE_SIZE_BYTES} bytes",
        );

        let state_bytes =
            read_bounded_open_file(&state_file, MAX_SAVED_STATE_SIZE_BYTES, "saved state")?;
        anyhow::ensure!(
            state_bytes.len() as u64 == manifest.state_size_bytes,
            "state.bin size ({} bytes) doesn't match manifest ({} bytes)",
            state_bytes.len(),
            manifest.state_size_bytes,
        );

        let memory_generation = opened_file_generation(&memory_file, MEMORY_FILE_NAME)?;
        anyhow::ensure!(
            memory_generation.length() == manifest.memory_size_bytes,
            "memory.bin size ({} bytes) doesn't match manifest ({} bytes)",
            memory_generation.length(),
            manifest.memory_size_bytes,
        );

        Ok(Self {
            directory,
            manifest_file,
            state_file,
            memory_file,
            memory_generation,
            manifest,
            state_bytes,
        })
    }

    /// Returns the authoritative manifest read from this opened generation.
    pub fn manifest(&self) -> &SnapshotManifest {
        &self.manifest
    }

    /// Returns the saved-state bytes read from this opened generation.
    pub fn state_bytes(&self) -> &[u8] {
        &self.state_bytes
    }

    /// Returns total logical and allocated bytes for the opened artifacts.
    ///
    /// This is intended for opt-in profiling. Restore validation does not
    /// depend on allocation accounting being available.
    pub fn artifact_size_counters(&self) -> anyhow::Result<(u64, u64)> {
        [&self.manifest_file, &self.state_file, &self.memory_file]
            .into_iter()
            .try_fold((0_u64, 0_u64), |(logical, allocated), file| {
                let length = file.metadata()?.len();
                Ok((
                    logical
                        .checked_add(length)
                        .context("logical byte count overflow")?,
                    allocated
                        .checked_add(allocated_file_bytes(file, length)?)
                        .context("allocated byte count overflow")?,
                ))
            })
    }

    /// Opens the paired scratch artifact relative to this snapshot generation.
    pub fn open_paired_scratch_file(&self) -> anyhow::Result<Option<std::fs::File>> {
        open_paired_scratch_file_in_directory(&self.directory, &self.manifest)
    }

    /// Claims this exact opened generation for a single-use resume.
    pub fn claim_for_restore(&self) -> anyhow::Result<()> {
        claim_snapshot_for_restore_in_directory(&self.directory, &self.manifest)
    }

    /// Duplicates the exact memory handle used to create a private mapping.
    pub fn duplicate_memory_file_for_mapping(
        &self,
        expected_memory_size: u64,
    ) -> anyhow::Result<std::fs::File> {
        self.validate_memory_generation(expected_memory_size)?;
        let duplicate = self
            .memory_file
            .try_clone()
            .context("failed to duplicate snapshot memory handle")?;
        anyhow::ensure!(
            opened_file_generation(&duplicate, MEMORY_FILE_NAME)? == self.memory_generation,
            "snapshot memory duplicate does not refer to the opened generation",
        );
        Ok(duplicate)
    }

    /// Verifies that the opened memory generation and EOF are unchanged.
    pub fn validate_memory_generation(&self, expected_memory_size: u64) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.manifest.memory_size_bytes == expected_memory_size,
            "memory.bin size in the manifest ({} bytes) doesn't match expected ({expected_memory_size} bytes)",
            self.manifest.memory_size_bytes,
        );
        anyhow::ensure!(
            opened_file_generation(&self.memory_file, MEMORY_FILE_NAME)? == self.memory_generation,
            "snapshot memory generation changed after it was opened",
        );
        Ok(())
    }

    /// Consumes this snapshot into decoded data and lifetime guard handles.
    pub fn into_parts(
        self,
    ) -> (
        SnapshotManifest,
        Vec<u8>,
        openvmm_defs::worker::SnapshotRestoreGuards,
    ) {
        (
            self.manifest,
            self.state_bytes,
            openvmm_defs::worker::SnapshotRestoreGuards {
                directory: self.directory.into_file(),
                manifest: self.manifest_file,
                state: self.state_file,
                memory: self.memory_file,
            },
        )
    }
}

fn read_snapshot_manifest_from_directory(
    directory: &OpenedSnapshotDirectory,
) -> anyhow::Result<(SnapshotManifest, std::fs::File)> {
    let manifest_file = directory.open_regular_file(MANIFEST_FILE_NAME, "snapshot manifest")?;
    let manifest = decode_snapshot_manifest(&manifest_file)?;
    Ok((manifest, manifest_file))
}

fn decode_snapshot_manifest(manifest_file: &std::fs::File) -> anyhow::Result<SnapshotManifest> {
    let manifest_bytes =
        read_bounded_open_file(manifest_file, MAX_MANIFEST_SIZE_BYTES, "snapshot manifest")?;
    let manifest: SnapshotManifest =
        mesh::payload::decode(&manifest_bytes).context("failed to decode snapshot manifest")?;
    validate_manifest_header(&manifest)?;
    validate_manifest_version(&manifest)?;
    if let Some(contract) = &manifest.machine_contract {
        validate_machine_contract_shape(contract, manifest.memory_size_bytes, manifest.vp_count)?;
    }
    Ok(manifest)
}

/// Read a snapshot from the given directory.
///
/// Returns the decoded manifest and raw saved-state bytes after structurally
/// validating all three artifacts. `expected_memory_size` bounds memory.
pub fn read_snapshot(
    dir: &Path,
    expected_memory_size: u64,
) -> anyhow::Result<(SnapshotManifest, Vec<u8>)> {
    let snapshot = OpenedSnapshot::open(dir)?;
    snapshot.validate_memory_generation(expected_memory_size)?;
    let (manifest, state_bytes, _) = snapshot.into_parts();
    Ok((manifest, state_bytes))
}

/// Reads and structurally validates only `manifest.bin`.
///
/// Production restore should use [`OpenedSnapshot`] so later artifact access
/// remains anchored to the same opened directory generation.
pub fn read_snapshot_manifest(dir: &Path) -> anyhow::Result<SnapshotManifest> {
    let directory = OpenedSnapshotDirectory::open(dir)?;
    let (manifest, _) = read_snapshot_manifest_from_directory(&directory)?;
    validate_snapshot_directory(&directory, &manifest)?;
    Ok(manifest)
}

/// Read and structurally validate a snapshot, returning its exact memory handle.
///
/// The returned file is positioned at offset zero and must be used directly
/// for restore so a path replacement cannot substitute different memory.
pub fn read_snapshot_with_memory(
    dir: &Path,
    expected_memory_size: u64,
) -> anyhow::Result<(SnapshotManifest, Vec<u8>, std::fs::File)> {
    let snapshot = OpenedSnapshot::open(dir)?;
    snapshot.validate_memory_generation(expected_memory_size)?;
    let (manifest, state_bytes, guards) = snapshot.into_parts();
    let openvmm_defs::worker::SnapshotRestoreGuards { memory, .. } = guards;
    Ok((manifest, state_bytes, memory))
}

/// Opens snapshot artifacts against an already validated manifest.
///
/// This compatibility helper reopens the directory. Production restore should
/// keep an [`OpenedSnapshot`] alive through worker construction instead.
pub fn read_snapshot_artifacts_with_memory(
    dir: &Path,
    manifest: &SnapshotManifest,
    expected_memory_size: u64,
) -> anyhow::Result<(Vec<u8>, std::fs::File)> {
    let directory = OpenedSnapshotDirectory::open(dir)?;
    validate_manifest_header(manifest)?;
    validate_manifest_version(manifest)?;
    if let Some(contract) = &manifest.machine_contract {
        validate_machine_contract_shape(contract, manifest.memory_size_bytes, manifest.vp_count)?;
    }
    validate_snapshot_directory(&directory, manifest)?;
    anyhow::ensure!(
        manifest.state_size_bytes <= MAX_SAVED_STATE_SIZE_BYTES,
        "state.bin length in the manifest exceeds the maximum size of \
         {MAX_SAVED_STATE_SIZE_BYTES} bytes",
    );

    let state_file = directory.open_regular_file(STATE_FILE_NAME, "saved state")?;
    let state_bytes =
        read_bounded_open_file(&state_file, MAX_SAVED_STATE_SIZE_BYTES, "saved state")?;
    anyhow::ensure!(
        state_bytes.len() as u64 == manifest.state_size_bytes,
        "state.bin size ({} bytes) doesn't match manifest ({} bytes)",
        state_bytes.len(),
        manifest.state_size_bytes,
    );
    anyhow::ensure!(
        manifest.memory_size_bytes == expected_memory_size,
        "memory.bin size in the manifest ({} bytes) doesn't match expected ({expected_memory_size} bytes)",
        manifest.memory_size_bytes,
    );
    let memory_file = directory.open_file_with_length(
        MEMORY_FILE_NAME,
        expected_memory_size,
        MEMORY_FILE_NAME,
    )?;

    Ok((state_bytes, memory_file))
}

/// Opens and verifies the scratch image paired to a snapshot, if present.
pub fn open_paired_scratch_file(
    dir: &Path,
    manifest: &SnapshotManifest,
) -> anyhow::Result<Option<std::fs::File>> {
    let directory = OpenedSnapshotDirectory::open(dir)?;
    open_paired_scratch_file_in_directory(&directory, manifest)
}

fn open_paired_scratch_file_in_directory(
    directory: &OpenedSnapshotDirectory,
    manifest: &SnapshotManifest,
) -> anyhow::Result<Option<std::fs::File>> {
    let Some(scratch) = paired_scratch_block(manifest) else {
        return Ok(None);
    };
    let file =
        directory.open_file_with_length(SCRATCH_FILE_NAME, scratch.length, SCRATCH_FILE_NAME)?;
    verify_file_digest(&file, scratch.length, &scratch.identity, SCRATCH_FILE_NAME)?;
    Ok(Some(file))
}

/// Copies a verified immutable artifact to a new private path.
pub fn copy_verified_file(
    source: &std::fs::File,
    destination: &Path,
    expected_length: u64,
    expected_digest: &[u8],
    description: &str,
) -> anyhow::Result<()> {
    verify_file_digest(source, expected_length, expected_digest, description)?;
    copy_exact(
        source,
        destination,
        expected_length,
        description,
        "private scratch copy",
    )?;
    let copy = open_file_with_length(destination, expected_length, "private scratch copy")?;
    verify_file_digest(
        &copy,
        expected_length,
        expected_digest,
        "private scratch copy",
    )
}

/// Atomically consumes a single-use resume snapshot.
///
/// Call this after artifact and configuration validation and before constructing
/// execution-owned workers. A later startup failure does not roll back the claim.
/// Clone snapshots are unchanged.
pub fn claim_snapshot_for_restore(dir: &Path, manifest: &SnapshotManifest) -> anyhow::Result<()> {
    let directory = OpenedSnapshotDirectory::open(dir)?;
    claim_snapshot_for_restore_in_directory(&directory, manifest)
}

fn claim_snapshot_for_restore_in_directory(
    directory: &OpenedSnapshotDirectory,
    manifest: &SnapshotManifest,
) -> anyhow::Result<()> {
    validate_manifest_header(manifest)?;
    validate_manifest_version(manifest)?;
    if manifest.restore_policy != SNAPSHOT_RESTORE_POLICY_RESUME {
        return Ok(());
    }

    let mut claim = match directory.create_new_file(RESUME_CLAIM_FILE_NAME) {
        Ok(claim) => claim,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            anyhow::bail!("resume snapshot has already been claimed")
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "failed to claim resume snapshot at {}",
                    directory.display_path(RESUME_CLAIM_FILE_NAME).display()
                )
            });
        }
    };
    claim
        .write_all(b"OPENVMM_RESUME_CLAIM_V1\n")
        .context("failed to write resume snapshot claim")?;
    claim
        .sync_all()
        .context("failed to flush resume snapshot claim")?;
    directory
        .sync()
        .context("failed to commit resume snapshot claim")?;
    Ok(())
}

/// Returns whether restore must hold external device input until guest repair completes.
pub fn requires_post_restore_gate(manifest: &SnapshotManifest) -> bool {
    manifest.version == MANIFEST_VERSION
        && manifest.machine_contract.as_ref().is_some_and(|contract| {
            matches!(
                contract.microvm_abi_version,
                openvmm_defs::config::MICROVM_ABI_VERSION_2
            )
        })
        && !manifest.snapshot_tier.is_empty()
}

fn paired_scratch_block(manifest: &SnapshotManifest) -> Option<&SnapshotMicrovmSandboxBlock> {
    manifest
        .machine_contract
        .as_ref()?
        .microvm_sandbox_blocks
        .iter()
        .find(|block| block.artifact == SCRATCH_FILE_NAME)
}

fn snapshot_parent(dir: &Path) -> &Path {
    match dir.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

fn path_exists(path: &Path) -> anyhow::Result<bool> {
    match fs_err::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err).with_context(|| format!("failed to inspect {}", path.display())),
    }
}

fn ensure_path_absent(path: &Path, description: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !path_exists(path)?,
        "{description} already exists: {}",
        path.display(),
    );
    Ok(())
}

fn validate_directory(path: &Path, description: &str) -> anyhow::Result<()> {
    let metadata = fs_err::symlink_metadata(path)
        .with_context(|| format!("failed to inspect {description} {}", path.display()))?;
    anyhow::ensure!(
        metadata.file_type().is_dir(),
        "{description} is not a directory: {}",
        path.display(),
    );
    Ok(())
}

struct StagingDirectory {
    path: Option<PathBuf>,
    source_memory_alias: bool,
    #[cfg(test)]
    inject_cleanup_failure: bool,
}

impl StagingDirectory {
    fn create(parent: &Path, destination: &Path) -> anyhow::Result<Self> {
        let destination_name = destination
            .file_name()
            .context("snapshot destination must name a directory")?
            .to_string_lossy();
        let sequence = STAGING_SEQUENCE.fetch_add(1, Ordering::Relaxed);

        for attempt in 0..100_u32 {
            let path = parent.join(format!(
                ".{destination_name}.staging-{}-{sequence}-{attempt}",
                std::process::id()
            ));
            match create_private_directory(&path) {
                Ok(()) => {
                    return Ok(Self {
                        path: Some(path),
                        source_memory_alias: false,
                        #[cfg(test)]
                        inject_cleanup_failure: false,
                    });
                }
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(err) => {
                    return Err(err).with_context(|| {
                        format!("failed to create staging directory {}", path.display())
                    });
                }
            }
        }

        anyhow::bail!(
            "failed to allocate a unique snapshot staging directory in {}",
            parent.display()
        )
    }

    fn path(&self) -> &Path {
        self.path.as_deref().expect("staging path is present")
    }

    fn publish(&mut self, destination: &Path) -> anyhow::Result<()> {
        let staging_path = self.path();
        rename_no_replace(staging_path, destination).with_context(|| {
            format!(
                "failed to publish snapshot {} to {}",
                staging_path.display(),
                destination.display()
            )
        })?;
        self.path = None;
        Ok(())
    }

    fn mark_source_memory_alias(&mut self) {
        self.source_memory_alias = true;
    }

    fn rollback(mut self, error: anyhow::Error) -> SnapshotWriteError {
        if self.source_memory_alias
            && let Err(cleanup_error) = self.remove_staging_directory()
        {
            return SnapshotWriteError::CleanupUncertain {
                path: self
                    .path
                    .clone()
                    .expect("unpublished staging path is present"),
                error,
                cleanup_error,
            };
        }

        if let Err(cleanup_error) = self.remove_staging_directory() {
            tracing::warn!(
                error = cleanup_error.as_ref() as &dyn std::error::Error,
                "failed to remove independent snapshot staging artifacts"
            );
        }
        SnapshotWriteError::BeforeCommit(error)
    }

    fn remove_staging_directory(&mut self) -> anyhow::Result<()> {
        #[cfg(test)]
        if self.inject_cleanup_failure {
            anyhow::bail!("injected snapshot staging cleanup failure");
        }
        let Some(path) = self.path.as_ref() else {
            return Ok(());
        };
        let parent = snapshot_parent(path).to_owned();
        match fs_err::remove_dir_all(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to remove staging directory {}", path.display())
                });
            }
        }
        anyhow::ensure!(
            !path_exists(path)?,
            "snapshot staging directory still exists after removal: {}",
            path.display()
        );
        sync_directory(&parent).context("failed to flush snapshot staging cleanup")?;
        self.path = None;
        self.source_memory_alias = false;
        Ok(())
    }
}

fn create_private_directory(path: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        let descriptor: pal::windows::security::LocalSecurityDescriptor =
            "D:P(A;;FA;;;SY)(A;;FA;;;OW)".parse()?;
        pal::windows::security::create_directory_with_security(path, &descriptor)
    }
    #[cfg(not(windows))]
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    #[cfg(not(windows))]
    return builder.create(path);
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn rename_no_replace(source: &Path, destination: &Path) -> anyhow::Result<()> {
    let source_parent = snapshot_parent(source);
    let destination_parent = snapshot_parent(destination);
    anyhow::ensure!(
        source_parent == destination_parent,
        "snapshot staging and destination directories have different parents",
    );
    let source_name = source
        .file_name()
        .context("snapshot staging path must name a directory")?;
    let destination_name = destination
        .file_name()
        .context("snapshot destination must name a directory")?;
    let parent = std::fs::File::open(destination_parent).with_context(|| {
        format!(
            "failed to open snapshot parent directory {}",
            destination_parent.display()
        )
    })?;
    nix::fcntl::renameat2(
        &parent,
        source_name,
        &parent,
        destination_name,
        nix::fcntl::RenameFlags::RENAME_NOREPLACE,
    )?;
    Ok(())
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
fn rename_no_replace(source: &Path, destination: &Path) -> anyhow::Result<()> {
    fs_err::rename(source, destination)?;
    Ok(())
}

impl Drop for StagingDirectory {
    fn drop(&mut self) {
        if let Some(path) = &self.path {
            if let Err(error) = fs_err::remove_dir_all(path) {
                tracing::error!(
                    error = &error as &dyn std::error::Error,
                    source_memory_alias = self.source_memory_alias,
                    path = %path.display(),
                    "failed to remove dropped snapshot staging directory"
                );
            }
        }
    }
}

fn create_file(path: &Path, description: &str) -> anyhow::Result<std::fs::File> {
    #[cfg(windows)]
    {
        let descriptor: pal::windows::security::LocalSecurityDescriptor =
            "D:P(A;;FA;;;SY)(A;;FA;;;OW)".parse()?;
        pal::windows::security::create_file_with_security(path, &descriptor)
            .with_context(|| format!("failed to create {description} at {}", path.display()))
    }
    #[cfg(not(windows))]
    let mut options = fs_err::OpenOptions::new();
    #[cfg(not(windows))]
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use fs_err::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    #[cfg(not(windows))]
    return options
        .open(path)
        .map(Into::into)
        .with_context(|| format!("failed to create {description} at {}", path.display()));
}

fn write_bytes(path: &Path, bytes: &[u8], description: &str) -> anyhow::Result<()> {
    let mut file = create_file(path, description)?;
    file.write_all(bytes)
        .with_context(|| format!("failed to write {description}"))?;
    file.sync_all()
        .with_context(|| format!("failed to flush {description}"))?;
    Ok(())
}

fn profile_path_counters(
    path: &Path,
    logical_bytes: u64,
) -> openvmm_defs::profile::ProfileCounters {
    if !openvmm_defs::profile::enabled() {
        return Default::default();
    }
    let allocated_bytes = std::fs::File::open(path)
        .ok()
        .and_then(|file| allocated_file_bytes(&file, logical_bytes).ok());
    openvmm_defs::profile::ProfileCounters {
        logical_bytes: Some(logical_bytes),
        allocated_bytes,
        ..Default::default()
    }
}

fn publish_owned_memory_file(
    staging: &mut StagingDirectory,
    source: &std::fs::File,
    expected_length: u64,
) -> anyhow::Result<()> {
    let source_metadata = source
        .metadata()
        .context("failed to inspect automatic snapshot RAM handle")?;
    anyhow::ensure!(
        source_metadata.file_type().is_file(),
        "automatic snapshot RAM handle is not a regular file"
    );
    anyhow::ensure!(
        source_metadata.len() == expected_length,
        "automatic snapshot RAM size ({} bytes) doesn't match manifest ({expected_length} bytes)",
        source_metadata.len()
    );

    // The VM worker has stopped all writers before this call. Flush through
    // the exact handle immediately before linking the same file generation.
    source
        .sync_all()
        .context("failed to flush automatic snapshot RAM handle")?;

    let memory_path = staging.path().join(MEMORY_FILE_NAME);
    let directory = OpenedSnapshotDirectory::open_for_publication(staging.path())?;
    // From this point, any failure is treated as though the link may have been
    // installed until the complete staging directory is proven absent.
    staging.mark_source_memory_alias();
    let method = match create_hard_link_from_handle(source, &directory.file, MEMORY_FILE_NAME) {
        Ok(method) => method,
        Err(error) if hard_link_is_unsupported(&error) => {
            tracing::info!(
                error = &error as &dyn std::error::Error,
                "exact-file snapshot RAM publication is unavailable; using independent copy"
            );
            drop(directory);
            return copy_exact(
                source,
                &memory_path,
                expected_length,
                "automatic snapshot RAM handle",
                "snapshot memory",
            );
        }
        Err(error) => {
            return Err(error).context("failed to create automatic RAM staging hard link");
        }
    };
    let linked =
        directory.open_regular_file_for_identity(MEMORY_FILE_NAME, "linked snapshot memory")?;
    verify_hard_link_identity(source, &linked, expected_length)?;
    tracing::info!(
        method,
        logical_bytes = expected_length,
        "published exact snapshot memory artifact"
    );
    Ok(())
}

#[cfg(target_os = "linux")]
fn create_hard_link_from_handle(
    source: &std::fs::File,
    directory: &std::fs::File,
    name: &str,
) -> std::io::Result<&'static str> {
    use nix::fcntl::AT_FDCWD;
    use nix::fcntl::AtFlags;
    use std::os::fd::AsRawFd;

    match nix::unistd::linkat(
        source,
        Path::new(""),
        directory,
        name,
        AtFlags::AT_EMPTY_PATH,
    ) {
        Ok(()) => return Ok("linkat-empty-path"),
        Err(error)
            if matches!(
                error,
                nix::errno::Errno::EPERM | nix::errno::Errno::EINVAL | nix::errno::Errno::ENOENT
            ) =>
        {
            tracing::debug!(
                error = &nix_error(error) as &dyn std::error::Error,
                "AT_EMPTY_PATH hard link is unavailable"
            );
        }
        Err(error) => return Err(nix_error(error)),
    }

    let source_path = PathBuf::from(format!("/proc/self/fd/{}", source.as_raw_fd()));
    // Following is required to link the descriptor's target rather than the
    // procfs symlink itself. The descriptor remains live, and the caller proves
    // the resulting device/inode against `source` before publication.
    nix::unistd::linkat(
        AT_FDCWD,
        &source_path,
        directory,
        name,
        AtFlags::AT_SYMLINK_FOLLOW,
    )
    .map(|()| "linkat-proc-fd")
    .map_err(nix_error)
}

#[cfg(windows)]
fn create_hard_link_from_handle(
    source: &std::fs::File,
    directory: &std::fs::File,
    name: &str,
) -> std::io::Result<&'static str> {
    pal::windows::fs::hard_link_relative(source, directory, std::ffi::OsStr::new(name))?;
    Ok("file-link-information")
}

#[cfg(not(any(target_os = "linux", windows)))]
fn create_hard_link_from_handle(
    _source: &std::fs::File,
    _directory: &std::fs::File,
    _name: &str,
) -> std::io::Result<&'static str> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "exact-file hard links are unsupported on this platform",
    ))
}

#[cfg(target_os = "linux")]
fn hard_link_is_unsupported(error: &std::io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(
            libc::EACCES
                | libc::EMLINK
                | libc::EINVAL
                | libc::ELOOP
                | libc::ENOENT
                | libc::ENOSYS
                | libc::EOPNOTSUPP
                | libc::EPERM
                | libc::EXDEV
        )
    )
}

#[cfg(windows)]
fn hard_link_is_unsupported(error: &std::io::Error) -> bool {
    use windows_sys::Win32::Foundation::ERROR_ACCESS_DENIED;
    use windows_sys::Win32::Foundation::ERROR_FILE_SYSTEM_LIMITATION;
    use windows_sys::Win32::Foundation::ERROR_INVALID_FUNCTION;
    use windows_sys::Win32::Foundation::ERROR_INVALID_PARAMETER;
    use windows_sys::Win32::Foundation::ERROR_NOT_SAME_DEVICE;
    use windows_sys::Win32::Foundation::ERROR_NOT_SUPPORTED;
    use windows_sys::Win32::Foundation::ERROR_PRIVILEGE_NOT_HELD;
    use windows_sys::Win32::Foundation::ERROR_TOO_MANY_LINKS;

    matches!(
        error.raw_os_error(),
        Some(raw) if matches!(
            raw as u32,
            ERROR_ACCESS_DENIED
                | ERROR_FILE_SYSTEM_LIMITATION
                | ERROR_INVALID_FUNCTION
                | ERROR_INVALID_PARAMETER
                | ERROR_NOT_SAME_DEVICE
                | ERROR_NOT_SUPPORTED
                | ERROR_PRIVILEGE_NOT_HELD
                | ERROR_TOO_MANY_LINKS
        )
    )
}

#[cfg(not(any(target_os = "linux", windows)))]
fn hard_link_is_unsupported(_error: &std::io::Error) -> bool {
    true
}

fn verify_hard_link_identity(
    source: &std::fs::File,
    linked: &std::fs::File,
    expected_length: u64,
) -> anyhow::Result<()> {
    let source_metadata = source
        .metadata()
        .context("failed to re-inspect automatic snapshot RAM handle")?;
    let linked_metadata = linked
        .metadata()
        .context("failed to inspect linked snapshot memory")?;
    anyhow::ensure!(
        source_metadata.file_type().is_file() && linked_metadata.file_type().is_file(),
        "automatic snapshot RAM hard link does not resolve to regular files"
    );
    anyhow::ensure!(
        source_metadata.len() == expected_length && linked_metadata.len() == expected_length,
        "automatic snapshot RAM hard-link EOF does not match manifest ({expected_length} bytes)"
    );

    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::MetadataExt;
        anyhow::ensure!(
            source_metadata.dev() == linked_metadata.dev()
                && source_metadata.ino() == linked_metadata.ino(),
            "automatic snapshot RAM hard link has the wrong device or inode"
        );
    }
    #[cfg(windows)]
    {
        let source_identity = pal::windows::fs::file_identity(source)
            .context("failed to query automatic snapshot RAM identity")?;
        let linked_identity = pal::windows::fs::file_identity(linked)
            .context("failed to query linked snapshot RAM identity")?;
        anyhow::ensure!(
            source_identity == linked_identity && source_identity.end_of_file == expected_length,
            "automatic snapshot RAM hard link has the wrong file identity or EOF"
        );
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    anyhow::bail!("exact-file hard-link identity checks are unsupported on this platform");

    Ok(())
}

/// Sizes a newly created snapshot RAM backing.
///
/// Windows leaves the file non-sparse to avoid slow copy-on-write faults from
/// sparse files. Other supported platforms retain sparse allocation.
pub fn initialize_snapshot_memory_backing_file(
    file: &std::fs::File,
    size: u64,
) -> anyhow::Result<u64> {
    let metadata = file
        .metadata()
        .context("failed to inspect new snapshot memory backing")?;
    anyhow::ensure!(
        metadata.file_type().is_file(),
        "snapshot memory backing handle is not a regular file"
    );
    anyhow::ensure!(
        metadata.len() == 0,
        "new snapshot memory backing is not empty"
    );

    size_empty_file(file, size, "snapshot memory backing")?;
    let allocated_bytes = allocated_file_bytes(file, size)
        .context("failed to inspect snapshot memory backing allocation")?;
    #[cfg(not(windows))]
    anyhow::ensure!(
        allocated_bytes == 0,
        "snapshot memory backing allocated {allocated_bytes} bytes while sizing to {size} bytes"
    );
    tracing::info!(
        logical_bytes = size,
        allocated_bytes,
        "initialized snapshot RAM backing"
    );
    Ok(allocated_bytes)
}

fn copy_exact(
    source_file: &std::fs::File,
    destination_path: &Path,
    expected_length: u64,
    source_description: &str,
    destination_description: &str,
) -> anyhow::Result<()> {
    let source_metadata = source_file
        .metadata()
        .with_context(|| format!("failed to inspect {source_description}"))?;
    anyhow::ensure!(
        source_metadata.file_type().is_file(),
        "{source_description} handle is not a regular file"
    );
    let source_length = source_metadata.len();
    anyhow::ensure!(
        source_length == expected_length,
        "{source_description} size ({source_length} bytes) doesn't match manifest ({expected_length} bytes)",
    );
    let destination = create_file(destination_path, destination_description)?;
    let method = clone_or_copy(source_file, &destination, expected_length)
        .with_context(|| format!("failed to copy {source_description}"))?;

    anyhow::ensure!(
        source_file
            .metadata()
            .with_context(|| format!("failed to re-inspect {source_description}"))?
            .len()
            == expected_length,
        "{source_description} changed length while it was being copied",
    );
    anyhow::ensure!(
        destination
            .metadata()
            .with_context(|| format!("failed to inspect {destination_description}"))?
            .len()
            == expected_length,
        "{destination_description} length does not match {source_description}",
    );

    destination
        .sync_all()
        .with_context(|| format!("failed to flush {destination_description}"))?;
    let source_allocated_bytes = allocated_file_bytes(source_file, expected_length).ok();
    let allocated_bytes = allocated_file_bytes(&destination, expected_length).ok();
    tracing::info!(
        method,
        logical_bytes = expected_length,
        ?source_allocated_bytes,
        ?allocated_bytes,
        "published independent snapshot artifact"
    );
    Ok(())
}

fn size_empty_file(file: &std::fs::File, length: u64, description: &str) -> anyhow::Result<()> {
    file.set_len(0)
        .with_context(|| format!("failed to reset {description}"))?;
    file.set_len(length)
        .with_context(|| format!("failed to size {description}"))?;
    anyhow::ensure!(
        file.metadata()
            .with_context(|| format!("failed to inspect {description}"))?
            .len()
            == length,
        "{description} has the wrong logical length after sizing"
    );
    Ok(())
}

#[cfg(target_os = "linux")]
fn clone_or_copy(
    source: &std::fs::File,
    destination: &std::fs::File,
    length: u64,
) -> anyhow::Result<&'static str> {
    let clone_error = match pal::fs::reflink(source, destination) {
        Ok(()) => return Ok("ficlone"),
        Err(error) => error,
    };
    tracing::debug!(
        error = &clone_error as &dyn std::error::Error,
        "FICLONE unavailable"
    );
    size_empty_file(destination, length, "snapshot clone destination")?;
    match linux_allocated_ranges(source, length) {
        Ok(ranges) => {
            copy_allocated_ranges(source, destination, length, &ranges)?;
            Ok("seek-data-hole")
        }
        Err(error) => {
            tracing::debug!(
                error = &error as &dyn std::error::Error,
                "SEEK_DATA/SEEK_HOLE unavailable"
            );
            copy_nonzero_data(source, destination, 0, length)?;
            Ok("zero-scan")
        }
    }
}

#[cfg(windows)]
fn clone_or_copy(
    source: &std::fs::File,
    destination: &std::fs::File,
    length: u64,
) -> anyhow::Result<&'static str> {
    let mut source = source
        .try_clone()
        .context("failed to duplicate snapshot memory source handle")?;
    source
        .seek(SeekFrom::Start(0))
        .context("failed to rewind snapshot memory source")?;
    let mut destination = destination
        .try_clone()
        .context("failed to duplicate snapshot memory destination handle")?;
    destination
        .seek(SeekFrom::Start(0))
        .context("failed to rewind snapshot memory destination")?;

    let limit = length
        .checked_add(1)
        .context("snapshot memory length cannot be bounded")?;
    let mut source = Read::by_ref(&mut source).take(limit);
    let mut total = 0_u64;
    let mut buffer = vec![0_u8; COPY_BUFFER_SIZE];
    loop {
        let count = source
            .read(&mut buffer)
            .context("failed to read snapshot memory source")?;
        if count == 0 {
            break;
        }
        destination
            .write_all(&buffer[..count])
            .context("failed to write dense snapshot memory destination")?;
        total = total
            .checked_add(count as u64)
            .context("snapshot memory length overflowed u64")?;
    }
    anyhow::ensure!(
        total == length,
        "snapshot memory changed while it was copied (expected {length} bytes, copied {total} bytes)",
    );
    Ok("dense-copy")
}

#[cfg(not(any(target_os = "linux", windows)))]
fn clone_or_copy(
    source: &std::fs::File,
    destination: &std::fs::File,
    length: u64,
) -> anyhow::Result<&'static str> {
    size_empty_file(destination, length, "snapshot clone destination")?;
    copy_nonzero_data(source, destination, 0, length)?;
    Ok("zero-scan")
}

#[cfg(target_os = "linux")]
fn copy_allocated_ranges(
    source: &std::fs::File,
    destination: &std::fs::File,
    length: u64,
    ranges: &[(u64, u64)],
) -> anyhow::Result<()> {
    let mut cursor = 0_u64;
    for &(offset, range_length) in ranges {
        let end = offset
            .checked_add(range_length)
            .context("allocated range overflowed u64")?;
        anyhow::ensure!(
            offset >= cursor && range_length != 0 && end <= length,
            "invalid allocated range for sparse copy"
        );
        copy_nonzero_data(source, destination, offset, range_length)?;
        cursor = end;
    }
    Ok(())
}

#[cfg(not(windows))]
fn copy_nonzero_data(
    source: &std::fs::File,
    destination: &std::fs::File,
    offset: u64,
    length: u64,
) -> anyhow::Result<()> {
    let end = offset
        .checked_add(length)
        .context("snapshot copy range overflowed u64")?;
    let mut cursor = offset;
    let mut buffer = vec![0_u8; COPY_BUFFER_SIZE];
    while cursor < end {
        let count = usize::try_from((end - cursor).min(buffer.len() as u64)).unwrap();
        let read = read_file_at(source, &mut buffer[..count], cursor)
            .context("failed to read snapshot source")?;
        anyhow::ensure!(
            read != 0,
            "snapshot source changed while it was being copied"
        );
        if buffer[..read].iter().any(|byte| *byte != 0) {
            write_file_all_at(destination, &buffer[..read], cursor)
                .context("failed to write snapshot destination")?;
        }
        cursor = cursor
            .checked_add(read as u64)
            .context("snapshot copy offset overflowed u64")?;
    }
    Ok(())
}

#[cfg(not(windows))]
fn read_file_at(file: &std::fs::File, buffer: &mut [u8], offset: u64) -> std::io::Result<usize> {
    #[cfg(unix)]
    {
        std::os::unix::fs::FileExt::read_at(file, buffer, offset)
    }
    #[cfg(windows)]
    {
        std::os::windows::fs::FileExt::seek_read(file, buffer, offset)
    }
}

#[cfg(not(windows))]
fn write_file_all_at(
    file: &std::fs::File,
    mut bytes: &[u8],
    mut offset: u64,
) -> std::io::Result<()> {
    while !bytes.is_empty() {
        #[cfg(unix)]
        let written = std::os::unix::fs::FileExt::write_at(file, bytes, offset)?;
        #[cfg(windows)]
        let written = std::os::windows::fs::FileExt::seek_write(file, bytes, offset)?;
        if written == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "failed to write snapshot destination",
            ));
        }
        bytes = &bytes[written..];
        offset = offset
            .checked_add(written as u64)
            .ok_or_else(|| std::io::Error::other("snapshot write offset overflowed u64"))?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn linux_allocated_ranges(file: &std::fs::File, length: u64) -> std::io::Result<Vec<(u64, u64)>> {
    use std::os::fd::AsRawFd;

    let extent_file = std::fs::File::open(format!("/proc/self/fd/{}", file.as_raw_fd()))?;
    let mut ranges = Vec::new();
    let mut cursor = 0_u64;
    while cursor < length {
        let Some(data) = pal::fs::seek_data(&extent_file, cursor)? else {
            break;
        };
        if data >= length {
            break;
        }
        let hole = pal::fs::seek_hole(&extent_file, data)?.unwrap_or(length);
        if hole <= data || hole > length {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "filesystem returned an invalid sparse extent",
            ));
        }
        ranges.push((data, hole - data));
        cursor = hole;
    }
    Ok(ranges)
}

#[cfg(target_os = "linux")]
fn allocated_file_bytes(file: &std::fs::File, _length: u64) -> anyhow::Result<u64> {
    use std::os::unix::fs::MetadataExt;

    file.metadata()?
        .blocks()
        .checked_mul(512)
        .context("allocated byte count overflowed u64")
}

#[cfg(windows)]
fn allocated_file_bytes(file: &std::fs::File, length: u64) -> anyhow::Result<u64> {
    match pal::fs::allocated_ranges(file, length) {
        Ok(ranges) => ranges
            .into_iter()
            .try_fold(0_u64, |total, (_, range_length)| {
                total
                    .checked_add(range_length)
                    .context("allocated byte count overflowed u64")
            }),
        Err(error) => {
            tracing::debug!(
                error = &error as &dyn std::error::Error,
                "allocated-range accounting is unavailable"
            );
            pal::fs::allocation_size(file).map_err(anyhow::Error::new)
        }
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
fn allocated_file_bytes(file: &std::fs::File, _length: u64) -> anyhow::Result<u64> {
    Ok(file.metadata()?.len())
}

fn validate_snapshot_directory(
    directory: &OpenedSnapshotDirectory,
    manifest: &SnapshotManifest,
) -> anyhow::Result<()> {
    let has_scratch = paired_scratch_block(manifest).is_some();
    let mut entries = HashSet::new();
    for name in directory.entry_names()? {
        if name == RESUME_CLAIM_FILE_NAME
            && manifest.restore_policy == SNAPSHOT_RESTORE_POLICY_RESUME
        {
            anyhow::bail!("resume snapshot has already been claimed");
        }
        anyhow::ensure!(
            name == MANIFEST_FILE_NAME
                || name == STATE_FILE_NAME
                || name == MEMORY_FILE_NAME
                || (has_scratch && name == SCRATCH_FILE_NAME),
            "unexpected artifact in snapshot directory: {}",
            directory.display_path(&name).display(),
        );
        entries.insert(name);
    }
    anyhow::ensure!(
        entries.len() == 3 + usize::from(has_scratch)
            && entries.contains(std::ffi::OsStr::new(MANIFEST_FILE_NAME))
            && entries.contains(std::ffi::OsStr::new(STATE_FILE_NAME))
            && entries.contains(std::ffi::OsStr::new(MEMORY_FILE_NAME))
            && (!has_scratch || entries.contains(std::ffi::OsStr::new(SCRATCH_FILE_NAME))),
        "snapshot directory is incomplete"
    );
    Ok(())
}

struct OpenedSnapshotDirectory {
    file: std::fs::File,
    path: PathBuf,
}

impl OpenedSnapshotDirectory {
    fn open(path: &Path) -> anyhow::Result<Self> {
        Self::open_impl(path, false)
    }

    fn open_for_publication(path: &Path) -> anyhow::Result<Self> {
        Self::open_impl(path, true)
    }

    fn open_impl(path: &Path, allow_changes: bool) -> anyhow::Result<Self> {
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS;
            use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
            use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_DELETE;
            use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
            use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_WRITE;

            options
                .share_mode(if allow_changes {
                    FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE
                } else {
                    FILE_SHARE_READ
                })
                .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT);
        }
        #[cfg(not(windows))]
        let _ = allow_changes;
        let file = options
            .open(path)
            .with_context(|| format!("failed to open snapshot directory {}", path.display()))?;
        let metadata = file
            .metadata()
            .with_context(|| format!("failed to inspect snapshot directory {}", path.display()))?;
        anyhow::ensure!(
            metadata.file_type().is_dir(),
            "snapshot path is not a directory: {}",
            path.display(),
        );
        reject_windows_reparse_point(&metadata, "snapshot directory", path)?;
        Ok(Self {
            file,
            path: path.to_owned(),
        })
    }

    fn open_regular_file(&self, name: &str, description: &str) -> anyhow::Result<std::fs::File> {
        #[cfg(target_os = "linux")]
        let file = {
            use nix::fcntl::OFlag;
            use nix::sys::stat::Mode;

            let fd = nix::fcntl::openat(
                &self.file,
                name,
                OFlag::O_RDONLY | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW,
                Mode::empty(),
            )
            .map_err(nix_error)
            .with_context(|| {
                format!(
                    "failed to open {description} at {}",
                    self.display_path(name).display()
                )
            })?;
            std::fs::File::from(fd)
        };
        #[cfg(windows)]
        let file =
            pal::windows::fs::open_relative_read_only(&self.file, std::ffi::OsStr::new(name))
                .with_context(|| {
                    format!(
                        "failed to open {description} at {}",
                        self.display_path(name).display()
                    )
                })?;
        #[cfg(not(any(target_os = "linux", windows)))]
        let file = open_regular_file_impl(&self.path.join(name), description)?;

        validate_opened_regular_file(&file, description, &self.display_path(name))?;
        Ok(file)
    }

    fn open_regular_file_for_identity(
        &self,
        name: &str,
        description: &str,
    ) -> anyhow::Result<std::fs::File> {
        #[cfg(windows)]
        let file =
            pal::windows::fs::open_relative_for_identity(&self.file, std::ffi::OsStr::new(name))
                .with_context(|| {
                    format!(
                        "failed to open {description} at {}",
                        self.display_path(name).display()
                    )
                })?;
        #[cfg(not(windows))]
        let file = self.open_regular_file(name, description)?;

        validate_opened_regular_file(&file, description, &self.display_path(name))?;
        Ok(file)
    }

    fn open_file_with_length(
        &self,
        name: &str,
        expected_length: u64,
        artifact_name: &str,
    ) -> anyhow::Result<std::fs::File> {
        let file = self.open_regular_file(name, artifact_name)?;
        let length = opened_file_generation(&file, artifact_name)?.length();
        anyhow::ensure!(
            length == expected_length,
            "{artifact_name} size ({length} bytes) doesn't match manifest ({expected_length} bytes)",
        );
        Ok(file)
    }

    fn entry_names(&self) -> anyhow::Result<Vec<std::ffi::OsString>> {
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::OwnedFd;
            use std::os::unix::ffi::OsStrExt;

            let directory: OwnedFd = self
                .file
                .try_clone()
                .context("failed to duplicate snapshot directory handle")?
                .into();
            let mut directory = nix::dir::Dir::from_fd(directory).map_err(nix_error)?;
            let mut names = Vec::new();
            for entry in directory.iter() {
                let entry = entry.map_err(nix_error)?;
                let name = std::ffi::OsStr::from_bytes(entry.file_name().to_bytes());
                if name != "." && name != ".." {
                    names.push(name.to_owned());
                }
            }
            Ok(names)
        }
        #[cfg(windows)]
        {
            pal::windows::fs::directory_entry_names(&self.file)
                .context("failed to enumerate opened snapshot directory")
        }
        #[cfg(not(any(target_os = "linux", windows)))]
        {
            fs_err::read_dir(&self.path)
                .with_context(|| {
                    format!(
                        "failed to enumerate snapshot directory {}",
                        self.path.display()
                    )
                })?
                .map(|entry| {
                    entry
                        .context("failed to inspect snapshot directory entry")
                        .map(|entry| entry.file_name())
                })
                .collect()
        }
    }

    fn create_new_file(&self, name: &str) -> std::io::Result<std::fs::File> {
        #[cfg(target_os = "linux")]
        {
            use nix::fcntl::OFlag;
            use nix::sys::stat::Mode;

            nix::fcntl::openat(
                &self.file,
                name,
                OFlag::O_WRONLY
                    | OFlag::O_CREAT
                    | OFlag::O_EXCL
                    | OFlag::O_CLOEXEC
                    | OFlag::O_NOFOLLOW,
                Mode::S_IRUSR | Mode::S_IWUSR,
            )
            .map(std::fs::File::from)
            .map_err(nix_error)
        }
        #[cfg(windows)]
        {
            pal::windows::fs::create_relative_new(&self.file, std::ffi::OsStr::new(name))
        }
        #[cfg(not(any(target_os = "linux", windows)))]
        {
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            options.open(self.path.join(name))
        }
    }

    fn sync(&self) -> anyhow::Result<()> {
        #[cfg(unix)]
        {
            self.file.sync_all().context("failed to flush directory")
        }
        #[cfg(not(unix))]
        {
            Ok(())
        }
    }

    fn display_path(&self, name: impl AsRef<Path>) -> PathBuf {
        self.path.join(name)
    }

    fn into_file(self) -> std::fs::File {
        self.file
    }
}

#[cfg(target_os = "linux")]
fn nix_error(error: nix::errno::Errno) -> std::io::Error {
    std::io::Error::from_raw_os_error(error as i32)
}

fn open_regular_file(path: &Path, description: &str) -> anyhow::Result<std::fs::File> {
    open_regular_file_impl(path, description)
}

fn open_regular_file_impl(path: &Path, description: &str) -> anyhow::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;

        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }

    let file = options
        .open(path)
        .with_context(|| format!("failed to open {description} at {}", path.display()))?;
    validate_opened_regular_file(&file, description, path)?;
    Ok(file)
}

fn validate_opened_regular_file(
    file: &std::fs::File,
    description: &str,
    path: &Path,
) -> anyhow::Result<()> {
    let metadata = file
        .metadata()
        .with_context(|| format!("failed to inspect opened {description}"))?;
    anyhow::ensure!(
        metadata.file_type().is_file(),
        "{description} is not a regular file: {}",
        path.display(),
    );
    reject_windows_reparse_point(&metadata, description, path)
}

#[cfg(windows)]
fn reject_windows_reparse_point(
    metadata: &std::fs::Metadata,
    description: &str,
    path: &Path,
) -> anyhow::Result<()> {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

    anyhow::ensure!(
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0,
        "{description} is a reparse point: {}",
        path.display(),
    );
    Ok(())
}

#[cfg(not(windows))]
fn reject_windows_reparse_point(
    _metadata: &std::fs::Metadata,
    _description: &str,
    _path: &Path,
) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct OpenedFileGeneration {
    device: u64,
    inode: u64,
    length: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    changed_seconds: i64,
    changed_nanoseconds: i64,
}

#[cfg(windows)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct OpenedFileGeneration {
    volume_serial_number: u64,
    file_id: [u8; 16],
    length: u64,
}

#[cfg(not(any(target_os = "linux", windows)))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct OpenedFileGeneration {
    length: u64,
}

impl OpenedFileGeneration {
    fn length(self) -> u64 {
        self.length
    }
}

#[cfg(target_os = "linux")]
fn opened_file_generation(
    file: &std::fs::File,
    description: &str,
) -> anyhow::Result<OpenedFileGeneration> {
    use std::os::unix::fs::MetadataExt;

    let metadata = file
        .metadata()
        .with_context(|| format!("failed to inspect opened {description}"))?;
    Ok(OpenedFileGeneration {
        device: metadata.dev(),
        inode: metadata.ino(),
        length: metadata.len(),
        modified_seconds: metadata.mtime(),
        modified_nanoseconds: metadata.mtime_nsec(),
        changed_seconds: metadata.ctime(),
        changed_nanoseconds: metadata.ctime_nsec(),
    })
}

#[cfg(windows)]
fn opened_file_generation(
    file: &std::fs::File,
    description: &str,
) -> anyhow::Result<OpenedFileGeneration> {
    let identity = pal::windows::fs::file_identity(file)
        .with_context(|| format!("failed to query {description} FILE_ID_INFO and EOF"))?;
    Ok(OpenedFileGeneration {
        volume_serial_number: identity.volume_serial_number,
        file_id: identity.file_id,
        length: identity.end_of_file,
    })
}

#[cfg(not(any(target_os = "linux", windows)))]
fn opened_file_generation(
    file: &std::fs::File,
    description: &str,
) -> anyhow::Result<OpenedFileGeneration> {
    let metadata = file
        .metadata()
        .with_context(|| format!("failed to inspect opened {description}"))?;
    Ok(OpenedFileGeneration {
        length: metadata.len(),
    })
}

fn read_bounded_open_file(
    file: &std::fs::File,
    maximum_size: u64,
    description: &str,
) -> anyhow::Result<Vec<u8>> {
    let generation = opened_file_generation(file, description)?;
    let length = generation.length();
    anyhow::ensure!(
        length <= maximum_size,
        "{description} is {length} bytes, exceeding the maximum of {maximum_size} bytes",
    );
    let capacity = usize::try_from(length).context("artifact length does not fit in usize")?;
    let mut bytes = Vec::with_capacity(capacity);
    let mut reader = file
        .try_clone()
        .with_context(|| format!("failed to duplicate {description} handle"))?;
    reader
        .seek(SeekFrom::Start(0))
        .with_context(|| format!("failed to rewind {description}"))?;
    reader
        .take(maximum_size + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("failed to read {description}"))?;
    anyhow::ensure!(
        bytes.len() as u64 == length && opened_file_generation(file, description)? == generation,
        "{description} changed while it was being read",
    );
    Ok(bytes)
}

fn validate_sha256(digest: &[u8], description: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        digest.len() == SHA256_SIZE,
        "{description} SHA-256 digest has invalid length {}",
        digest.len(),
    );
    Ok(())
}

fn verify_digest(bytes: &[u8], expected: &[u8], description: &str) -> anyhow::Result<()> {
    let actual: [u8; 32] = sha2::Sha256::digest(bytes).into();
    anyhow::ensure!(
        actual.as_slice() == expected,
        "{description} SHA-256 digest mismatch",
    );
    Ok(())
}

/// Computes SHA-256 over an exact-length regular file handle.
pub fn file_sha256(
    file: &std::fs::File,
    expected_length: u64,
    description: &str,
) -> anyhow::Result<Vec<u8>> {
    let mut file = file
        .try_clone()
        .with_context(|| format!("failed to duplicate {description} handle"))?;
    file.seek(SeekFrom::Start(0))
        .with_context(|| format!("failed to rewind {description}"))?;
    let actual_length = file
        .metadata()
        .with_context(|| format!("failed to inspect {description}"))?
        .len();
    anyhow::ensure!(
        actual_length == expected_length,
        "{description} size ({actual_length} bytes) doesn't match expected ({expected_length} bytes)"
    );
    let mut digest = sha2::Sha256::new();
    let mut buffer = vec![0_u8; COPY_BUFFER_SIZE];
    let mut total = 0_u64;
    loop {
        let count = file
            .read(&mut buffer)
            .with_context(|| format!("failed to read {description}"))?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
        total = total
            .checked_add(count as u64)
            .context("file length overflowed u64 while hashing")?;
        anyhow::ensure!(
            total <= expected_length,
            "{description} grew while it was being hashed"
        );
    }
    anyhow::ensure!(
        total == expected_length,
        "{description} changed while it was being hashed"
    );
    Ok(digest.finalize().to_vec())
}

fn verify_file_digest(
    file: &std::fs::File,
    expected_length: u64,
    expected_digest: &[u8],
    description: &str,
) -> anyhow::Result<()> {
    validate_sha256(expected_digest, description)?;
    anyhow::ensure!(
        file_sha256(file, expected_length, description)? == expected_digest,
        "{description} SHA-256 digest mismatch"
    );
    Ok(())
}

fn open_file_with_length(
    path: &Path,
    expected_length: u64,
    artifact_name: &str,
) -> anyhow::Result<std::fs::File> {
    let file = open_regular_file(path, artifact_name)?;
    let length = opened_file_generation(&file, artifact_name)?.length();
    anyhow::ensure!(
        length == expected_length,
        "{artifact_name} size ({length} bytes) doesn't match manifest ({expected_length} bytes)",
    );
    Ok(file)
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> anyhow::Result<()> {
    fs_err::File::open(path)
        .with_context(|| format!("failed to open directory {} for flush", path.display()))?
        .sync_all()
        .with_context(|| format!("failed to flush directory {}", path.display()))
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> anyhow::Result<()> {
    // Windows has no portable directory-flush equivalent. Artifact handles are
    // flushed before the same-volume directory rename, which remains the
    // publication boundary; crash durability of that rename is filesystem-defined.
    Ok(())
}

/// Validate that a snapshot manifest is compatible with the running VM config.
///
/// Checks version, architecture, memory size, VP count, and page size.
/// Returns `Ok(())` if the manifest matches, or an error describing the
/// first mismatch found.
pub fn validate_manifest(
    manifest: &SnapshotManifest,
    expected_arch: &str,
    expected_memory_size: u64,
    expected_vp_count: u32,
    expected_page_size: u32,
) -> anyhow::Result<()> {
    validate_manifest_header(manifest)?;
    validate_manifest_version(manifest)?;

    if manifest.architecture != expected_arch {
        anyhow::bail!(
            "snapshot architecture '{}' doesn't match expected '{}'",
            manifest.architecture,
            expected_arch,
        );
    }

    if manifest.memory_size_bytes != expected_memory_size {
        anyhow::bail!(
            "snapshot memory size ({} bytes) doesn't match expected ({} bytes)",
            manifest.memory_size_bytes,
            expected_memory_size,
        );
    }

    if manifest.vp_count != expected_vp_count {
        anyhow::bail!(
            "snapshot VP count ({}) doesn't match expected ({})",
            manifest.vp_count,
            expected_vp_count,
        );
    }

    if manifest.page_size != expected_page_size {
        anyhow::bail!(
            "snapshot page size ({}) doesn't match expected ({})",
            manifest.page_size,
            expected_page_size,
        );
    }

    Ok(())
}

/// Rejects a snapshot contract that does not use the supported persisted microVM identities.
pub fn validate_supported_microvm_contract(
    contract: &SnapshotMachineContract,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        contract.microvm_abi_version == openvmm_defs::config::MICROVM_ABI_VERSION_2,
        "snapshot microVM ABI version {} is unsupported; this OpenVMM supports version {}",
        contract.microvm_abi_version,
        openvmm_defs::config::MICROVM_ABI_VERSION_2,
    );
    anyhow::ensure!(
        contract.pvh_layout_version == MICROVM_PVH_LAYOUT_VERSION,
        "snapshot PVH layout version {} is unsupported; this OpenVMM supports version {}",
        contract.pvh_layout_version,
        MICROVM_PVH_LAYOUT_VERSION,
    );
    Ok(())
}

/// Validate and exactly compare an authoritative microVM machine contract.
pub fn validate_microvm_machine_contract(
    manifest: &SnapshotManifest,
    expected: &SnapshotMachineContract,
) -> anyhow::Result<()> {
    let contract = manifest
        .machine_contract
        .as_ref()
        .context("snapshot is missing the authoritative machine contract")?;
    validate_machine_contract_shape(contract, manifest.memory_size_bytes, manifest.vp_count)?;

    anyhow::ensure!(
        contract.machine_profile == "microvm",
        "snapshot machine profile '{}' is not microvm",
        contract.machine_profile,
    );
    anyhow::ensure!(
        contract.machine_profile == expected.machine_profile,
        "snapshot machine profile doesn't match the requested machine"
    );
    anyhow::ensure!(
        contract.microvm_abi_version == expected.microvm_abi_version,
        "snapshot microVM ABI version {} doesn't match expected {}",
        contract.microvm_abi_version,
        expected.microvm_abi_version,
    );
    anyhow::ensure!(
        contract.microvm_filesystem_slot_version == expected.microvm_filesystem_slot_version,
        "snapshot microVM filesystem slot capability doesn't match the requested machine"
    );
    anyhow::ensure!(
        contract.source_hypervisor == expected.source_hypervisor,
        "snapshot source hypervisor '{}' doesn't match expected '{}'",
        contract.source_hypervisor,
        expected.source_hypervisor,
    );
    anyhow::ensure!(
        contract.pvh_layout_version == expected.pvh_layout_version,
        "snapshot PVH layout version doesn't match the requested machine"
    );
    anyhow::ensure!(
        contract.virtio_interrupt_mode == expected.virtio_interrupt_mode
            && contract.virtio_shared_status_page_gpa == expected.virtio_shared_status_page_gpa
            && contract.virtio_shared_status_page_size == expected.virtio_shared_status_page_size,
        "snapshot virtio interrupt contract doesn't match the requested machine"
    );
    anyhow::ensure!(
        contract.clock_policy == expected.clock_policy,
        "snapshot clock policy doesn't match the requested machine"
    );
    anyhow::ensure!(
        contract.effective_command_line == expected.effective_command_line,
        "snapshot effective command line doesn't match the requested machine"
    );
    anyhow::ensure!(
        contract.memory_ranges == expected.memory_ranges,
        "snapshot RAM layout doesn't match the requested machine"
    );
    anyhow::ensure!(
        contract.memory_expansion_version == expected.memory_expansion_version
            && contract.memory_capacity_bytes == expected.memory_capacity_bytes
            && contract.memory_block_size_bytes == expected.memory_block_size_bytes
            && contract.memory_expansion_ranges == expected.memory_expansion_ranges,
        "snapshot RAM capacity contract doesn't match the requested machine"
    );
    anyhow::ensure!(
        contract.topology == expected.topology,
        "snapshot processor topology doesn't match the requested machine"
    );
    anyhow::ensure!(
        contract.boot_online_vp_count == 0
            || contract.boot_online_vp_count == expected.boot_online_vp_count,
        "snapshot boot-online VP count doesn't match the requested machine"
    );
    anyhow::ensure!(
        contract.devices == expected.devices,
        "snapshot device inventory, order, or configuration doesn't match the requested machine"
    );
    anyhow::ensure!(
        contract.state_unit_names == expected.state_unit_names,
        "snapshot state-unit inventory or order doesn't match the requested machine"
    );
    anyhow::ensure!(
        contract.attachments == expected.attachments,
        "snapshot attachment inventory doesn't match the supplied attachments"
    );
    anyhow::ensure!(
        contract.microvm_network == expected.microvm_network,
        "snapshot static network identity doesn't match the requested machine"
    );
    anyhow::ensure!(
        contract.microvm_filesystem == expected.microvm_filesystem,
        "snapshot filesystem policy doesn't match the requested machine"
    );
    anyhow::ensure!(
        {
            let mut expected_blocks = expected.microvm_sandbox_blocks.clone();
            if manifest.snapshot_tier == SNAPSHOT_TIER_PLATFORM {
                for block in &mut expected_blocks {
                    if block.read_only {
                        block.identity_kind = "unbound".to_owned();
                        block.identity.clear();
                    }
                }
            }
            contract.microvm_sandbox_blocks == expected_blocks
        },
        "snapshot sandbox block topology or identity doesn't match the requested machine"
    );
    anyhow::ensure!(
        contract.tsc_frequency_hz == expected.tsc_frequency_hz
            && contract.tsc_tolerance_ppm == expected.tsc_tolerance_ppm,
        "snapshot TSC frequency contract doesn't match the destination"
    );
    anyhow::ensure!(
        contract.apic_frequency_hz == expected.apic_frequency_hz,
        "snapshot APIC frequency contract doesn't match the destination"
    );
    anyhow::ensure!(
        contract.cpu_contract == expected.cpu_contract,
        "snapshot CPU compatibility contract doesn't match the destination"
    );
    Ok(())
}

fn validate_machine_contract_shape(
    contract: &SnapshotMachineContract,
    memory_size: u64,
    vp_count: u32,
) -> anyhow::Result<()> {
    validate_supported_microvm_contract(contract)?;
    anyhow::ensure!(
        contract.virtio_interrupt_mode == MICROVM_SHARED_STATUS_INTERRUPT_MODE
            && contract.virtio_shared_status_page_gpa
                == openvmm_defs::config::MICROVM_SHARED_STATUS_PAGE_GPA
            && contract.virtio_shared_status_page_size
                == openvmm_defs::config::MICROVM_SHARED_STATUS_PAGE_SIZE,
        "snapshot microVM shared-status interrupt contract is invalid"
    );
    anyhow::ensure!(
        contract.clock_policy == ADVANCE_BY_HOST_DOWNTIME,
        "snapshot clock policy '{}' is unsupported",
        contract.clock_policy
    );
    anyhow::ensure!(
        contract.effective_command_line.len() < MAX_COMMAND_LINE_BYTES,
        "snapshot command line exceeds the 64-KiB limit"
    );
    anyhow::ensure!(
        !contract.effective_command_line.contains('\0'),
        "snapshot command line contains an embedded NUL"
    );
    validate_sha256(
        &contract.effective_command_line_sha256,
        "effective command line",
    )?;
    verify_digest(
        contract.effective_command_line.as_bytes(),
        &contract.effective_command_line_sha256,
        "effective command line",
    )?;
    anyhow::ensure!(
        contract.tsc_frequency_hz != 0,
        "snapshot TSC frequency must be nonzero"
    );
    if let Some(apic_frequency_hz) = contract.apic_frequency_hz {
        anyhow::ensure!(
            apic_frequency_hz != 0,
            "snapshot APIC frequency must be nonzero"
        );
    }
    anyhow::ensure!(
        !contract.cpu_contract.is_empty() && contract.cpu_contract.len() <= MAX_CPU_CONTRACT_BYTES,
        "snapshot CPU contract size is invalid"
    );
    validate_sha256(&contract.cpu_contract_sha256, "CPU contract")?;
    verify_digest(
        &contract.cpu_contract,
        &contract.cpu_contract_sha256,
        "CPU contract",
    )?;
    let _: std::time::SystemTime = contract
        .capture_wall_clock
        .try_into()
        .context("snapshot capture wall clock is invalid")?;

    anyhow::ensure!(
        !contract.memory_ranges.is_empty() && contract.memory_ranges.len() <= MAX_MEMORY_RANGES,
        "snapshot RAM range count is invalid"
    );
    let mut total_memory = 0_u64;
    for (index, range) in contract.memory_ranges.iter().enumerate() {
        anyhow::ensure!(range.length != 0, "snapshot RAM range {index} is empty");
        let gpa_end = range
            .gpa_start
            .checked_add(range.length)
            .with_context(|| format!("snapshot RAM range {index} overflows GPA space"))?;
        let file_end = range
            .file_offset
            .checked_add(range.length)
            .with_context(|| format!("snapshot RAM range {index} overflows file space"))?;
        anyhow::ensure!(
            file_end <= memory_size,
            "snapshot RAM range {index} exceeds memory.bin"
        );
        total_memory = total_memory
            .checked_add(range.length)
            .context("snapshot total RAM size overflows")?;
        for previous in &contract.memory_ranges[..index] {
            let previous_gpa_end = previous.gpa_start + previous.length;
            let previous_file_end = previous.file_offset + previous.length;
            anyhow::ensure!(
                gpa_end <= previous.gpa_start || range.gpa_start >= previous_gpa_end,
                "snapshot RAM ranges overlap in GPA space"
            );
            anyhow::ensure!(
                file_end <= previous.file_offset || range.file_offset >= previous_file_end,
                "snapshot RAM ranges overlap in memory.bin"
            );
        }
    }
    anyhow::ensure!(
        total_memory == memory_size,
        "snapshot RAM ranges cover {total_memory} bytes, expected {memory_size}"
    );
    anyhow::ensure!(
        contract.memory_expansion_version == 0
            && contract.memory_capacity_bytes == 0
            && contract.memory_block_size_bytes == 0
            && contract.memory_expansion_ranges.is_empty(),
        "base microVM snapshots do not support memory expansion"
    );

    if let Some(network) = &contract.microvm_network {
        anyhow::ensure!(
            network.profile == openvmm_defs::config::MicrovmNetworkProfile::Portable.as_str(),
            "snapshot microVM network profile '{}' is unsupported",
            network.profile
        );
        let prefix_length = u8::try_from(network.prefix_length)
            .context("snapshot network prefix does not fit in u8")?;
        let parsed = format!(
            "{}/{}",
            std::net::Ipv4Addr::from(network.guest_ipv4),
            prefix_length
        )
        .parse::<openvmm_defs::config::MicrovmNetworkConfig>()
        .context("snapshot static network identity is invalid")?;
        anyhow::ensure!(
            network.gateway_ipv4 == u32::from(parsed.derived_gateway_ipv4)
                && network.guest_mac == parsed.guest_mac.to_bytes()
                && network.gateway_mac == parsed.gateway_mac.to_bytes(),
            "snapshot static network identity is not canonical"
        );
        anyhow::ensure!(
            matches!(
                network.egress_policy_mode.as_str(),
                "allow-all" | "allow-list" | "block-list" | "endpoint"
            ) && network.egress_policy_required == (network.egress_policy_mode != "allow-all"),
            "snapshot egress policy requirement is invalid"
        );
        anyhow::ensure!(
            matches!(
                network.egress_policy_encoding_version,
                0 | 1 | net_backend_resources::egress::EGRESS_POLICY_ENCODING_VERSION
            ),
            "snapshot egress policy encoding version {} is unsupported",
            network.egress_policy_encoding_version
        );
        validate_sha256(&network.egress_policy_sha256, "egress policy")?;
    }

    if let Some(filesystem) = &contract.microvm_filesystem {
        anyhow::ensure!(
            !filesystem.canonical_host_path.is_empty(),
            "snapshot filesystem canonical host path is missing; this snapshot predates path-bound filesystem restore"
        );
        let access = match filesystem.access_mode.as_str() {
            "ro" => openvmm_defs::config::MicrovmFilesystemAccess::ReadOnly,
            "rw" => openvmm_defs::config::MicrovmFilesystemAccess::ReadWrite,
            mode => anyhow::bail!("snapshot filesystem access mode '{mode}' is unsupported"),
        };
        let parsed = openvmm_defs::config::MicrovmFilesystemConfig::new(
            filesystem.guest_mount_target.clone(),
            access,
        )
        .context("snapshot filesystem policy is invalid")?;
        anyhow::ensure!(
            *filesystem == SnapshotMicrovmFilesystem::new(&parsed, &filesystem.canonical_host_path),
            "snapshot filesystem policy is not canonical"
        );
    }

    let has_filesystem_device = contract
        .devices
        .iter()
        .any(|device| device.stable_id == "fs:microvm0");
    let has_filesystem_attachment = contract
        .attachments
        .iter()
        .any(|attachment| attachment.stable_id == "fs:microvm0");
    match contract.microvm_filesystem_slot_version {
        0 => anyhow::ensure!(
            has_filesystem_device == contract.microvm_filesystem.is_some()
                && has_filesystem_attachment == contract.microvm_filesystem.is_some(),
            "legacy snapshot microVM filesystem device, policy, and attachment inventories disagree"
        ),
        MICROVM_FILESYSTEM_SLOT_VERSION => anyhow::ensure!(
            has_filesystem_device
                && has_filesystem_attachment == contract.microvm_filesystem.is_some(),
            "snapshot reserved microVM filesystem slot, policy, and attachment inventories disagree"
        ),
        version => anyhow::bail!(
            "snapshot microVM filesystem slot capability version {version} is unsupported"
        ),
    }

    if contract.boot_online_vp_count != 0 {
        anyhow::ensure!(
            contract.boot_online_vp_count
                == microvm_boot_online_vp_count(vp_count, &contract.effective_command_line,)?,
            "snapshot boot-online VP count does not match the effective command line"
        );
    }

    let topology = &contract.topology;
    let topology_vp_count = u64::from(topology.sockets)
        .checked_mul(u64::from(topology.dies_per_socket))
        .and_then(|count| count.checked_mul(u64::from(topology.cores_per_die)))
        .and_then(|count| count.checked_mul(u64::from(topology.threads_per_core)))
        .context("snapshot processor topology overflows")?;
    anyhow::ensure!(
        topology_vp_count == u64::from(vp_count) && topology.apic_ids.len() == vp_count as usize,
        "snapshot processor topology doesn't describe {vp_count} virtual processors"
    );
    ensure_unique(&topology.apic_ids, "APIC ID")?;
    anyhow::ensure!(
        *topology == microvm_snapshot_topology(vp_count)?,
        "snapshot processor topology is not canonical for the microVM"
    );
    anyhow::ensure!(
        contract.microvm_sandbox_blocks.is_empty()
            && contract.attachments.iter().all(|attachment| matches!(
                attachment.kind.as_str(),
                "virtio-console" | "virtio-net" | "virtio-fs"
            )),
        "base microVM snapshots cannot contain device attachments or expanded topology"
    );

    anyhow::ensure!(
        contract.devices.len() <= MAX_DEVICES,
        "snapshot device inventory is too large"
    );
    let mut device_ids = HashSet::new();
    for (index, device) in contract.devices.iter().enumerate() {
        anyhow::ensure!(
            device.order == index as u32,
            "snapshot device order is not canonical"
        );
        anyhow::ensure!(
            !device.stable_id.is_empty() && device_ids.insert(device.stable_id.as_str()),
            "snapshot contains an empty or duplicate device ID"
        );
        anyhow::ensure!(
            !device.state_unit_name.is_empty(),
            "snapshot device '{}' has no state-unit name",
            device.stable_id,
        );
        anyhow::ensure!(
            device.ranges.len() <= MAX_DEVICE_RANGES,
            "snapshot device '{}' has too many address ranges",
            device.stable_id,
        );
        for range in &device.ranges {
            anyhow::ensure!(
                matches!(range.address_space.as_str(), "pmio" | "mmio") && range.length != 0,
                "snapshot device '{}' has an invalid address range",
                device.stable_id,
            );
            range.start.checked_add(range.length).with_context(|| {
                format!("snapshot device '{}' range overflows", device.stable_id)
            })?;
        }
        anyhow::ensure!(
            device.queue_count as usize == device.queue_max_sizes.len(),
            "snapshot device '{}' queue inventory is inconsistent",
            device.stable_id,
        );
    }

    anyhow::ensure!(
        !contract.state_unit_names.is_empty() && contract.state_unit_names.len() <= MAX_STATE_UNITS,
        "snapshot state-unit inventory size is invalid"
    );
    ensure_unique(&contract.state_unit_names, "state-unit name")?;
    let state_units = contract
        .state_unit_names
        .iter()
        .map(String::as_str)
        .collect::<HashSet<_>>();
    for device in &contract.devices {
        anyhow::ensure!(
            state_units.contains(device.state_unit_name.as_str()),
            "snapshot device '{}' references an unknown state unit",
            device.stable_id,
        );
    }

    anyhow::ensure!(
        contract.attachments.len() <= MAX_ATTACHMENTS,
        "snapshot attachment inventory is too large"
    );
    let mut attachment_ids = HashSet::new();
    for attachment in &contract.attachments {
        anyhow::ensure!(
            !attachment.stable_id.is_empty()
                && attachment_ids.insert(attachment.stable_id.as_str()),
            "snapshot contains an empty or duplicate attachment ID"
        );
        anyhow::ensure!(
            !attachment.kind.is_empty()
                && !attachment.reconnect_policy.is_empty()
                && !attachment.identity_kind.is_empty()
                && !attachment.identity.is_empty()
                && attachment.identity.len() <= MAX_ATTACHMENT_IDENTITY_BYTES,
            "snapshot attachment '{}' has an incomplete identity",
            attachment.stable_id,
        );
        anyhow::ensure!(
            match attachment.reconnect_policy.as_str() {
                "recreate-listener" => {
                    !attachment.required && attachment.reconnect_timeout_ms == 0
                }
                "reconnect-client" => {
                    attachment.required && attachment.reconnect_timeout_ms != 0
                }
                "require-inherited-attachment" => {
                    attachment.required && attachment.reconnect_timeout_ms == 0
                }
                "discard-while-disconnected" => {
                    !attachment.required && attachment.reconnect_timeout_ms == 0
                }
                "recreate-endpoint" => {
                    !attachment.required && attachment.reconnect_timeout_ms == 0
                }
                "live-revalidate" => {
                    attachment.required
                        && attachment.reconnect_timeout_ms == 0
                        && attachment.length == 0
                }
                _ => false,
            },
            "snapshot attachment '{}' has an invalid reconnect policy",
            attachment.stable_id,
        );
    }
    Ok(())
}

fn validate_manifest_header(manifest: &SnapshotManifest) -> anyhow::Result<()> {
    let expected_magic = match manifest.version {
        LEGACY_MANIFEST_VERSION => LEGACY_SNAPSHOT_FORMAT_MAGIC,
        PREVIOUS_MANIFEST_VERSION => PREVIOUS_SNAPSHOT_FORMAT_MAGIC,
        VERSION_4_MANIFEST_VERSION => VERSION_4_SNAPSHOT_FORMAT_MAGIC,
        MANIFEST_VERSION => SNAPSHOT_FORMAT_MAGIC,
        version => anyhow::bail!(
            "snapshot manifest version {version} is not supported (expected {LEGACY_MANIFEST_VERSION} through {MANIFEST_VERSION})"
        ),
    };
    anyhow::ensure!(
        manifest.format_magic == expected_magic,
        "snapshot format magic is invalid"
    );
    anyhow::ensure!(
        manifest.saved_state_schema_version == SAVED_STATE_SCHEMA_VERSION,
        "snapshot saved-state schema version {} is unsupported",
        manifest.saved_state_schema_version
    );
    anyhow::ensure!(
        manifest.saved_state_root_type == SAVED_STATE_ROOT_TYPE,
        "snapshot saved-state root type '{}' is unsupported",
        manifest.saved_state_root_type
    );
    Ok(())
}

fn validate_manifest_version(manifest: &SnapshotManifest) -> anyhow::Result<()> {
    if manifest.version < MANIFEST_VERSION {
        anyhow::ensure!(
            manifest.snapshot_tier.is_empty()
                && manifest.restore_policy.is_empty()
                && manifest.consumed_config_sections == 0,
            "snapshot manifest version {} cannot contain snapshot tier metadata",
            manifest.version,
        );
    }
    match manifest.version {
        LEGACY_MANIFEST_VERSION => {
            anyhow::ensure!(
                manifest.state_sha256.len() == SHA256_SIZE,
                "legacy state.bin SHA-256 digest has invalid length {}",
                manifest.state_sha256.len(),
            );
            anyhow::ensure!(
                manifest.memory_sha256.len() == SHA256_SIZE,
                "legacy memory.bin SHA-256 digest has invalid length {}",
                manifest.memory_sha256.len(),
            );
            anyhow::ensure!(
                manifest
                    .machine_contract
                    .as_ref()
                    .is_none_or(|contract| contract.microvm_sandbox_blocks.is_empty()),
                "snapshot manifest version {LEGACY_MANIFEST_VERSION} cannot contain microVM sandbox blocks"
            );
        }
        PREVIOUS_MANIFEST_VERSION | VERSION_4_MANIFEST_VERSION | MANIFEST_VERSION => {
            anyhow::ensure!(
                manifest.state_sha256.is_empty() && manifest.memory_sha256.is_empty(),
                "snapshot manifest version {} must not contain legacy artifact digests",
                manifest.version,
            );
            if manifest.version == PREVIOUS_MANIFEST_VERSION {
                anyhow::ensure!(
                    manifest
                        .machine_contract
                        .as_ref()
                        .is_none_or(|contract| contract.microvm_sandbox_blocks.is_empty()),
                    "snapshot manifest version {PREVIOUS_MANIFEST_VERSION} cannot contain microVM sandbox blocks"
                );
            }
            if manifest.version == MANIFEST_VERSION {
                validate_snapshot_tier(manifest)?;
            }
        }
        version => anyhow::bail!(
            "snapshot manifest version {version} is not supported (expected {LEGACY_MANIFEST_VERSION} through {MANIFEST_VERSION})"
        ),
    }
    Ok(())
}

fn validate_snapshot_tier(manifest: &SnapshotManifest) -> anyhow::Result<()> {
    let Some(contract) = manifest.machine_contract.as_ref() else {
        anyhow::ensure!(
            manifest.snapshot_tier.is_empty()
                && manifest.restore_policy.is_empty()
                && manifest.consumed_config_sections == 0,
            "snapshot tier metadata requires a microVM machine contract"
        );
        return Ok(());
    };
    if contract.microvm_sandbox_blocks.is_empty() {
        anyhow::ensure!(
            manifest.snapshot_tier.is_empty()
                && manifest.restore_policy.is_empty()
                && manifest.consumed_config_sections == 0,
            "snapshot tier metadata requires microVM sandbox blocks"
        );
        return Ok(());
    }

    let paired_scratch = paired_scratch_block(manifest).is_some();
    let expected_consumed_sections = match manifest.snapshot_tier.as_str() {
        SNAPSHOT_TIER_PLATFORM => SNAPSHOT_CONFIG_INVARIANTS,
        SNAPSHOT_TIER_WORKLOAD_START | SNAPSHOT_TIER_INSTANCE_CHECKPOINT => SNAPSHOT_CONFIG_ALL,
        _ => 0,
    };
    let valid = manifest.consumed_config_sections == expected_consumed_sections
        && matches!(
            (
                manifest.snapshot_tier.as_str(),
                manifest.restore_policy.as_str(),
                paired_scratch,
            ),
            (SNAPSHOT_TIER_PLATFORM, SNAPSHOT_RESTORE_POLICY_CLONE, false)
                | (
                    SNAPSHOT_TIER_WORKLOAD_START,
                    SNAPSHOT_RESTORE_POLICY_CLONE,
                    true
                )
                | (
                    SNAPSHOT_TIER_INSTANCE_CHECKPOINT,
                    SNAPSHOT_RESTORE_POLICY_RESUME,
                    true
                )
        );
    anyhow::ensure!(
        valid,
        "snapshot tier '{}', restore policy '{}', and scratch policy are not a canonical microVM combination",
        manifest.snapshot_tier,
        manifest.restore_policy,
    );
    let expected_tier_token = format!("nvx_snapshot_tier={}", manifest.snapshot_tier);
    let tier_tokens = contract
        .effective_command_line
        .split_ascii_whitespace()
        .filter(|token| token.starts_with("nvx_snapshot_tier="))
        .collect::<Vec<_>>();
    anyhow::ensure!(
        tier_tokens == [expected_tier_token.as_str()],
        "snapshot tier '{}' does not match its saved host policy",
        manifest.snapshot_tier,
    );
    let layers_are_unbound = contract
        .microvm_sandbox_blocks
        .iter()
        .filter(|block| block.read_only)
        .all(|block| block.identity_kind == "unbound" && block.identity.is_empty());
    anyhow::ensure!(
        layers_are_unbound == (manifest.snapshot_tier == SNAPSHOT_TIER_PLATFORM),
        "snapshot layer identity binding does not match tier '{}'",
        manifest.snapshot_tier,
    );
    if manifest.snapshot_tier == SNAPSHOT_TIER_PLATFORM {
        anyhow::ensure!(
            contract
                .effective_command_line
                .split_ascii_whitespace()
                .all(platform_command_line_token_is_invariant),
            "platform snapshot command line contains tenant or unsupported configuration"
        );
    }
    Ok(())
}

fn platform_command_line_token_is_invariant(token: &str) -> bool {
    matches!(
        token,
        "earlycon=xe9"
            | "console=hvc0"
            | "console=hvc1"
            | "reboot=t"
            | "panic=-1"
            | "nvx_sandbox=1"
            | "nvx_config=0xd0010000,65536"
            | "nvx_snapshot_tier=platform"
    ) || [
        "virtio_mmio.device=",
        "virtnet_ip=",
        "virtnet_mask=",
        "virtnet_gw=",
        "virtnet_dns=",
    ]
    .iter()
    .any(|prefix| token.starts_with(prefix))
}

fn ensure_unique<T>(values: &[T], description: &str) -> anyhow::Result<()>
where
    T: Eq + std::hash::Hash,
{
    let mut unique = HashSet::with_capacity(values.len());
    anyhow::ensure!(
        values.iter().all(|value| unique.insert(value)),
        "snapshot contains a duplicate {description}"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: build a test manifest with sensible defaults.
    fn test_manifest() -> SnapshotManifest {
        SnapshotManifest {
            version: MANIFEST_VERSION,
            created_at: Timestamp {
                seconds: 1234567890,
                nanos: 0,
            },
            openvmm_version: "test-0.1.0".to_string(),
            memory_size_bytes: 1024,
            vp_count: 1,
            page_size: 4096,
            architecture: "x86_64".to_string(),
            state_size_bytes: 0,
            state_sha256: Vec::new(),
            memory_sha256: Vec::new(),
            machine_contract: None,
            format_magic: SNAPSHOT_FORMAT_MAGIC.to_vec(),
            saved_state_schema_version: SAVED_STATE_SCHEMA_VERSION,
            saved_state_root_type: SAVED_STATE_ROOT_TYPE.to_owned(),
            snapshot_tier: String::new(),
            restore_policy: String::new(),
            consumed_config_sections: 0,
        }
    }

    fn test_machine_contract() -> SnapshotMachineContract {
        let mut contract = SnapshotMachineContract {
            machine_profile: "microvm".to_owned(),
            microvm_abi_version: openvmm_defs::config::MICROVM_ABI_VERSION_2,
            source_hypervisor: "whp".to_owned(),
            effective_command_line: String::new(),
            effective_command_line_sha256: Vec::new(),
            memory_ranges: vec![SnapshotMemoryRange {
                gpa_start: 0,
                length: 1024,
                file_offset: 0,
            }],
            topology: SnapshotProcessorTopology {
                sockets: 1,
                dies_per_socket: 1,
                cores_per_die: 1,
                threads_per_core: 1,
                apic_ids: vec![0],
            },
            devices: vec![
                SnapshotDevice {
                    stable_id: "portb".to_owned(),
                    state_unit_name: "portb".to_owned(),
                    kind: "portb".to_owned(),
                    order: 0,
                    ranges: vec![SnapshotDeviceRange {
                        address_space: "pmio".to_owned(),
                        start: 0xe9,
                        length: 2,
                    }],
                    irq: None,
                    transport: String::new(),
                    feature_banks: Vec::new(),
                    queue_count: 0,
                    queue_max_sizes: Vec::new(),
                },
                SnapshotDevice {
                    stable_id: "shutdown".to_owned(),
                    state_unit_name: "shutdown".to_owned(),
                    kind: "shutdown".to_owned(),
                    order: 1,
                    ranges: vec![SnapshotDeviceRange {
                        address_space: "pmio".to_owned(),
                        start: 0x604,
                        length: 1,
                    }],
                    irq: None,
                    transport: String::new(),
                    feature_banks: Vec::new(),
                    queue_count: 0,
                    queue_max_sizes: Vec::new(),
                },
            ],
            state_unit_names: vec!["portb".to_owned(), "shutdown".to_owned()],
            attachments: Vec::new(),
            capture_wall_clock: std::time::SystemTime::now().into(),
            tsc_frequency_hz: 1_000_000_000,
            tsc_tolerance_ppm: 0,
            cpu_contract: Vec::new(),
            cpu_contract_sha256: Vec::new(),
            pvh_layout_version: MICROVM_PVH_LAYOUT_VERSION,
            clock_policy: ADVANCE_BY_HOST_DOWNTIME.to_owned(),
            microvm_network: None,
            microvm_filesystem: None,
            apic_frequency_hz: Some(1_000_000_000),
            microvm_sandbox_blocks: Vec::new(),
            microvm_filesystem_slot_version: 0,
            boot_online_vp_count: 0,
            virtio_interrupt_mode: MICROVM_SHARED_STATUS_INTERRUPT_MODE.to_owned(),
            virtio_shared_status_page_gpa: openvmm_defs::config::MICROVM_SHARED_STATUS_PAGE_GPA,
            virtio_shared_status_page_size: openvmm_defs::config::MICROVM_SHARED_STATUS_PAGE_SIZE,
            memory_expansion_version: 0,
            memory_capacity_bytes: 0,
            memory_block_size_bytes: 0,
            memory_expansion_ranges: Vec::new(),
        };
        contract.set_effective_command_line("console=hvc0".to_owned());
        contract.set_cpu_compatibility_contract(vec![1, 2, 3]);
        contract
    }

    #[test]
    fn validate_microvm_machine_contract_ok() {
        let mut manifest = test_manifest();
        let contract = test_machine_contract();
        manifest.machine_contract = Some(contract.clone());
        validate_microvm_machine_contract(&manifest, &contract).unwrap();
    }

    #[test]
    fn validate_microvm_machine_contract_rejects_abi() {
        let mut manifest = test_manifest();
        let expected = test_machine_contract();
        let mut contract = expected.clone();
        contract.microvm_abi_version = 0;
        manifest.machine_contract = Some(contract);
        let err = validate_microvm_machine_contract(&manifest, &expected).unwrap_err();
        assert!(err.to_string().contains("ABI version 0 is unsupported"));
    }

    #[test]
    fn validate_microvm_machine_contract_rejects_backend() {
        let mut manifest = test_manifest();
        let contract = test_machine_contract();
        manifest.machine_contract = Some(contract.clone());
        let mut expected = contract;
        expected.source_hypervisor = "kvm".to_owned();
        let err = validate_microvm_machine_contract(&manifest, &expected).unwrap_err();
        assert!(err.to_string().contains("source hypervisor"));
    }

    #[test]
    fn validate_microvm_machine_contract_rejects_command_line() {
        let mut manifest = test_manifest();
        let contract = test_machine_contract();
        manifest.machine_contract = Some(contract.clone());
        let mut expected = contract;
        expected.set_effective_command_line("console=other".to_owned());
        let err = validate_microvm_machine_contract(&manifest, &expected).unwrap_err();
        assert!(err.to_string().contains("command line"));
    }

    #[test]
    fn validate_microvm_machine_contract_rejects_cpu_contract() {
        let mut manifest = test_manifest();
        let contract = test_machine_contract();
        manifest.machine_contract = Some(contract.clone());
        let mut expected = contract;
        expected.set_cpu_compatibility_contract(vec![9, 9, 9]);
        let err = validate_microvm_machine_contract(&manifest, &expected).unwrap_err();
        assert!(err.to_string().contains("CPU compatibility"));
    }

    #[test]
    fn validate_microvm_machine_contract_rejects_tsc_frequency() {
        let mut manifest = test_manifest();
        let contract = test_machine_contract();
        manifest.machine_contract = Some(contract.clone());
        let mut expected = contract;
        expected.tsc_frequency_hz += 1;
        let err = validate_microvm_machine_contract(&manifest, &expected).unwrap_err();
        assert!(err.to_string().contains("TSC frequency"));
    }

    #[test]
    fn validate_microvm_machine_contract_rejects_apic_frequency() {
        let mut manifest = test_manifest();
        let contract = test_machine_contract();
        manifest.machine_contract = Some(contract.clone());
        let mut expected = contract;
        expected.apic_frequency_hz = expected.apic_frequency_hz.map(|frequency| frequency + 1);
        let err = validate_microvm_machine_contract(&manifest, &expected).unwrap_err();
        assert!(err.to_string().contains("APIC frequency"));
    }

    #[test]
    fn validate_microvm_machine_contract_accepts_legacy_apic_frequency() {
        let mut manifest = test_manifest();
        let mut contract = test_machine_contract();
        contract.apic_frequency_hz = None;
        manifest.machine_contract = Some(contract.clone());

        validate_microvm_machine_contract(&manifest, &contract).unwrap();
    }

    #[test]
    fn validate_microvm_machine_contract_rejects_pvh_layout() {
        let mut manifest = test_manifest();
        let expected = test_machine_contract();
        let mut contract = expected.clone();
        contract.pvh_layout_version = 0;
        manifest.machine_contract = Some(contract);
        let err = validate_microvm_machine_contract(&manifest, &expected).unwrap_err();
        assert!(
            err.to_string()
                .contains("PVH layout version 0 is unsupported")
        );
    }

    #[test]
    fn validate_microvm_machine_contract_rejects_clock_policy() {
        let mut manifest = test_manifest();
        let contract = test_machine_contract();
        manifest.machine_contract = Some(contract.clone());
        let mut expected = contract;
        expected.clock_policy = "freeze_during_downtime".to_owned();
        let err = validate_microvm_machine_contract(&manifest, &expected).unwrap_err();
        assert!(err.to_string().contains("clock policy"));
    }

    #[test]
    fn validate_microvm_machine_contract_rejects_topology() {
        let mut manifest = test_manifest();
        let contract = test_machine_contract();
        manifest.machine_contract = Some(contract.clone());
        let mut expected = contract;
        expected.topology.apic_ids[0] = 1;
        let err = validate_microvm_machine_contract(&manifest, &expected).unwrap_err();
        assert!(err.to_string().contains("processor topology"));
    }

    #[test]
    fn validate_microvm_machine_contract_rejects_device_order() {
        let mut manifest = test_manifest();
        let contract = test_machine_contract();
        manifest.machine_contract = Some(contract.clone());
        let mut expected = contract;
        expected.devices.reverse();
        let err = validate_microvm_machine_contract(&manifest, &expected).unwrap_err();
        assert!(err.to_string().contains("device inventory"));
    }

    #[test]
    fn validate_microvm_machine_contract_rejects_removed_device() {
        let mut manifest = test_manifest();
        let contract = test_machine_contract();
        manifest.machine_contract = Some(contract.clone());
        let mut expected = contract;
        expected.devices.pop();
        let err = validate_microvm_machine_contract(&manifest, &expected).unwrap_err();
        assert!(err.to_string().contains("device inventory"));
    }

    #[test]
    fn validate_microvm_machine_contract_rejects_state_unit_order() {
        let mut manifest = test_manifest();
        let contract = test_machine_contract();
        manifest.machine_contract = Some(contract.clone());
        let mut expected = contract;
        expected.state_unit_names.reverse();
        let err = validate_microvm_machine_contract(&manifest, &expected).unwrap_err();
        assert!(err.to_string().contains("state-unit inventory"));
    }

    #[test]
    fn validate_microvm_machine_contract_rejects_duplicate_state_unit() {
        let mut manifest = test_manifest();
        let mut contract = test_machine_contract();
        contract.state_unit_names.push("portb".to_owned());
        manifest.machine_contract = Some(contract.clone());
        let err = validate_microvm_machine_contract(&manifest, &contract).unwrap_err();
        assert!(err.to_string().contains("duplicate state-unit name"));
    }

    #[test]
    fn validate_microvm_machine_contract_rejects_overlapping_ram() {
        let mut manifest = test_manifest();
        let mut contract = test_machine_contract();
        contract.memory_ranges = vec![
            SnapshotMemoryRange {
                gpa_start: 0,
                length: 768,
                file_offset: 0,
            },
            SnapshotMemoryRange {
                gpa_start: 512,
                length: 256,
                file_offset: 768,
            },
        ];
        manifest.machine_contract = Some(contract.clone());
        let err = validate_microvm_machine_contract(&manifest, &contract).unwrap_err();
        assert!(err.to_string().contains("overlap in GPA space"));
    }

    #[test]
    fn write_read_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");

        // Create a fake memory backing file in the same directory (same fs).
        let mem_path = dir.path().join("memory.bin");
        std::fs::write(&mem_path, vec![0_u8; 1024]).unwrap();

        let manifest = test_manifest();
        let state = b"saved-state-data";

        write_snapshot(&snap_dir, &manifest, state, &mem_path).unwrap();

        let (read_manifest, read_state) = read_snapshot(&snap_dir, 1024).unwrap();
        assert_eq!(read_manifest.version, manifest.version);
        assert_eq!(read_manifest.memory_size_bytes, manifest.memory_size_bytes);
        assert_eq!(read_manifest.vp_count, manifest.vp_count);
        assert_eq!(read_manifest.architecture, manifest.architecture);
        assert_eq!(read_manifest.state_size_bytes, state.len() as u64);
        assert!(read_manifest.state_sha256.is_empty());
        assert!(read_manifest.memory_sha256.is_empty());
        assert_eq!(read_state, state);

        // memory.bin should exist in the snapshot directory.
        assert!(snap_dir.join("memory.bin").exists());
    }

    #[test]
    fn write_snapshot_rejects_missing_parent() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("a").join("b").join("c");

        let mem_path = dir.path().join("memory.bin");
        std::fs::write(&mem_path, vec![0_u8; 1024]).unwrap();

        let err = write_snapshot(&snap_dir, &test_manifest(), b"state", &mem_path).unwrap_err();
        assert!(!err.is_committed());
        assert!(err.to_string().contains("snapshot parent directory"));
        assert!(!snap_dir.exists());
    }

    #[test]
    fn write_snapshot_copies_memory() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        let mem_path = dir.path().join("memory.bin");
        std::fs::write(&mem_path, b"SAMEFILE").unwrap();

        let mut manifest = test_manifest();
        manifest.memory_size_bytes = 8;
        write_snapshot(&snap_dir, &manifest, b"state", &mem_path).unwrap();
        std::fs::write(&mem_path, b"MODIFIED").unwrap();

        assert_eq!(
            std::fs::read(snap_dir.join(MEMORY_FILE_NAME)).unwrap(),
            b"SAMEFILE"
        );
    }

    #[cfg(any(target_os = "linux", windows))]
    #[test]
    fn owned_memory_snapshot_publishes_exact_file() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        let memory_path = dir.path().join("automatic-memory.bin");
        std::fs::write(&memory_path, vec![0x5a_u8; 1024]).unwrap();
        let memory_file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&memory_path)
            .unwrap();

        write_snapshot_from_owned_memory_and_scratch_files(
            &snap_dir,
            &test_manifest(),
            b"state",
            &memory_file,
            None,
        )
        .unwrap();

        let published = std::fs::File::open(snap_dir.join(MEMORY_FILE_NAME)).unwrap();
        verify_hard_link_identity(&memory_file, &published, 1024).unwrap();
    }

    #[cfg(any(target_os = "linux", windows))]
    #[test]
    fn owned_memory_link_uses_exact_handle_after_path_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        let memory_path = dir.path().join("automatic-memory.bin");
        let moved_path = dir.path().join("mapped-memory.bin");
        std::fs::write(&memory_path, vec![0x5a_u8; 1024]).unwrap();
        let memory_file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&memory_path)
            .unwrap();
        std::fs::rename(&memory_path, &moved_path).unwrap();
        std::fs::write(&memory_path, vec![0xa5_u8; 1024]).unwrap();

        write_snapshot_from_owned_memory_and_scratch_files(
            &snap_dir,
            &test_manifest(),
            b"state",
            &memory_file,
            None,
        )
        .unwrap();

        let published = std::fs::File::open(snap_dir.join(MEMORY_FILE_NAME)).unwrap();
        verify_hard_link_identity(&memory_file, &published, 1024).unwrap();
        assert_eq!(
            std::fs::read(snap_dir.join(MEMORY_FILE_NAME)).unwrap(),
            vec![0x5a_u8; 1024]
        );
    }

    #[cfg(any(target_os = "linux", windows))]
    #[test]
    fn injected_failure_removes_live_memory_staging_alias() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        let memory_path = dir.path().join("automatic-memory.bin");
        std::fs::write(&memory_path, vec![0x5a_u8; 1024]).unwrap();
        let memory_file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&memory_path)
            .unwrap();
        let staging = stage_snapshot(
            &snap_dir,
            &test_manifest(),
            b"state",
            &memory_file,
            None,
            MemoryPublication::OwnedExactFile,
        )
        .unwrap();
        assert!(staging.source_memory_alias);
        let staging_path = staging.path().to_owned();

        let error = staging.rollback(anyhow::anyhow!("injected failure after memory link"));

        assert!(error.is_rollback_safe());
        assert!(!staging_path.exists());
        assert!(!snap_dir.exists());
        std::fs::write(&memory_path, vec![0xa5_u8; 1024]).unwrap();
    }

    #[test]
    fn uncertain_live_alias_cleanup_is_not_rollback_safe() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        let mut staging = StagingDirectory::create(dir.path(), &snap_dir).unwrap();
        staging.mark_source_memory_alias();
        staging.inject_cleanup_failure = true;

        let error = staging.rollback(anyhow::anyhow!("injected publication failure"));

        assert!(!error.is_committed());
        assert!(!error.is_rollback_safe());
        assert!(matches!(error, SnapshotWriteError::CleanupUncertain { .. }));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn write_snapshot_preserves_sparse_memory_and_clone_independence() {
        const MEMORY_SIZE: u64 = 16 * 1024 * 1024;
        let dir = tempfile::tempdir().unwrap();
        for (case, sparse) in [("zero", true), ("mixed", true), ("dense", false)] {
            let case_dir = dir.path().join(case);
            std::fs::create_dir(&case_dir).unwrap();
            let snap_dir = case_dir.join("snap");
            let mem_path = case_dir.join("memory.bin");
            let mut memory_file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&mem_path)
                .unwrap();
            assert_eq!(
                initialize_snapshot_memory_backing_file(&memory_file, MEMORY_SIZE).unwrap(),
                0
            );

            let mut expected = vec![0_u8; MEMORY_SIZE as usize];
            match case {
                "zero" => {}
                "mixed" => {
                    let tail_offset = expected.len() - 4;
                    expected[..4].copy_from_slice(b"head");
                    expected[tail_offset..].copy_from_slice(b"tail");
                    memory_file.write_all(b"head").unwrap();
                    memory_file.seek(SeekFrom::End(-4)).unwrap();
                    memory_file.write_all(b"tail").unwrap();
                }
                "dense" => {
                    expected.fill(0x5a);
                    memory_file.seek(SeekFrom::Start(0)).unwrap();
                    memory_file.write_all(&expected).unwrap();
                }
                _ => unreachable!(),
            }
            memory_file.sync_all().unwrap();

            let mut manifest = test_manifest();
            manifest.memory_size_bytes = MEMORY_SIZE;
            write_snapshot_from_memory_file(&snap_dir, &manifest, b"state", &memory_file).unwrap();

            memory_file.seek(SeekFrom::Start(0)).unwrap();
            memory_file.write_all(b"xxxx").unwrap();
            memory_file.sync_all().unwrap();

            let published_path = snap_dir.join(MEMORY_FILE_NAME);
            let published = std::fs::File::open(&published_path).unwrap();
            assert_eq!(published.metadata().unwrap().len(), MEMORY_SIZE);
            if sparse {
                let allocated = allocated_file_bytes(&published, MEMORY_SIZE).unwrap();
                assert!(
                    allocated < MEMORY_SIZE / 4,
                    "{case} snapshot allocated {allocated} bytes for a {MEMORY_SIZE}-byte file",
                );
            }
            assert_eq!(std::fs::read(published_path).unwrap(), expected, "{case}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn write_snapshot_uses_dense_memory_files_on_windows() {
        use std::os::windows::fs::MetadataExt;

        const FILE_ATTRIBUTE_SPARSE_FILE: u32 = 0x200;
        const MEMORY_SIZE: u64 = 4 * 1024 * 1024;

        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        let mem_path = dir.path().join("memory.bin");
        let mut memory_file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(mem_path)
            .unwrap();
        initialize_snapshot_memory_backing_file(&memory_file, MEMORY_SIZE).unwrap();
        assert_eq!(
            memory_file.metadata().unwrap().file_attributes() & FILE_ATTRIBUTE_SPARSE_FILE,
            0
        );

        memory_file.write_all(b"head").unwrap();
        memory_file.seek(SeekFrom::End(-4)).unwrap();
        memory_file.write_all(b"tail").unwrap();
        memory_file.sync_all().unwrap();

        let mut manifest = test_manifest();
        manifest.memory_size_bytes = MEMORY_SIZE;
        write_snapshot_from_memory_file(&snap_dir, &manifest, b"state", &memory_file).unwrap();

        let published_path = snap_dir.join(MEMORY_FILE_NAME);
        let published = std::fs::File::open(&published_path).unwrap();
        assert_eq!(
            published.metadata().unwrap().file_attributes() & FILE_ATTRIBUTE_SPARSE_FILE,
            0
        );

        let mut expected = vec![0_u8; MEMORY_SIZE as usize];
        expected[..4].copy_from_slice(b"head");
        let tail_offset = expected.len() - 4;
        expected[tail_offset..].copy_from_slice(b"tail");
        assert_eq!(std::fs::read(published_path).unwrap(), expected);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn sparse_copy_fallbacks_preserve_extents_and_clone_independence() {
        const MEMORY_SIZE: u64 = 16 * 1024 * 1024;
        const EXTENT_SIZE: u64 = 4096;
        let dir = tempfile::tempdir().unwrap();
        let source_path = dir.path().join("source.bin");
        let mut source = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(source_path)
            .unwrap();
        initialize_snapshot_memory_backing_file(&source, MEMORY_SIZE).unwrap();
        source.write_all(b"head").unwrap();
        source.seek(SeekFrom::End(-4)).unwrap();
        source.write_all(b"tail").unwrap();
        source.sync_all().unwrap();

        let mut expected = vec![0_u8; MEMORY_SIZE as usize];
        let tail_offset = expected.len() - 4;
        expected[..4].copy_from_slice(b"head");
        expected[tail_offset..].copy_from_slice(b"tail");

        let allocated_ranges = [(0, EXTENT_SIZE), (MEMORY_SIZE - EXTENT_SIZE, EXTENT_SIZE)];
        for fallback in ["allocated-ranges", "zero-scan"] {
            let path = dir.path().join(format!("{fallback}.bin"));
            let destination = create_file(&path, fallback).unwrap();
            size_empty_file(&destination, MEMORY_SIZE, fallback).unwrap();
            match fallback {
                "allocated-ranges" => {
                    copy_allocated_ranges(&source, &destination, MEMORY_SIZE, &allocated_ranges)
                        .unwrap()
                }
                "zero-scan" => copy_nonzero_data(&source, &destination, 0, MEMORY_SIZE).unwrap(),
                _ => unreachable!(),
            }
            destination.sync_all().unwrap();
            let allocated = allocated_file_bytes(&destination, MEMORY_SIZE).unwrap();
            assert!(
                allocated < MEMORY_SIZE / 4,
                "{fallback} allocated {allocated} bytes for a {MEMORY_SIZE}-byte file",
            );
        }

        source.seek(SeekFrom::Start(0)).unwrap();
        source.write_all(b"xxxx").unwrap();
        source.sync_all().unwrap();
        for fallback in ["allocated-ranges", "zero-scan"] {
            assert_eq!(
                std::fs::read(dir.path().join(format!("{fallback}.bin"))).unwrap(),
                expected,
                "{fallback}"
            );
        }
    }

    #[test]
    fn exact_memory_handle_survives_path_replacement_before_publish() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        let mem_path = dir.path().join("memory.bin");
        let moved_path = dir.path().join("mapped-memory.bin");
        std::fs::write(&mem_path, vec![0x5a_u8; 1024]).unwrap();
        let memory_file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&mem_path)
            .unwrap();
        std::fs::rename(&mem_path, &moved_path).unwrap();
        std::fs::write(&mem_path, vec![0xa5_u8; 1024]).unwrap();

        write_snapshot_from_memory_file(&snap_dir, &test_manifest(), b"state", &memory_file)
            .unwrap();

        assert_eq!(
            std::fs::read(snap_dir.join(MEMORY_FILE_NAME)).unwrap(),
            vec![0x5a_u8; 1024]
        );
    }

    #[test]
    fn write_snapshot_rejects_existing_destination() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        std::fs::create_dir(&snap_dir).unwrap();
        std::fs::write(snap_dir.join("sentinel"), b"keep").unwrap();
        let mem_path = dir.path().join("memory.bin");
        std::fs::write(&mem_path, vec![0_u8; 1024]).unwrap();

        let err = write_snapshot(&snap_dir, &test_manifest(), b"state", &mem_path).unwrap_err();
        assert!(err.to_string().contains("already exists"));
        assert_eq!(std::fs::read(snap_dir.join("sentinel")).unwrap(), b"keep");
    }

    #[test]
    fn publish_does_not_replace_destination_created_after_staging() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        let mut staging = StagingDirectory::create(dir.path(), &snap_dir).unwrap();
        std::fs::create_dir(&snap_dir).unwrap();
        std::fs::write(snap_dir.join("sentinel"), b"keep").unwrap();

        assert!(staging.publish(&snap_dir).is_err());
        assert_eq!(std::fs::read(snap_dir.join("sentinel")).unwrap(), b"keep");
    }

    #[test]
    fn write_snapshot_rejects_oversized_memory_without_publishing() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        let mem_path = dir.path().join("memory.bin");
        std::fs::write(&mem_path, vec![0_u8; 1025]).unwrap();

        let err = write_snapshot(&snap_dir, &test_manifest(), b"state", &mem_path).unwrap_err();
        assert!(err.to_string().contains("doesn't match manifest"));
        assert!(!snap_dir.exists());
        assert!(std::fs::read_dir(dir.path()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains("staging")
        }));
    }

    #[test]
    fn write_snapshot_rejects_oversized_manifest_without_publishing() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        let mem_path = dir.path().join("memory.bin");
        std::fs::write(&mem_path, vec![0_u8; 1024]).unwrap();
        let mut manifest = test_manifest();
        manifest.openvmm_version = "x".repeat(MAX_MANIFEST_SIZE_BYTES as usize);

        let err = write_snapshot(&snap_dir, &manifest, b"state", &mem_path).unwrap_err();
        assert!(err.to_string().contains("manifest exceeds"));
        assert!(!snap_dir.exists());
    }

    #[test]
    fn read_snapshot_accepts_same_length_state_change_without_legacy_checksum_validation() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        let mem_path = dir.path().join("memory.bin");
        std::fs::write(&mem_path, vec![0_u8; 1024]).unwrap();
        write_snapshot(&snap_dir, &test_manifest(), b"state", &mem_path).unwrap();
        std::fs::write(snap_dir.join(STATE_FILE_NAME), b"other").unwrap();

        let (_, state) = read_snapshot(&snap_dir, 1024).unwrap();
        assert_eq!(state, b"other");
    }

    #[test]
    fn read_snapshot_accepts_same_length_memory_change_without_legacy_checksum_validation() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        let mem_path = dir.path().join("memory.bin");
        std::fs::write(&mem_path, vec![0_u8; 1024]).unwrap();
        write_snapshot(&snap_dir, &test_manifest(), b"state", &mem_path).unwrap();
        std::fs::write(snap_dir.join(MEMORY_FILE_NAME), vec![1_u8; 1024]).unwrap();

        let (_, _, mut memory) = read_snapshot_with_memory(&snap_dir, 1024).unwrap();
        let mut bytes = Vec::new();
        memory.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, vec![1_u8; 1024]);
    }

    #[test]
    fn supplied_manifest_remains_authoritative_during_artifact_open() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        let mem_path = dir.path().join("memory.bin");
        std::fs::write(&mem_path, vec![0_u8; 1024]).unwrap();
        write_snapshot(&snap_dir, &test_manifest(), b"state", &mem_path).unwrap();
        let manifest = read_snapshot_manifest(&snap_dir).unwrap();

        std::fs::write(snap_dir.join(MANIFEST_FILE_NAME), b"replacement").unwrap();
        let (state, mut memory) =
            read_snapshot_artifacts_with_memory(&snap_dir, &manifest, 1024).unwrap();
        let mut bytes = Vec::new();
        memory.read_to_end(&mut bytes).unwrap();

        assert_eq!(state, b"state");
        assert_eq!(bytes, vec![0_u8; 1024]);
    }

    #[test]
    fn read_snapshot_accepts_legacy_v2_manifest_without_verifying_digests() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        let mem_path = dir.path().join("memory.bin");
        std::fs::write(&mem_path, vec![0_u8; 1024]).unwrap();
        write_snapshot(&snap_dir, &test_manifest(), b"state", &mem_path).unwrap();
        let mut manifest = read_snapshot_manifest(&snap_dir).unwrap();
        manifest.version = LEGACY_MANIFEST_VERSION;
        manifest.format_magic = LEGACY_SNAPSHOT_FORMAT_MAGIC.to_vec();
        manifest.state_sha256 = vec![0xa5; SHA256_SIZE];
        manifest.memory_sha256 = vec![0x5a; SHA256_SIZE];
        std::fs::write(
            snap_dir.join(MANIFEST_FILE_NAME),
            mesh::payload::encode(manifest),
        )
        .unwrap();
        std::fs::write(snap_dir.join(STATE_FILE_NAME), b"other").unwrap();
        std::fs::write(snap_dir.join(MEMORY_FILE_NAME), vec![1_u8; 1024]).unwrap();

        let (manifest, state) = read_snapshot(&snap_dir, 1024).unwrap();
        assert_eq!(manifest.version, LEGACY_MANIFEST_VERSION);
        assert_eq!(state, b"other");
    }

    #[test]
    fn read_snapshot_rejects_malformed_legacy_digest_lengths() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        let mem_path = dir.path().join("memory.bin");
        std::fs::write(&mem_path, vec![0_u8; 1024]).unwrap();
        write_snapshot(&snap_dir, &test_manifest(), b"state", &mem_path).unwrap();
        let mut manifest = read_snapshot_manifest(&snap_dir).unwrap();
        manifest.version = LEGACY_MANIFEST_VERSION;
        manifest.format_magic = LEGACY_SNAPSHOT_FORMAT_MAGIC.to_vec();
        manifest.state_sha256 = vec![0; SHA256_SIZE - 1];
        manifest.memory_sha256 = vec![0; SHA256_SIZE];
        std::fs::write(
            snap_dir.join(MANIFEST_FILE_NAME),
            mesh::payload::encode(manifest.clone()),
        )
        .unwrap();
        let error = read_snapshot(&snap_dir, 1024).err().unwrap();
        assert!(error.to_string().contains("state.bin SHA-256 digest"));

        manifest.state_sha256 = vec![0; SHA256_SIZE];
        manifest.memory_sha256 = vec![0; SHA256_SIZE + 1];
        std::fs::write(
            snap_dir.join(MANIFEST_FILE_NAME),
            mesh::payload::encode(manifest),
        )
        .unwrap();
        let error = read_snapshot(&snap_dir, 1024).err().unwrap();
        assert!(error.to_string().contains("memory.bin SHA-256 digest"));
    }

    #[test]
    fn read_snapshot_still_requires_exact_memory_length() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        let mem_path = dir.path().join("memory.bin");
        std::fs::write(&mem_path, vec![0_u8; 1024]).unwrap();
        write_snapshot(&snap_dir, &test_manifest(), b"state", &mem_path).unwrap();
        std::fs::write(snap_dir.join(MEMORY_FILE_NAME), vec![1_u8; 1023]).unwrap();

        let error = read_snapshot(&snap_dir, 1024).err().unwrap();
        assert!(error.to_string().contains("memory.bin size"));
    }

    #[test]
    fn current_manifest_rejects_legacy_payload_digests() {
        let mut manifest = test_manifest();
        manifest.state_sha256 = vec![0; SHA256_SIZE];
        let error = validate_manifest(&manifest, "x86_64", 1024, 2, 4096).unwrap_err();
        assert!(error.to_string().contains("legacy artifact digests"));
    }

    #[test]
    fn previous_v3_manifest_remains_accepted() {
        let mut manifest = test_manifest();
        manifest.version = PREVIOUS_MANIFEST_VERSION;
        manifest.format_magic = PREVIOUS_SNAPSHOT_FORMAT_MAGIC.to_vec();

        validate_manifest(&manifest, "x86_64", 1024, 1, 4096).unwrap();
    }

    #[test]
    fn read_snapshot_rejects_corrupt_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        let mem_path = dir.path().join("memory.bin");
        std::fs::write(&mem_path, vec![0_u8; 1024]).unwrap();
        write_snapshot(&snap_dir, &test_manifest(), b"state", &mem_path).unwrap();
        std::fs::write(snap_dir.join(MANIFEST_FILE_NAME), b"not-a-manifest").unwrap();

        let err = read_snapshot(&snap_dir, 1024).err().unwrap();
        assert!(err.to_string().contains("decode snapshot manifest"));
    }

    #[test]
    fn read_snapshot_rejects_truncated_state() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        let mem_path = dir.path().join("memory.bin");
        std::fs::write(&mem_path, vec![0_u8; 1024]).unwrap();
        write_snapshot(&snap_dir, &test_manifest(), b"state", &mem_path).unwrap();
        std::fs::write(snap_dir.join(STATE_FILE_NAME), b"sta").unwrap();

        let err = read_snapshot(&snap_dir, 1024).err().unwrap();
        assert!(err.to_string().contains("state.bin size"));
    }

    #[test]
    fn read_snapshot_rejects_truncated_memory() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        let mem_path = dir.path().join("memory.bin");
        std::fs::write(&mem_path, vec![0_u8; 1024]).unwrap();
        write_snapshot(&snap_dir, &test_manifest(), b"state", &mem_path).unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(snap_dir.join(MEMORY_FILE_NAME))
            .unwrap()
            .set_len(512)
            .unwrap();

        let err = read_snapshot(&snap_dir, 1024).err().unwrap();
        assert!(err.to_string().contains("memory.bin size"));
    }

    #[test]
    fn read_snapshot_rejects_oversized_state() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        let mem_path = dir.path().join("memory.bin");
        std::fs::write(&mem_path, vec![0_u8; 1024]).unwrap();
        write_snapshot(&snap_dir, &test_manifest(), b"state", &mem_path).unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(snap_dir.join(STATE_FILE_NAME))
            .unwrap()
            .set_len(MAX_SAVED_STATE_SIZE_BYTES + 1)
            .unwrap();

        let err = read_snapshot(&snap_dir, 1024).err().unwrap();
        assert!(err.to_string().contains("saved state is"));
        assert!(err.to_string().contains("exceeding the maximum"));
    }

    #[test]
    fn read_snapshot_rejects_unexpected_artifact() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        let mem_path = dir.path().join("memory.bin");
        std::fs::write(&mem_path, vec![0_u8; 1024]).unwrap();
        write_snapshot(&snap_dir, &test_manifest(), b"state", &mem_path).unwrap();
        std::fs::write(snap_dir.join("extra.bin"), b"unexpected").unwrap();

        let err = read_snapshot(&snap_dir, 1024).err().unwrap();
        assert!(err.to_string().contains("unexpected artifact"));
    }

    #[cfg(unix)]
    #[test]
    fn read_snapshot_rejects_symlinked_artifact() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        let mem_path = dir.path().join("memory.bin");
        std::fs::write(&mem_path, vec![0_u8; 1024]).unwrap();
        write_snapshot(&snap_dir, &test_manifest(), b"state", &mem_path).unwrap();
        let state_path = snap_dir.join(STATE_FILE_NAME);
        let moved_state_path = snap_dir.join("state-target.bin");
        std::fs::rename(&state_path, &moved_state_path).unwrap();
        symlink(&moved_state_path, &state_path).unwrap();

        let err = read_snapshot(&snap_dir, 1024).err().unwrap();
        assert!(
            err.to_string().contains("unexpected artifact")
                || err.to_string().contains("not a regular file")
                || err.to_string().contains("failed to open saved state")
        );
    }

    #[cfg(unix)]
    #[test]
    fn opened_memory_handle_survives_path_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        let mem_path = dir.path().join("memory.bin");
        std::fs::write(&mem_path, vec![0x5a_u8; 1024]).unwrap();
        write_snapshot(&snap_dir, &test_manifest(), b"state", &mem_path).unwrap();

        let (_, _, mut opened_memory) = read_snapshot_with_memory(&snap_dir, 1024).unwrap();
        let original_path = snap_dir.join(MEMORY_FILE_NAME);
        let moved_path = snap_dir.join("opened-memory.bin");
        std::fs::rename(&original_path, &moved_path).unwrap();
        std::fs::write(&original_path, vec![0xa5_u8; 1024]).unwrap();

        let mut bytes = Vec::new();
        opened_memory.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, vec![0x5a_u8; 1024]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn opened_snapshot_survives_directory_path_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        let moved_dir = dir.path().join("opened-snapshot");
        let memory_source = dir.path().join("memory-source.bin");
        std::fs::write(&memory_source, vec![0x5a_u8; 1024]).unwrap();
        write_snapshot(&snap_dir, &test_manifest(), b"state", &memory_source).unwrap();

        let snapshot = OpenedSnapshot::open(&snap_dir).unwrap();
        std::fs::rename(&snap_dir, &moved_dir).unwrap();
        std::fs::write(&memory_source, vec![0xa5_u8; 1024]).unwrap();
        write_snapshot(&snap_dir, &test_manifest(), b"other", &memory_source).unwrap();

        snapshot.validate_memory_generation(1024).unwrap();
        let (_, state, guards) = snapshot.into_parts();
        let mut memory = guards.memory;
        let mut bytes = Vec::new();
        memory.read_to_end(&mut bytes).unwrap();
        assert_eq!(state, b"state");
        assert_eq!(bytes, vec![0x5a_u8; 1024]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn opened_snapshot_detects_same_length_memory_write_before_mapping() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        let memory_source = dir.path().join("memory-source.bin");
        std::fs::write(&memory_source, vec![0_u8; 1024]).unwrap();
        write_snapshot(&snap_dir, &test_manifest(), b"state", &memory_source).unwrap();

        let snapshot = OpenedSnapshot::open(&snap_dir).unwrap();
        std::thread::sleep(std::time::Duration::from_secs(1));
        let mut writer = std::fs::OpenOptions::new()
            .write(true)
            .open(snap_dir.join(MEMORY_FILE_NAME))
            .unwrap();
        writer.write_all(&[1]).unwrap();
        writer.sync_all().unwrap();

        let error = snapshot
            .duplicate_memory_file_for_mapping(1024)
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("memory generation changed after it was opened")
        );
    }

    #[cfg(windows)]
    #[test]
    fn opened_snapshot_denies_mutation_until_guards_drop() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        let moved_dir = dir.path().join("moved");
        let memory_source = dir.path().join("memory-source.bin");
        std::fs::write(&memory_source, vec![0_u8; 1024]).unwrap();
        write_snapshot(&snap_dir, &test_manifest(), b"state", &memory_source).unwrap();

        let snapshot = OpenedSnapshot::open(&snap_dir).unwrap();
        let memory_path = snap_dir.join(MEMORY_FILE_NAME);
        assert!(
            std::fs::OpenOptions::new()
                .write(true)
                .open(&memory_path)
                .is_err()
        );
        assert!(
            std::fs::OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(&memory_path)
                .is_err()
        );
        assert!(std::fs::remove_file(&memory_path).is_err());
        assert!(std::fs::rename(&snap_dir, &moved_dir).is_err());

        drop(snapshot);
        std::fs::rename(&snap_dir, &moved_dir).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn opened_snapshot_rejects_reparse_artifact() {
        use std::os::windows::fs::symlink_file;

        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        let memory_source = dir.path().join("memory-source.bin");
        let replacement = dir.path().join("replacement.bin");
        std::fs::write(&memory_source, vec![0_u8; 1024]).unwrap();
        write_snapshot(&snap_dir, &test_manifest(), b"state", &memory_source).unwrap();
        std::fs::rename(snap_dir.join(MEMORY_FILE_NAME), &replacement).unwrap();
        match symlink_file(&replacement, snap_dir.join(MEMORY_FILE_NAME)) {
            Ok(()) => {}
            Err(error)
                if error.kind() == std::io::ErrorKind::PermissionDenied
                    || error.raw_os_error() == Some(1314) =>
            {
                return;
            }
            Err(error) => panic!("failed to create test symlink: {error}"),
        }

        let error = OpenedSnapshot::open(&snap_dir).err().unwrap();
        assert!(
            error.to_string().contains("reparse point")
                || error.to_string().contains("not a regular file")
        );
    }

    #[test]
    fn read_snapshot_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        // No files written — read should fail.
        let result = read_snapshot(dir.path(), 1024);
        assert!(result.is_err());
    }

    #[test]
    fn validate_manifest_ok() {
        let manifest = test_manifest();
        validate_manifest(&manifest, "x86_64", 1024, 1, 4096).unwrap();
    }

    #[test]
    fn validate_manifest_wrong_arch() {
        let manifest = test_manifest();
        let err = validate_manifest(&manifest, "aarch64", 1024, 1, 4096).unwrap_err();
        assert!(
            err.to_string().contains("architecture"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn validate_manifest_wrong_memory_size() {
        let manifest = test_manifest();
        let err = validate_manifest(&manifest, "x86_64", 9999, 1, 4096).unwrap_err();
        assert!(
            err.to_string().contains("memory size"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn validate_manifest_wrong_vp_count() {
        let manifest = test_manifest();
        let err = validate_manifest(&manifest, "x86_64", 1024, 99, 4096).unwrap_err();
        assert!(
            err.to_string().contains("VP count"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn validate_manifest_wrong_page_size() {
        let manifest = test_manifest();
        let err = validate_manifest(&manifest, "x86_64", 1024, 1, 65536).unwrap_err();
        assert!(
            err.to_string().contains("page size"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn validate_manifest_wrong_version() {
        let mut manifest = test_manifest();
        manifest.version = 999;
        let err = validate_manifest(&manifest, "x86_64", 1024, 1, 4096).unwrap_err();
        assert!(
            err.to_string().contains("version"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn validate_manifest_wrong_magic() {
        let mut manifest = test_manifest();
        manifest.format_magic = b"NOT_OPENVMM".to_vec();
        let err = validate_manifest(&manifest, "x86_64", 1024, 1, 4096).unwrap_err();
        assert!(err.to_string().contains("format magic"));
    }

    #[test]
    fn validate_manifest_wrong_saved_state_schema() {
        let mut manifest = test_manifest();
        manifest.saved_state_schema_version += 1;
        let err = validate_manifest(&manifest, "x86_64", 1024, 1, 4096).unwrap_err();
        assert!(err.to_string().contains("schema version"));
    }

    #[test]
    fn validate_manifest_wrong_saved_state_root() {
        let mut manifest = test_manifest();
        manifest.saved_state_root_type = "other.SavedState".to_owned();
        let err = validate_manifest(&manifest, "x86_64", 1024, 1, 4096).unwrap_err();
        assert!(err.to_string().contains("root type"));
    }
    #[test]
    fn microvm_snapshot_topology_is_canonical() {
        for processor_count in [1, 2, 4, 8] {
            let topology = microvm_snapshot_topology(processor_count).unwrap();
            assert_eq!(topology.sockets, 1);
            assert_eq!(topology.dies_per_socket, 1);
            assert_eq!(topology.cores_per_die, processor_count);
            assert_eq!(topology.threads_per_core, 1);
            assert_eq!(topology.apic_ids, (0..processor_count).collect::<Vec<_>>());
            assert_eq!(MICROVM_PVH_LAYOUT_VERSION, 2);
        }

        for processor_count in [0, 3, 5, 16] {
            assert!(microvm_snapshot_topology(processor_count).is_err());
        }
    }
}
