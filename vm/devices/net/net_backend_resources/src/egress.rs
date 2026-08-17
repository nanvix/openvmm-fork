// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Canonical microVM IPv4 egress policy and packet validation.

use mesh::MeshPayload;
use std::net::Ipv4Addr;
use std::str::FromStr;
use thiserror::Error;

/// A canonical IPv4 network prefix.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, MeshPayload)]
pub struct Ipv4Cidr {
    network: Ipv4Addr,
    prefix_length: u8,
}

impl Ipv4Cidr {
    /// Returns whether `address` belongs to this prefix.
    pub fn contains(self, address: Ipv4Addr) -> bool {
        let mask = prefix_mask(self.prefix_length);
        u32::from(address) & mask == u32::from(self.network)
    }

    /// Returns the canonical network address.
    pub fn network(self) -> Ipv4Addr {
        self.network
    }

    /// Returns the prefix length.
    pub fn prefix_length(self) -> u8 {
        self.prefix_length
    }
}

/// Error returned when parsing an IPv4 address or CIDR policy rule.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ParseIpv4CidrError {
    /// The address portion is not IPv4.
    #[error("invalid IPv4 address '{0}'")]
    InvalidAddress(String),
    /// The prefix portion is not an integer.
    #[error("invalid IPv4 prefix '{0}'")]
    InvalidPrefix(String),
    /// The prefix is greater than 32.
    #[error("IPv4 prefix /{0} is outside the supported range /0 through /32")]
    PrefixOutOfRange(u8),
    /// The rule has more than one prefix separator.
    #[error("invalid IPv4 CIDR '{0}'")]
    InvalidFormat(String),
}

impl FromStr for Ipv4Cidr {
    type Err = ParseIpv4CidrError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (address, prefix) = match value.split_once('/') {
            Some((address, prefix)) if !prefix.contains('/') => (address, Some(prefix)),
            Some(_) => return Err(ParseIpv4CidrError::InvalidFormat(value.to_owned())),
            None => (value, None),
        };
        let address = address
            .parse::<Ipv4Addr>()
            .map_err(|_| ParseIpv4CidrError::InvalidAddress(address.to_owned()))?;
        let prefix_length = prefix
            .map(|prefix| {
                prefix
                    .parse::<u8>()
                    .map_err(|_| ParseIpv4CidrError::InvalidPrefix(prefix.to_owned()))
            })
            .transpose()?
            .unwrap_or(32);
        if prefix_length > 32 {
            return Err(ParseIpv4CidrError::PrefixOutOfRange(prefix_length));
        }
        let network = Ipv4Addr::from(u32::from(address) & prefix_mask(prefix_length));
        Ok(Self {
            network,
            prefix_length,
        })
    }
}

/// An exact IPv4 TCP destination.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, MeshPayload)]
pub struct TcpEndpoint {
    address: Ipv4Addr,
    port: u16,
}

impl TcpEndpoint {
    /// Returns the destination address.
    pub fn address(self) -> Ipv4Addr {
        self.address
    }

    /// Returns the destination TCP port.
    pub fn port(self) -> u16 {
        self.port
    }
}

/// Error returned when parsing an exact TCP endpoint policy rule.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ParseTcpEndpointError {
    /// The endpoint is missing its port separator.
    #[error("expected <IPv4>:<TCP-port>")]
    InvalidFormat,
    /// The address portion is not IPv4.
    #[error("invalid IPv4 address '{0}'")]
    InvalidAddress(String),
    /// The port portion is not a `u16`.
    #[error("invalid TCP port '{0}'")]
    InvalidPort(String),
    /// Port zero is not a connectable endpoint.
    #[error("TCP port must be nonzero")]
    ZeroPort,
}

impl FromStr for TcpEndpoint {
    type Err = ParseTcpEndpointError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (address, port) = value
            .rsplit_once(':')
            .ok_or(ParseTcpEndpointError::InvalidFormat)?;
        let address = address
            .parse::<Ipv4Addr>()
            .map_err(|_| ParseTcpEndpointError::InvalidAddress(address.to_owned()))?;
        let port = port
            .parse::<u16>()
            .map_err(|_| ParseTcpEndpointError::InvalidPort(port.to_owned()))?;
        if port == 0 {
            return Err(ParseTcpEndpointError::ZeroPort);
        }
        Ok(Self { address, port })
    }
}

