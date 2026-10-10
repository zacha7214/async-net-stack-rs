//! Offline capture inventory and deterministic IPv4 playback.
//!
//! This reproduces observations, not TCP connection state or inferred routes.
//! Original capture bytes remain available; only link encapsulation and bytes
//! beyond IPv4 total length are removed for L3 delivery. Checksums are not repaired.
pub mod bundle;
mod capture;
pub mod experiment;
use crate::{
    device::{buffer_pool::FramePool, Device, PacketBuf},
    net::checksum,
};
pub use capture::{Capture, Interface, Limits, Record};
use std::{collections::BTreeMap, io, net::Ipv4Addr, ops::Range, time::Duration};

const FIN_FLAG_BIT: u8 = 1;
const SYN_FLAG_BIT: u8 = 2;
const RST_FLAG_BIT: u8 = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    Skipped,
    Warning,
    Transformation,
}

#[derive(Clone, Debug)]
pub struct Diagnostic {
    pub record: usize,
    pub severity: Severity,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Endpoint {
    pub address: Ipv4Addr,
    pub port: Option<u16>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct FlowKey {
    pub interface: usize,
    pub vlan_tags: Vec<u16>,
    pub protocol: u8,
    pub endpoints: [Endpoint; 2],
}

#[derive(Clone, Debug)]
pub struct Conversation {
    pub key: FlowKey,

    /// A new observed TCP opening after an earlier opening/partial stream.
    pub instance: usize,
    pub first_record: usize,
    pub last_record: usize,
    pub packets: [usize; 2],
    pub syn_seen: bool,
    pub fin_seen: bool,
    pub reset_seen: bool,
    syn_sequences: [Option<u32>; 2],
}

#[derive(Clone, Debug)]
pub struct ReplayPacket {
    pub record: usize,
    pub at: Duration,
    pub protocol: u8,
    pub conversation: usize,
    range: Range<usize>,
}

/// An immutable capture plus a derived, ordered replay plan.
#[derive(Debug)]
pub struct Trace {
    capture: Capture,
    packets: Vec<ReplayPacket>,
    conversations: Vec<Conversation>,
    diagnostics: Vec<Diagnostic>,
}

impl Trace {
    pub fn capture(&self) -> &Capture {
        &self.capture
    }

    pub fn packets(&self) -> &[ReplayPacket] {
        &self.packets
    }

    pub fn conversations(&self) -> &[Conversation] {
        &self.conversations
    }

    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }

    pub fn is_tcp(proto_byte: u8) -> bool {
        proto_byte == 6
    }

    pub fn is_udp(proto_byte: u8) -> bool {
        proto_byte == 17
    }

    pub fn packet_bytes(&self, packet: &ReplayPacket) -> Option<&[u8]> {
        self.capture
            .records
            .get(packet.record.checked_sub(1)?)?
            .bytes
            .get(packet.range.clone())
    }

    /// Preserve capture order. Missing times use the preceding deadline (zero
    /// initially); backward times clamp to the preceding deadline. Every such
    /// adjustment is reported. The origin is the first timestamped record.
    pub fn analyze(capture: Capture) -> Self {
        let origin = capture
            .records
            .iter()
            .find_map(|r| r.timestamp)
            .unwrap_or_default();

        let mut result = Self {
            capture,
            packets: Vec::new(),
            conversations: Vec::new(),
            diagnostics: Vec::new(),
        };

        let mut current: BTreeMap<FlowKey, usize> = BTreeMap::new();
        let mut previous = Duration::ZERO;
        for record in &result.capture.records {
            let mut report = |severity, message: &str| {
                result.diagnostics.push(Diagnostic {
                    record: record.index,
                    severity,
                    message: message.into(),
                })
            };

            let at = match record.timestamp {
                None => {
                    report(
                        Severity::Warning,
                        "missing timestamp: scheduled at preceding deadline",
                    );
                    previous
                }
                Some(t) if t < origin || t.saturating_sub(origin) < previous => {
                    report(Severity::Warning, "backward timestamp: clamped to preceding deadline to preserve record order");
                    previous
                }
                Some(t) => t - origin,
            };

            previous = at;
            if record.bytes.len() < record.original_len as usize {
                report(
                    Severity::Skipped,
                    "truncated capture record: missing bytes cannot be recovered",
                );
                continue;
            }

            let link = result.capture.interfaces[record.interface].link_type;
            let (offset, vlan_tags) = match ipv4_offset(link, &record.bytes) {
                Ok(v) => v,
                Err(message) => {
                    report(Severity::Skipped, message);
                    continue;
                }
            };

            let ip = &record.bytes[offset..];
            if ip.len() < 20 || ip[0] >> 4 != 4 {
                report(Severity::Skipped, "not a complete IPv4 header");
                continue;
            }

            let header = (ip[0] & 15) as usize * 4;
            let len = u16::from_be_bytes([ip[2], ip[3]]) as usize;
            if header < 20 || len < header || len > ip.len() {
                report(Severity::Skipped, "invalid or incomplete IPv4 length");
                continue;
            }

            if header != 20 {
                report(
                    Severity::Skipped,
                    "IPv4 options are not supported by the replay target",
                );
                continue;
            }

            if u16::from_be_bytes([ip[6], ip[7]]) & !0x4000 != 0 {
                report(
                    Severity::Skipped,
                    "IPv4 fragments or reserved flags are not supported by the replay target",
                );
                continue;
            }

            if ip[8] == 0 || checksum(&ip[..20]) != 0 {
                report(
                    Severity::Skipped,
                    "invalid IPv4 TTL/checksum (no automatic offload repair)",
                );
                continue;
            }

            if offset != 0 {
                report(
                    Severity::Transformation,
                    "removed link header for L3 replay; original retained",
                );
            }

            if len < ip.len() {
                report(
                    Severity::Transformation,
                    "excluded trailing padding/FCS beyond IPv4 total length; original retained",
                );
            }

            let protocol = ip[9];
            let transport = &ip[20..len];
            let ports = if Self::is_udp(protocol) || Self::is_tcp(protocol) && transport.len() >= 4
            {
                [
                    Some(u16::from_be_bytes([transport[0], transport[1]])),
                    Some(u16::from_be_bytes([transport[2], transport[3]])),
                ]
            } else {
                [None, None]
            };

            let udp_error =
                Self::is_udp(protocol) && crate::transport::udp::parse_ipv4(&ip[..len]).is_err();
            let tcp_valid =
                Self::is_tcp(protocol) && crate::transport::tcp::parse_ipv4(&ip[..len]).is_ok();

            if (Self::is_tcp(protocol) && !tcp_valid) || udp_error {
                report(Severity::Warning, "invalid/unsupported transport header or checksum: bytes preserved, engine may reject");
            }

            let source = Endpoint {
                address: Ipv4Addr::new(ip[12], ip[13], ip[14], ip[15]),
                port: ports[0],
            };

            let destination = Endpoint {
                address: Ipv4Addr::new(ip[16], ip[17], ip[18], ip[19]),
                port: ports[1],
            };

            let direction = usize::from(source > destination);
            let endpoints = if direction == 0 {
                [source, destination]
            } else {
                [destination, source]
            };

            let key = FlowKey {
                interface: record.interface,
                vlan_tags,
                protocol,
                endpoints,
            };

            let flags = if tcp_valid { transport[13] } else { 0 };
            let opening = flags & 0x12 == 0x02;
            let seq = if tcp_valid {
                u32::from_be_bytes(transport[4..8].try_into().unwrap())
            } else {
                0
            };

            let old = current.get(&key).copied();
            let new_open = old.is_some_and(|i| {
                let c = &result.conversations[i];
                opening
                    && (c.reset_seen
                        || c.fin_seen
                        || !c.syn_seen
                        || c.syn_sequences[direction].is_some_and(|old_seq| old_seq != seq))
            });

            let index = if let Some(index) = old.filter(|_| !new_open) {
                index
            } else {
                let index = result.conversations.len();
                let instance = old.map_or(1, |i| result.conversations[i].instance + 1);
                result.conversations.push(Conversation {
                    key: key.clone(),
                    instance,
                    first_record: record.index,
                    last_record: record.index,
                    packets: [0; 2],
                    syn_seen: false,
                    fin_seen: false,
                    reset_seen: false,
                    syn_sequences: [None; 2],
                });
                current.insert(key, index);
                index
            };

            let c = &mut result.conversations[index];
            c.last_record = record.index;
            c.packets[direction] += 1;
            c.syn_seen |= flags & SYN_FLAG_BIT != 0;
            c.fin_seen |= flags & FIN_FLAG_BIT != 0;
            c.reset_seen |= flags & RST_FLAG_BIT != 0;
            if flags & 2 != 0 {
                c.syn_sequences[direction] = Some(seq);
            }

            result.packets.push(ReplayPacket {
                record: record.index,
                at,
                protocol,
                conversation: index,
                range: offset..offset + len,
            });
        }

        result
    }
}

const LINK_TYPE_RAW: u16 = 101;
const LINK_TYPE_IPV4: u16 = 228;
const LINK_TYPE_ETH: u16 = 1;

fn ipv4_offset(link: u16, bytes: &[u8]) -> Result<(usize, Vec<u16>), &'static str> {
    match link {
        LINK_TYPE_RAW | LINK_TYPE_IPV4 => Ok((0, Vec::new())),
        LINK_TYPE_ETH => {
            if bytes.len() < 14 {
                return Err("truncated Ethernet header");
            }

            let mut kind = u16::from_be_bytes([bytes[12], bytes[13]]);
            let mut offset = 14;
            let mut tags = Vec::new();
            while matches!(kind, 0x8100 | 0x88a8) {
                if tags.len() == 2 {
                    return Err("more than two VLAN tags unsupported");
                }

                if bytes.len() < offset + 4 {
                    return Err("truncated VLAN header");
                }

                tags.push(u16::from_be_bytes([bytes[offset], bytes[offset + 1]]) & 0x0fff);

                kind = u16::from_be_bytes([bytes[offset + 2], bytes[offset + 3]]);
                offset += 4;
            }

            if kind != 0x0800 {
                return Err("non-IPv4 Ethernet payload (including ARP/IPv6) not replayed");
            }

            Ok((offset, tags))
        }

        _ => Err("unsupported link type: initial replay supports Ethernet and raw IPv4"),
    }
}

