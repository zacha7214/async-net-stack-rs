use crate::device::PacketBuf;

pub(super) const HEADER: usize = 14;
pub(super) const IPV4: u16 = 0x0800;
pub(super) const ARP: u16 = 0x0806;
pub(super) const BROADCAST: [u8; 6] = [0xff; 6];

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
    let mut header = [0; HEADER];
    header[..6].copy_from_slice(&destination);
    header[6..12].copy_from_slice(&source);
    header[12..].copy_from_slice(&kind.to_be_bytes());
    frame.push_header(&header);
    let len = frame.len();
    if len < 60 {
        frame.set_len(60);
        frame.as_mut_packet()[len..].fill(0);
    }
}