/// Run-scoped egress policy mode.
#[derive(Clone, Debug, PartialEq, Eq, MeshPayload)]
pub enum EgressPolicyMode {
    /// No packet filtering.
    AllowAll,
    /// Permit only destinations in these IPv4 prefixes.
    AllowList(Vec<Ipv4Cidr>),
    /// Permit IPv4 destinations except those in these prefixes.
    BlockList(Vec<Ipv4Cidr>),
    /// Permit only exact IPv4 TCP destinations.
    TcpEndpoints(Vec<TcpEndpoint>),
}

/// Egress policy bound to one static microVM link.
#[derive(Clone, Debug, PartialEq, Eq, MeshPayload)]
pub struct EgressPolicy {
    guest_ipv4: Ipv4Addr,
    gateway_ipv4: Ipv4Addr,
    mode: EgressPolicyMode,
}

impl EgressPolicy {
    /// Creates a policy and canonicalizes rule order and duplicates.
    pub fn new(guest_ipv4: Ipv4Addr, gateway_ipv4: Ipv4Addr, mut mode: EgressPolicyMode) -> Self {
        match &mut mode {
            EgressPolicyMode::AllowList(rules) | EgressPolicyMode::BlockList(rules) => {
                rules.sort_unstable();
                rules.dedup();
            }
            EgressPolicyMode::TcpEndpoints(endpoints) => {
                endpoints.sort_unstable();
                endpoints.dedup();
            }
            EgressPolicyMode::AllowAll => {}
        }
        Self {
            guest_ipv4,
            gateway_ipv4,
            mode,
        }
    }

    /// Returns the canonical policy mode.
    pub fn mode(&self) -> &EgressPolicyMode {
        &self.mode
    }

