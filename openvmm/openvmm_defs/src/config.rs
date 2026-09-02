// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Configuration for the VM worker.

use guid::Guid;
use input_core::InputData;
use memory_range::MemoryRange;
use mesh::MeshPayload;
use mesh::payload::Protobuf;
use net_backend_resources::mac_address::MacAddress;
use openvmm_pcat_locator::RomFileLocation;
use std::fs::File;
use vm_resource::Resource;
use vm_resource::kind::PciDeviceHandleKind;
use vm_resource::kind::VirtioDeviceHandle;
use vm_resource::kind::VmbusDeviceHandleKind;
use vmgs_resources::VmgsResource;
use vmotherboard::ChipsetDeviceHandle;
use vmotherboard::LegacyPciChipsetDeviceHandle;
use vmotherboard::options::BaseChipsetManifest;
use vmotherboard::options::VmChipsetCapabilities;

#[derive(MeshPayload, Debug)]
pub struct Config {
    pub load_mode: LoadMode,
    pub floppy_disks: Vec<floppy_resources::FloppyDiskConfig>,
    pub ide_disks: Vec<ide_resources::IdeDeviceConfig>,
    pub pcie_root_complexes: Vec<PcieRootComplexConfig>,
    pub pcie_devices: Vec<PcieDeviceConfig>,
    pub pcie_switches: Vec<PcieSwitchConfig>,
    pub pcie_generic_initiators: Vec<PcieGenericInitiatorConfig>,
    pub vpci_devices: Vec<VpciDeviceConfig>,
    pub numa: NumaTopology,
    pub processor_topology: ProcessorTopologyConfig,
    pub hypervisor: HypervisorConfig,
    pub chipset: BaseChipsetManifest,
    pub vmbus: Option<VmbusConfig>,
    pub vtl2_vmbus: Option<VmbusConfig>,
    #[cfg(windows)]
    pub kernel_vmnics: Vec<KernelVmNicConfig>,
    pub input: mesh::Receiver<InputData>,
    pub framebuffer: Option<framebuffer::Framebuffer>,
    pub vga_firmware: Option<RomFileLocation>,
    pub vtl2_gfx: bool,
    pub virtio_devices: Vec<(VirtioBus, Resource<VirtioDeviceHandle>)>,
    #[cfg(windows)]
    pub vpci_resources: Vec<virt_whp::device::DeviceHandle>,
    pub vmgs: Option<VmgsResource>,
    // TODO: move FirmwareEvent somewhere not GED-specific.
    pub firmware_event_send: Option<mesh::Sender<get_resources::ged::FirmwareEvent>>,
    pub debugger_rpc: Option<mesh::Receiver<vmm_core_defs::debug_rpc::DebugRequest>>,
    pub vmbus_devices: Vec<(DeviceVtl, Resource<VmbusDeviceHandleKind>)>,
    pub chipset_devices: Vec<ChipsetDeviceHandle>,
    pub pci_chipset_devices: Vec<LegacyPciChipsetDeviceHandle>,
    pub isa_dma_controller: Option<Resource<vm_resource::kind::IsaDmaControllerHandleKind>>,
    pub chipset_capabilities: VmChipsetCapabilities,
    /// Memory layout sizing for the layout engine. Determines chipset MMIO
    /// range sizes; addresses are allocated dynamically by the resolver.
    pub layout: vmm_core_defs::LayoutConfig,
    // This is used for testing. TODO: resourcify, and also store this in VMGS.
    pub rtc_delta_milliseconds: i64,
    /// The versioned guest-visible machine contract.
    pub machine_profile: MachineProfile,
    /// Static identity for the optional microVM virtio-net device.
    pub microvm_network: Option<MicrovmNetworkConfig>,
    /// Guest-visible policy for the optional microVM virtio-fs device.
    pub microvm_filesystem: Option<MicrovmFilesystemConfig>,
    /// Stable sandbox block-device roles for microVM ABI version 2.
    ///
    /// This list is in virtio-blk device order and is empty for ABI version 1.
    pub microvm_sandbox_blocks: Vec<MicrovmSandboxBlockConfig>,
    /// Whether the effective command line bootstraps the active microVM filesystem.
    pub microvm_filesystem_bootstrap: bool,
}

/// The initial microVM guest ABI version.
pub const MICROVM_ABI_VERSION_1: u32 = 1;
/// The microVM ABI version with fixed sandbox block-device roles and deterministic SMP topology.
pub const MICROVM_ABI_VERSION_2: u32 = 2;

/// Returns whether a processor count is valid for the selected microVM ABI.
pub const fn microvm_processor_count_supported(abi_version: u32, processor_count: u32) -> bool {
    match abi_version {
        MICROVM_ABI_VERSION_1 => processor_count == 1,
        MICROVM_ABI_VERSION_2 => matches!(processor_count, 1 | 2 | 4 | 8),
        _ => false,
    }
}

/// ABI-v1 command line owned by the microVM profile.
pub const MICROVM_BASE_COMMAND_LINE: &str = "earlycon=xe9 console=hvc0 reboot=t panic=-1";
/// ABI-v1 command line when the virtio console is present.
pub const MICROVM_CONSOLE_COMMAND_LINE: &str = "earlycon=xe9 console=hvc1 reboot=t panic=-1";
/// Maximum ABI-v1 command-line size, including its trailing NUL.
pub const MICROVM_COMMAND_LINE_MAX_SIZE: usize = 64 * 1024;
/// Fixed ABI-v1 virtio-blk MMIO base.
pub const MICROVM_VIRTIO_BLK_MMIO_BASE: u64 = 0xd000_3000;
/// Reserved ABI-v1 virtio-net MMIO base.
pub const MICROVM_VIRTIO_NET_MMIO_BASE: u64 = 0xd000_0000;
/// Reserved ABI-v1 virtio-fs MMIO base.
pub const MICROVM_VIRTIO_FS_MMIO_BASE: u64 = 0xd000_1000;
/// Reserved ABI-v1 virtio-console MMIO base.
pub const MICROVM_VIRTIO_CONSOLE_MMIO_BASE: u64 = 0xd000_2000;
/// Fixed ABI-v1 virtio transport window length.
pub const MICROVM_VIRTIO_MMIO_LEN: u64 = 0x1000;
/// Fixed ABI-v1 virtio-blk interrupt.
pub const MICROVM_VIRTIO_BLK_IRQ: u32 = 4;
/// Fixed ABI-v2 virtio-blk interrupt for the runtime lower layer.
///
/// IRQ 8 is exclusively owned by the microVM RTC.
pub const MICROVM_VIRTIO_RUNTIME_BLK_IRQ: u32 = 12;
/// Fixed ABI-v2 virtio-blk interrupt for the custom lower layer.
pub const MICROVM_VIRTIO_CUSTOM_BLK_IRQ: u32 = 9;
/// Fixed ABI-v2 virtio-blk interrupt for the writable scratch layer.
pub const MICROVM_VIRTIO_SCRATCH_BLK_IRQ: u32 = 11;
/// Fixed ABI-v1 virtio-console interrupt.
pub const MICROVM_VIRTIO_CONSOLE_IRQ: u32 = 7;
/// Fixed ABI-v1 virtio-fs interrupt.
pub const MICROVM_VIRTIO_FS_IRQ: u32 = 6;
/// Fixed ABI-v1 virtio-net interrupt on KVM.
pub const MICROVM_VIRTIO_NET_KVM_IRQ: u32 = 10;
/// Fixed ABI-v1 virtio-net interrupt on WHP.
pub const MICROVM_VIRTIO_NET_WHP_IRQ: u32 = 5;
/// Exact ABI-v1 virtio-net feature mask: MAC and virtio version 1.
pub const MICROVM_VIRTIO_NET_FEATURES: u64 = (1 << 5) | (1 << 32);
/// Exact ABI-v1 virtio-fs feature mask: indirect descriptors, event index,
/// virtio version 1, and access-platform.
pub const MICROVM_VIRTIO_FS_FEATURES: u64 = (1 << 28) | (1 << 29) | (1 << 32) | (1 << 33);
/// ABI-v1 client console reconnect timeout.
pub const MICROVM_CONSOLE_RECONNECT_TIMEOUT_MS: u64 = 5_000;
/// ABI-v1 virtio MMIO reservations in stable device order.
pub const MICROVM_VIRTIO_MMIO_BASES: [u64; 4] = [
    MICROVM_VIRTIO_NET_MMIO_BASE,
    MICROVM_VIRTIO_FS_MMIO_BASE,
    MICROVM_VIRTIO_CONSOLE_MMIO_BASE,
    MICROVM_VIRTIO_BLK_MMIO_BASE,
];
/// ABI-v2 fixed sandbox virtio-blk MMIO slots in layer order.
pub const MICROVM_VIRTIO_SANDBOX_BLOCK_MMIO_BASES: [u64; 4] = [
    MICROVM_VIRTIO_BLK_MMIO_BASE,
    0xd000_4000,
    0xd000_5000,
    0xd000_6000,
];
/// Level-triggered ISA IRQs published in the ABI-v1 MADT.
pub const MICROVM_VIRTIO_V1_LEVEL_TRIGGERED_IRQS: [u32; 5] = [
    MICROVM_VIRTIO_BLK_IRQ,
    MICROVM_VIRTIO_NET_WHP_IRQ,
    MICROVM_VIRTIO_FS_IRQ,
    MICROVM_VIRTIO_CONSOLE_IRQ,
    MICROVM_VIRTIO_NET_KVM_IRQ,
];
/// Level-triggered ISA IRQs published in the ABI-v2 MADT.
pub const MICROVM_VIRTIO_V2_LEVEL_TRIGGERED_IRQS: [u32; 8] = [
    MICROVM_VIRTIO_BLK_IRQ,
    MICROVM_VIRTIO_NET_WHP_IRQ,
    MICROVM_VIRTIO_FS_IRQ,
    MICROVM_VIRTIO_CONSOLE_IRQ,
    MICROVM_VIRTIO_CUSTOM_BLK_IRQ,
    MICROVM_VIRTIO_NET_KVM_IRQ,
    MICROVM_VIRTIO_SCRATCH_BLK_IRQ,
    MICROVM_VIRTIO_RUNTIME_BLK_IRQ,
];