#[derive(Clone, Copy, Debug)]
pub struct ReplayConfig {
    pub frames: usize,
    pub frame_size: usize,
}

impl Default for ReplayConfig {
    fn default() -> Self {
        Self {
            frames: 64,
            frame_size: 65535,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Delivery {
    pub record: usize,
    pub scheduled: Duration,
    pub actual: Duration,
}

/// Input-only recorded playback. `send` rejects output without consuming it:
/// generated responses are not silently discarded or compared with the trace.
/// Pool exhaustion stops progress; the caller can release handles and retry.
/// Deliveries record any resulting lateness. No packets are silently dropped.
pub struct TraceDevice {
    trace: Trace,
    pool: FramePool,
    config: ReplayConfig,
    cursor: usize,
    now: Duration,
    deliveries: Vec<Delivery>,
}

impl TraceDevice {
    pub fn new(trace: Trace, config: ReplayConfig) -> io::Result<Self> {
        if config.frames == 0
            || !(20..=65535).contains(&config.frame_size)
            || config
                .frames
                .checked_mul(config.frame_size)
                .map_or(true, |n| n > 256 * 1024 * 1024)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid replay arena (limit 256 MiB)",
            ));
        }

        if trace
            .packets
            .iter()
            .any(|p| p.range.len() > config.frame_size)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "replay frame size cannot fit the capture; increase frame_size",
            ));
        }

        Ok(Self {
            trace,
            pool: FramePool::new(config.frames, config.frame_size, 4096),
            config,
            cursor: 0,
            now: Duration::ZERO,
            deliveries: Vec::new(),
        })
    }

    pub fn trace(&self) -> &Trace {
        &self.trace
    }

    pub fn finished(&self) -> bool {
        self.cursor == self.trace.packets.len()
    }

    pub fn take_deliveries(&mut self) -> Vec<Delivery> {
        std::mem::take(&mut self.deliveries)
    }
}