    /// Returns the stable manifest name of this policy mode.
    pub fn mode_name(&self) -> &'static str {
        match self.mode {
            EgressPolicyMode::AllowAll => "allow-all",
            EgressPolicyMode::AllowList(_) => "allow-list",
            EgressPolicyMode::BlockList(_) => "block-list",
            EgressPolicyMode::TcpEndpoints(_) => "endpoint",
        }
    }

    /// Returns whether packet parsing and filtering are active.
    pub fn is_active(&self) -> bool {
        !matches!(self.mode, EgressPolicyMode::AllowAll)
    }

    /// Returns whether the gateway DNS proxy is reachable under this policy.
    pub fn allows_gateway_dns(&self) -> bool {
        match &self.mode {
            EgressPolicyMode::AllowAll => true,
            EgressPolicyMode::AllowList(rules) => {
                rules.iter().any(|rule| rule.contains(self.gateway_ipv4))
            }
            EgressPolicyMode::BlockList(rules) => {
                !rules.iter().any(|rule| rule.contains(self.gateway_ipv4))
            }
            EgressPolicyMode::TcpEndpoints(_) => false,
        }
    }

    /// Returns stable bytes suitable for a policy digest.
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.push(1);
        bytes.extend_from_slice(&self.guest_ipv4.octets());
        bytes.extend_from_slice(&self.gateway_ipv4.octets());
        match &self.mode {
            EgressPolicyMode::AllowAll => bytes.push(0),
            EgressPolicyMode::AllowList(rules) => {
                bytes.push(1);
                append_cidrs(&mut bytes, rules);
            }
            EgressPolicyMode::BlockList(rules) => {
                bytes.push(2);
                append_cidrs(&mut bytes, rules);
            }
            EgressPolicyMode::TcpEndpoints(endpoints) => {
                bytes.push(3);
                let mut endpoints = endpoints.clone();
                endpoints.sort_unstable();
                endpoints.dedup();
                for endpoint in endpoints {
                    bytes.extend_from_slice(&endpoint.address.octets());
                    bytes.extend_from_slice(&endpoint.port.to_be_bytes());
                }
            }
        }
        bytes
    }

    /// Validates and authorizes an Ethernet frame before backend submission.
    pub fn authorize_frame(
        &self,
        frame_prefix: &[u8],
        frame_length: usize,
    ) -> Result<(), EgressDenied> {
        if !self.is_active() {
            return Ok(());
        }
        ensure_available(frame_prefix, frame_length, 14, "Ethernet header")?;
        let mut ether_type = read_u16(frame_prefix, 12);
        let mut l3_offset = 14;
        let vlan = matches!(ether_type, 0x8100 | 0x88a8);
        if vlan {
            if matches!(self.mode, EgressPolicyMode::TcpEndpoints(_)) {
                return Err(EgressDenied::VlanDenied);
            }
            ensure_available(frame_prefix, frame_length, 18, "VLAN header")?;
            ether_type = read_u16(frame_prefix, 16);
            l3_offset = 18;
            if matches!(ether_type, 0x8100 | 0x88a8) {
                return Err(EgressDenied::Malformed("stacked VLAN header"));
            }
        }

        match ether_type {
            0x0806 => self.authorize_arp(frame_prefix, frame_length, l3_offset),
            0x0800 => self.authorize_ipv4(frame_prefix, frame_length, l3_offset),
            other => Err(EgressDenied::UnsupportedEtherType(other)),
        }
    }

    fn authorize_arp(
        &self,
        frame: &[u8],
        frame_length: usize,
        offset: usize,
    ) -> Result<(), EgressDenied> {
        ensure_available(frame, frame_length, offset + 28, "ARP packet")?;
        if read_u16(frame, offset) != 1
            || read_u16(frame, offset + 2) != 0x0800
            || frame[offset + 4] != 6
            || frame[offset + 5] != 4
            || !matches!(read_u16(frame, offset + 6), 1 | 2)
        {
            return Err(EgressDenied::Malformed("ARP packet"));
        }
        let sender = read_ipv4(frame, offset + 14);
        let target = read_ipv4(frame, offset + 24);
        if sender != self.guest_ipv4 || target != self.gateway_ipv4 {
            return Err(EgressDenied::ArpDenied);
        }
        Ok(())
    }

    fn authorize_ipv4(
        &self,
        frame: &[u8],
        frame_length: usize,
        offset: usize,
    ) -> Result<(), EgressDenied> {
        ensure_available(frame, frame_length, offset + 20, "IPv4 header")?;
        let version_ihl = frame[offset];
        let header_length = usize::from(version_ihl & 0x0f) * 4;
        if version_ihl >> 4 != 4 || header_length < 20 {
            return Err(EgressDenied::Malformed("IPv4 header"));
        }
        if header_length != 20 {
            return Err(EgressDenied::Ipv4OptionsDenied);
        }
        ensure_available(frame, frame_length, offset + header_length, "IPv4 options")?;
        let total_length = usize::from(read_u16(frame, offset + 2));
        if total_length < header_length
            || offset
                .checked_add(total_length)
                .is_none_or(|end| end > frame_length)
        {
            return Err(EgressDenied::Malformed("IPv4 total length"));
        }
        if !ipv4_checksum_is_valid(&frame[offset..offset + header_length]) {
            return Err(EgressDenied::Malformed("IPv4 checksum"));
        }
        if read_ipv4(frame, offset + 12) != self.guest_ipv4 {
            return Err(EgressDenied::SourceAddressDenied);
        }

        let destination = read_ipv4(frame, offset + 16);
        let fragments = read_u16(frame, offset + 6) & 0x3fff;
        match &self.mode {
            EgressPolicyMode::AllowAll => Ok(()),
            EgressPolicyMode::AllowList(rules) => {
                validate_transport(
                    frame,
                    frame_length,
                    offset,
                    header_length,
                    total_length,
                    fragments,
                )?;
                if rules.iter().any(|rule| rule.contains(destination)) {
                    Ok(())
                } else {
                    Err(EgressDenied::DestinationDenied)
                }
            }
            EgressPolicyMode::BlockList(rules) => {
                validate_transport(
                    frame,
                    frame_length,
                    offset,
                    header_length,
                    total_length,
                    fragments,
                )?;
                if rules.iter().any(|rule| rule.contains(destination)) {
                    Err(EgressDenied::DestinationDenied)
                } else {
                    Ok(())
                }
            }
            EgressPolicyMode::TcpEndpoints(endpoints) => {
                if fragments != 0 || frame[offset + 9] != 6 {
                    return Err(EgressDenied::EndpointDenied);
                }
                let tcp_offset = offset + header_length;
                ensure_available(frame, frame_length, tcp_offset + 20, "TCP header")?;
                if total_length < header_length + 20 {
                    return Err(EgressDenied::Malformed("TCP length"));
                }
                let tcp_header_length = usize::from(frame[tcp_offset + 12] >> 4) * 4;
                if tcp_header_length < 20 || total_length < header_length + tcp_header_length {
                    return Err(EgressDenied::Malformed("TCP header"));
                }
                let destination_port = read_u16(frame, tcp_offset + 2);
                if endpoints.iter().any(|endpoint| {
                    endpoint.address == destination && endpoint.port == destination_port
                }) {
                    Ok(())
                } else {
                    Err(EgressDenied::EndpointDenied)
                }
            }
        }
    }
}