/// The stable role of a microVM ABI-v2 sandbox block device.
#[derive(MeshPayload, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum MicrovmSandboxBlockRole {
    /// The lowest, widest-shared read-only layer.
    Distro,
    /// The read-only runtime layer above the distro layer.
    Runtime,
    /// The optional read-only customer layer above the runtime layer.
    Custom,
    /// The writable overlayfs upper and work directories.
    Scratch,
}

impl MicrovmSandboxBlockRole {
    /// Returns the canonical manifest and CLI name of this role.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Distro => "distro",
            Self::Runtime => "runtime",
            Self::Custom => "custom",
            Self::Scratch => "scratch",
        }
    }

    /// Returns the role's fixed ABI-v2 virtio-mmio address.
    pub const fn mmio_base(self) -> u64 {
        MICROVM_VIRTIO_SANDBOX_BLOCK_MMIO_BASES[self.index()]
    }

    /// Returns the role's fixed ABI-v2 interrupt.
    pub const fn irq(self) -> u32 {
        match self {
            Self::Distro => MICROVM_VIRTIO_BLK_IRQ,
            Self::Runtime => MICROVM_VIRTIO_RUNTIME_BLK_IRQ,
            Self::Custom => MICROVM_VIRTIO_CUSTOM_BLK_IRQ,
            Self::Scratch => MICROVM_VIRTIO_SCRATCH_BLK_IRQ,
        }
    }

    /// Returns whether the role must be read-only.
    pub const fn is_read_only(self) -> bool {
        !matches!(self, Self::Scratch)
    }

    const fn index(self) -> usize {
        match self {
            Self::Distro => 0,
            Self::Runtime => 1,
            Self::Custom => 2,
            Self::Scratch => 3,
        }
    }
}

/// Returns the fixed ABI-v2 virtio-blk feature mask for a sandbox role.
pub const fn microvm_sandbox_block_features(role: MicrovmSandboxBlockRole) -> u64 {
    const RING_INDIRECT_DESC: u64 = 1 << 28;
    const RING_EVENT_IDX: u64 = 1 << 29;
    const VERSION_1: u64 = 1 << 32;
    const ACCESS_PLATFORM: u64 = 1 << 33;
    const BLK_SEG_MAX: u64 = 1 << 2;
    const BLK_READ_ONLY: u64 = 1 << 5;
    const BLK_SIZE: u64 = 1 << 6;
    const BLK_FLUSH: u64 = 1 << 9;
    const BLK_TOPOLOGY: u64 = 1 << 10;

    RING_INDIRECT_DESC
        | RING_EVENT_IDX
        | VERSION_1
        | ACCESS_PLATFORM
        | BLK_SEG_MAX
        | BLK_SIZE
        | BLK_FLUSH
        | BLK_TOPOLOGY
        | if role.is_read_only() {
            BLK_READ_ONLY
        } else {
            0
        }
}

/// Returns the IRQs that must be described as level-triggered in a microVM
/// ABI's x86 MADT.
pub fn microvm_level_triggered_irqs(abi_version: u32) -> anyhow::Result<&'static [u32]> {
    match abi_version {
        MICROVM_ABI_VERSION_1 => Ok(&MICROVM_VIRTIO_V1_LEVEL_TRIGGERED_IRQS),
        MICROVM_ABI_VERSION_2 => Ok(&MICROVM_VIRTIO_V2_LEVEL_TRIGGERED_IRQS),
        _ => anyhow::bail!("unsupported microVM ABI version {abi_version}"),
    }
}

/// Returns the level-triggered ISA IRQs encoded in the Xen PVH MP table.
pub fn microvm_pvh_level_triggered_irqs(abi_version: u32) -> anyhow::Result<&'static [u32]> {
    match abi_version {
        MICROVM_ABI_VERSION_1 => Ok(&MICROVM_VIRTIO_V1_LEVEL_TRIGGERED_IRQS),
        MICROVM_ABI_VERSION_2 => Ok(&MICROVM_VIRTIO_V2_LEVEL_TRIGGERED_IRQS),
        _ => anyhow::bail!("unsupported microVM ABI version {abi_version}"),
    }
}

/// The immutable role and access mode of a microVM ABI-v2 sandbox block device.
#[derive(MeshPayload, Clone, Copy, Debug, PartialEq, Eq)]
pub struct MicrovmSandboxBlockConfig {
    /// The fixed guest-visible role and transport location.
    pub role: MicrovmSandboxBlockRole,
    /// Whether writes are rejected by the VMM.
    pub read_only: bool,
}

/// Static guest-visible network identity for the microVM ABI-v1 NIC.
#[derive(MeshPayload, Clone, Debug, PartialEq, Eq)]
pub struct MicrovmNetworkConfig {
    /// Required cross-platform host-network implementation contract.
    pub profile: MicrovmNetworkProfile,
    pub guest_ipv4: std::net::Ipv4Addr,
    pub prefix_length: u8,
    pub derived_gateway_ipv4: std::net::Ipv4Addr,
    pub guest_mac: MacAddress,
    pub gateway_mac: MacAddress,
}

/// Required host-network implementation contract for a microVM NIC.
///
/// Profiles are explicit so snapshots never silently acquire different host
/// networking semantics on another supported hypervisor.
#[derive(MeshPayload, Clone, Copy, Debug, PartialEq, Eq)]
pub enum MicrovmNetworkProfile {
    /// User-mode Consomme NAT on every supported host backend.
    Portable,
}

impl MicrovmNetworkProfile {
    /// Returns the stable command-line and snapshot spelling of this profile.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Portable => "portable",
        }
    }
}

/// Access policy for the microVM ABI-v1 host filesystem.
#[derive(MeshPayload, Clone, Copy, Debug, PartialEq, Eq)]
pub enum MicrovmFilesystemAccess {
    /// Reject guest mutations before invoking host filesystem operations.
    ReadOnly,
    /// Permit the common cross-platform mutation contract.
    ReadWrite,
}

impl MicrovmFilesystemAccess {
    /// Returns the command-line spelling of this policy.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "ro",
            Self::ReadWrite => "rw",
        }
    }

    /// Returns whether host filesystem mutations are allowed.
    pub fn is_read_only(self) -> bool {
        matches!(self, Self::ReadOnly)
    }
}

/// Guest-visible configuration for the microVM ABI-v1 virtio-fs device.
#[derive(MeshPayload, Clone, Debug, PartialEq, Eq)]
pub struct MicrovmFilesystemConfig {
    /// Absolute guest path at which the initramfs mounts the filesystem.
    pub guest_mount_target: String,
    /// Snapshot-authoritative access policy.
    pub access: MicrovmFilesystemAccess,
}

impl MicrovmFilesystemConfig {
    /// Validates and constructs the ABI-v1 filesystem configuration.
    pub fn new(
        guest_mount_target: String,
        access: MicrovmFilesystemAccess,
    ) -> Result<Self, InvalidMicrovmFilesystemConfig> {
        if guest_mount_target.is_empty()
            || !guest_mount_target.starts_with('/')
            || guest_mount_target == "/"
            || guest_mount_target.len() > 4096
            || guest_mount_target.chars().any(|character| {
                character.is_whitespace() || matches!(character, '\0' | '\\' | '=')
            })
            || guest_mount_target
                .split('/')
                .skip(1)
                .any(|component| component.is_empty() || matches!(component, "." | ".."))
        {
            return Err(InvalidMicrovmFilesystemConfig::InvalidGuestTarget(
                guest_mount_target,
            ));
        }
        Ok(Self {
            guest_mount_target,
            access,
        })
    }

    /// Returns the pinned guest bootstrap command-line tokens.
    pub fn command_line_fragment(&self) -> String {
        format!(
            "virtfs_dir={} virtfs_tag=microvm virtfs_mode={}",
            self.guest_mount_target,
            self.access.as_str()
        )
    }
}

/// Error returned for an invalid microVM filesystem specification.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum InvalidMicrovmFilesystemConfig {
    /// The guest mount target is not a canonical absolute Linux path.
    #[error(
        "invalid guest mount target '{0}': expected an absolute non-root Linux path without empty, dot, parent, whitespace, backslash, or '=' components"
    )]
    InvalidGuestTarget(String),
}

impl MicrovmNetworkConfig {
    /// Returns the subnet mask derived from `prefix_length`.
    pub fn netmask(&self) -> std::net::Ipv4Addr {
        std::net::Ipv4Addr::from(u32::MAX << (32 - self.prefix_length))
    }

    /// Returns the pinned NVX guest-bootstrap command-line tokens.
    pub fn command_line_fragment(&self) -> String {
        self.command_line_fragment_with_dns(false)
    }