impl Device for TraceDevice {
    fn poll_at(&mut self, now: Duration) -> io::Result<()> {
        if now < self.now {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "time went backwards",
            ));
        }

        self.now = now;
        Ok(())
    }

    fn next_deadline(&self) -> Option<Duration> {
        self.trace
            .packets
            .get(self.cursor)
            .map(|p| p.at.max(self.now))
    }

    fn recv(&mut self, max: usize, out: &mut Vec<PacketBuf>) -> io::Result<usize> {
        out.clear();
        while out.len() < max {
            let Some(packet) = self.trace.packets.get(self.cursor) else {
                break;
            };

            if packet.at > self.now {
                break;
            }

            let Some(index) = self.pool.alloc() else {
                if out.is_empty() {
                    return Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        "replay arena exhausted; release held packets and retry",
                    ));
                }
                break;
            };

            let mut frame = self.pool.packet_buf(index, 0);

            let bytes = self
                .trace
                .packet_bytes(packet)
                .expect("validated replay range");

            frame.set_len(bytes.len());
            frame.as_mut_packet().copy_from_slice(bytes);

            self.deliveries.push(Delivery {
                record: packet.record,
                scheduled: packet.at,
                actual: self.now,
            });

            out.push(frame);

            self.cursor += 1;
        }

        Ok(out.len())
    }

    fn send(&mut self, frames: &mut [PacketBuf]) -> io::Result<usize> {
        if frames.is_empty() {
            return Ok(0);
        }

        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "recorded playback is input-only; use an explicit response observer for live engines",
        ))
    }

    fn alloc(&mut self) -> Option<PacketBuf> {
        self.pool.alloc().map(|i| self.pool.packet_buf(i, 0))
    }

    fn frame_size(&self) -> usize {
        self.config.frame_size
    }
}
