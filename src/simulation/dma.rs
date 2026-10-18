//! Scripted DMA backend: synthetic addresses, no hardware access or host clocks.
//! Controls are intentionally separate from the ownership queue under test.
use crate::{
    device::{
        dma::{Completion, DmaAddress, DmaMemory, Submission, TxEngine},
        PacketBuf,
    },
    platform::io,
};

use alloc::{
    collections::{BTreeMap, VecDeque},
    rc::Rc,
    vec::Vec,
};
use core::{cell::RefCell, mem, time::Duration};

#[derive(Clone, Copy, Debug)]
pub enum TxFault {
    /// Submission was not published; the caller retains the packet.
    Reject,
    /// Publish but withhold completion until explicitly injected or stopped.
    Hold,
    Complete {
        delay: Duration,
        failed: bool,
        duplicate: bool,
    },
}

impl Default for TxFault {
    fn default() -> Self {
        Self::Complete {
            delay: Duration::ZERO,
            failed: false,
            duplicate: false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuditEvent {
    Map(u64),
    DeviceSync(u64),
    Publish(u64, u64),
    Complete(u64),
    CpuSync(u64),
    Unmap(u64),
    Stop(bool),
}

struct State {
    limit: usize,
    next_address: u64,
    mapped: BTreeMap<u64, bool>,
    active: BTreeMap<u64, Submission>,
    plans: VecDeque<TxFault>,
    completions: Vec<(Duration, Completion)>,
    fail_map: bool,
    allow_stop: bool,
    stopped: bool,
    audit: VecDeque<AuditEvent>,
}

impl State {
    fn record(&mut self, event: AuditEvent) {
        if self.audit.len() == 256 {
            self.audit.pop_front();
        }

        self.audit.push_back(event);
    }
}

#[derive(Clone)]
pub struct DmaControl(Rc<RefCell<State>>);
pub struct SimMemory(Rc<RefCell<State>>);
pub struct SimEngine(Rc<RefCell<State>>);
pub struct SimMapping {
    address: u64,
}

/// Descriptor/mapping capacity is independent of the queue's flight capacity.
/// Scripts and injected completion lists are bounded to twice this capacity.
pub fn backend(capacity: usize) -> io::Result<(SimMemory, SimEngine, DmaControl)> {
    if capacity == 0 || capacity.checked_mul(2).is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid simulated DMA capacity",
        ));
    }

    let state = Rc::new(RefCell::new(State {
        limit: capacity,
        next_address: 0,
        mapped: BTreeMap::new(),
        active: BTreeMap::new(),
        plans: VecDeque::new(),
        completions: Vec::new(),
        fail_map: false,
        allow_stop: true,
        stopped: false,
        audit: VecDeque::with_capacity(256),
    }));

    Ok((
        SimMemory(state.clone()),
        SimEngine(state.clone()),
        DmaControl(state),
    ))
}

impl DmaControl {
    pub fn plan(&self, fault: TxFault) -> io::Result<()> {
        let mut s = self.0.borrow_mut();

        if s.plans.len() == s.limit * 2 {
            return Err(full());
        }

        s.plans.push_back(fault);

        Ok(())
    }

    pub fn fail_next_mapping(&self) {
        self.0.borrow_mut().fail_map = true;
    }

    pub fn allow_stop(&self, allow: bool) {
        self.0.borrow_mut().allow_stop = allow;
    }

    /// A live token ends that simulated transfer. Unknown/retired tokens exercise
    /// stale-completion rejection; this does not authorize premature real DMA reclaim.
    pub fn inject_completion(&self, at: Duration, completion: Completion) -> io::Result<()> {
        let mut s = self.0.borrow_mut();
        if s.completions.len() == s.limit * 2 {
            return Err(full());
        }

        s.completions.push((at, completion));

        Ok(())
    }

    pub fn mapped(&self) -> usize {
        self.0.borrow().mapped.len()
    }

    pub fn in_flight(&self) -> usize {
        self.0.borrow().active.len()
    }

    /// Bounded diagnostic history; oldest records are overwritten after 256 events.
    pub fn take_audit(&self) -> VecDeque<AuditEvent> {
        mem::take(&mut self.0.borrow_mut().audit)
    }
}

fn full() -> io::Error {
    io::Error::new(
        io::ErrorKind::WouldBlock,
        "simulated DMA capacity exhausted",
    )
}

// SAFETY: this backend never dereferences DMA addresses or accesses packet memory.
// It checks the mapping/publication/reclaim lifecycle with synthetic addresses.
unsafe impl DmaMemory for SimMemory {
    type Mapping = SimMapping;

