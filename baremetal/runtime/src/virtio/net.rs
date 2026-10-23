//! Polling Ethernet Device over modern MMIO split queues in a coherent DMA domain.
use super::{mmio, split::Buffer};
use crate::memory::dma::{self, DmaAllocator, Mapping, PacketDmaMemory};

use alloc::{collections::VecDeque, vec::Vec};

use async_net_stack_core::{
    device::{
        dma::{DmaMemory, RxDmaMemory},
        Device, PacketBuf, PacketPool,
    },
    platform::io,
};

use core::{mem, time::Duration};

const HEADER: usize = 12;
const HEADER_FRAME: usize = 64;
const MAX_FRAME: usize = 1514; // Untagged Ethernet, 1500-byte MTU, no FCS.

#[derive(Clone, Copy, Debug)]
pub struct Config {
    /// Preferred descriptors per queue; each packet consumes two descriptors.
    pub queue_size: u16,
    pub rx_buffers: usize,
    pub tx_buffers: usize,
    pub frame_size: usize,
    pub rx_headroom: usize,

    /// Maximum completions per direction and refill attempts per progress call.
    pub budget: usize,
    pub reset_poll_budget: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            queue_size: 64,
            rx_buffers: 64,
            tx_buffers: 64,
            frame_size: 2048,
            rx_headroom: 128,
            budget: 32,
            reset_poll_budget: 1024,
        }
    }
}

#[derive(Debug)]
pub enum Error {
    InvalidConfig,
    Allocation(dma::Error),
    Transport(mmio::Error),
    Mapping(io::Error),
}

impl From<mmio::Error> for Error {
    fn from(e: mmio::Error) -> Self {
        Self::Transport(e)
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    pub tx_submitted: u64,
    pub tx_completed: u64,
    pub tx_cancelled: u64,
    pub tx_backpressure: u64,
    pub rx_posted: u64,
    pub rx_completed: u64,
    pub rx_delivered: u64,
    pub rx_invalid: u64,
    pub rx_cancelled: u64,
}

// Private linear owner.
// It may be dropped ONLY before publication, after a valid
// completion, or following confirmed device wide reset.
// Active rings quarantine
// the entire Flight on reset failure.
// This destructor cannot run during DMA.
struct Lease {
    packet: PacketBuf,
    memory: PacketDmaMemory,
    mapping: Option<Mapping>,
    receive: bool,
}

impl Lease {
    fn new(
        mut packet: PacketBuf,
        mut memory: PacketDmaMemory,
        receive: bool,
    ) -> Result<Self, (io::Error, PacketBuf)> {
        // SAFETY: exclusive handle; retained here through completion/reset.
        let mapped = unsafe {
            if receive {
                RxDmaMemory::map(&mut memory, &mut packet)
            } else {
                DmaMemory::map(&mut memory, &packet)
            }
        };

        let mut mapping = match mapped {
            Ok(m) => m,
            Err(e) => return Err((e, packet)),
        };
        unsafe {
            if receive {
                RxDmaMemory::sync_for_device(&mut memory, &mut mapping);
            } else {
                DmaMemory::sync_for_device(&mut memory, &mut mapping);
            }
        }

        Ok(Self {
            packet,
            memory,
            mapping: Some(mapping),
            receive,
        })
    }

    fn buffer(&self) -> Buffer {
        let mapping = self.mapping.as_ref().unwrap();

        Buffer {
            address: DmaMemory::address(&self.memory, mapping).0,
            length: mapping.length() as u32,
            writable: self.receive,
        }
    }

    fn release(&mut self) {
        if let Some(mut mapping) = self.mapping.take() {
            // SAFETY: all call sites satisfy the private owner's quiescence rule.
            unsafe {
                if self.receive {
                    RxDmaMemory::sync_for_cpu(&mut self.memory, &mut mapping);
                    RxDmaMemory::unmap(&mut self.memory, mapping);
                } else {
                    DmaMemory::sync_for_cpu(&mut self.memory, &mut mapping);
                    DmaMemory::unmap(&mut self.memory, mapping);
                }
            }
        }
    }