/// Reason an active egress policy rejected a frame.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum EgressDenied {
    /// A required packet structure was truncated or internally inconsistent.
    #[error("malformed {0}")]
    Malformed(&'static str),
    /// The policy does not permit this layer-2 protocol.
    #[error("unsupported Ethernet type {0:#06x}")]
    UnsupportedEtherType(u16),
    /// Exact endpoint mode does not permit VLAN encapsulation.
    #[error("VLAN traffic is denied by exact endpoint policy")]
    VlanDenied,
    /// ARP was not a valid exchange with the configured gateway.
    #[error("ARP traffic is not limited to the configured gateway")]
    ArpDenied,
    /// The packet tried to spoof another source IPv4 address.
    #[error("IPv4 source does not match the configured guest")]
    SourceAddressDenied,
    /// IPv4 options are not supported because source routing can change the
    /// effective destination after policy evaluation.
    #[error("IPv4 options are denied by policy")]
    Ipv4OptionsDenied,
    /// An allow-list or block-list denied the destination.
    #[error("IPv4 destination is denied")]
    DestinationDenied,
    /// Exact endpoint mode denied the protocol, address, or TCP port.
    #[error("traffic does not match an allowed TCP endpoint")]
    EndpointDenied,
}

fn prefix_mask(prefix_length: u8) -> u32 {
    if prefix_length == 0 {
        0
    } else {
        u32::MAX << (32 - prefix_length)
    }
}

fn append_cidrs(bytes: &mut Vec<u8>, rules: &[Ipv4Cidr]) {
    let mut rules = rules.to_vec();
    rules.sort_unstable();
    rules.dedup();
    for rule in rules {
        bytes.extend_from_slice(&rule.network.octets());
        bytes.push(rule.prefix_length);
    }
}

fn ensure_available(
    prefix: &[u8],
    frame_length: usize,
    needed: usize,
    description: &'static str,
) -> Result<(), EgressDenied> {
    if needed > frame_length || needed > prefix.len() {
        Err(EgressDenied::Malformed(description))
    } else {
        Ok(())
    }
}

fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_be_bytes([bytes[offset], bytes[offset + 1]])
}

fn read_ipv4(bytes: &[u8], offset: usize) -> Ipv4Addr {
    Ipv4Addr::new(
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    )
}

fn ipv4_checksum_is_valid(header: &[u8]) -> bool {
    let mut sum = header
        .chunks_exact(2)
        .fold(0u32, |sum, word| sum + u32::from(read_u16(word, 0)));
    while sum > u32::from(u16::MAX) {
        sum = (sum & u32::from(u16::MAX)) + (sum >> 16);
    }
    sum == u32::from(u16::MAX)
}