    /// Returns the pinned bootstrap tokens, optionally including gateway DNS.
    pub fn command_line_fragment_with_dns(&self, gateway_dns: bool) -> String {
        let dns = if gateway_dns {
            format!(" virtnet_dns={}", self.derived_gateway_ipv4)
        } else {
            String::new()
        };
        format!(
            "virtnet_ip={} virtnet_mask={} virtnet_gw={}{}",
            self.guest_ipv4,
            self.netmask(),
            self.derived_gateway_ipv4,
            dns,
        )
    }

    fn derive_mac(address: std::net::Ipv4Addr) -> MacAddress {
        let [_, second, third, fourth] = address.octets();
        MacAddress::new([0x52, 0x54, 0x00, second, third, fourth])
    }
}

/// Error returned for an invalid microVM static network specification.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum InvalidMicrovmNetworkConfig {
    #[error("expected <IPv4>/<prefix>, for example 10.0.0.2/24")]
    InvalidFormat,
    #[error("invalid IPv4 address '{0}'")]
    InvalidAddress(String),
    #[error("invalid IPv4 prefix '{0}'")]
    InvalidPrefix(String),
    #[error("IPv4 prefix /{0} is outside the supported range /1 through /30")]
    PrefixOutOfRange(u8),
    #[error("guest IPv4 address {0} is the subnet network address")]
    NetworkAddress(std::net::Ipv4Addr),
    #[error("guest IPv4 address {0} is the subnet broadcast address")]
    BroadcastAddress(std::net::Ipv4Addr),
    #[error("guest IPv4 address {0} collides with the derived gateway")]
    GatewayCollision(std::net::Ipv4Addr),
}

impl std::str::FromStr for MicrovmNetworkConfig {
    type Err = InvalidMicrovmNetworkConfig;

    fn from_str(spec: &str) -> Result<Self, Self::Err> {
        let (address, prefix) = spec
            .split_once('/')
            .filter(|(_, prefix)| !prefix.contains('/'))
            .ok_or(InvalidMicrovmNetworkConfig::InvalidFormat)?;
        let guest_ipv4 = address
            .parse::<std::net::Ipv4Addr>()
            .map_err(|_| InvalidMicrovmNetworkConfig::InvalidAddress(address.to_owned()))?;
        let prefix_length = prefix
            .parse::<u8>()
            .map_err(|_| InvalidMicrovmNetworkConfig::InvalidPrefix(prefix.to_owned()))?;
        if !(1..=30).contains(&prefix_length) {
            return Err(InvalidMicrovmNetworkConfig::PrefixOutOfRange(prefix_length));
        }

        let mask = u32::MAX << (32 - prefix_length);
        let guest = u32::from(guest_ipv4);
        let network = guest & mask;
        let broadcast = network | !mask;
        if guest == network {
            return Err(InvalidMicrovmNetworkConfig::NetworkAddress(guest_ipv4));
        }
        if guest == broadcast {
            return Err(InvalidMicrovmNetworkConfig::BroadcastAddress(guest_ipv4));
        }

        let derived_gateway_ipv4 = std::net::Ipv4Addr::from(network + 1);
        if guest_ipv4 == derived_gateway_ipv4 {
            return Err(InvalidMicrovmNetworkConfig::GatewayCollision(guest_ipv4));
        }

        Ok(Self {
            profile: MicrovmNetworkProfile::Portable,
            guest_ipv4,
            prefix_length,
            derived_gateway_ipv4,
            guest_mac: Self::derive_mac(guest_ipv4),
            gateway_mac: Self::derive_mac(derived_gateway_ipv4),
        })
    }
}

/// Returns the pinned virtio-net IRQ for the selected microVM backend.
pub fn microvm_virtio_net_irq(hypervisor_id: Option<&str>) -> anyhow::Result<u32> {
    match hypervisor_id {
        Some("kvm" | "mshv") => Ok(MICROVM_VIRTIO_NET_KVM_IRQ),
        Some("whp") => Ok(MICROVM_VIRTIO_NET_WHP_IRQ),
        Some(other) => anyhow::bail!("microVM virtio-net does not support hypervisor '{other}'"),
        None if cfg!(target_os = "linux") => Ok(MICROVM_VIRTIO_NET_KVM_IRQ),
        None if cfg!(windows) => Ok(MICROVM_VIRTIO_NET_WHP_IRQ),
        None => {
            anyhow::bail!("microVM virtio-net requires an explicit KVM, MSHV, or WHP hypervisor")
        }
    }
}

fn validate_microvm_virtio_reservations(abi_version: u32) -> anyhow::Result<()> {
    let bases: &[u64] = match abi_version {
        MICROVM_ABI_VERSION_1 => &MICROVM_VIRTIO_MMIO_BASES,
        MICROVM_ABI_VERSION_2 => &[
            MICROVM_VIRTIO_NET_MMIO_BASE,
            MICROVM_VIRTIO_FS_MMIO_BASE,
            MICROVM_VIRTIO_CONSOLE_MMIO_BASE,
            MICROVM_VIRTIO_SANDBOX_BLOCK_MMIO_BASES[0],
            MICROVM_VIRTIO_SANDBOX_BLOCK_MMIO_BASES[1],
            MICROVM_VIRTIO_SANDBOX_BLOCK_MMIO_BASES[2],
            MICROVM_VIRTIO_SANDBOX_BLOCK_MMIO_BASES[3],
        ],
        _ => anyhow::bail!("unsupported microVM ABI version {abi_version}"),
    };
    for (index, base) in bases.iter().copied().enumerate() {
        let end = base
            .checked_add(MICROVM_VIRTIO_MMIO_LEN)
            .ok_or_else(|| anyhow::anyhow!("microVM virtio MMIO reservation overflows"))?;
        anyhow::ensure!(
            base >= 0xc000_0000 && end <= 0x1_0000_0000,
            "microVM virtio MMIO reservation {index} is outside the fixed aperture"
        );
        if let Some(next) = bases.get(index + 1) {
            anyhow::ensure!(end <= *next, "microVM virtio MMIO reservations overlap");
        }
    }
    Ok(())
}

fn validate_microvm_sandbox_blocks(blocks: &[MicrovmSandboxBlockConfig]) -> anyhow::Result<()> {
    anyhow::ensure!(
        blocks.len() <= MICROVM_VIRTIO_SANDBOX_BLOCK_MMIO_BASES.len(),
        "microVM ABI version 2 permits at most three read-only layers and one writable scratch device"
    );
    for (index, block) in blocks.iter().enumerate() {
        anyhow::ensure!(
            block.read_only == block.role.is_read_only(),
            "microVM sandbox block role {:?} must be {}",
            block.role,
            if block.role.is_read_only() {
                "read-only"
            } else {
                "writable"
            }
        );
        if let Some(previous) = index.checked_sub(1).and_then(|index| blocks.get(index)) {
            anyhow::ensure!(
                previous.role < block.role,
                "microVM sandbox block roles must be unique and in fixed order"
            );
        }
    }
    if !blocks.is_empty() {
        anyhow::ensure!(
            blocks
                .last()
                .is_some_and(|block| block.role == MicrovmSandboxBlockRole::Scratch),
            "microVM sandbox block topology requires a writable scratch device"
        );
    }
    Ok(())
}

/// Appends present virtio devices in fixed-address order.
pub fn append_microvm_virtio_discovery(
    cmdline: &mut String,
    network: Option<(&MicrovmNetworkConfig, u32, bool)>,
    filesystem_slot: bool,
    filesystem: Option<&MicrovmFilesystemConfig>,
    has_console: bool,
    has_block: bool,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        !cmdline
            .split_ascii_whitespace()
            .any(|token| token.starts_with("virtio_mmio.device=")),
        "microVM command line already contains virtio-mmio discovery"
    );
    anyhow::ensure!(
        filesystem.is_none() || filesystem_slot,
        "microVM filesystem policy requires the fixed virtio-fs slot"
    );
    use std::fmt::Write as _;
    if let Some((_, irq, _)) = network {
        anyhow::ensure!(
            matches!(irq, MICROVM_VIRTIO_NET_KVM_IRQ | MICROVM_VIRTIO_NET_WHP_IRQ),
            "microVM virtio-net IRQ {irq} is not part of ABI version 1"
        );
        write!(
            cmdline,
            " virtio_mmio.device={MICROVM_VIRTIO_MMIO_LEN:#x}@{MICROVM_VIRTIO_NET_MMIO_BASE:#x}:{irq}"
        )?;
    }

    if filesystem_slot {
        write!(
            cmdline,
            " virtio_mmio.device={MICROVM_VIRTIO_MMIO_LEN:#x}@{MICROVM_VIRTIO_FS_MMIO_BASE:#x}:{MICROVM_VIRTIO_FS_IRQ}"
        )?;
    }
    if has_console {
        write!(
            cmdline,
            " virtio_mmio.device={MICROVM_VIRTIO_MMIO_LEN:#x}@{MICROVM_VIRTIO_CONSOLE_MMIO_BASE:#x}:{MICROVM_VIRTIO_CONSOLE_IRQ}"
        )?;
    }
    if has_block {
        write!(
            cmdline,
            " virtio_mmio.device={MICROVM_VIRTIO_MMIO_LEN:#x}@{MICROVM_VIRTIO_BLK_MMIO_BASE:#x}:{MICROVM_VIRTIO_BLK_IRQ}"
        )?;
    }
    if let Some((network, _, gateway_dns)) = network {
        write!(
            cmdline,
            " {}",
            network.command_line_fragment_with_dns(gateway_dns)
        )?;
    }
    if let Some(filesystem) = filesystem {
        write!(cmdline, " {}", filesystem.command_line_fragment())?;
    }
    anyhow::ensure!(
        cmdline.len() < MICROVM_COMMAND_LINE_MAX_SIZE,
        "microVM kernel command line exceeds the 64-KiB ABI limit after device discovery"
    );
    Ok(())
}