    fn into_packet(mut self) -> PacketBuf {
        self.release();
        mem::take(&mut self.packet)
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.release();
    }
}

struct Flight {
    header: Lease,
    payload: Lease,
}

impl Flight {
    fn buffers(&self) -> [Buffer; 2] {
        let mut header = self.header.buffer();
        // The RX header mapping covers its isolated frame; only 12 bytes are
        // exposed to the device. No mutable view of that frame escapes in flight.
        header.length = HEADER as u32;
        [header, self.payload.buffer()]
    }

    fn received(self, written: u32) -> Option<PacketBuf> {
        let mut header = self.header.into_packet();
        let mut packet = self.payload.into_packet();
        let length = (written as usize).checked_sub(HEADER)?;
        if !(14..=MAX_FRAME).contains(&length) || length > packet.tail_capacity() {
            return None;
        }

        // No negotiated checksum/GSO/RSC support. Ignore unknown flag bits and
        // unused checksum/GSO fields; they do not enable any offload operation.
        header.set_len(HEADER);
        let h = header.as_slice();
        if h[0] & 7 != 0 || h[1] != 0 || u16::from_le_bytes([h[10], h[11]]) > 1 {
            return None;
        }

        packet.set_len(length);
        Some(packet)
    }
}

/// Single owner, bounded Ethernet adapter. Public methods never expose mappings
/// or the transport; received packet handles may outlive this device safely.
/// Poll explicitly, even while idle. No IRQ handlers or background work run here.
pub struct VirtioNet<R: mmio::Registers> {
    transport: mmio::NetTransport<R, Flight>,
    tx_pool: PacketPool,
    tx_memory: PacketDmaMemory,
    rx_pool: PacketPool,
    rx_memory: PacketDmaMemory,
    headers: PacketPool,
    header_memory: PacketDmaMemory,
    ready: VecDeque<PacketBuf>,
    config: Config,
    net_config: mmio::NetConfig,
    rx_target: usize,
    rx_pending: usize,
    tx_pending: usize,
    tokens: [u64; 2],
    now: Duration,
    stopped: bool,
    fault: Option<mmio::Error>,
    stats: Stats,
}

impl<R: mmio::Registers> VirtioNet<R> {
    /// Initialize both queues, private DMA pools, and the first RX postings.
    /// # Safety
    /// `registers` and `dma` must satisfy NetTransport::new's exclusive device,
    /// truthful completion/reset and coherent, no-IOMMU domain contracts.
    pub unsafe fn new(registers: R, dma: DmaAllocator, config: Config) -> Result<Self, Error> {
        if config.queue_size < 2
            || config.rx_buffers == 0
            || config.tx_buffers == 0
            || config.budget == 0
            || config.reset_poll_budget == 0
            || config.frame_size % 64 != 0
            || config.frame_size.saturating_sub(config.rx_headroom) < MAX_FRAME
            || config.frame_size.saturating_sub(128) < MAX_FRAME
        {
            return Err(Error::InvalidConfig);
        }

        let mut transport =
            unsafe { mmio::NetTransport::new(registers, dma.clone(), config.reset_poll_budget)? };

        transport.initialize()?;
        let net_config = transport.read_config()?;
        let rx_size = transport.configure_queue(0, config.queue_size)?;
        let tx_size = transport.configure_queue(1, config.queue_size)?;
        if rx_size < 2 || tx_size < 2 {
            return Err(Error::InvalidConfig);
        }

        let pool = |count: usize, size: usize| -> Result<_, Error> {
            dma.allocate(count.checked_mul(size).ok_or(Error::InvalidConfig)?, 64)
                .and_then(|r| r.into_packet_pool(count, size))
                .map_err(Error::Allocation)
        };

        let (tx_pool, tx_memory) = pool(config.tx_buffers, config.frame_size)?;
        let (rx_pool, rx_memory) = pool(config.rx_buffers, config.frame_size)?;
        let rx_target = (rx_size as usize / 2).min(config.rx_buffers);
        let (headers, header_memory) = pool(rx_target + tx_size as usize / 2, HEADER_FRAME)?;
        transport.start()?;

        let mut net = Self {
            transport,
            tx_pool,
            tx_memory,
            rx_pool,
            rx_memory,
            headers,
            header_memory,
            ready: VecDeque::with_capacity(rx_target),
            config,
            net_config,
            rx_target,
            rx_pending: 0,
            tx_pending: 0,
            tokens: [0; 2],
            now: Duration::ZERO,
            stopped: false,
            fault: None,
            stats: Stats::default(),
        };

        if let Err(error) = net.refill() {
            return Err(net
                .fault
                .map(Error::Transport)
                .unwrap_or(Error::Mapping(error)));
        }
        Ok(net)
    }

