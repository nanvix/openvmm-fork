// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Configuration for the VM worker.

pub use smbios_defs::SmbiosBiosOverrides;
pub use smbios_defs::SmbiosConfig;
pub use smbios_defs::SmbiosSystemOverrides;

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
    pub pcie_ecam_below_4gb: bool,
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
    /// Static identity of the optional microVM NIC.
    pub microvm_network: Option<MicrovmNetworkConfig>,
}

/// Static guest-visible network identity for the microVM NIC.
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

/// Fixed microVM virtio-net interrupt on KVM and MSHV.
pub const MICROVM_VIRTIO_NET_KVM_IRQ: u32 = 10;
/// Fixed microVM virtio-net interrupt on WHP.
pub const MICROVM_VIRTIO_NET_WHP_IRQ: u32 = 5;
/// Portable microVM network feature mask.
pub const MICROVM_VIRTIO_NET_FEATURES: u64 = (1 << 5) | (1 << 32);

/// The initial microVM guest ABI version.
pub const MICROVM_ABI_VERSION_1: u32 = 1;
/// ABI-v1 command line owned by the microVM profile.
pub const MICROVM_BASE_COMMAND_LINE: &str = "earlycon=xe9 console=hvc0 reboot=t panic=-1";

/// Command line when the microVM virtio console is present.
pub const MICROVM_CONSOLE_COMMAND_LINE: &str = "earlycon=xe9 console=hvc1 reboot=t panic=-1";
/// Fixed microVM console interrupt.
pub const MICROVM_VIRTIO_CONSOLE_IRQ: u32 = 7;
/// Bounded reconnect timeout for a microVM console client.
pub const MICROVM_CONSOLE_RECONNECT_TIMEOUT_MS: u64 = 5_000;

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
/// ABI-v1 virtio MMIO reservations in stable device order.
pub const MICROVM_VIRTIO_MMIO_BASES: [u64; 4] = [
    MICROVM_VIRTIO_NET_MMIO_BASE,
    MICROVM_VIRTIO_FS_MMIO_BASE,
    MICROVM_VIRTIO_CONSOLE_MMIO_BASE,
    MICROVM_VIRTIO_BLK_MMIO_BASE,
];

fn validate_microvm_virtio_reservations() -> anyhow::Result<()> {
    for (index, base) in MICROVM_VIRTIO_MMIO_BASES.iter().copied().enumerate() {
        let end = base
            .checked_add(MICROVM_VIRTIO_MMIO_LEN)
            .ok_or_else(|| anyhow::anyhow!("microVM virtio MMIO reservation overflows"))?;
        anyhow::ensure!(
            base >= 0xc000_0000 && end <= 0x1_0000_0000,
            "microVM virtio MMIO reservation {index} is outside the fixed aperture"
        );
        if let Some(next) = MICROVM_VIRTIO_MMIO_BASES.get(index + 1) {
            anyhow::ensure!(end <= *next, "microVM virtio MMIO reservations overlap");
        }
    }
    Ok(())
}