/// Appends ABI-v2 sandbox virtio devices in fixed-address order.
pub fn append_microvm_v2_virtio_discovery(
    cmdline: &mut String,
    network: Option<(&MicrovmNetworkConfig, u32, bool)>,
    filesystem_slot: bool,
    filesystem: Option<&MicrovmFilesystemConfig>,
    has_console: bool,
    blocks: &[MicrovmSandboxBlockConfig],
) -> anyhow::Result<()> {
    validate_microvm_sandbox_blocks(blocks)?;
    anyhow::ensure!(
        !cmdline
            .split_ascii_whitespace()
            .any(|token| token.starts_with("virtio_mmio.device=")),
        "microVM command line already contains virtio-mmio discovery"
    );
    anyhow::ensure!(
        filesystem.is_none() || filesystem_slot,
        "microVM filesystem policy requires the fixed virtio-fs slot"
    );

    use std::fmt::Write as _;
    if let Some((_, irq, _)) = network {
        anyhow::ensure!(
            matches!(irq, MICROVM_VIRTIO_NET_KVM_IRQ | MICROVM_VIRTIO_NET_WHP_IRQ),
            "microVM virtio-net IRQ {irq} is not part of ABI version 2"
        );
        write!(
            cmdline,
            " virtio_mmio.device={MICROVM_VIRTIO_MMIO_LEN:#x}@{MICROVM_VIRTIO_NET_MMIO_BASE:#x}:{irq}"
        )?;
    }
    if filesystem_slot {
        write!(
            cmdline,
            " virtio_mmio.device={MICROVM_VIRTIO_MMIO_LEN:#x}@{MICROVM_VIRTIO_FS_MMIO_BASE:#x}:{MICROVM_VIRTIO_FS_IRQ}"
        )?;
    }
    if has_console {
        write!(
            cmdline,
            " virtio_mmio.device={MICROVM_VIRTIO_MMIO_LEN:#x}@{MICROVM_VIRTIO_CONSOLE_MMIO_BASE:#x}:{MICROVM_VIRTIO_CONSOLE_IRQ}"
        )?;
    }
    for block in blocks {
        write!(
            cmdline,
            " virtio_mmio.device={MICROVM_VIRTIO_MMIO_LEN:#x}@{:#x}:{}",
            block.role.mmio_base(),
            block.role.irq()
        )?;
    }
    if let Some((network, _, gateway_dns)) = network {
        write!(
            cmdline,
            " {}",
            network.command_line_fragment_with_dns(gateway_dns)
        )?;
    }
    if let Some(filesystem) = filesystem {
        write!(cmdline, " {}", filesystem.command_line_fragment())?;
    }
    anyhow::ensure!(
        cmdline.len() < MICROVM_COMMAND_LINE_MAX_SIZE,
        "microVM kernel command line exceeds the 64-KiB ABI limit after device discovery"
    );
    Ok(())
}

fn validate_microvm_command_line(
    config: &Config,
    hypervisor_id: Option<&str>,
) -> anyhow::Result<()> {
    let MachineProfile::Microvm { abi_version } = config.machine_profile else {
        unreachable!("microVM command-line validation requires the microVM profile");
    };
    let LoadMode::Pvh { cmdline, .. } = &config.load_mode else {
        anyhow::bail!("microVM ABI version {abi_version} requires PVH load mode");
    };
    anyhow::ensure!(
        !cmdline.contains('\0'),
        "microVM command line contains an embedded NUL"
    );
    anyhow::ensure!(
        cmdline.len() < MICROVM_COMMAND_LINE_MAX_SIZE,
        "microVM command line exceeds the 64-KiB ABI limit"
    );

    let tokens = cmdline.split_ascii_whitespace().collect::<Vec<_>>();
    let has_console = config
        .virtio_devices
        .iter()
        .any(|(_, device)| device.id() == "virtio-console");
    let block_count = config
        .virtio_devices
        .iter()
        .filter(|(_, device)| device.id() == "virtio-blk")
        .count();
    let has_block = block_count != 0;
    let has_network = config
        .virtio_devices
        .iter()
        .any(|(_, device)| device.id() == "virtio-net");
    let has_filesystem = config
        .virtio_devices
        .iter()
        .any(|(_, device)| device.id() == "virtiofs");
    anyhow::ensure!(
        has_network == config.microvm_network.is_some(),
        "microVM virtio-net device and static network identity must be configured together"
    );
    anyhow::ensure!(
        config.microvm_filesystem.is_none() || has_filesystem,
        "microVM filesystem policy requires a virtio-fs device"
    );
    anyhow::ensure!(
        !config.microvm_filesystem_bootstrap || config.microvm_filesystem.is_some(),
        "microVM filesystem bootstrap requires an active filesystem policy"
    );
    match abi_version {
        MICROVM_ABI_VERSION_1 => anyhow::ensure!(
            config.microvm_sandbox_blocks.is_empty() && block_count <= 1,
            "microVM ABI version 1 permits at most one unroled virtio-blk device"
        ),
        MICROVM_ABI_VERSION_2 => {
            validate_microvm_sandbox_blocks(&config.microvm_sandbox_blocks)?;
            anyhow::ensure!(
                block_count == config.microvm_sandbox_blocks.len(),
                "microVM ABI version {abi_version} sandbox block roles do not match the virtio-blk device inventory"
            );
        }
        _ => anyhow::bail!("unsupported microVM ABI version {abi_version}"),
    }
    let base_tokens = if has_console {
        MICROVM_CONSOLE_COMMAND_LINE
    } else {
        MICROVM_BASE_COMMAND_LINE
    }
    .split_ascii_whitespace()
    .collect::<Vec<_>>();
    anyhow::ensure!(
        tokens.starts_with(&base_tokens),
        "microVM command line does not begin with the ABI base tokens"
    );
    for prefix in [
        "earlycon=",
        "console=",
        "virtio_mmio.device=",
        "virtnet_ip=",
        "virtnet_mask=",
        "virtnet_gw=",
        "virtfs_dir=",
        "virtfs_tag=",
        "virtfs_mode=",
    ] {
        let count = tokens
            .iter()
            .filter(|token| token.starts_with(prefix))
            .count();
        let expected = match prefix {
            "virtio_mmio.device=" => config.virtio_devices.len(),
            "virtnet_ip=" | "virtnet_mask=" | "virtnet_gw=" => usize::from(has_network),
            "virtfs_dir=" | "virtfs_tag=" | "virtfs_mode=" => {
                usize::from(config.microvm_filesystem_bootstrap)
            }
            _ => 1,
        };
        anyhow::ensure!(
            count == expected,
            "microVM command line has an invalid number of {prefix} tokens"
        );
    }
    let dns_tokens = tokens
        .iter()
        .filter(|token| token.starts_with("virtnet_dns="))
        .copied()
        .collect::<Vec<_>>();
    anyhow::ensure!(
        dns_tokens.len() <= 1,
        "microVM command line has an invalid number of virtnet_dns= tokens"
    );
    if let Some(dns) = dns_tokens.first() {
        let network = config
            .microvm_network
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("microVM DNS bootstrap requires virtio-net"))?;
        anyhow::ensure!(
            **dns == format!("virtnet_dns={}", network.derived_gateway_ipv4),
            "microVM DNS bootstrap does not match the portable gateway"
        );
    }
    let mut expected_discovery = Vec::new();
    if has_network {
        let irq = microvm_virtio_net_irq(hypervisor_id)?;
        expected_discovery.push(format!(
            "virtio_mmio.device={MICROVM_VIRTIO_MMIO_LEN:#x}@{MICROVM_VIRTIO_NET_MMIO_BASE:#x}:{irq}"
        ));
    }
    if has_filesystem {
        expected_discovery.push(format!(
            "virtio_mmio.device={MICROVM_VIRTIO_MMIO_LEN:#x}@{MICROVM_VIRTIO_FS_MMIO_BASE:#x}:{MICROVM_VIRTIO_FS_IRQ}"
        ));
    }
    if has_console {
        expected_discovery.push(format!(
            "virtio_mmio.device={MICROVM_VIRTIO_MMIO_LEN:#x}@{MICROVM_VIRTIO_CONSOLE_MMIO_BASE:#x}:{MICROVM_VIRTIO_CONSOLE_IRQ}"
        ));
    }
    match abi_version {
        MICROVM_ABI_VERSION_1 if has_block => {
            expected_discovery.push(format!(
                "virtio_mmio.device={MICROVM_VIRTIO_MMIO_LEN:#x}@{MICROVM_VIRTIO_BLK_MMIO_BASE:#x}:{MICROVM_VIRTIO_BLK_IRQ}"
            ));
        }
        MICROVM_ABI_VERSION_2 => {
            expected_discovery.extend(config.microvm_sandbox_blocks.iter().map(|block| {
                format!(
                    "virtio_mmio.device={MICROVM_VIRTIO_MMIO_LEN:#x}@{:#x}:{}",
                    block.role.mmio_base(),
                    block.role.irq()
                )
            }));
        }
        _ => {}
    }
    if let Some(network) = &config.microvm_network {
        expected_discovery.extend(
            network
                .command_line_fragment_with_dns(!dns_tokens.is_empty())
                .split_ascii_whitespace()
                .map(str::to_owned),
        );
    }
    if config.microvm_filesystem_bootstrap {
        let filesystem = config
            .microvm_filesystem
            .as_ref()
            .expect("filesystem bootstrap policy was validated above");
        expected_discovery.extend(
            filesystem
                .command_line_fragment()
                .split_ascii_whitespace()
                .map(str::to_owned),
        );
    }
    if !expected_discovery.is_empty() {
        anyhow::ensure!(
            tokens.ends_with(
                &expected_discovery
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
            ),
            "microVM virtio discovery tokens are not in fixed-address order"
        );
    }
    Ok(())
}

