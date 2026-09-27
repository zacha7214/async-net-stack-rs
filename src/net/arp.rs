//! Ethernet/IPv4 ARP wire format (RFC 826). Offsets are relative to the ARP
//! message, not the Ethernet frame. Parsing never learns a neighbor by itself.
use std::net::Ipv4Addr;

pub const LEN: usize = 28;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation {
    Request,
    Reply,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Packet {
    pub operation: Operation,
    pub sender_mac: [u8; 6],
    pub sender_ip: Ipv4Addr,
    pub target_mac: [u8; 6],
    pub target_ip: Ipv4Addr,
}

impl Packet {
    /// Accept Ethernet padding after the fixed Ethernet/IPv4 ARP message.
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < LEN || bytes[..6] != [0, 1, 8, 0, 6, 4] {
            return None;
        }
        let operation = match u16::from_be_bytes([bytes[6], bytes[7]]) {
            1 => Operation::Request,
            2 => Operation::Reply,
            _ => return None,
        };

        Some(Self {
            operation,
            sender_mac: bytes[8..14].try_into().ok()?,
            sender_ip: Ipv4Addr::from(<[u8; 4]>::try_from(&bytes[14..18]).ok()?),
            target_mac: bytes[18..24].try_into().ok()?,
            target_ip: Ipv4Addr::from(<[u8; 4]>::try_from(&bytes[24..28]).ok()?),
        })
    }

    pub fn encode(self) -> [u8; LEN] {
        let mut bytes = [0; LEN];
        bytes[..6].copy_from_slice(&[0, 1, 8, 0, 6, 4]);
        bytes[6..8].copy_from_slice(
            &(match self.operation {
                Operation::Request => 1u16,
                Operation::Reply => 2u16,
            })
            .to_be_bytes(),
        );

        bytes[8..14].copy_from_slice(&self.sender_mac);
        bytes[14..18].copy_from_slice(&self.sender_ip.octets());
        bytes[18..24].copy_from_slice(&self.target_mac);
        bytes[24..28].copy_from_slice(&self.target_ip.octets());

        bytes
    }
}
