//! Bounded stop-and-wait TCP state machine. Payload is retained separately from
//! device buffers: a lost packet must not pin a scarce AF_XDP RX/UMEM frame.
use super::{blocked, invalid, Reset, TcpConfig, TcpState};
use crate::{
    device::Device,
    transport::tcp::{self, before, Segment, ACK, FIN, PSH, RST, SYN},
};
use std::{collections::VecDeque, io, net::SocketAddrV4, time::Duration};

struct Flight {
    sequence: u32,
    flags: u8,
    payload: Vec<u8>,
    created: Duration,
    last_sent: Option<Duration>,
    transmissions: u32,
    rto: Duration,
    persist: bool,
    rtt_eligible: bool,
}
impl Flight {
    fn end(&self) -> u32 {
        self.sequence
            .wrapping_add(self.payload.len() as u32)
            .wrapping_add(u32::from(self.flags & SYN != 0))
            .wrapping_add(u32::from(self.flags & FIN != 0))
    }
    fn due(&self, now: Duration) -> bool {
        self.last_sent
            .map_or(true, |at| now >= at.saturating_add(self.rto))
    }
}

pub(super) struct Connection {
    pub local: SocketAddrV4,
    pub remote: SocketAddrV4,
    pub state: TcpState,
    pub accepted: bool,
    pub received: VecDeque<u8>,
    pub local_error: Option<io::ErrorKind>,
    queued: VecDeque<u8>,
    flight: Option<Flight>,
    send_una: u32,
    send_next: u32,
    // Unlike send_next, this excludes a prepared segment that the device has
    // not accepted. A peer cannot acknowledge bytes we have never submitted.
    send_sent: u32,
    receive_next: u32,
    peer_window: u16,
    window_sequence: u32,
    window_ack: u32,
    peer_mss: u16,
    ack_pending: bool,
    peer_eof: bool,
    write_closed: bool,
    phase_deadline: Option<Duration>,
    close_deadline: Option<Duration>,
    rto: Duration,
    srtt: Option<Duration>,
    rttvar: Duration,
    window_probe_at: Duration,
    probe_pending: bool,
}

impl Connection {
    pub fn active(
        local: SocketAddrV4,
        remote: SocketAddrV4,
        iss: u32,
        now: Duration,
        cfg: &TcpConfig,
    ) -> Self {
        let mut c = Self {
            local,
            remote,
            state: TcpState::SynSent,
            accepted: true,
            received: VecDeque::with_capacity(cfg.receive_capacity),
            local_error: None,
            queued: VecDeque::with_capacity(cfg.send_capacity),
            flight: None,
            send_una: iss,
            send_next: iss,
            send_sent: iss,
            receive_next: 0,
            peer_window: 0,
            window_sequence: 0,
            window_ack: 0,
            peer_mss: 536,
            ack_pending: false,
            peer_eof: false,
            write_closed: false,
            phase_deadline: Some(now.saturating_add(cfg.handshake_timeout)),
            close_deadline: None,
            rto: cfg.initial_rto,
            srtt: None,
            rttvar: Duration::ZERO,
            window_probe_at: now,
            probe_pending: false,
        };
        c.start_flight(SYN, Vec::new(), false, now);
        c
    }

    pub fn passive(syn: &Segment<'_>, iss: u32, now: Duration, cfg: &TcpConfig) -> Self {
        let mut c = Self::active(syn.destination, syn.source, iss, now, cfg);
        c.state = TcpState::SynReceived;
        c.accepted = false;
        c.receive_next = syn.sequence.wrapping_add(1);
        c.peer_window = syn.window;
        c.peer_mss = syn.mss.unwrap_or(536);
        c.window_sequence = syn.sequence;
        c.flight.as_mut().unwrap().flags = SYN | ACK;
        c
    }

    pub fn send_buffered(&self) -> usize {
        self.queued.len() + self.flight.as_ref().map_or(0, |f| f.payload.len())
    }