/// Builds the ABI-v1 microVM command line and rejects profile-owned user tokens.
pub fn build_microvm_command_line(
    user_args: &[String],
    has_console: bool,
) -> anyhow::Result<String> {
    for arg in user_args {
        if arg.contains('\0') {
            anyhow::bail!("microVM kernel command line contains an embedded NUL");
        }
        if arg.split_ascii_whitespace().any(|token| {
            [
                "earlycon=",
                "console=",
                "virtio_mmio.device=",
                "virtnet_ip=",
                "virtnet_mask=",
                "virtnet_gw=",
                "virtnet_dns=",
                "virtfs_dir=",
                "virtfs_tag=",
                "virtfs_mode=",
                "nvx_snapshot_tier=",
            ]
            .iter()
            .any(|reserved| token.starts_with(reserved))
        }) {
            anyhow::bail!(
                "microVM kernel command line cannot override profile-owned configuration"
            );
        }
    }

    let mut cmdline = if has_console {
        MICROVM_CONSOLE_COMMAND_LINE
    } else {
        MICROVM_BASE_COMMAND_LINE
    }
    .to_owned();
    for arg in user_args.iter().filter(|arg| !arg.is_empty()) {
        cmdline.push(' ');
        cmdline.push_str(arg);
    }
    if cmdline.len() >= MICROVM_COMMAND_LINE_MAX_SIZE {
        anyhow::bail!("microVM kernel command line exceeds the 64-KiB ABI limit");
    }
    Ok(cmdline)
}

/// The guest-visible machine contract, independent of the hypervisor backend.
#[derive(MeshPayload, Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum MachineProfile {
    /// The standard OpenVMM machine.
    #[default]
    Standard,
    /// The microVM machine.
    Microvm { abi_version: u32 },
}

/// Validates the microVM machine contract. Standard-machine configurations are unchanged.
pub fn validate_machine_config(config: &Config, hypervisor_id: Option<&str>) -> anyhow::Result<()> {
    let MachineProfile::Microvm { abi_version } = config.machine_profile else {
        anyhow::ensure!(
            !matches!(config.load_mode, LoadMode::Pvh { .. }),
            "PVH load mode requires the microVM profile"
        );
        anyhow::ensure!(
            config.microvm_network.is_none(),
            "static microVM network identity requires the microVM profile"
        );
        anyhow::ensure!(
            config.microvm_filesystem.is_none(),
            "microVM filesystem policy requires the microVM profile"
        );
        anyhow::ensure!(
            config.microvm_sandbox_blocks.is_empty(),
            "microVM sandbox block roles require the microVM profile"
        );
        return Ok(());
    };

    anyhow::ensure!(
        matches!(abi_version, MICROVM_ABI_VERSION_1 | MICROVM_ABI_VERSION_2),
        "unsupported microVM ABI version {abi_version}"
    );
    validate_microvm_virtio_reservations(abi_version)?;
    validate_microvm_command_line(config, hypervisor_id)?;
    anyhow::ensure!(
        matches!(config.load_mode, LoadMode::Pvh { .. }),
        "microVM ABI version 1 requires PVH load mode"
    );
    if let Some(hypervisor_id) = hypervisor_id {
        anyhow::ensure!(
            matches!(hypervisor_id, "kvm" | "mshv" | "whp"),
            "microVM ABI version 1 requires the KVM, MSHV, or WHP hypervisor"
        );
    }
    anyhow::ensure!(
        microvm_processor_count_supported(abi_version, config.processor_topology.proc_count),
        "microVM ABI version {abi_version} does not support {} vCPUs",
        config.processor_topology.proc_count
    );
    let has_expected_topology = if abi_version == MICROVM_ABI_VERSION_2 {
        config.processor_topology.vps_per_socket == Some(config.processor_topology.proc_count)
            && config.processor_topology.enable_smt == Some(false)
            && matches!(
                &config.processor_topology.arch,
                Some(ArchTopologyConfig::X86(X86TopologyConfig {
                    apic_id_offset: 0,
                    x2apic: X2ApicConfig::Unsupported,
                }))
            )
    } else {
        config.processor_topology.vps_per_socket.is_none()
            && config.processor_topology.enable_smt.is_none()
            && matches!(
                &config.processor_topology.arch,
                Some(ArchTopologyConfig::X86(X86TopologyConfig {
                    apic_id_offset: 0,
                    x2apic: X2ApicConfig::Auto,
                }))
            )
    };
    anyhow::ensure!(
        has_expected_topology,
        "microVM ABI version {abi_version} requires its fixed x86 APIC topology"
    );
    anyhow::ensure!(
        config.numa.nodes.len() == 1 && config.numa.distances.is_empty(),
        "microVM ABI version 1 requires a single NUMA node"
    );
    anyhow::ensure!(
        !config.hypervisor.with_hv
            && config.hypervisor.with_vtl2.is_none()
            && config.hypervisor.with_isolation.is_none()
            && !config.hypervisor.nested_virt,
        "microVM ABI version 1 does not support Hyper-V enlightenments, VTL2, isolation, or nested virtualization"
    );

    let expected_chipset = BaseChipsetManifest {
        with_generic_cmos_rtc: true,
        ..BaseChipsetManifest::empty()
    };
    anyhow::ensure!(
        config.chipset == expected_chipset,
        "microVM ABI version 1 chipset is not the microVM allowlist"
    );
    anyhow::ensure!(
        config.chipset_capabilities.with_ioapic
            && config.chipset_capabilities.with_pic
            && config.chipset_capabilities.with_pit
            && !config.chipset_capabilities.with_generic_isa_dma
            && !config.chipset_capabilities.with_psp
            && !config.chipset_capabilities.with_guest_watchdog
            && !config.chipset_capabilities.with_i440bx_host_pci_bridge,
        "microVM ABI version 1 chipset capabilities do not match the fixed profile"
    );

    let mut chipset_ids = config
        .chipset_devices
        .iter()
        .map(|device| (device.name.as_str(), device.resource.id()))
        .collect::<Vec<_>>();
    chipset_ids.sort_unstable();
    anyhow::ensure!(
        chipset_ids
            == [
                ("ioapic", "generic-ioapic"),
                ("microvm-portb", "microvm-portb"),
                ("microvm-shutdown", "microvm-shutdown"),
                ("microvm-snapshot-request", "microvm-snapshot-request"),
                ("pic", "pic"),
                ("pit", "pit"),
            ],
        "microVM ABI version 1 chipset-device inventory is not exact"
    );

    anyhow::ensure!(
        config.floppy_disks.is_empty() && config.ide_disks.is_empty(),
        "microVM ABI version 1 does not support floppy or IDE devices"
    );
    anyhow::ensure!(
        config.pcie_root_complexes.is_empty()
            && config.pcie_devices.is_empty()
            && config.pcie_switches.is_empty()
            && config.pcie_generic_initiators.is_empty()
            && config.vpci_devices.is_empty()
            && config.pci_chipset_devices.is_empty()
            && config.isa_dma_controller.is_none(),
        "microVM ABI version 1 does not support PCI, PCIe, VPCI, or ISA DMA"
    );
    anyhow::ensure!(
        config.vmbus.is_none() && config.vtl2_vmbus.is_none() && config.vmbus_devices.is_empty(),
        "microVM ABI version 1 does not support VMBus"
    );
    anyhow::ensure!(
        config.framebuffer.is_none() && config.vga_firmware.is_none() && !config.vtl2_gfx,
        "microVM ABI version 1 does not support graphics or VGA firmware"
    );
    anyhow::ensure!(
        config.vmgs.is_none(),
        "microVM ABI version 1 does not support VMGS"
    );
    anyhow::ensure!(
        config.firmware_event_send.is_none() && config.debugger_rpc.is_none(),
        "microVM ABI version 1 does not support firmware or debugger resources"
    );
    anyhow::ensure!(
        config.rtc_delta_milliseconds == 0,
        "microVM ABI version 1 RTC must be anchored directly to UTC"
    );
    #[cfg(windows)]
    anyhow::ensure!(
        config.kernel_vmnics.is_empty() && config.vpci_resources.is_empty(),
        "microVM ABI version 1 does not support kernel NIC or VPCI resources"
    );

    let max_virtio_devices = match abi_version {
        MICROVM_ABI_VERSION_1 => 4,
        MICROVM_ABI_VERSION_2 => 7,
        _ => unreachable!("unsupported ABI was rejected above"),
    };
    anyhow::ensure!(
        config.virtio_devices.len() <= max_virtio_devices,
        "microVM ABI version {abi_version} has too many virtio devices"
    );
    let mut has_network = false;
    let mut has_filesystem = false;
    let mut has_console = false;
    let mut block_count = 0;
    for (bus, device) in &config.virtio_devices {
        anyhow::ensure!(
            *bus == VirtioBus::Mmio,
            "microVM ABI version 1 permits only virtio-mmio devices"
        );
        match device.id() {
            "virtio-net" => anyhow::ensure!(
                !std::mem::replace(&mut has_network, true),
                "microVM ABI version 1 permits only one virtio-net device"
            ),
            "virtiofs" => anyhow::ensure!(
                !std::mem::replace(&mut has_filesystem, true),
                "microVM ABI version 1 permits only one virtio-fs device"
            ),
            "virtio-console" => anyhow::ensure!(
                !std::mem::replace(&mut has_console, true),
                "microVM ABI version 1 permits only one virtio-console device"
            ),
            "virtio-blk" => block_count += 1,
            id => anyhow::bail!(
                "microVM ABI version {abi_version} does not permit virtio device '{id}'"
            ),
        }
    }
    if abi_version == MICROVM_ABI_VERSION_1 {
        anyhow::ensure!(
            block_count <= 1 && config.microvm_sandbox_blocks.is_empty(),
            "microVM ABI version 1 permits only one unroled virtio-blk device"
        );
    } else {
        validate_microvm_sandbox_blocks(&config.microvm_sandbox_blocks)?;
        anyhow::ensure!(
            block_count == config.microvm_sandbox_blocks.len(),
            "microVM ABI version {abi_version} sandbox block roles do not match the virtio-blk device inventory"
        );
    }
    anyhow::ensure!(
        has_network == config.microvm_network.is_some(),
        "microVM virtio-net device and static network identity must be configured together"
    );
    anyhow::ensure!(
        config.microvm_filesystem.is_none() || has_filesystem,
        "microVM filesystem policy requires a virtio-fs device"
    );
    anyhow::ensure!(
        !config.microvm_filesystem_bootstrap || config.microvm_filesystem.is_some(),
        "microVM filesystem bootstrap requires an active filesystem policy"
    );
    anyhow::ensure!(
        config.layout.chipset_low_mmio_size == 1024 * 1024 * 1024
            && config.layout.chipset_high_mmio_size == 0
            && config.layout.vtl2_chipset_mmio_size == 0,
        "microVM ABI version 1 requires the fixed 3-GiB/4-GiB RAM split"
    );
    Ok(())
}

