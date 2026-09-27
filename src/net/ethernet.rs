//! Untagged Ethernet II headers. VLAN dispatch remains in the original responder.
use crate::device::PacketBuf;
use std::io;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub destination: [u8; 6],
    pub source: [u8; 6],
    pub ether_type: u16,
}

impl Header {
    pub const LEN: usize = 14;

    pub fn parse(bytes: &[u8]) -> Option<Self> {
        let (destination, source, ether_type) = parse(bytes)?;
        Some(Self {
            destination,
            source,
            ether_type,
        })
    }

    pub fn encode(self) -> [u8; Self::LEN] {
        let mut bytes = [0; Self::LEN];
        bytes[..6].copy_from_slice(&self.destination);
        bytes[6..12].copy_from_slice(&self.source);
        bytes[12..].copy_from_slice(&self.ether_type.to_be_bytes());

        bytes
    }

    /// Add this header and zero minimum-frame padding. Failure leaves the
    /// original packet unchanged; allocation remains owned by its device pool.
    pub fn prepend(self, frame: &mut PacketBuf) -> io::Result<()> {
        if frame.data_offset() < Self::LEN || frame.tail_capacity() + Self::LEN < 60 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "insufficient Ethernet headroom or padding capacity",
            ));
        }

        prepend(frame, self.source, self.destination, self.ether_type);
        Ok(())
    }

    /// Remove one Ethernet II header. Payload-specific length validation and
    /// padding removal belong to the next protocol layer.
    pub fn strip(frame: &mut PacketBuf) -> io::Result<Self> {
        let header = Self::parse(frame.as_slice()).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "truncated Ethernet header")
        })?;

        frame.pull_header(Self::LEN);
        Ok(header)
    }
}

pub(super) const HEADER: usize = 14;
pub const IPV4: u16 = 0x0800;
pub const ARP: u16 = 0x0806;
pub const BROADCAST: [u8; 6] = [0xff; 6];

pub(super) fn unicast(mac: [u8; 6]) -> bool {
    mac != [0; 6] && mac[0] & 1 == 0
}

pub(super) fn parse(bytes: &[u8]) -> Option<([u8; 6], [u8; 6], u16)> {
    if bytes.len() < HEADER {
        return None;
    }

    Some((
        bytes[..6].try_into().ok()?,
        bytes[6..12].try_into().ok()?,
        u16::from_be_bytes([bytes[12], bytes[13]]),
    ))
}

/// Caller has checked header space and capacity for minimum-frame padding.
pub(super) fn prepend(frame: &mut PacketBuf, source: [u8; 6], destination: [u8; 6], kind: u16) {
    let header = Header {
        destination,
        source,
        ether_type: kind,
    }
    .encode();
    frame.push_header(&header);

    let len = frame.len();
    if len < 60 {
        frame.set_len(60);
        frame.as_mut_packet()[len..].fill(0);
    }
}
