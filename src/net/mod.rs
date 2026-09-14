//! Stateless IPv4 packet handling for datapath experiments. No routing, TCP,
//! IP reassembly, or neighbor cache yet. All parsing is checked before edits.
use crate::device::PacketBuf;

/// Internet checksum, including odd-length payloads. Returns zero when a
/// complete checksummed message is valid.
pub fn checksum(bytes: &[u8]) -> u16 {
    finish_sum(sum_words(bytes))
}
pub(crate) fn sum_words(bytes: &[u8]) -> u64 {
    let mut sum = 0u64;
    let mut pairs = bytes.chunks_exact(2);
    for p in &mut pairs {
        sum += u16::from_be_bytes([p[0], p[1]]) as u64;
    }
    if let Some(&last) = pairs.remainder().first() {
        sum += (last as u64) << 8;
    }
    sum
}
pub(crate) fn finish_sum(mut sum: u64) -> u16 {
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkLayer {
    Ip,
    Ethernet,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reply {
    Arp,
    Icmp,
    Udp,
    Ignored,
    Malformed,
}
impl Reply {
    pub fn is_reply(self) -> bool {
        matches!(self, Self::Arp | Self::Icmp | Self::Udp)
    }
}

/// A small in-place responder. Supply the stack's IPv4 address, Ethernet MAC,
/// and optional UDP echo port. Unrelated traffic is ignored. Checksums are
/// verified in software; benchmark this separately from an RX-drop loop.
#[derive(Clone, Copy, Debug)]
pub struct Responder {
    pub ipv4: [u8; 4],
    pub mac: [u8; 6],
    pub udp_port: Option<u16>,
}
impl Responder {
    pub fn respond(&self, frame: &mut PacketBuf, link: LinkLayer) -> Reply {
        let packet = frame.as_mut_packet();
        let (offset, protocol) = if link == LinkLayer::Ethernet {
            if packet.len() < 14 {
                return Reply::Malformed;
            }
            let mut offset = 14;
            let mut protocol = read16(packet, 12);
            for _ in 0..2 {
                if protocol != 0x8100 && protocol != 0x88a8 {
                    break;
                }
                if packet.len() < offset + 4 {
                    return Reply::Malformed;
                }
                protocol = read16(packet, offset + 2);
                offset += 4;
            }
            (offset, protocol)
        } else {
            (0, 0x0800)
        };
        if protocol == 0x0806 {
            return self.arp(frame, offset);
        }
        if protocol != 0x0800 {
            return Reply::Ignored;
        }
        let ip = &mut packet[offset..];
        if ip.len() < 20 || ip[0] >> 4 != 4 {
            return Reply::Malformed;
        }
        let ihl = (ip[0] as usize & 15) * 4;
        let total = read16(ip, 2) as usize;
        if ihl < 20 || total < ihl || total > ip.len() {
            return Reply::Malformed;
        }
        if checksum(&ip[..ihl]) != 0 {
            return Reply::Malformed;
        }
        // Reject MF, nonzero fragment offset, and the reserved flag. DF is OK.
        if read16(ip, 6) & !0x4000 != 0 {
            return Reply::Ignored;
        }
        if ip[16..20] != self.ipv4 || ip[8] == 0 {
            return Reply::Ignored;
        }
        // Options can contain routing semantics; leave them for a future IP layer.
        if ihl != 20 {
            return Reply::Ignored;
        }
        let source: [u8; 4] = ip[12..16].try_into().unwrap();
        let protocol = ip[9];
        let body = &mut ip[ihl..total];
        let reply = match protocol {
            1 => {
                if body.len() < 8 || checksum(body) != 0 {
                    return Reply::Malformed;
                }
                if body[0] != 8 || body[1] != 0 {
                    return Reply::Ignored;
                }
                body[0] = 0;
                body[2..4].fill(0);
                let sum = checksum(body);
                body[2..4].copy_from_slice(&sum.to_be_bytes());
                Reply::Icmp
            }
            17 => {
                if body.len() < 8 || read16(body, 4) as usize != body.len() {
                    return Reply::Malformed;
                }
                if Some(read16(body, 2)) != self.udp_port {
                    return Reply::Ignored;
                }
                if read16(body, 6) != 0
                    && crate::transport::udp::ipv4_checksum(source, self.ipv4, body) != 0
                {
                    return Reply::Malformed;
                }
                body.swap(0, 2);
                body.swap(1, 3);
                // Swapping source/destination IPs and ports preserves their
                // one's-complement sum, including the IPv4 pseudo-header.
                Reply::Udp
            }
            _ => return Reply::Ignored,
        };
        ip[12..16].copy_from_slice(&self.ipv4);
        ip[16..20].copy_from_slice(&source);
        ip[8] = 64;
        ip[10..12].fill(0);
        let sum = checksum(&ip[..ihl]);
        ip[10..12].copy_from_slice(&sum.to_be_bytes());
        if link == LinkLayer::Ethernet {
            packet.copy_within(6..12, 0);
            packet[6..12].copy_from_slice(&self.mac);
        }
        let len = offset + total;
        self.finish_frame(frame, len, link);
        reply
    }
    fn arp(&self, frame: &mut PacketBuf, offset: usize) -> Reply {
        let packet = frame.as_mut_packet();
        let arp = &mut packet[offset..];
        if arp.len() < 28 {
            return Reply::Malformed;
        }
        if read16(arp, 0) != 1
            || read16(arp, 2) != 0x0800
            || arp[4..6] != [6, 4]
            || read16(arp, 6) != 1
            || arp[24..28] != self.ipv4
        {
            return Reply::Ignored;
        }
        let sender_mac: [u8; 6] = arp[8..14].try_into().unwrap();
        let sender_ip: [u8; 4] = arp[14..18].try_into().unwrap();
        arp[6..8].copy_from_slice(&2u16.to_be_bytes());
        arp[8..14].copy_from_slice(&self.mac);
        arp[14..18].copy_from_slice(&self.ipv4);
        arp[18..24].copy_from_slice(&sender_mac);
        arp[24..28].copy_from_slice(&sender_ip);
        packet[..6].copy_from_slice(&sender_mac);
        packet[6..12].copy_from_slice(&self.mac);
        self.finish_frame(frame, offset + 28, LinkLayer::Ethernet);
        Reply::Arp
    }
    fn finish_frame(&self, frame: &mut PacketBuf, len: usize, link: LinkLayer) {
        let padded = if link == LinkLayer::Ethernet {
            len.max(60).min(frame.tail_capacity())
        } else {
            len
        };
        frame.set_len(padded);
        frame.as_mut_packet()[len..].fill(0);
    }
}
fn read16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_be_bytes([bytes[offset], bytes[offset + 1]])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::{Device, LoopbackDevice};
    #[test]
    fn checksum_handles_odd_lengths() {
        assert_eq!(checksum(&[0, 1, 0xf2, 3, 0xf4, 0xf5, 0xf6, 0xf7]), 0x220d);
        assert_eq!(checksum(&[1, 2, 3]), 0xfbfd);
    }
    #[test]
    fn arp_reply_and_truncated_input() {
        let responder = Responder {
            ipv4: [10, 0, 0, 2],
            mac: [2, 0, 0, 0, 0, 2],
            udp_port: None,
        };
        let mut dev = LoopbackDevice::new();
        let mut buf = dev.alloc().unwrap();
        buf.set_len(42);
        let p = buf.as_mut_packet();
        p.fill(0);
        p[..6].fill(0xff);
        p[6..12].copy_from_slice(&[2, 0, 0, 0, 0, 1]);
        p[12..22].copy_from_slice(&[8, 6, 0, 1, 8, 0, 6, 4, 0, 1]);
        p[22..28].copy_from_slice(&[2, 0, 0, 0, 0, 1]);
        p[28..32].copy_from_slice(&[10, 0, 0, 1]);
        p[38..42].copy_from_slice(&responder.ipv4);
        assert_eq!(responder.respond(&mut buf, LinkLayer::Ethernet), Reply::Arp);
        assert_eq!(buf.len(), 60);
        assert_eq!(&buf.as_slice()[20..22], &[0, 2]);
        assert_eq!(&buf.as_slice()[28..32], &responder.ipv4);
        assert!(buf.as_slice()[42..].iter().all(|&b| b == 0));
        for len in 0..42 {
            buf.set_len(len);
            assert!(!responder.respond(&mut buf, LinkLayer::Ethernet).is_reply());
        }
    }
    #[test]
    fn icmp_reply_has_valid_checksums() {
        let mut dev = LoopbackDevice::new();
        let mut buf = dev.alloc().unwrap();
        buf.set_len(33);
        let p = buf.as_mut_packet();
        p.fill(0);
        p[0] = 0x45;
        p[2..4].copy_from_slice(&33u16.to_be_bytes());
        p[8] = 30;
        p[9] = 1;
        p[12..16].copy_from_slice(&[10, 0, 0, 1]);
        p[16..20].copy_from_slice(&[10, 0, 0, 2]);
        p[20] = 8;
        p[28..].copy_from_slice(b"hello");
        let c = checksum(&p[20..]);
        p[22..24].copy_from_slice(&c.to_be_bytes());
        let c = checksum(&p[..20]);
        p[10..12].copy_from_slice(&c.to_be_bytes());
        let responder = Responder {
            ipv4: [10, 0, 0, 2],
            mac: [0; 6],
            udp_port: None,
        };
        assert_eq!(responder.respond(&mut buf, LinkLayer::Ip), Reply::Icmp);
        assert_eq!(checksum(&buf.as_slice()[..20]), 0);
        assert_eq!(checksum(&buf.as_slice()[20..]), 0);
        assert_eq!(&buf.as_slice()[16..20], &[10, 0, 0, 1]);
        assert_eq!(buf.as_slice()[20], 0);
    }
}