pub const DEFAULT_GIC_DISTRIBUTOR_BASE: u64 = 0xFFFF_0000;
// The KVM in-kernel vGICv3 requires the distributor and redistributor bases be 64KiB aligned.
pub const DEFAULT_GIC_REDISTRIBUTORS_BASE: u64 = if cfg!(target_os = "linux") {
    0xEFFF_0000
} else {
    0xEFFE_E000
};

/// Base address of the guest-visible GIC v2m MSI frame (exposed via the MADT
/// and used by the software v2m SETSPI decoder for emulated devices). This is
/// OpenVMM-emulated MMIO (one 4 KiB page), not shadowed by the hypervisor, so
/// it stays at the conventional address.
pub const DEFAULT_GIC_V2M_MSI_FRAME_BASE: u64 = 0xEFFE_8000;
/// Size of the v2m MSI frame (one 4KB page is the architectural minimum).
pub const GIC_V2M_MSI_FRAME_SIZE: u64 = 0x1000;

/// Base address of the GIC v2m MSI doorbell used for passthrough on the
/// MSHV root/arm64 backend. Registered with the hypervisor as
/// GITS_TRANSLATER_BASE_ADDRESS.
/// The hypervisor shadows a ~64 KiB region at this base,
/// so it uses the Hyper-V convention address 0xEFF6_8000.
pub const DEFAULT_GIC_V2M_DOORBELL_BASE: u64 = 0xEFF6_8000;

/// Base address of the GICv3 ITS MMIO region. Must be 64 KiB aligned,
/// below the v2m frame address, and not overlap other devices.
/// The region extends from this base to base + GIC_ITS_SIZE (128 KiB).
pub const DEFAULT_GIC_ITS_BASE: u64 = 0xEFFC_0000;
/// Size of the ITS MMIO region (control frame + translation frame, 2×64 KiB).
pub const GIC_ITS_SIZE: u64 = 0x2_0000;

/// Default virtual timer PPI (GIC INTID). PPI 4 = INTID 16 + 4 = 20.
/// This is the EL1 virtual timer interrupt used across Hyper-V, KVM, and HVF.
pub const DEFAULT_VIRT_TIMER_PPI: u32 = 20;

/// Default total number of GIC interrupts (SGIs + PPIs + SPIs).
/// Must satisfy KVM constraints: 64 <= n <= 1023, multiple of 32.
/// 992 = 31 × 32 is the largest valid value.
pub const DEFAULT_GIC_NR_IRQS: u32 = 992;

/// Default VMBus PPI (GIC INTID). PPI 2 = INTID 16 + 2 = 18.
pub const DEFAULT_VMBUS_PPI: u32 = 18;

/// How firmware tables are presented to the guest in Linux direct boot.
///
/// On x86, `DeviceTree` is not supported and will be rejected. On aarch64,
/// this selects between a full device tree or an ACPI boot path.
#[derive(MeshPayload, Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinuxDirectBootMode {
    /// Full device tree with all devices described in DT nodes (aarch64 only).
    DeviceTree,
    /// ACPI tables for device discovery. On aarch64, this also synthesizes
    /// an EFI system table so the kernel enters its ACPI code path. On x86,
    /// ACPI tables are always provided via the zero page.
    Acpi,
}

#[derive(MeshPayload, Debug)]
pub enum LoadMode {
    Linux {
        kernel: File,
        initrd: Option<File>,
        cmdline: String,
        enable_serial: bool,
        boot_mode: LinuxDirectBootMode,
    },
    Uefi {
        firmware: File,
        enable_debugging: bool,
        enable_memory_protections: bool,
        disable_frontpage: bool,
        enable_tpm: bool,
        enable_battery: bool,
        enable_serial: bool,
        enable_vpci_boot: bool,
        uefi_console_mode: Option<UefiConsoleMode>,
        default_boot_always_attempt: bool,
        bios_guid: Guid,
        enable_vmbus: bool,
        force_dma_bounce: bool,
        enable_hv: bool,
    },
    Pcat {
        firmware: RomFileLocation,
        boot_order: [PcatBootDevice; 4],
    },
    Igvm {
        file: File,
        cmdline: String,
        vtl2_base_address: Vtl2BaseAddressType,
        com_serial: Option<SerialInformation>,
    },
    None,
    Pvh {
        kernel: File,
        initrd: Option<File>,
        cmdline: String,
    },
}

#[derive(Debug, Clone, Copy, MeshPayload)]
pub struct SerialInformation {
    pub io_port: u16,
    pub irq: u32,
}

/// Different types to specify the base address for the VTL2 region of the IGVM
/// file.
#[derive(Debug, Clone, Copy, MeshPayload)]
pub enum Vtl2BaseAddressType {
    /// Use the addresses specified in the file. The IGVM file does not need to
    /// support relocations.
    File,
    /// Put VTL2 at the specified address. The IGVM file must support
    /// relocations.
    Absolute(u64),
    /// Use the specified range in the supplied MemoryLayout, as the caller has
    /// created a specific range for VTL2. The IGVM file must support
    /// relocations.
    ///
    /// An optional size may be specified to override the size describing VTL2
    /// provided in the IGVM file. It must be larger than the IGVM file provided
    /// size.
    MemoryLayout { size: Option<u64> },
    /// Tell VTL2 to allocate out it's own memory. This will load the file at
    /// the base address specified in the file, and the host will tell VTL2 the
    /// size of memory to allocate for itself.
    ///
    /// An optional size may be specified to override the size describing VTL2
    /// provided in the IGVM file. It must be larger than the IGVM file provided
    /// size.
    Vtl2Allocate { size: Option<u64> },
}

/// Specifies a PCIe MMIO BAR window, either by size (the resolver allocates) or
/// by a fixed location. Fixed locations exist for assigned-device, IOMMU, and
/// physical-topology compatibility.
#[derive(Debug, MeshPayload)]
pub enum PcieMmioRangeConfig {
    /// Dynamically allocate a range of the given size.
    Dynamic {
        /// Size of the range in bytes.
        size: u64,
    },
    /// Use the specified fixed memory range.
    Fixed(MemoryRange),
}

#[derive(Debug, MeshPayload)]
pub struct RootComplexCxlConfig {
    /// HDM window size in bytes for this CXL root complex.
    pub hdm_size: u64,
    /// CFMWS HDM window restrictions bitmask.
    pub hdm_window_restrictions: u16,
}

#[derive(Debug, MeshPayload)]
pub struct PcieRootComplexConfig {
    pub index: u32,
    pub name: String,
    pub segment: u16,
    pub start_bus: u8,
    pub end_bus: u8,
    pub low_mmio: PcieMmioRangeConfig,
    pub high_mmio: PcieMmioRangeConfig,
    pub ports: Vec<PciePortConfig>,
    /// Optional CXL configuration for root-complex CXL mode.
    pub cxl: Option<RootComplexCxlConfig>,
    /// Optional IOMMU for this root complex.
    pub iommu: Option<PcieIommuConfig>,
    /// NUMA node affinity for this root complex. Used to generate `_PXM` in
    /// the ACPI SSDT so the guest OS sees correct NUMA locality for devices
    /// under this root complex.
    pub vnode: Option<u32>,
    /// When true, treat non-zero BAR values found during probing as pinned
    /// addresses. Used for P2P DMA with GPA = HPA.
    pub preserve_bars: bool,
}

