//! Bounded readers for classic pcap and the common pcapng packet blocks.
use std::{
    io::{self, Read},
    time::Duration,
};

const PCAPNG_MAGIC: &[u8] = &[0x0a, 0x0d, 0x0d, 0x0a];

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub bytes: usize,
    pub records: usize,
    pub interfaces: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            bytes: 256 * 1024 * 1024,
            records: 1_000_000,
            interfaces: 4096,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Interface {
    /// Global index across pcapng sections; records refer to this index.
    pub id: usize,
    pub link_type: u16,
    pub snaplen: u32,
    pub name: Option<String>,
    pub timestamp_resolution: u8,
    pub timestamp_offset_seconds: i64,
}

#[derive(Clone, Debug)]
pub struct Record {
    /// One-based capture order, never changed by analysis or replay.
    pub index: usize,
    pub interface: usize,
    pub timestamp: Option<Duration>,
    pub original_len: u32,
    /// Complete captured bytes, including link headers, padding and any FCS.
    pub bytes: Vec<u8>,
}

#[derive(Debug)]
pub struct Capture {
    pub interfaces: Vec<Interface>,
    pub records: Vec<Record>,
    /// Metadata blocks/options not interpreted by this reader.
    pub notes: Vec<String>,
}

#[derive(Clone, Copy)]
struct Endian(bool);

impl Endian {
    fn u16(self, b: &[u8]) -> u16 {
        let a = b[..2].try_into().unwrap();
        if self.0 {
            u16::from_le_bytes(a)
        } else {
            u16::from_be_bytes(a)
        }
    }

    fn u32(self, b: &[u8]) -> u32 {
        let a = b[..4].try_into().unwrap();
        if self.0 {
            u32::from_le_bytes(a)
        } else {
            u32::from_be_bytes(a)
        }
    }

    fn i64(self, b: &[u8]) -> i64 {
        let a = b[..8].try_into().unwrap();
        if self.0 {
            i64::from_le_bytes(a)
        } else {
            i64::from_be_bytes(a)
        }
    }
}

pub(super) fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}
fn take<'a>(b: &mut &'a [u8], n: usize) -> io::Result<&'a [u8]> {
    if n > b.len() {
        return Err(invalid("truncated capture structure"));
    }

    let (a, rest) = b.split_at(n);
    *b = rest;

    Ok(a)
}

fn padded(n: usize) -> io::Result<usize> {
    n.checked_add(3)
        .map(|v| v & !3)
        .ok_or_else(|| invalid("length overflow"))
}

impl Capture {
    pub fn read(reader: impl Read, limits: Limits) -> io::Result<Self> {
        let mut bytes = Vec::new();
        reader
            .take((limits.bytes as u64).saturating_add(1))
            .read_to_end(&mut bytes)?;
        
        Self::parse(&bytes, limits)
    }

    pub fn parse(bytes: &[u8], limits: Limits) -> io::Result<Self> {
        if bytes.len() > limits.bytes {
            return Err(invalid("capture byte limit exceeded"));
        }

        let mut result = Self {
            interfaces: Vec::new(),
            records: Vec::new(),
            notes: Vec::new(),
        };

        if bytes.starts_with(PCAPNG_MAGIC) {
            result
                .pcapng(bytes, limits)?;
        } else {
            result
                .pcap(bytes, limits)?;
        }

        Ok(result)
    }

    fn interface(&mut self, mut interface: Interface, limits: Limits) -> io::Result<()> {
        if self.interfaces.len() >= limits.interfaces {
            return Err(invalid("interface limit exceeded"));
        }
        
        interface.id = self.interfaces.len();
        self.interfaces.push(interface);
        
        Ok(())
    }

    fn record(
        &mut self,
        interface: usize,
        timestamp: Option<Duration>,
        original_len: u32,
        bytes: &[u8],
        limits: Limits,
    ) -> io::Result<()> {
        if self.records.len() >= limits.records {
            return Err(invalid("record limit exceeded"));
        }

        let iface = self
            .interfaces
            .get(interface)
            .ok_or_else(|| invalid("unknown packet interface"))?;

        let exceeds_original_length = bytes.len() > original_len as usize;
        let exceeds_snaplen = iface.snaplen != 0 && bytes.len() > iface.snaplen as usize;
        if exceeds_original_length || exceeds_snaplen {
            return Err(invalid(
                "captured length exceeds original length or snaplen",
            ));
        }

        self.records
            .push(
                Record { 
                    index: self.records.len() + 1, 
                    interface, 
                    timestamp, 
                    original_len, 
                    bytes: bytes.to_vec(), 
                }
            );

        Ok(())
    }