    pub fn write(&mut self, bytes: &[u8], cfg: &TcpConfig) -> io::Result<usize> {
        if self.write_closed || !matches!(self.state, TcpState::Established | TcpState::CloseWait) {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "TCP write side is not established/open",
            ));
        }
        if bytes.is_empty() {
            return Ok(0);
        }
        let n = bytes.len().min(cfg.send_capacity - self.send_buffered());
        if n == 0 {
            return Err(blocked("TCP send buffer full"));
        }
        self.queued.extend(bytes[..n].iter().copied());
        Ok(n)
    }

    pub fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        if let Some(kind) = self.local_error {
            return Err(io::Error::new(kind, "TCP local network failure"));
        }
        if self.state == TcpState::Reset {
            return Err(io::Error::new(io::ErrorKind::ConnectionReset, "TCP reset"));
        }
        if self.state == TcpState::TimedOut {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "TCP timed out"));
        }
        let n = out.len().min(self.received.len());
        for byte in &mut out[..n] {
            *byte = self.received.pop_front().unwrap();
        }
        if n > 0 {
            self.ack_pending = true; // Reopen the advertised receive window.
            return Ok(n);
        }
        if self.peer_eof || self.state == TcpState::Closed {
            Ok(0)
        } else {
            Err(blocked("no TCP data available"))
        }
    }

    pub fn close(&mut self, now: Duration, cfg: &TcpConfig) -> io::Result<()> {
        if self.write_closed {
            return Ok(());
        }
        if !matches!(self.state, TcpState::Established | TcpState::CloseWait) {
            return Err(invalid("close requires an established TCP connection"));
        }
        self.write_closed = true;
        self.close_deadline = Some(now.saturating_add(cfg.close_timeout));
        Ok(())
    }

    pub fn fail(&mut self, state: TcpState) {
        self.state = state;
        self.local_error = None;
        self.queued.clear();
        self.received.clear();
        self.flight = None;
        self.ack_pending = false;
        self.phase_deadline = None;
        self.close_deadline = None;
    }

    pub fn reset_packet(&self) -> Reset {
        Reset {
            local: self.local,
            remote: self.remote,
            sequence: self.send_sent,
            acknowledgment: self.receive_next,
            flags: if self.state == TcpState::SynSent {
                RST
            } else {
                RST | ACK
            },
        }
    }

    pub fn tick(&mut self, now: Duration, cfg: &TcpConfig) {
        if self.state.is_terminal() {
            return;
        }
        if self.state == TcpState::TimeWait {
            if self.phase_deadline.is_some_and(|deadline| now >= deadline) {
                self.state = TcpState::Closed;
                self.phase_deadline = None;
                self.ack_pending = false;
            }
            return;
        }
        let expired = self.phase_deadline.is_some_and(|deadline| now >= deadline)
            || self.close_deadline.is_some_and(|deadline| now >= deadline)
            || self.flight.as_ref().is_some_and(|f| {
                now >= f.created.saturating_add(cfg.send_timeout)
                    || (!f.persist && f.due(now) && f.transmissions > cfg.max_retransmits)
            });
        if expired {
            self.fail(TcpState::TimedOut);
        }
    }

    pub fn next_deadline(&self, cfg: &TcpConfig) -> Option<Duration> {
        if self.state.is_terminal() {
            return None;
        }
        let flight = self.flight.as_ref().map(|f| {
            f.last_sent
                .map_or(f.created, |at| at.saturating_add(f.rto))
                .min(f.created.saturating_add(cfg.send_timeout))
        });
        let probe = if self.write_closed
            && self.peer_window == 0
            && self.flight.is_none()
            && matches!(self.state, TcpState::Established | TcpState::CloseWait)
        {
            Some(self.window_probe_at)
        } else {
            None
        };
        [self.phase_deadline, self.close_deadline, flight, probe]
            .into_iter()
            .flatten()
            .min()
    }

    pub fn input(&mut self, segment: &Segment<'_>, now: Duration, cfg: &TcpConfig) {
        if self.state.is_terminal() {
            return;
        }
        if self.state == TcpState::SynSent {
            self.syn_sent(segment, now, cfg);
            return;
        }
        if self.state == TcpState::TimeWait {
            // Ignore RST here to avoid prematurely assassinating TIME-WAIT.
            if segment.flags & RST == 0 {
                self.ack_pending = true;
                if segment.flags & FIN != 0
                    && segment.sequence.wrapping_add(segment.sequence_len()) == self.receive_next
                {
                    self.phase_deadline = Some(now.saturating_add(cfg.time_wait));
                }
            }
            return;
        }
        if self.state == TcpState::SynReceived
            && segment.flags & (SYN | ACK | RST) == SYN
            && segment.sequence.wrapping_add(1) == self.receive_next
        {
            // Duplicate SYN: repeat SYN-ACK, keeping the retry count and Karn state.
            if let Some(flight) = &mut self.flight {
                flight.last_sent = None;
            }
            return;
        }
        let window = cfg.receive_capacity - self.received.len();
        let relative = segment.sequence.wrapping_sub(self.receive_next) as i32 as i64;
        let length = segment.sequence_len() as i64;
        let acceptable = if length == 0 {
            relative == 0 || (relative > 0 && relative < window as i64)
        } else {
            window > 0 && relative < window as i64 && relative + length > 0
        };
        if !acceptable {
            if segment.flags & RST == 0 {
                self.ack_pending = true;
            }
            return;
        }
        if segment.flags & RST != 0 {
            if segment.sequence == self.receive_next {
                self.fail(TcpState::Reset);
            } else {
                self.ack_pending = true;
            }
            return;
        }
        if segment.flags & SYN != 0 {
            self.ack_pending = true;
            return;
        }
        if segment.flags & ACK == 0 {
            return;
        }
        if before(self.send_sent, segment.acknowledgment) {
            self.ack_pending = true;
            return;
        }
        if self.state == TcpState::SynReceived {
            if segment.acknowledgment != self.send_sent || self.send_sent == self.send_una {
                return;
            }
            self.state = TcpState::Established;
            self.phase_deadline = None;
        }
        self.acknowledge(segment, now, cfg);
        if self.state.is_terminal() {
            return;
        }
        self.receive_data(segment, now, cfg);
    }

    fn syn_sent(&mut self, segment: &Segment<'_>, now: Duration, cfg: &TcpConfig) {
        let valid_ack = segment.flags & ACK != 0
            && segment.acknowledgment == self.send_sent
            && self.send_sent != self.send_una;
        if segment.flags & RST != 0 {
            if valid_ack {
                self.fail(TcpState::Reset);
            }
            return;
        }
        // Simultaneous open is deliberately not implemented in this initial engine.
        if !valid_ack || segment.flags & SYN == 0 || segment.flags & FIN != 0 {
            return;
        }
        self.receive_next = segment.sequence.wrapping_add(1);
        self.peer_window = segment.window;
        self.peer_mss = segment.mss.unwrap_or(536);
        self.window_sequence = segment.sequence;
        self.window_ack = segment.acknowledgment;
        self.acknowledge(segment, now, cfg);
        self.state = TcpState::Established;
        self.phase_deadline = None;
        self.ack_pending = true;
        if !segment.payload.is_empty() {
            let data = Segment {
                sequence: segment.sequence.wrapping_add(1),
                flags: segment.flags & !SYN,
                ..*segment
            };
            self.receive_data(&data, now, cfg);
        }
    }

    fn acknowledge(&mut self, segment: &Segment<'_>, now: Duration, cfg: &TcpConfig) {
        let ack = segment.acknowledgment;
        if before(ack, self.send_una) {
            return;
        }
        if before(self.window_sequence, segment.sequence)
            || (self.window_sequence == segment.sequence && !before(ack, self.window_ack))
        {
            self.peer_window = segment.window;
            self.window_sequence = segment.sequence;
            self.window_ack = ack;
            if self.peer_window > 0 {
                if let Some(f) = &mut self.flight {
                    f.persist = false;
                }
            }
        }
        if ack == self.send_una {
            return;
        }
        let mut completed_flags = 0;
        let mut sample = None;
        if let Some(flight) = &mut self.flight {
            if ack == flight.end() {
                completed_flags = flight.flags;
                if flight.transmissions == 1 && flight.rtt_eligible {
                    sample = flight.last_sent.map(|at| now.saturating_sub(at));
                }
            } else {
                // Only data segments can be partially acknowledged: SYN/FIN are
                // emitted separately and each occupies a single sequence number.
                let consumed = ack.wrapping_sub(flight.sequence) as usize;
                if consumed > flight.payload.len() {
                    return;
                }
                flight.payload.drain(..consumed);
                flight.sequence = ack;
                // Don't sample RTT after a partial ACK; restart the retransmission timer.
                flight.rtt_eligible = false;
                flight.last_sent = Some(now);
            }
        }
        self.send_una = ack;
        if completed_flags != 0 {
            let was_retransmitted = self.flight.as_ref().is_some_and(|f| f.transmissions > 1);
            self.flight = None;
            if let Some(sample) = sample {
                self.update_rto(sample, cfg);
            } else if was_retransmitted && completed_flags & SYN != 0 {
                self.rto = Duration::from_secs(3).max(cfg.initial_rto).min(cfg.max_rto);
            }
            if completed_flags & FIN != 0 {
                match self.state {
                    TcpState::FinWait1 => self.state = TcpState::FinWait2,
                    TcpState::Closing => self.enter_time_wait(now, cfg),
                    TcpState::LastAck => {
                        self.state = TcpState::Closed;
                        self.close_deadline = None;
                        self.ack_pending = false;
                    }
                    _ => {}
                }
            }
        }
    }

    fn receive_data(&mut self, segment: &Segment<'_>, now: Duration, cfg: &TcpConfig) {
        if segment.payload.is_empty() && segment.flags & FIN == 0 {
            return;
        }
        self.ack_pending = true;
        if self.peer_eof
            || !matches!(
                self.state,
                TcpState::Established | TcpState::FinWait1 | TcpState::FinWait2
            )
        {
            return;
        }
        if before(self.receive_next, segment.sequence) {
            return;
        } // No out-of-order storage.
        let window = cfg.receive_capacity - self.received.len();
        let skip = self.receive_next.wrapping_sub(segment.sequence) as usize;
        if skip > segment.payload.len() {
            return;
        }
        let count = (segment.payload.len() - skip).min(window);
        self.received
            .extend(segment.payload[skip..skip + count].iter().copied());
        self.receive_next = self.receive_next.wrapping_add(count as u32);
        let fin_sequence = segment.sequence.wrapping_add(segment.payload.len() as u32);
        if segment.flags & FIN != 0 && fin_sequence == self.receive_next && count < window {
            self.receive_next = self.receive_next.wrapping_add(1);
            self.peer_eof = true;
            match self.state {
                TcpState::Established => self.state = TcpState::CloseWait,
                TcpState::FinWait1 => self.state = TcpState::Closing,
                TcpState::FinWait2 => self.enter_time_wait(now, cfg),
                _ => {}
            }
        }
    }

    fn enter_time_wait(&mut self, now: Duration, cfg: &TcpConfig) {
        self.state = TcpState::TimeWait;
        self.close_deadline = None;
        self.phase_deadline = Some(now.saturating_add(cfg.time_wait));
    }

    fn update_rto(&mut self, sample: Duration, cfg: &TcpConfig) {
        // RFC 6298 estimator; sampling is suppressed for retransmitted segments.
        if let Some(srtt) = self.srtt {
            let difference = if srtt > sample {
                srtt - sample
            } else {
                sample - srtt
            };
            self.rttvar = (self.rttvar.saturating_mul(3) / 4).saturating_add(difference / 4);
            self.srtt = Some((srtt.saturating_mul(7) / 8).saturating_add(sample / 8));
        } else {
            self.srtt = Some(sample);
            self.rttvar = sample / 2;
        }
        self.rto = self
            .srtt
            .unwrap()
            .saturating_add(self.rttvar.saturating_mul(4).max(Duration::from_millis(1)))
            .max(cfg.initial_rto)
            .min(cfg.max_rto);
    }

    fn start_flight(&mut self, flags: u8, payload: Vec<u8>, persist: bool, now: Duration) {
        let flight = Flight {
            sequence: self.send_next,
            flags,
            payload,
            created: now,
            last_sent: None,
            transmissions: 0,
            rto: self.rto,
            persist,
            rtt_eligible: true,
        };
        self.send_next = flight.end();
        self.flight = Some(flight);
    }

    fn prepare(&mut self, now: Duration, cfg: &TcpConfig) {
        if self.flight.is_some()
            || !matches!(self.state, TcpState::Established | TcpState::CloseWait)
        {
            return;
        }
        if !self.queued.is_empty() {
            let persist = self.peer_window == 0;
            // A single-byte zero-window probe remains unacknowledged until the
            // receiver opens its window. It consumes no additional pool frame.
            let window = if persist {
                1
            } else {
                self.peer_window as usize
            };
            let n = self
                .queued
                .len()
                .min(cfg.mss as usize)
                .min(self.peer_mss as usize)
                .min(window);
            let payload = self.queued.drain(..n).collect();
            self.start_flight(ACK | PSH, payload, persist, now);
        } else if self.write_closed && self.peer_window > 0 {
            self.start_flight(FIN | ACK, Vec::new(), false, now);
            self.state = if self.state == TcpState::CloseWait {
                TcpState::LastAck
            } else {
                TcpState::FinWait1
            };
        } else if self.write_closed && self.peer_window == 0 && now >= self.window_probe_at {
            // An ACK at the preceding sequence requests a fresh window report.
            // The close deadline bounds a peer that never reopens its window.
            self.probe_pending = true;
        }
    }

    pub fn transmit<D: Device>(
        &mut self,
        device: &mut D,
        now: Duration,
        cfg: &TcpConfig,
    ) -> io::Result<(bool, bool)> {
        if self.state.is_terminal() {
            return Ok((false, false));
        }
        self.prepare(now, cfg);
        let send_flight = self.flight.as_ref().is_some_and(|f| f.due(now));
        if !send_flight && !self.ack_pending && !self.probe_pending {
            return Ok((false, false));
        }
        let Some(mut frame) = device.alloc() else {
            return Ok((false, false));
        };
        let window = (cfg.receive_capacity - self.received.len()) as u16;
        let (sequence, flags, payload) = if send_flight {
            let flight = self.flight.as_ref().unwrap();
            (flight.sequence, flight.flags, flight.payload.as_slice())
        } else {
            let sequence = if self.probe_pending && !self.ack_pending {
                self.send_sent.wrapping_sub(1)
            } else {
                self.send_sent
            };
            (sequence, ACK, &[][..])
        };
        tcp::build_ipv4(
            &mut frame,
            &Segment {
                source: self.local,
                destination: self.remote,
                sequence,
                acknowledgment: self.receive_next,
                flags,
                window,
                mss: if flags & SYN != 0 {
                    Some(cfg.mss)
                } else {
                    None
                },
                payload,
            },
        )?;
        let n = match device.send(std::slice::from_mut(&mut frame)) {
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => 0,
            result => result?,
        };
        if n == 0 {
            return Ok((false, false));
        }
        let mut retransmitted = false;
        if send_flight {
            let flight = self.flight.as_mut().unwrap();
            retransmitted = flight.transmissions > 0;
            if retransmitted {
                flight.rto = flight.rto.saturating_mul(2).min(cfg.max_rto);
                flight.rtt_eligible = false;
                self.rto = flight.rto;
            }
            flight.transmissions = flight.transmissions.saturating_add(1);
            flight.last_sent = Some(now);
            self.send_sent = flight.end();
        }
        if flags & ACK != 0 {
            self.ack_pending = false;
        }
        if self.probe_pending {
            self.probe_pending = false;
            self.window_probe_at = now.saturating_add(self.rto);
        }
        Ok((true, retransmitted))
    }
}