/// Configuration for a single PCIe port — either a root-complex root port or a
/// switch downstream port.
#[derive(Debug, MeshPayload)]
pub struct PciePortConfig {
    /// Port name used for topology wiring and lookup.
    pub name: String,
    /// The device/function (`device << 3 | function`) to place this port at on
    /// its bus.
    ///
    /// When `None`, the port is assigned the lowest available devfn. Ports are
    /// assigned in order, so an explicit devfn that collides with a
    /// previously-assigned port (including one assigned automatically) is an
    /// error. Honored for both root-complex root ports and switch downstream
    /// ports.
    pub devfn: Option<u8>,
    /// Enables PCIe hotplug capabilities for this port.
    pub hotplug: bool,
    /// Optional ACS capability bitmask to expose on this port.
    pub acs_capabilities_supported: Option<u16>,
    /// Marks this port as CXL-capable.
    ///
    /// Runtime port construction derives required BAR/subregion layout from
    /// this flag (currently CXL component registers for BAR0).
    pub cxl: bool,
    /// Enables PASID support for functions downstream of this port.
    pub pasid: bool,
}

#[derive(Debug, MeshPayload)]
pub struct PcieSwitchConfig {
    pub name: String,
    pub parent_port: String,
    /// The downstream ports of this switch.
    pub ports: Vec<PciePortConfig>,
}

/// Declares that the device directly behind a named PCIe port (a root port or
/// a switch downstream port) is a generic initiator (GI) for the given NUMA
/// node. Used to generate an SRAT Generic Initiator Affinity structure so the
/// guest attaches the device's memory to that (typically CPU-less) proximity
/// domain.
///
/// The port is resolved against the live topology by port name after switch
/// downstream ports have been enumerated, so it can target devices that sit
/// behind a switch.
#[derive(Debug, MeshPayload)]
pub struct PcieGenericInitiatorConfig {
    /// Name of the PCIe port (root port or switch downstream port) behind
    /// which the generic-initiator device resides.
    pub port_name: String,
    /// NUMA node the device is a generic initiator for.
    pub node: u32,
}

#[derive(Debug, MeshPayload)]
pub struct PcieDeviceConfig {
    pub port_name: String,
    pub resource: Resource<PciDeviceHandleKind>,
}

#[derive(Debug, MeshPayload)]
pub struct VpciDeviceConfig {
    pub vtl: DeviceVtl,
    /// The ID of the device. Vpci devices are identified by a portion of `data2` and `data3` of the
    /// instance ID, which is used to generate the guest-visible device ID.
    pub instance_id: Guid,
    pub resource: Resource<PciDeviceHandleKind>,
    /// NUMA node affinity for this VPCI device.
    pub vnode: Option<u32>,
}

#[derive(Debug, Protobuf)]
pub struct ProcessorTopologyConfig {
    pub proc_count: u32,
    pub vps_per_socket: Option<u32>,
    pub enable_smt: Option<bool>,
    pub arch: Option<ArchTopologyConfig>,
}

#[derive(Debug, Protobuf, Default, Clone)]
pub struct X86TopologyConfig {
    pub apic_id_offset: u32,
    pub x2apic: X2ApicConfig,
}

#[derive(Debug, Default, Copy, Clone, Protobuf)]
pub enum X2ApicConfig {
    #[default]
    /// Support the X2APIC if recommended by the hypervisor or if needed by the
    /// topology configuration.
    Auto,
    /// Support the X2APIC, and automatically enable it if needed to address all
    /// processors.
    Supported,
    /// Do not support the X2APIC.
    Unsupported,
    /// Support and enable the X2APIC.
    Enabled,
}

#[derive(Debug, Protobuf, Default, Clone)]
pub enum PmuGsivConfig {
    #[default]
    /// Use the hypervisor's platform GSIV value for the PMU.
    Platform,
    /// Use the specified GSIV value for the PMU.
    Gsiv(u32),
    /// Disable the PMU.
    Disabled,
}

/// MSI controller selection for aarch64 PCIe interrupt delivery.
#[derive(Debug, Protobuf, Default, Clone)]
pub enum GicMsiConfig {
    /// Automatically select the best available MSI controller:
    /// ITS when the hypervisor supports it, otherwise GICv2m.
    #[default]
    Auto,
    /// Force GICv3 ITS for MSI delivery via LPIs.
    Its,
    /// Force GICv2m for MSI delivery via SPIs.
    V2m {
        /// Number of SPIs to reserve for PCIe MSIs. Defaults to a
        /// platform-specific value when `None`.
        spi_count: Option<u32>,
    },
}

/// IOMMU configuration for a single PCIe root complex.
#[derive(Debug, MeshPayload, Clone)]
pub enum PcieIommuConfig {
    /// AMD IOMMU (AMD-Vi) for x86_64 guests.
    AmdVi,
    /// Arm SMMUv3 for aarch64 guests.
    Smmu {
        /// Enable HW-accelerated nested translation (iommufd). Requires VFIO
        /// devices with `iommu=` behind this SMMU.
        accel: bool,
        /// Output address size (OAS) resolution policy.
        oas: SmmuOas,
    },
    /// Intel VT-d for x86_64 guests.
    IntelVtd,
}

/// Output address size (OAS) policy for an emulated SMMUv3.
#[derive(Debug, MeshPayload, Clone, Copy)]
pub enum SmmuOas {
    /// Advertise a fixed default OAS. See `DEFAULT_AUTO_OAS_BITS` for the
    /// sizing policy and its limits.
    Auto,
    /// Use a fixed OAS in bits (one of 32, 36, 40, 42, 44, 48, 52).
    Fixed(u8),
}

#[derive(Debug, Protobuf, Default, Clone)]
pub struct Aarch64TopologyConfig {
    pub gic_config: Option<GicConfig>,
    pub pmu_gsiv: PmuGsivConfig,
    pub gic_msi: GicMsiConfig,
}

/// GIC configuration for the virtual machine.
///
/// The variant selects the GIC version. `None` inner config means use
/// defaults for that version's addresses.
#[derive(Debug, Protobuf, Clone)]
pub enum GicConfig {
    /// GICv2 with optional address overrides.
    V2(Option<GicV2Config>),
    /// GICv3 with optional address overrides.
    V3(Option<GicV3Config>),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn microvm_snapshot_tier_command_line_token_is_host_owned() {
        assert!(
            build_microvm_command_line(&["nvx_snapshot_tier=platform".to_owned()], false).is_err()
        );
    }

    #[test]
    fn microvm_network_identity_has_portable_profile() {
        let network: MicrovmNetworkConfig = "10.0.0.2/24".parse().unwrap();
        assert_eq!(network.profile, MicrovmNetworkProfile::Portable);
        assert_eq!(network.profile.as_str(), "portable");
    }

    #[test]
    fn microvm_v2_sandbox_block_slots_are_stable() {
        let blocks = [
            MicrovmSandboxBlockConfig {
                role: MicrovmSandboxBlockRole::Distro,
                read_only: true,
            },
            MicrovmSandboxBlockConfig {
                role: MicrovmSandboxBlockRole::Runtime,
                read_only: true,
            },
            MicrovmSandboxBlockConfig {
                role: MicrovmSandboxBlockRole::Custom,
                read_only: true,
            },
            MicrovmSandboxBlockConfig {
                role: MicrovmSandboxBlockRole::Scratch,
                read_only: false,
            },
        ];
        validate_microvm_sandbox_blocks(&blocks).unwrap();

        let mut cmdline = MICROVM_BASE_COMMAND_LINE.to_owned();
        append_microvm_v2_virtio_discovery(&mut cmdline, None, false, None, false, &blocks)
            .unwrap();
        assert_eq!(
            cmdline,
            format!(
                "{MICROVM_BASE_COMMAND_LINE} \
                 virtio_mmio.device=0x1000@0xd0003000:4 \
                virtio_mmio.device=0x1000@0xd0004000:12 \
                 virtio_mmio.device=0x1000@0xd0005000:9 \
                 virtio_mmio.device=0x1000@0xd0006000:11"
            )
        );
    }

    #[test]
    fn microvm_v2_block_irqs_avoid_rtc_and_are_level_triggered() {
        assert_eq!(MICROVM_VIRTIO_RUNTIME_BLK_IRQ, 12);
        assert!(
            !microvm_level_triggered_irqs(MICROVM_ABI_VERSION_2)
                .unwrap()
                .contains(&8)
        );
        for role in [
            MicrovmSandboxBlockRole::Distro,
            MicrovmSandboxBlockRole::Runtime,
            MicrovmSandboxBlockRole::Custom,
            MicrovmSandboxBlockRole::Scratch,
        ] {
            assert!(
                microvm_level_triggered_irqs(MICROVM_ABI_VERSION_2)
                    .unwrap()
                    .contains(&role.irq()),
                "{role:?} IRQ must be level-triggered"
            );
        }
        assert_eq!(
            microvm_level_triggered_irqs(MICROVM_ABI_VERSION_1).unwrap(),
            &[
                MICROVM_VIRTIO_BLK_IRQ,
                MICROVM_VIRTIO_NET_WHP_IRQ,
                MICROVM_VIRTIO_FS_IRQ,
                MICROVM_VIRTIO_CONSOLE_IRQ,
                MICROVM_VIRTIO_NET_KVM_IRQ,
            ]
        );
        assert_eq!(
            microvm_pvh_level_triggered_irqs(MICROVM_ABI_VERSION_2).unwrap(),
            &MICROVM_VIRTIO_V2_LEVEL_TRIGGERED_IRQS
        );
        assert!(microvm_pvh_level_triggered_irqs(3).is_err());
    }