    fn pcap(&mut self, mut bytes: &[u8], limits: Limits) -> io::Result<()> {
        let h = take(&mut bytes, 24)?;

        let (e, nano) = match h[..4] {
            [0xd4, 0xc3, 0xb2, 0xa1] => (Endian(true), false),
            [0xa1, 0xb2, 0xc3, 0xd4] => (Endian(false), false),
            [0x4d, 0x3c, 0xb2, 0xa1] => (Endian(true), true),
            [0xa1, 0xb2, 0x3c, 0x4d] => (Endian(false), true),
            _ => return Err(invalid("unsupported capture magic")),
        };

        if e.u16(&h[4..]) != 2 || e.u16(&h[6..]) != 4 {
            return Err(invalid("unsupported pcap version (expected 2.4)"));
        }

        if e.u32(&h[16..]) == 0 {
            return Err(invalid("zero pcap snaplen"));
        }

        self.interface(
            Interface {
                id: 0,
                link_type: e.u32(&h[20..]) as u16,
                snaplen: e.u32(&h[16..]),
                name: None,
                timestamp_resolution: if nano { 9 } else { 6 },
                timestamp_offset_seconds: 0,
            },
            limits,
        )?;

        if e.u32(&h[20..]) >> 16 != 0 {
            self
                .notes
                .push("pcap link-type flags retained only in source file; IPv4 extraction excludes trailing FCS".into());
        }

        while !bytes.is_empty() {
            let h = take(&mut bytes, 16)?;
            let fraction = e.u32(&h[4..]);
            let units = if nano { 1_000_000_000 } else { 1_000_000 };
            if fraction >= units {
                return Err(invalid("invalid pcap timestamp fraction"));
            }

            let at_nsecs = fraction * if nano { 1 } else { 1000 };
            let at = Duration::new(e.u32(h) as u64, at_nsecs);
            let packet = take(&mut bytes, e.u32(&h[8..]) as usize)?;
            let original_pkt_len = e.u32(&h[12..]);
            self
                .record(
                    0, 
                    Some(at), 
                    original_pkt_len, packet,
                    limits
                )?;
        }

        Ok(())
    }