/// Appends the fixed ABI-v1 virtio-blk discovery token.
pub fn append_microvm_virtio_blk_discovery(cmdline: &mut String) -> anyhow::Result<()> {
    anyhow::ensure!(
        !cmdline
            .split_ascii_whitespace()
            .any(|token| token.starts_with("virtio_mmio.device=")),
        "microVM command line already contains virtio-mmio discovery"
    );
    use std::fmt::Write as _;
    write!(
        cmdline,
        " virtio_mmio.device={MICROVM_VIRTIO_MMIO_LEN:#x}@{MICROVM_VIRTIO_BLK_MMIO_BASE:#x}:{MICROVM_VIRTIO_BLK_IRQ}"
    )?;
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
    let LoadMode::Pvh { cmdline, .. } = &config.load_mode else {
        anyhow::bail!("microVM ABI version 1 requires PVH load mode");
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
    let base_command_line = if has_console {
        MICROVM_CONSOLE_COMMAND_LINE
    } else {
        MICROVM_BASE_COMMAND_LINE
    };
    let base_tokens = base_command_line
        .split_ascii_whitespace()
        .collect::<Vec<_>>();
    anyhow::ensure!(
        tokens.starts_with(&base_tokens),
        "microVM command line does not begin with the ABI-v1 base tokens"
    );
    for prefix in ["earlycon=", "console=", "virtio_mmio.device="] {
        let count = tokens
            .iter()
            .filter(|token| token.starts_with(prefix))
            .count();
        let expected = if prefix == "virtio_mmio.device=" {
            config.virtio_devices.len()
        } else {
            1
        };
        anyhow::ensure!(
            count == expected,
            "microVM command line has an invalid number of {prefix} tokens"
        );
    }
    let actual_discovery = tokens
        .iter()
        .copied()
        .filter(|token| token.starts_with("virtio_mmio.device="))
        .collect::<Vec<_>>();
    let expected_discovery = config
        .virtio_devices
        .iter()
        .map(|(_, device)| {
            let (base, irq) = match device.id() {
                "virtio-net" => (
                    MICROVM_VIRTIO_NET_MMIO_BASE,
                    microvm_virtio_net_irq(hypervisor_id)?,
                ),
                "virtio-console" => (MICROVM_VIRTIO_CONSOLE_MMIO_BASE, MICROVM_VIRTIO_CONSOLE_IRQ),
                "virtio-blk" => (MICROVM_VIRTIO_BLK_MMIO_BASE, MICROVM_VIRTIO_BLK_IRQ),
                id => anyhow::bail!("unsupported microVM virtio device '{id}'"),
            };
            Ok(format!(
                "virtio_mmio.device={MICROVM_VIRTIO_MMIO_LEN:#x}@{base:#x}:{irq}"
            ))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    anyhow::ensure!(
        actual_discovery == expected_discovery,
        "microVM virtio discovery is not canonical"
    );
    Ok(())
}

/// Builds the ABI-v1 microVM command line and rejects profile-owned user tokens.
pub fn build_microvm_command_line(user_args: &[String]) -> anyhow::Result<String> {
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
            ]
            .iter()
            .any(|reserved| token.starts_with(reserved))
        }) {
            anyhow::bail!(
                "microVM kernel command line cannot override earlycon, console, or virtio-mmio discovery"
            );
        }
    }

    let mut cmdline = MICROVM_BASE_COMMAND_LINE.to_owned();
    for arg in user_args.iter().filter(|arg| !arg.is_empty()) {
        cmdline.push(' ');
        cmdline.push_str(arg);
    }
    if cmdline.len() >= MICROVM_COMMAND_LINE_MAX_SIZE {
        anyhow::bail!("microVM kernel command line exceeds the 64-KiB ABI limit");
    }
    Ok(cmdline)
}

/// Appends canonical fixed-slot virtio discovery for a microVM.
pub fn append_microvm_device_discovery(
    config: &mut Config,
    hypervisor_id: Option<&str>,
) -> anyhow::Result<()> {
    let LoadMode::Pvh { cmdline, .. } = &mut config.load_mode else {
        anyhow::bail!("microVM discovery requires PVH load mode");
    };
    use std::fmt::Write as _;
    for (_, device) in &config.virtio_devices {
        let (base, irq) = match device.id() {
            "virtio-net" => (
                MICROVM_VIRTIO_NET_MMIO_BASE,
                microvm_virtio_net_irq(hypervisor_id)?,
            ),
            "virtio-console" => (MICROVM_VIRTIO_CONSOLE_MMIO_BASE, MICROVM_VIRTIO_CONSOLE_IRQ),
            "virtio-blk" => (MICROVM_VIRTIO_BLK_MMIO_BASE, MICROVM_VIRTIO_BLK_IRQ),
            id => anyhow::bail!("unsupported microVM virtio device '{id}'"),
        };
        write!(
            cmdline,
            " virtio_mmio.device={MICROVM_VIRTIO_MMIO_LEN:#x}@{base:#x}:{irq}"
        )?;
    }
    anyhow::ensure!(
        cmdline.len() < MICROVM_COMMAND_LINE_MAX_SIZE,
        "microVM command line exceeds the 64-KiB limit"
    );
    Ok(())
}

/// The guest-visible machine contract, independent of the hypervisor backend.
#[derive(MeshPayload, Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum MachineProfile {
    /// The standard OpenVMM machine.
    #[default]
    Standard,
    /// The microVM machine.
    Microvm,
}

