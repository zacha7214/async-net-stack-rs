//! Bounded sliding-window TCP state machine. Payload is retained separately from
//! device buffers: a lost packet must not pin a scarce AF_XDP RX/UMEM frame.
use super::{blocked, invalid, ConnectionStatus, FlowStatus, Reset, TcpConfig, TcpState, TcpWait};
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
    flights: VecDeque<Flight>,
    flight_payload: usize,
    cwnd: usize,
    ssthresh: usize,
    congestion_credit: usize,
    last_data_sent: Option<Duration>,
    syn_retransmitted: bool,
    timeout_retransmissions: u64,
    device_blocked: bool,

    pub reported: Option<(TcpState, TcpWait, u64)>,
    pub reported_flow: Option<FlowStatus>,
    pub reported_at: Duration,

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
            flights: VecDeque::with_capacity(cfg.max_inflight_segments),
            flight_payload: 0,
            cwnd: cfg.max_cwnd_bytes.min(cfg.mss as usize),
            ssthresh: cfg.initial_ssthresh_bytes,
            congestion_credit: 0,
            last_data_sent: None,
            syn_retransmitted: false,
            timeout_retransmissions: 0,
            device_blocked: false,
            reported: None,
            reported_flow: None,
            reported_at: now,
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
        c.flights.front_mut().unwrap().flags = SYN | ACK;

        c
    }

    pub fn send_buffered(&self) -> usize {
        self.queued.len() + self.flight_payload
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
        self.flights.clear();
        self.flight_payload = 0;
        self.send_una = self.send_sent;
        self.send_next = self.send_sent;
        self.device_blocked = false;
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
            || self.flights.front().is_some_and(|f| {
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

        let flight = self.flights.front().map(|f| {
            f.last_sent
                .map_or(f.created, |at| at.saturating_add(f.rto))
                .min(f.created.saturating_add(cfg.send_timeout))
        });

        let probe = if self.write_closed
            && self.peer_window == 0
            && self.flights.is_empty()
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
            if let Some(flight) = self.flights.front_mut() {
                flight.last_sent = None;
            }
            return;
        }

        let window = self.advertised_window(cfg) as usize;
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
                for f in &mut self.flights {
                    f.persist = false;
                }
            }
        }

        if ack == self.send_una {
            return;
        }

        let mut completed_flags = 0;
        let mut sample = None;
        let mut ambiguous = false;
        let mut acknowledged_payload = 0;
        while let Some(f) = self.flights.front_mut() {
            if !before(f.sequence, ack) {
                break;
            }

            ambiguous |= !f.rtt_eligible || f.transmissions > 1;
            if !before(ack, f.end()) {
                completed_flags |= f.flags;
                acknowledged_payload += f.payload.len();
                if f.transmissions == 1 && f.rtt_eligible {
                    sample = f.last_sent.map(|at| now.saturating_sub(at));
                }
                self.flights.pop_front();
            } else {
                let consumed = ack.wrapping_sub(f.sequence) as usize;
                // SYN and FIN are always separate from data.
                if consumed > f.payload.len() {
                    return;
                }
                f.payload.drain(..consumed);
                f.sequence = ack;
                f.rtt_eligible = false;
                acknowledged_payload += consumed;
                break;
            }
        }

        self.flight_payload -= acknowledged_payload;
        self.send_una = ack;
        if !ambiguous {
            if let Some(sample) = sample {
                self.update_rto(sample, cfg);
            }
        }

        // RFC 6298: restart the single retransmission timer on ACK progress.
        if let Some(f) = self.flights.front_mut() {
            if f.last_sent.is_some() {
                f.last_sent = Some(now);
                f.rto = self.rto;
                // last_sent now denotes the timer origin, not original send time.
                f.rtt_eligible = false;
            }
        }

        if completed_flags & SYN != 0 {
            self.cwnd = self.initial_cwnd(cfg);
            if self.syn_retransmitted {
                self.rto = Duration::from_secs(3).max(cfg.initial_rto).min(cfg.max_rto);
            }
        } else if acknowledged_payload > 0 {
            let mss = self.send_mss(cfg);
            if self.cwnd < self.ssthresh {
                self.cwnd = self.cwnd.saturating_add(acknowledged_payload.min(mss));
            } else {
                self.congestion_credit =
                    self.congestion_credit.saturating_add(acknowledged_payload);
                if self.congestion_credit >= self.cwnd {
                    self.congestion_credit -= self.cwnd;
                    self.cwnd = self.cwnd.saturating_add(mss);
                }
            }

            self.cwnd = self.cwnd.min(cfg.max_cwnd_bytes);
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

        let window = self.advertised_window(cfg) as usize;
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
        self.flight_payload += flight.payload.len();
        self.flights.push_back(flight);
    }

    fn send_mss(&self, cfg: &TcpConfig) -> usize {
        usize::from(cfg.mss.min(self.peer_mss.max(1)))
    }

    fn initial_cwnd(&self, cfg: &TcpConfig) -> usize {
        let mss = self.send_mss(cfg);
        let segments = if self.syn_retransmitted {
            1
        } else if mss > 2190 {
            2
        } else if mss > 1095 {
            3
        } else {
            4
        };
        (mss * usize::from(cfg.initial_cwnd_segments).min(segments)).min(cfg.max_cwnd_bytes)
    }

    fn advertised_window(&self, cfg: &TcpConfig) -> u16 {
        (cfg.receive_capacity - self.received.len()).min(cfg.receive_window_limit as usize) as u16
    }

    fn bytes_in_flight(&self) -> usize {
        self.send_sent.wrapping_sub(self.send_una) as usize
    }

    pub fn status(&self, cfg: &TcpConfig) -> ConnectionStatus {
        let inflight = self.bytes_in_flight();
        let wait = if self.state.is_terminal() {
            TcpWait::Terminal
        } else if self.device_blocked {
            TcpWait::Device
        } else if matches!(self.state, TcpState::SynSent | TcpState::SynReceived) {
            TcpWait::Handshake
        } else if !self.queued.is_empty() {
            if self.peer_window as usize <= inflight {
                TcpWait::PeerWindow
            } else if self.cwnd <= inflight {
                TcpWait::CongestionWindow
            } else if self.flights.len() >= cfg.max_inflight_segments {
                TcpWait::FlightLimit
            } else {
                TcpWait::Ready
            }
        } else if self.peer_window == 0 && (!self.flights.is_empty() || self.write_closed) {
            TcpWait::PeerWindow
        } else if !self.flights.is_empty() {
            TcpWait::Acknowledgment
        } else if self.write_closed {
            TcpWait::Closing
        } else {
            TcpWait::Application
        };

        ConnectionStatus {
            local: self.local,
            remote: self.remote,
            state: self.state,
            send_buffered: self.send_buffered(),
            receive_buffered: self.received.len(),
            local_error: self.local_error,
            flow: FlowStatus {
                advertised_window: self.advertised_window(cfg),
                peer_window: self.peer_window,
                congestion_window: self.cwnd,
                slow_start_threshold: self.ssthresh,
                bytes_in_flight: inflight,
                outstanding_segments: self.flights.len(),
                retransmission_timeout: self.rto,
                timeout_retransmissions: self.timeout_retransmissions,
                wait,
            },
        }
    }

    fn prepare(&mut self, now: Duration, cfg: &TcpConfig) {
        if !matches!(self.state, TcpState::Established | TcpState::CloseWait)
            || self.flights.len() >= cfg.max_inflight_segments
            || self.flights.back().is_some_and(|f| f.transmissions == 0)
        {
            return;
        }

        if !self.queued.is_empty() {
            if self.flights.is_empty()
                && self
                    .last_data_sent
                    .is_some_and(|at| now >= at.saturating_add(self.rto))
            {
                self.cwnd = self.cwnd.min(self.initial_cwnd(cfg));
                self.congestion_credit = 0;
            }

            let persist = self.peer_window == 0 && self.flights.is_empty();
            let reserved = self.send_next.wrapping_sub(self.send_una) as usize;
            let available = if persist {
                1
            } else {
                self.cwnd
                    .min(self.peer_window as usize)
                    .saturating_sub(reserved)
            };

            let n = self.queued.len().min(self.send_mss(cfg)).min(available);
            if n == 0 {
                return;
            }

            let payload = self.queued.drain(..n).collect();
            self.start_flight(ACK | PSH, payload, persist, now);
        } else if self.flights.is_empty() && self.write_closed {
            if self.peer_window > 0 {
                self.start_flight(FIN | ACK, Vec::new(), false, now);
                self.state = if self.state == TcpState::CloseWait {
                    TcpState::LastAck
                } else {
                    TcpState::FinWait1
                };
            } else if now >= self.window_probe_at {
                self.probe_pending = true;
            }
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
        // Only the oldest outstanding segment drives loss recovery. New segments
        // use the remaining window; retransmissions never move send_sent backwards.
        let mut flight_index = if self.flights.front().is_some_and(|f| f.due(now)) {
            Some(0)
        } else {
            self.flights
                .back()
                .filter(|f| f.transmissions == 0)
                .map(|_| self.flights.len() - 1)
        };

        if let Some(index) = flight_index {
            if self.flights[index].transmissions == 0 && self.flights[index].flags & SYN == 0 {
                // A device-blocked segment may outlive a window update or loss.
                // Recheck both windows before submitting any previously unsent bytes.
                let inflight = self.bytes_in_flight();
                let persist = self.peer_window == 0 && inflight == 0;
                let available = if persist {
                    1
                } else {
                    self.cwnd
                        .min(self.peer_window as usize)
                        .saturating_sub(inflight)
                };

                let f = &mut self.flights[index];
                if available == 0 || (persist && f.flags & FIN != 0) {
                    flight_index = None;
                    if persist && now >= self.window_probe_at {
                        self.probe_pending = true;
                    }
                } else {
                    if f.payload.len() > available {
                        for byte in f.payload.drain(available..).rev() {
                            self.queued.push_front(byte);
                        }

                        let removed = self.send_next.wrapping_sub(f.end()) as usize;
                        self.flight_payload -= removed;
                        self.send_next = f.end();
                    }

                    f.persist = persist;
                }
            }
        }

        let send_flight = flight_index.is_some();
        self.device_blocked = false;
        if !send_flight && !self.ack_pending && !self.probe_pending {
            return Ok((false, false));
        }

        let Some(mut frame) = device.alloc() else {
            self.device_blocked = true;
            return Ok((false, false));
        };

        let window = self.advertised_window(cfg);
        let (sequence, flags, payload) = if send_flight {
            let flight = &self.flights[flight_index.unwrap()];
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
            self.device_blocked = true;
            return Ok((false, false));
        }

        let mut retransmitted = false;
        if let Some(index) = flight_index {
            let inflight = self.bytes_in_flight();
            let mss = self.send_mss(cfg);
            let flight = &mut self.flights[index];
            retransmitted = flight.transmissions > 0;
            if retransmitted {
                let timed_out = flight
                    .last_sent
                    .is_some_and(|at| now >= at.saturating_add(flight.rto));

                self.syn_retransmitted |= flight.flags & SYN != 0;
                flight.rto = flight.rto.saturating_mul(2).min(cfg.max_rto);
                flight.rtt_eligible = false;
                self.rto = flight.rto;

                if !flight.persist && timed_out {
                    // Repeated timeouts of this same segment retain ssthresh.
                    if flight.transmissions == 1 {
                        self.ssthresh = (inflight / 2).max(2 * mss);
                    }
                    self.cwnd = mss.min(cfg.max_cwnd_bytes);
                    self.congestion_credit = 0;
                    self.timeout_retransmissions = self.timeout_retransmissions.saturating_add(1);
                }
            }

            flight.transmissions = flight.transmissions.saturating_add(1);
            flight.last_sent = Some(now);
            if before(self.send_sent, flight.end()) {
                self.send_sent = flight.end();
            }

            if !flight.payload.is_empty() {
                self.last_data_sent = Some(now);
            }

            if retransmitted {
                // Karn: a cumulative ACK after loss cannot identify which send
                // produced it, including other segments already in flight.
                for f in &mut self.flights {
                    f.rtt_eligible = false;
                }
            }
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
