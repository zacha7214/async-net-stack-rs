//! IPv4 UDP construction/checksums for the echo and traffic-generator examples.
use crate::device::PacketBuf;
use crate::net::{checksum, finish_sum, sum_words};
use std::io;

pub fn ipv4_checksum(source: [u8; 4], destination: [u8; 4], datagram: &[u8]) -> u16 {
    finish_sum(
        sum_words(&source)
            + sum_words(&destination)
            + 17
            + datagram.len() as u64
            + sum_words(datagram),
    )
}

/// Write one complete IPv4/UDP datagram into a pool frame (no Ethernet header).
pub fn build_ipv4(
    buf: &mut PacketBuf,
    source: ([u8; 4], u16),
    destination: ([u8; 4], u16),
    payload: &[u8],
    identification: u16,
) -> io::Result<()> {
    let total = payload
        .len()
        .checked_add(28)
        .filter(|&n| n <= u16::MAX as usize && n <= buf.tail_capacity())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "UDP packet exceeds frame capacity",
            )
        })?;
    buf.set_len(total);
    let p = buf.as_mut_packet();
    p[..28].fill(0);
    p[0] = 0x45;
    p[2..4].copy_from_slice(&(total as u16).to_be_bytes());
    p[4..6].copy_from_slice(&identification.to_be_bytes());
    p[6] = 0x40;
    p[8] = 64;
    p[9] = 17;
    p[12..16].copy_from_slice(&source.0);
    p[16..20].copy_from_slice(&destination.0);
    let ip_sum = checksum(&p[..20]);
    p[10..12].copy_from_slice(&ip_sum.to_be_bytes());
    p[20..22].copy_from_slice(&source.1.to_be_bytes());
    p[22..24].copy_from_slice(&destination.1.to_be_bytes());
    p[24..26].copy_from_slice(&((total - 20) as u16).to_be_bytes());
    p[28..].copy_from_slice(payload);
    let sum = ipv4_checksum(source.0, destination.0, &p[20..]);
    p[26..28].copy_from_slice(&(if sum == 0 { 0xffff } else { sum }).to_be_bytes());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::{Device, LoopbackDevice};
    use crate::net::{LinkLayer, Reply, Responder};
    #[test]
    fn udp_echo_preserves_odd_payload_and_checksum() {
        let mut dev = LoopbackDevice::new();
        let mut buf = dev.alloc().unwrap();
        let a = [10, 0, 0, 1];
        let b = [10, 0, 0, 2];
        build_ipv4(&mut buf, (a, 12345), (b, 9000), b"hello", 3).unwrap();
        let responder = Responder {
            ipv4: b,
            mac: [0; 6],
            udp_port: Some(9000),
        };
        assert_eq!(responder.respond(&mut buf, LinkLayer::Ip), Reply::Udp);
        let p = buf.as_slice();
        assert_eq!(&p[20..24], &[0x23, 0x28, 0x30, 0x39]);
        assert_eq!(&p[28..], b"hello");
        assert_eq!(checksum(&p[..20]), 0);
        assert_eq!(ipv4_checksum(b, a, &p[20..]), 0);
    }
    #[test]
    fn corrupt_and_fragmented_udp_are_not_replied_to() {
        let mut dev = LoopbackDevice::new();
        let mut buf = dev.alloc().unwrap();
        let a = [10, 0, 0, 1];
        let b = [10, 0, 0, 2];
        let responder = Responder {
            ipv4: b,
            mac: [0; 6],
            udp_port: Some(9000),
        };
        build_ipv4(&mut buf, (a, 12345), (b, 9000), b"hello", 3).unwrap();
        buf.as_mut_packet()[28] ^= 1;
        assert_eq!(responder.respond(&mut buf, LinkLayer::Ip), Reply::Malformed);
        build_ipv4(&mut buf, (a, 12345), (b, 9000), b"hello", 3).unwrap();
        let p = buf.as_mut_packet();
        p[6] = 0x20;
        p[10..12].fill(0);
        let c = checksum(&p[..20]);
        p[10..12].copy_from_slice(&c.to_be_bytes());
        assert_eq!(responder.respond(&mut buf, LinkLayer::Ip), Reply::Ignored);
    }
}

/// A validated, borrowed IPv4 UDP datagram. Ethernet framing must be removed
/// by the caller. Options and fragments are intentionally unsupported.
#[derive(Debug, Clone, Copy)]
pub struct Datagram<'a> {
    pub source: std::net::SocketAddrV4,
    pub destination: std::net::SocketAddrV4,
    pub payload: &'a [u8],
}

pub fn parse_ipv4(packet: &[u8]) -> io::Result<Datagram<'_>> {
    let invalid = || {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid or unsupported IPv4 UDP",
        )
    };
    if packet.len() < 28 || packet[0] != 0x45 || packet[9] != 17 || packet[8] == 0 {
        return Err(invalid());
    }
    let total = u16::from_be_bytes([packet[2], packet[3]]) as usize;
    if total < 28
        || total > packet.len()
        || u16::from_be_bytes([packet[6], packet[7]]) & !0x4000 != 0
        || checksum(&packet[..20]) != 0
    {
        return Err(invalid());
    }
    let source: [u8; 4] = packet[12..16].try_into().unwrap();
    let destination: [u8; 4] = packet[16..20].try_into().unwrap();
    let udp = &packet[20..total];
    if u16::from_be_bytes([udp[4], udp[5]]) as usize != udp.len()
        || (udp[6..8] != [0, 0] && ipv4_checksum(source, destination, udp) != 0)
    {
        return Err(invalid());
    }
    Ok(Datagram {
        source: std::net::SocketAddrV4::new(source.into(), u16::from_be_bytes([udp[0], udp[1]])),
        destination: std::net::SocketAddrV4::new(
            destination.into(),
            u16::from_be_bytes([udp[2], udp[3]]),
        ),
        payload: &udp[8..],
    })
}