    fn pcapng(&mut self, mut bytes: &[u8], limits: Limits) -> io::Result<()> {
        let mut endian = None;
        let mut local = Vec::new();
        
        while !bytes.is_empty() {
            if bytes.len() < 12 {
                return Err(invalid("truncated pcapng block"));
            }

            let section = bytes[..4] == *PCAPNG_MAGIC;
            if section {
                endian = Some(match bytes[8..12] {
                    [0x4d, 0x3c, 0x2b, 0x1a] => Endian(true),
                    [0x1a, 0x2b, 0x3c, 0x4d] => Endian(false),
                    _ => return Err(invalid("invalid pcapng byte-order magic")),
                });
            }

            let e = endian.ok_or_else(|| invalid("pcapng requires a section header"))?;
            let kind = e.u32(bytes);
            let len = e.u32(&bytes[4..]) as usize;
            if len < 12 || len % 4 != 0 {
                return Err(invalid("invalid pcapng block length"));
            }

            let block = take(&mut bytes, len)?;
            if e.u32(&block[len - 4..]) as usize != len {
                return Err(invalid("pcapng trailing length mismatch"));
            }

            let body = &block[8..len - 4];
            match kind {
                0x0a0d0d0a => {
                    if body.len() < 16 || e.u16(&body[4..]) != 1 || e.u16(&body[6..]) != 0 {
                        return Err(invalid("unsupported pcapng section version"));
                    }
                    local.clear();
                }

                1 => {
                    if body.len() < 8 {
                        return Err(invalid("truncated interface block"));
                    }

                    let mut iface = Interface {
                        id: 0,
                        link_type: e.u16(body),
                        snaplen: e.u32(&body[4..]),
                        name: None,
                        timestamp_resolution: 6,
                        timestamp_offset_seconds: 0,
                    };

                    for (code, value) in options(&body[8..], e)? {
                        match code {
                            2 => iface.name = Some(String::from_utf8_lossy(value).into_owned()),
                            9 if value.len() == 1 => iface.timestamp_resolution = value[0],
                            14 if value.len() == 8 => iface.timestamp_offset_seconds = e.i64(value),
                            9 | 14 => return Err(invalid("invalid interface timestamp option")),
                            _ => self.note(format!("interface option {code} not interpreted")),
                        }
                    }

                    local.push(self.interfaces.len());
                    self.interface(iface, limits)?;
                }

                6 | 2 => {
                    if body.len() < 20 {
                        return Err(invalid("truncated packet block"));
                    }

                    let id = if kind == 6 {
                        e.u32(body) as usize
                    } else {
                        e.u16(body) as usize
                    };

                    let id = *local
                        .get(id)
                        .ok_or_else(|| invalid("unknown section interface"))?;
                    
                    let ticks = ((e.u32(&body[4..]) as u64) << 32) | e.u32(&body[8..]) as u64;
                    let iface = &self.interfaces[id];
                    let (at, rounded) = timestamp(
                        ticks,
                        iface.timestamp_resolution,
                        iface.timestamp_offset_seconds,
                    )?;

                    if rounded {
                        self.note("sub-nanosecond timestamps rounded down".into());
                    }
                    let size = e.u32(&body[12..]) as usize;
                    let mut data = &body[20..];
                    let packet = take(&mut data, padded(size)?)?;
                    for (code, _) in options(data, e)? {
                        self.note(format!(
                            "packet option {code} not interpreted (including direction flags)"
                        ));
                    }

                    self.record(id, Some(at), e.u32(&body[16..]), &packet[..size], limits)?;
                }
                3 => {
                    if body.len() < 4 {
                        return Err(invalid("truncated simple packet block"));
                    }

                    let id = *local
                        .first()
                        .ok_or_else(|| invalid("simple packet without interface"))?;
                    
                    let original = e.u32(body);
                    let snaplen = self.interfaces[id].snaplen;
                    let size = if snaplen == 0 {
                        original
                    } 
                    else {
                        original.min(snaplen)
                    } as usize;

                    if body.len() - 4 != padded(size)? {
                        return Err(invalid("invalid simple packet size"));
                    }

                    self
                        .record(id, None, original, &body[4..4 + size], limits)?;
                }
                _ => self
                    .note(format!("pcapng block {kind:#x} not interpreted")),
            }
        }

        Ok(())
    }

    fn note(&mut self, note: String) {
        if !self.notes.contains(&note) {
            self.notes.push(note);
        }
    }
}

fn options(mut bytes: &[u8], e: Endian) -> io::Result<Vec<(u16, &[u8])>> {
    let mut result = Vec::new();
    while !bytes.is_empty() {
        let h = take(&mut bytes, 4)?;
        let code = e.u16(h);
        let size = e.u16(&h[2..]) as usize;
        if code == 0 {
            if size != 0 || bytes.iter().any(|&b| b != 0) {
                return Err(invalid("invalid option terminator"));
            }

            break;
        }

        let value = take(&mut bytes, padded(size)?)?;
        result
            .push((code, &value[..size]));
    }

    Ok(result)
}

fn timestamp(ticks: u64, resolution: u8, offset: i64) -> io::Result<(Duration, bool)> {
    let base = if resolution & 128 == 0 { 10u128 } else { 2 };
    let units = base
        .checked_pow((resolution & 127) as u32)
        .ok_or_else(|| invalid("unsupported timestamp resolution"))?;

    let scaled = ticks as u128 * 1_000_000_000;
    let nanos = scaled / units;
    let seconds = (nanos / 1_000_000_000) as i128 + offset as i128;
    if !(0..=u64::MAX as i128).contains(&seconds) {
        return Err(invalid("timestamp outside supported epoch"));
    }

    Ok((
        Duration::new(seconds as u64, (nanos % 1_000_000_000) as u32),
        scaled % units != 0,
    ))
}