    unsafe fn map(&mut self, _: &PacketBuf) -> io::Result<SimMapping> {
        let mut s = self.0.borrow_mut();
        if mem::take(&mut s.fail_map) || s.mapped.len() == s.limit {
            return Err(full());
        }

        let address = s
            .next_address
            .checked_add(1)
            .ok_or_else(|| io::Error::other("synthetic address exhausted"))?;

        s.next_address = address;
        s.mapped.insert(address, false);
        s.record(AuditEvent::Map(address));

        Ok(SimMapping { address })
    }

    fn address(&self, mapping: &SimMapping) -> DmaAddress {
        DmaAddress(mapping.address)
    }

    unsafe fn sync_for_device(&mut self, mapping: &mut SimMapping) {
        let mut s = self.0.borrow_mut();
        *s.mapped.get_mut(&mapping.address).unwrap() = true;

        s.record(AuditEvent::DeviceSync(mapping.address));
    }

    unsafe fn sync_for_cpu(&mut self, mapping: &mut SimMapping) {
        let mut s = self.0.borrow_mut();
        assert!(
            !s.active.values().any(|r| r.address.0 == mapping.address),
            "reclaim before completion"
        );

        *s.mapped.get_mut(&mapping.address).unwrap() = false;

        s.record(AuditEvent::CpuSync(mapping.address));
    }

    unsafe fn unmap(&mut self, mapping: SimMapping) {
        let mut s = self.0.borrow_mut();
        assert_eq!(
            s.mapped.remove(&mapping.address),
            Some(false),
            "unmap without CPU synchronization"
        );

        s.record(AuditEvent::Unmap(mapping.address));
    }
}

// SAFETY: no hardware accesses occur. Completion removes the simulated live
// transfer; successful stop removes all live transfers before returning true.
unsafe impl TxEngine for SimEngine {
    unsafe fn submit(&mut self, submission: Submission, now: Duration) -> io::Result<()> {
        let mut s = self.0.borrow_mut();
        if s.stopped {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "engine stopped",
            ));
        }

        if s.active.len() == s.limit {
            return Err(full());
        }

        let fault = s.plans.front().copied().unwrap_or_default();
        if let TxFault::Complete {
            delay,
            failed,
            duplicate,
        } = fault
        {
            let at = now.checked_add(delay).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "completion time overflow")
            })?;

            let needed = if duplicate { 2 } else { 1 };
            if s.completions.len() + needed > s.limit * 2 {
                return Err(full());
            }

            let completion = Completion {
                token: submission.token,
                failed,
            };

            for _ in 0..needed {
                s.completions.push((at, completion));
            }
        }

        s.plans.pop_front();
        if matches!(fault, TxFault::Reject) {
            return Err(full());
        }

        assert_eq!(s.mapped.get(&submission.address.0), Some(&true));
        assert!(!s.active.contains_key(&submission.token));
        s.record(AuditEvent::Publish(submission.token, submission.address.0));
        s.active.insert(submission.token, submission);

        Ok(())
    }

    fn completion(&mut self, now: Duration) -> Option<Completion> {
        let mut s = self.0.borrow_mut();
        let (index, &(at, completion)) = s
            .completions
            .iter()
            .enumerate()
            .min_by_key(|(_, (at, _))| *at)?;
        if at > now {
            return None;
        }

        s.completions.remove(index);
        s.active.remove(&completion.token);
        s.record(AuditEvent::Complete(completion.token));

        Some(completion)
    }

    fn next_deadline(&self) -> Option<Duration> {
        self.0.borrow().completions.iter().map(|(at, _)| *at).min()
    }

    fn stop(&mut self) -> bool {
        let mut s = self.0.borrow_mut();
        s.stopped = true;

        let allowed = s.allow_stop;
        s.record(AuditEvent::Stop(allowed));

        if allowed {
            s.active.clear();

            s.completions.clear();
        }

        allowed
    }
}

/// Safely bind the paired synthetic mapper/engine; no hardware DMA is possible.
pub fn tx_queue(
    pool: crate::device::PacketPool,
    config: crate::device::dma::TxConfig,
    backend_capacity: usize,
) -> io::Result<(
    crate::device::dma::TxQueue<SimMemory, SimEngine>,
    DmaControl,
)> {
    let (memory, engine, control) = backend(backend_capacity)?;

    // SAFETY: both objects share the same synthetic address domain, with no work.
    let queue = unsafe { crate::device::dma::TxQueue::new(pool, memory, engine, config)? };

    Ok((queue, control))
}