fn validate_transport(
    frame: &[u8],
    frame_length: usize,
    ip_offset: usize,
    ip_header_length: usize,
    ip_total_length: usize,
    fragments: u16,
) -> Result<(), EgressDenied> {
    if fragments != 0 {
        return Ok(());
    }
    let transport_offset = ip_offset + ip_header_length;
    let payload_length = ip_total_length - ip_header_length;
    match frame[ip_offset + 9] {
        6 => {
            ensure_available(frame, frame_length, transport_offset + 20, "TCP header")?;
            if payload_length < 20 {
                return Err(EgressDenied::Malformed("TCP length"));
            }
            let header_length = usize::from(frame[transport_offset + 12] >> 4) * 4;
            if header_length < 20 || header_length > payload_length {
                return Err(EgressDenied::Malformed("TCP header"));
            }
        }
        17 => {
            ensure_available(frame, frame_length, transport_offset + 8, "UDP header")?;
            let length = usize::from(read_u16(frame, transport_offset + 4));
            if length < 8 || length > payload_length {
                return Err(EgressDenied::Malformed("UDP length"));
            }
        }
        1 => {
            ensure_available(frame, frame_length, transport_offset + 8, "ICMP header")?;
            if payload_length < 8 {
                return Err(EgressDenied::Malformed("ICMP length"));
            }
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    fn set_ipv4_checksum(frame: &mut [u8], ip_offset: usize) {
        frame[ip_offset + 10..ip_offset + 12].fill(0);
        let header_length = usize::from(frame[ip_offset] & 0x0f) * 4;
        let header = &frame[ip_offset..ip_offset + header_length];
        let mut sum = header
            .chunks_exact(2)
            .fold(0u32, |sum, word| sum + u32::from(read_u16(word, 0)));
        while sum > u32::from(u16::MAX) {
            sum = (sum & u32::from(u16::MAX)) + (sum >> 16);
        }
        frame[ip_offset + 10..ip_offset + 12].copy_from_slice(&(!(sum as u16)).to_be_bytes());
    }

    fn tcp_frame(destination: Ipv4Addr, destination_port: u16) -> Vec<u8> {
        let mut frame = vec![0u8; 14 + 20 + 20];
        frame[12..14].copy_from_slice(&0x0800u16.to_be_bytes());
        let ip = &mut frame[14..];
        ip[0] = 0x45;
        ip[2..4].copy_from_slice(&40u16.to_be_bytes());
        ip[6..8].copy_from_slice(&0x4000u16.to_be_bytes());
        ip[8] = 64;
        ip[9] = 6;
        ip[12..16].copy_from_slice(&Ipv4Addr::new(10, 0, 0, 2).octets());
        ip[16..20].copy_from_slice(&destination.octets());
        ip[20..22].copy_from_slice(&12345u16.to_be_bytes());
        ip[22..24].copy_from_slice(&destination_port.to_be_bytes());
        ip[32] = 5 << 4;
        set_ipv4_checksum(&mut frame, 14);
        frame
    }

    fn udp_frame(destination: Ipv4Addr, destination_port: u16) -> Vec<u8> {
        let mut frame = vec![0u8; 14 + 20 + 8];
        frame[12..14].copy_from_slice(&0x0800u16.to_be_bytes());
        let ip = &mut frame[14..];
        ip[0] = 0x45;
        ip[2..4].copy_from_slice(&28u16.to_be_bytes());
        ip[6..8].copy_from_slice(&0x4000u16.to_be_bytes());
        ip[8] = 64;
        ip[9] = 17;
        ip[12..16].copy_from_slice(&Ipv4Addr::new(10, 0, 0, 2).octets());
        ip[16..20].copy_from_slice(&destination.octets());
        ip[20..22].copy_from_slice(&12345u16.to_be_bytes());
        ip[22..24].copy_from_slice(&destination_port.to_be_bytes());
        ip[24..26].copy_from_slice(&8u16.to_be_bytes());
        set_ipv4_checksum(&mut frame, 14);
        frame
    }

    fn gateway_arp(target: Ipv4Addr) -> Vec<u8> {
        let mut frame = vec![0u8; 42];
        frame[12..14].copy_from_slice(&0x0806u16.to_be_bytes());
        frame[14..16].copy_from_slice(&1u16.to_be_bytes());
        frame[16..18].copy_from_slice(&0x0800u16.to_be_bytes());
        frame[18] = 6;
        frame[19] = 4;
        frame[20..22].copy_from_slice(&1u16.to_be_bytes());
        frame[28..32].copy_from_slice(&Ipv4Addr::new(10, 0, 0, 2).octets());
        frame[38..42].copy_from_slice(&target.octets());
        frame
    }

    #[test]
    fn policy_rules_are_parsed_and_canonicalized() {
        let policy = EgressPolicy::new(
            Ipv4Addr::new(10, 0, 0, 2),
            Ipv4Addr::new(10, 0, 0, 1),
            EgressPolicyMode::AllowList(vec![
                "192.168.1.9/24".parse().unwrap(),
                "10.0.0.1".parse().unwrap(),
                "192.168.1.0/24".parse().unwrap(),
            ]),
        );
        let EgressPolicyMode::AllowList(rules) = policy.mode() else {
            panic!("unexpected policy mode")
        };
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[1].network(), Ipv4Addr::new(192, 168, 1, 0));
        assert!(policy.allows_gateway_dns());
    }

    #[test]
    fn exact_endpoint_policy_filters_before_transmission() {
        let policy = EgressPolicy::new(
            Ipv4Addr::new(10, 0, 0, 2),
            Ipv4Addr::new(10, 0, 0, 1),
            EgressPolicyMode::TcpEndpoints(vec!["192.0.2.7:443".parse().unwrap()]),
        );
        let allowed = tcp_frame(Ipv4Addr::new(192, 0, 2, 7), 443);
        policy.authorize_frame(&allowed, allowed.len()).unwrap();

        let denied = tcp_frame(Ipv4Addr::new(192, 0, 2, 7), 80);
        assert_eq!(
            policy.authorize_frame(&denied, denied.len()),
            Err(EgressDenied::EndpointDenied)
        );
        let mut malformed = allowed.clone();
        malformed[24] ^= 1;
        assert_eq!(
            policy.authorize_frame(&malformed, malformed.len()),
            Err(EgressDenied::Malformed("IPv4 checksum"))
        );

        let mut with_options = allowed.clone();
        with_options.splice(34..34, [1, 1, 1, 0]);
        with_options[14] = 0x46;
        with_options[16..18].copy_from_slice(&44u16.to_be_bytes());
        set_ipv4_checksum(&mut with_options, 14);
        assert_eq!(
            policy.authorize_frame(&with_options, with_options.len()),
            Err(EgressDenied::Ipv4OptionsDenied)
        );

        let udp = udp_frame(Ipv4Addr::new(192, 0, 2, 7), 443);
        assert_eq!(
            policy.authorize_frame(&udp, udp.len()),
            Err(EgressDenied::EndpointDenied)
        );
        let mut ipv6 = vec![0u8; 14 + 40];
        ipv6[12..14].copy_from_slice(&0x86ddu16.to_be_bytes());
        assert_eq!(
            policy.authorize_frame(&ipv6, ipv6.len()),
            Err(EgressDenied::UnsupportedEtherType(0x86dd))
        );
        let mut vlan = vec![0u8; allowed.len() + 4];
        vlan[..12].copy_from_slice(&allowed[..12]);
        vlan[12..14].copy_from_slice(&0x8100u16.to_be_bytes());
        vlan[16..18].copy_from_slice(&0x0800u16.to_be_bytes());
        vlan[18..].copy_from_slice(&allowed[14..]);
        assert_eq!(
            policy.authorize_frame(&vlan, vlan.len()),
            Err(EgressDenied::VlanDenied)
        );
    }

    #[test]
    fn list_policies_filter_destinations_and_allow_only_gateway_arp() {
        let allowed_destination = Ipv4Addr::new(192, 0, 2, 7);
        let blocked_destination = Ipv4Addr::new(198, 51, 100, 9);
        let allow = EgressPolicy::new(
            Ipv4Addr::new(10, 0, 0, 2),
            Ipv4Addr::new(10, 0, 0, 1),
            EgressPolicyMode::AllowList(vec!["192.0.2.0/24".parse().unwrap()]),
        );
        let allowed = tcp_frame(allowed_destination, 80);
        allow.authorize_frame(&allowed, allowed.len()).unwrap();
        let denied = tcp_frame(blocked_destination, 80);
        assert_eq!(
            allow.authorize_frame(&denied, denied.len()),
            Err(EgressDenied::DestinationDenied)
        );

        let block = EgressPolicy::new(
            Ipv4Addr::new(10, 0, 0, 2),
            Ipv4Addr::new(10, 0, 0, 1),
            EgressPolicyMode::BlockList(vec!["198.51.100.0/24".parse().unwrap()]),
        );
        block.authorize_frame(&allowed, allowed.len()).unwrap();
        assert_eq!(
            block.authorize_frame(&denied, denied.len()),
            Err(EgressDenied::DestinationDenied)
        );

        let gateway_request = gateway_arp(Ipv4Addr::new(10, 0, 0, 1));
        allow
            .authorize_frame(&gateway_request, gateway_request.len())
            .unwrap();
        let other_arp = gateway_arp(Ipv4Addr::new(10, 0, 0, 9));
        assert_eq!(
            allow.authorize_frame(&other_arp, other_arp.len()),
            Err(EgressDenied::ArpDenied)
        );
    }

    #[test]
    fn allow_all_does_not_parse_guest_frames() {
        let policy = EgressPolicy::new(
            Ipv4Addr::UNSPECIFIED,
            Ipv4Addr::UNSPECIFIED,
            EgressPolicyMode::AllowAll,
        );
        policy.authorize_frame(&[], 0).unwrap();
    }
}