    pub fn net_config(&self) -> mmio::NetConfig {
        self.net_config
    }

    pub fn stats(&self) -> Stats {
        self.stats
    }

    pub fn pending_tx(&self) -> usize {
        self.tx_pending
    }

    pub fn posted_rx(&self) -> usize {
        self.rx_pending
    }

    pub fn ready_rx(&self) -> usize {
        self.ready.len()
    }

    pub fn queue_sizes(&self) -> [Option<u16>; 2] {
        [self.transport.queue_size(0), self.transport.queue_size(1)]
    }

    /// The first transport fault, including errors deferred after partial send.
    pub fn fault(&self) -> Option<mmio::Error> {
        self.fault
    }

    fn running(&self) -> io::Result<()> {
        if self.stopped {
            Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "Virtio device stopped",
            ))
        } else if self.fault.is_some() {
            Err(io::Error::other("Virtio device requires reset"))
        } else {
            Ok(())
        }
    }

    fn transport_error(&mut self, e: mmio::Error) -> io::Error {
        if self.fault.is_none() {
            self.fault = Some(e);
        }

        io::Error::other("Virtio transport fault; reset required")
    }

    fn header(&mut self, receive: bool) -> io::Result<Option<Lease>> {
        let Some(mut packet) = self.headers.alloc() else {
            return Ok(None);
        };
        packet.set_headroom(0);
        packet.set_len(HEADER);
        packet.as_mut_packet().fill(0);
        if receive {
            packet.set_len(0);
        }
        // Construction guarantees a valid owned pool and range.
        Lease::new(packet, self.header_memory.clone(), receive)
            .map(Some)
            .map_err(|(e, _)| e)
    }

    fn next_token(&self, queue: usize) -> io::Result<u64> {
        self.tokens[queue]
            .checked_add(1)
            .ok_or_else(|| io::Error::other("Virtio tokens exhausted"))
    }

    fn refill(&mut self) -> io::Result<()> {
        self.running()?;
        for _ in 0..self.config.budget {
            if self.rx_pending + self.ready.len() >= self.rx_target {
                break;
            }

            let Some(mut packet) = self.rx_pool.alloc() else {
                break;
            };

            packet.set_headroom(self.config.rx_headroom);
            let Some(header) = self.header(true)? else {
                break;
            };

            let payload = Lease::new(packet, self.rx_memory.clone(), true).map_err(|(e, _)| e)?;
            let flight = Flight { header, payload };
            let token = self.next_token(0)?;
            let buffers = flight.buffers();

            // SAFETY: this private flight owns synchronized exclusive mappings;
            // the transport retains it through completion or confirmed reset.
            unsafe { self.transport.submit(0, token, &buffers, flight) }
                .map_err(|(e, _)| self.transport_error(e))?;

            self.tokens[0] = token;
            self.rx_pending += 1;
            self.stats.rx_posted += 1;
        }

        Ok(())
    }

    fn progress(&mut self) -> io::Result<()> {
        self.running()?;
        for _ in 0..self.config.budget {
            let completion = self
                .transport
                .completion(1)
                .map_err(|e| self.transport_error(e))?;

            let Some(completion) = completion else {
                break;
            };
            // Readable-only used length is not a byte count or delivery report.
            drop(completion.owner);
            self.tx_pending -= 1;
            self.stats.tx_completed += 1;
        }

        for _ in 0..self.config.budget {
            let completion = self
                .transport
                .completion(0)
                .map_err(|e| self.transport_error(e))?;
            let Some(completion) = completion else {
                break;
            };
            self.rx_pending -= 1;
            self.stats.rx_completed += 1;
            if let Some(packet) = completion.owner.received(completion.written) {
                self.ready.push_back(packet);
            } else {
                self.stats.rx_invalid += 1;
            }
        }

        let pending = self
            .transport
            .pending_interrupts()
            .map_err(|e| self.transport_error(e))?;
        if pending & 2 != 0 {
            self.net_config = self
                .transport
                .read_config()
                .map_err(|e| self.transport_error(e))?;
        }

        self.transport
            .acknowledge_interrupts(pending)
            .map_err(|e| self.transport_error(e))?;

        self.refill()
    }

    fn send_one(&mut self, frame: &mut PacketBuf) -> io::Result<bool> {
        self.running()?;
        if !(14..=MAX_FRAME).contains(&frame.len()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid Ethernet frame length",
            ));
        }

        if self.transport.free_descriptors(1).unwrap_or(0) < 2 {
            self.stats.tx_backpressure += 1;
            return Ok(false);
        }

        let token = self.next_token(1)?;
        let Some(header) = self.header(false)? else {
            self.stats.tx_backpressure += 1;
            return Ok(false);
        };
        // Echo may transmit a handle from the RX pool. Foreign pools are rejected
        // without copying, changing headroom, or consuming caller ownership.
        let payload = Lease::new(mem::take(frame), self.tx_memory.clone(), false)
            .or_else(|(_, packet)| Lease::new(packet, self.rx_memory.clone(), false));
        let payload = match payload {
            Ok(payload) => payload,
            Err((e, packet)) => {
                *frame = packet;
                return Err(e);
            }
        };
        let flight = Flight { header, payload };
        let buffers = flight.buffers();
        // SAFETY: same exclusive flight contract as RX. Publication is the sole
        // commit point; any rejection returns the whole owner for rollback.
        if let Err((e, flight)) = unsafe { self.transport.submit(1, token, &buffers, flight) } {
            *frame = flight.payload.into_packet();
            return Err(self.transport_error(e));
        }
        self.tokens[1] = token;
        self.tx_pending += 1;
        self.stats.tx_submitted += 1;
        Ok(true)
    }
    /// Terminal, shared RX/TX stop. Retry on error. Outstanding packet handles
    /// remain valid; DMA-owned flights are released only after status reads zero.
    /// Pools stay allocated until this adapter and all packet handles are dropped.
    pub fn shutdown(&mut self) -> Result<(), mmio::Error> {
        self.stopped = true;
        self.transport.reset()?;
        self.stats.tx_cancelled += self.tx_pending as u64;
        self.stats.rx_cancelled += self.rx_pending as u64;
        self.tx_pending = 0;
        self.rx_pending = 0;
        self.ready.clear();
        Ok(())
    }
}

