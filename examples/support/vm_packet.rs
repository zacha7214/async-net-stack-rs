//! Shared wire format for the synthetic VM DMA experiment (EtherType 0x88b5).
pub const HEADER: usize = 48;
pub const MAC: [u8; 6] = [2, 0, 0, 0, 0, 2];
pub const PEER: [u8; 6] = [2, 0, 0, 0, 0, 1];

pub fn header(
    kind: [u8; 2],
    sequence: u64,
    gpa_or_count: u64,
    size: usize,
    session: u64,
) -> [u8; HEADER] {
    let mut h = [0u8; HEADER];
    h[..6].copy_from_slice(&MAC);
    h[6..12].copy_from_slice(&PEER);
    h[12..14].copy_from_slice(&[0x88, 0xb5]);
    h[14..16].copy_from_slice(&kind);
    h[16..24].copy_from_slice(&sequence.to_le_bytes());
    h[24..32].copy_from_slice(&gpa_or_count.to_le_bytes());
    h[32..36].copy_from_slice(&(size as u32).to_le_bytes());
    h[40..48].copy_from_slice(&session.to_le_bytes());
    h
}
pub fn u64_at(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}
pub fn payload_byte(sequence: u64, offset: usize) -> u8 {
    (sequence.rotate_right(((offset & 7) * 8) as u32) as u8) ^ (offset / 8) as u8
}
pub fn identify(bytes: &[u8], kind: &[u8; 2]) -> bool {
    bytes.len() >= HEADER && bytes[12..14] == [0x88, 0xb5] && &bytes[14..16] == kind
}