    #[test]
    fn microvm_v2_sandbox_block_validation_rejects_invalid_layouts() {
        assert!(
            validate_microvm_sandbox_blocks(&[MicrovmSandboxBlockConfig {
                role: MicrovmSandboxBlockRole::Scratch,
                read_only: true,
            }])
            .is_err()
        );
        assert!(
            validate_microvm_sandbox_blocks(&[
                MicrovmSandboxBlockConfig {
                    role: MicrovmSandboxBlockRole::Runtime,
                    read_only: true,
                },
                MicrovmSandboxBlockConfig {
                    role: MicrovmSandboxBlockRole::Distro,
                    read_only: true,
                },
                MicrovmSandboxBlockConfig {
                    role: MicrovmSandboxBlockRole::Scratch,
                    read_only: false,
                },
            ])
            .is_err()
        );
        assert!(
            validate_microvm_sandbox_blocks(&[
                MicrovmSandboxBlockConfig {
                    role: MicrovmSandboxBlockRole::Distro,
                    read_only: true,
                },
                MicrovmSandboxBlockConfig {
                    role: MicrovmSandboxBlockRole::Distro,
                    read_only: true,
                },
                MicrovmSandboxBlockConfig {
                    role: MicrovmSandboxBlockRole::Scratch,
                    read_only: false,
                },
            ])
            .is_err()
        );
        assert!(
            validate_microvm_sandbox_blocks(&[
                MicrovmSandboxBlockConfig {
                    role: MicrovmSandboxBlockRole::Distro,
                    read_only: true,
                },
                MicrovmSandboxBlockConfig {
                    role: MicrovmSandboxBlockRole::Runtime,
                    read_only: true,
                },
                MicrovmSandboxBlockConfig {
                    role: MicrovmSandboxBlockRole::Custom,
                    read_only: true,
                },
                MicrovmSandboxBlockConfig {
                    role: MicrovmSandboxBlockRole::Scratch,
                    read_only: false,
                },
                MicrovmSandboxBlockConfig {
                    role: MicrovmSandboxBlockRole::Scratch,
                    read_only: false,
                },
            ])
            .is_err()
        );
    }
}

/// GICv2-specific address configuration.
#[derive(Debug, Protobuf, Clone)]
pub struct GicV2Config {
    pub gic_distributor_base: u64,
    pub cpu_interface_base: u64,
}

/// GICv3-specific address configuration.
#[derive(Debug, Protobuf, Clone)]
pub struct GicV3Config {
    pub gic_distributor_base: u64,
    pub gic_redistributors_base: u64,
}

#[derive(Debug, Protobuf, Clone)]
pub enum ArchTopologyConfig {
    X86(X86TopologyConfig),
    Aarch64(Aarch64TopologyConfig),
}

/// Per-node memory allocation configuration.
#[derive(Debug, Clone, Copy, MeshPayload)]
pub struct MemoryConfig {
    pub mem_size: u64,
    pub prefetch_memory: bool,
    pub private_memory: bool,
    pub transparent_hugepages: bool,
    pub hugepages: bool,
    pub hugepage_size: Option<u64>,
    /// Host physical NUMA node to bind this allocation to (Linux:
    /// `mbind(MPOL_BIND)`). `None` means OS default placement.
    pub host_numa_node: Option<u32>,
}

/// Virtual NUMA topology for the VM.
#[derive(Debug, MeshPayload)]
pub struct NumaTopology {
    /// NUMA nodes. The vnode ID is the index into this vector.
    pub nodes: Vec<NumaNode>,
    /// Inter-node distances for the SLIT. If empty, defaults are used
    /// (10 for self, 20 for cross-node).
    pub distances: Vec<NumaDistance>,
}

/// A single virtual NUMA node.
#[derive(Debug, MeshPayload)]
pub struct NumaNode {
    /// Memory allocation for this node. `None` means a CPU-only or
    /// device-only node.
    pub mem: Option<MemoryConfig>,
    /// VP assignment for this node.
    pub vps: VpAssignment,
}

/// How VPs are assigned to a NUMA node.
#[derive(Debug, MeshPayload)]
pub enum VpAssignment {
    /// Assign VPs to nodes by round-robining sockets over the CPU-bearing
    /// nodes only: a VP with socket ID `vp_index / vps_per_socket` belongs to
    /// the `(vp_index / vps_per_socket) % num_cpu_nodes`-th `FromTopology`
    /// node. `vps_per_socket` comes from `ProcessorTopologyConfig`;
    /// `num_cpu_nodes` is the number of `FromTopology` nodes, so `Empty`
    /// (CPU-less) nodes are skipped and do not affect the distribution.
    FromTopology,
    /// Explicit VP indices assigned to this node.
    Explicit(Vec<u32>),
    /// A CPU-less node: no VPs are assigned to it. Unlike `Explicit`, this
    /// may be combined with `FromTopology` nodes, so a memory- or
    /// device-only node can be declared without forcing every other node to
    /// spell out its VP set.
    Empty,
}

/// An inter-node distance entry for the ACPI SLIT.
#[derive(Debug, MeshPayload)]
pub struct NumaDistance {
    /// Source node index.
    pub src: u32,
    /// Destination node index.
    pub dst: u32,
    /// Distance value (10 = local, 20 = default cross-node, 255 = unreachable).
    pub distance: u8,
}

#[derive(Debug, MeshPayload, Default)]
pub struct VmbusConfig {
    pub vsock_listener: Option<unix_socket::UnixListener>,
    pub vsock_path: Option<String>,
    pub vmbus_max_version: Option<u32>,
    #[cfg(windows)]
    pub vmbusproxy_handle: Option<vmbus_proxy::ProxyHandle>,
    pub vtl2_redirect: bool,
}

#[derive(Debug, MeshPayload, Default)]
pub struct HypervisorConfig {
    pub with_hv: bool,
    pub with_vtl2: Option<Vtl2Config>,
    pub with_isolation: Option<IsolationType>,
    /// Expose hardware virtualization (VMX/SVM) to the guest so that it can run
    /// its own hypervisor. A backend that does not recognize this request
    /// rejects it rather than silently ignoring it (see
    /// `virt::Hypervisor::recognizes_nested_virt`).
    pub nested_virt: bool,
}

#[derive(Debug, MeshPayload)]
pub struct KernelVmNicConfig {
    pub instance_id: Guid,
    pub mac_address: MacAddress,
    pub switch_port_id: SwitchPortId,
}

#[derive(Clone, Debug, MeshPayload)]
pub struct SwitchPortId {
    pub switch: Guid,
    pub port: Guid,
}

pub const DEFAULT_PCAT_BOOT_ORDER: [PcatBootDevice; 4] = [
    PcatBootDevice::Optical,
    PcatBootDevice::HardDrive,
    PcatBootDevice::Network,
    PcatBootDevice::Floppy,
];

#[derive(MeshPayload, Debug, Clone, Copy, PartialEq)]
pub enum PcatBootDevice {
    Floppy,
    HardDrive,
    Optical,
    Network,
}

#[derive(Eq, PartialEq, Debug, Copy, Clone, MeshPayload)]
pub enum VirtioBus {
    Mmio,
    Pci,
}

/// Policy for the partition when mapping VTL0 memory late.
#[derive(Eq, PartialEq, Debug, Copy, Clone, MeshPayload)]
pub enum LateMapVtl0MemoryPolicy {
    /// Halt execution of the VP if VTL0 memory is accessed.
    Halt,
    /// Log the error but emulate the access with the instruction emulator.
    Log,
    /// Inject an exception into the guest.
    InjectException,
}

impl From<LateMapVtl0MemoryPolicy> for virt::LateMapVtl0MemoryPolicy {
    fn from(value: LateMapVtl0MemoryPolicy) -> Self {
        match value {
            LateMapVtl0MemoryPolicy::Halt => virt::LateMapVtl0MemoryPolicy::Halt,
            LateMapVtl0MemoryPolicy::Log => virt::LateMapVtl0MemoryPolicy::Log,
            LateMapVtl0MemoryPolicy::InjectException => {
                virt::LateMapVtl0MemoryPolicy::InjectException
            }
        }
    }
}

/// Configuration for VTL2.
///
/// NOTE: This is distinct from `virt::Vtl2Config` to keep an abstraction
/// between the virt crate and this crate. Users should not be specifying
/// virt crate configuration directly.
#[derive(Debug, Clone, MeshPayload)]
pub struct Vtl2Config {
    /// Enable the VTL0 alias map. This maps VTL0's view of memory in VTL2 at
    /// the highest legal physical address bit.
    pub vtl0_alias_map: bool,
    /// If set, map VTL0 memory late after VTL2 has started. The current
    /// heuristic is to defer mapping VTL0 memory until the first
    /// `HvModifyVtlProtectionMask` hypercall is made.
    pub late_map_vtl0_memory: Option<LateMapVtl0MemoryPolicy>,
}

// Isolation type for a partition.
#[derive(Eq, PartialEq, Debug, Copy, Clone, MeshPayload)]
pub enum IsolationType {
    Vbs,
    Snp,
    Cca,
}

impl From<IsolationType> for virt::IsolationType {
    fn from(value: IsolationType) -> Self {
        match value {
            IsolationType::Vbs => Self::Vbs,
            IsolationType::Snp => Self::Snp,
            IsolationType::Cca => Self::Cca,
        }
    }
}

/// Which VTL to assign a particular device to.
#[derive(Copy, Clone, Debug, PartialEq, Eq, MeshPayload)]
pub enum DeviceVtl {
    Vtl0,
    Vtl1,
    Vtl2,
}

#[derive(Copy, Clone, Debug, MeshPayload)]
pub enum UefiConsoleMode {
    Default,
    Com1,
    Com2,
    None,
}