impl<R: mmio::Registers> Device for VirtioNet<R> {
    fn alloc(&mut self) -> Option<PacketBuf> {
        if self.running().is_err() {
            None
        } else {
            self.tx_pool.alloc()
        }
    }

    fn frame_size(&self) -> usize {
        self.config.frame_size
    }

    fn poll_at(&mut self, now: Duration) -> io::Result<()> {
        if now < self.now {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Virtio clock moved backwards",
            ));
        }
        self.now = now;
        self.progress()
    }

    fn recv(&mut self, max: usize, out: &mut Vec<PacketBuf>) -> io::Result<usize> {
        out.clear();
        self.progress()?;
        for _ in 0..max.min(self.ready.len()) {
            out.push(self.ready.pop_front().unwrap());
        }
        self.stats.rx_delivered += out.len() as u64;
        // Delivery frees posted/ready slots immediately. Application-held packets
        // remain unavailable until dropped; lack of a free buffer is backpressure.
        let refill = self.refill();
        if out.is_empty() {
            refill?;
        }
        Ok(out.len())
    }

    fn send(&mut self, frames: &mut [PacketBuf]) -> io::Result<usize> {
        let mut count = 0;
        for frame in frames {
            match self.send_one(frame) {
                Ok(true) => count += 1,
                Ok(false) => break,
                Err(_) if count != 0 => break,
                Err(e) => return Err(e),
            }
        }

        Ok(count)
    }
}

// NetTransport's destructor performs shared reset, quarantining live Flight
// owners on failure before the independent pool handles below it are dropped.

#[cfg(test)]
mod tests;