/// Validates the microVM machine contract. Standard-machine configurations are unchanged.
pub fn validate_machine_config(config: &Config, hypervisor_id: Option<&str>) -> anyhow::Result<()> {
    let MachineProfile::Microvm = config.machine_profile else {
        anyhow::ensure!(
            !matches!(config.load_mode, LoadMode::Pvh { .. }),
            "PVH load mode requires the microVM profile"
        );
        return Ok(());
    };

    validate_microvm_virtio_reservations()?;
    validate_microvm_command_line(config, hypervisor_id)?;
    anyhow::ensure!(
        matches!(config.load_mode, LoadMode::Pvh { .. }),
        "microVM ABI version 1 requires PVH load mode"
    );
    if let Some(hypervisor_id) = hypervisor_id {
        anyhow::ensure!(
            matches!(hypervisor_id, "kvm" | "whp"),
            "microVM ABI version 1 requires the KVM or WHP hypervisor"
        );
    }
    anyhow::ensure!(
        config.processor_topology.proc_count == 1,
        "microVM ABI version 1 requires exactly one vCPU"
    );
    anyhow::ensure!(
        config.processor_topology.vps_per_socket.is_none()
            && config.processor_topology.enable_smt.is_none()
            && matches!(
                &config.processor_topology.arch,
                Some(ArchTopologyConfig::X86(X86TopologyConfig {
                    apic_id_offset: 0,
                    x2apic: X2ApicConfig::Auto,
                }))
            ),
        "microVM ABI version 1 requires its fixed x86 APIC topology"
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

    anyhow::ensure!(
        config.virtio_devices.len() <= 3,
        "microVM ABI version 1 permits at most one virtio-blk device"
    );
    for (bus, device) in &config.virtio_devices {
        anyhow::ensure!(
            *bus == VirtioBus::Mmio
                && matches!(device.id(), "virtio-blk" | "virtio-console" | "virtio-net"),
            "microVM ABI version 1 permits only an MMIO virtio-blk device"
        );
    }
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

/// Default device-assignment MSI IOVA reservation for a physical SMMU
/// implementation that lets the VMM select the range. The base follows the
/// Hyper-V convention; 1 MiB matches Linux's Arm SMMU reservation size.
pub const DEFAULT_DEVICE_ASSIGNMENT_MSI_IOVA_RANGE: MemoryRange =
    MemoryRange::new(0xEFF6_8000..0xF006_8000);

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

/// Isolation-specific settings for Linux direct boot.
#[derive(MeshPayload, Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinuxIsolationConfig {
    /// No isolation-specific loader configuration.
    None,
    /// AMD SEV-SNP loader configuration.
    Snp {
        /// Enables restricted interrupt injection in the SNP VMSA.
        restricted_injection: bool,
    },
}

#[derive(MeshPayload, Debug)]
pub enum LoadMode {
    Linux {
        kernel: File,
        initrd: Option<File>,
        cmdline: String,
        enable_serial: bool,
        isolation: LinuxIsolationConfig,
        boot_mode: LinuxDirectBootMode,
        // Boxed to keep the `Linux` variant from dominating `LoadMode`'s size.
        smbios: Box<SmbiosConfig>,
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
        // Boxed to keep the `Uefi` variant from dominating `LoadMode`'s size.
        // The VM's BIOS GUID is sourced from `smbios.system.uuid`, so UEFI and
        // Linux direct boot share a single UUID origin.
        smbios: Box<SmbiosConfig>,
        enable_vmbus: bool,
        force_dma_bounce: bool,
        enable_hv: bool,
        /// Whether the guest firmware should enable hibernation (S4) support.
        hibernation_enabled: bool,
    },
    Pcat {
        firmware: RomFileLocation,
        boot_order: [PcatBootDevice; 4],
        /// Whether the guest firmware should enable hibernation (S4) support.
        hibernation_enabled: bool,
        // Boxed to keep the `Pcat` variant from dominating `LoadMode`'s size.
        // Only the system UUID and serial number are honored; the PCAT BIOS ROM
        // self-describes everything else, so other overrides are rejected.
        smbios: Box<SmbiosConfig>,
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
