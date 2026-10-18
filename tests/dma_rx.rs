use async_net_stack_rs::{
    device::{dma::*, Device, PacketPool},
    platform::io,
};

use std::{
    cell::RefCell,
    collections::VecDeque, 
    rc::Rc, 
    time::Duration
};

#[derive(Default)]
struct State {
    posts: Vec<RxSubmission>,
    completions: VecDeque<RxCompletion>,
    audit: Vec<&'static str>,
    mapped: usize,
    reject: bool,
    fail_map: bool,
    deny_stop: bool,
}

struct Memory(Rc<RefCell<State>>);
struct Engine(Rc<RefCell<State>>);

// SAFETY: test backend writes only during completion; mapping retains writable
// pointer provenance and the queue keeps its allocation alive and exclusive.
unsafe impl RxDmaMemory for Memory {
    type Mapping = *mut u8;
    unsafe fn map(
        &mut self,
        p: &mut async_net_stack_rs::device::PacketBuf,
    ) -> io::Result<Self::Mapping> {
        let mut s = self.0.borrow_mut();
        if std::mem::take(&mut s.fail_map) {
            return Err(io::Error::other("map rejected"));
        }
        
        s.mapped += 1;
        s.audit
            .push("map");
        
        let offset = p.data_offset();
        Ok(p
            .as_mut_slice()
            .as_mut_ptr()
            .add(offset)
        )
    }

    fn address(&self, p: &Self::Mapping) -> DmaAddress {
        DmaAddress(*p as u64)
    }

    unsafe fn sync_for_device(&mut self, _: &mut Self::Mapping) {
        self.0.borrow_mut()
            .audit
            .push("device");
    }

    unsafe fn sync_for_cpu(&mut self, _: &mut Self::Mapping) {
        self.0.borrow_mut()
            .audit
            .push("cpu");
    }

    unsafe fn unmap(&mut self, _: Self::Mapping) {
        let mut s = self.0.borrow_mut();
        s.audit.push("unmap");
        s.mapped -= 1;
    }
}

// SAFETY: each matched completion writes only within its posted range, once;
// failures never publish. Stop either refuses or removes all posted work.
unsafe impl RxEngine for Engine {
    unsafe fn post(&mut self, p: RxSubmission, _: Duration) -> io::Result<()> {
        let mut s = self.0.borrow_mut();
        if std::mem::take(&mut s.reject) {
            return Err(io::Error::other("post rejected"));
        }
        
        s.audit
            .push("post");
        s.posts
            .push(p);
        
        Ok(())
    }
    
    fn completion(&mut self, _: Duration) -> Option<RxCompletion> {
        let mut s = self.0.borrow_mut();
        let completion_rx = s.completions.pop_front()?;
        if let Some(i) = s.posts
            .iter()
            .position(|p| p.token == completion_rx.token) {
            let post = s.posts.remove(i);
            
            // Even malicious reported lengths cannot write outside the range.
            unsafe {
                std::ptr::write_bytes(
                    post.address.0 as *mut u8,
                    0x5a, 
                    completion_rx.length
                        .min(post.capacity)
                );
            }
        }
        
        s
            .audit
            .push("complete");
        
        Some(completion_rx)
    }
    
    fn stop(&mut self) -> bool {
        let mut s = self.0.borrow_mut();
        if s.deny_stop {
            return false;
        }

        s.posts.clear();
        true
    }

}

type Queue = RxQueue<Memory, Engine>;
fn setup(frames: usize, slots: usize, budget: usize) -> (Queue, PacketPool, Rc<RefCell<State>>) {
    let pool = PacketPool::new(frames, 256, 64);
    let s = Rc::new(RefCell::new(State::default()));
    let q = unsafe {
        RxQueue::new(
            pool.clone(),
            Memory(s.clone()),
            Engine(s.clone()),
            RxConfig {
                slots,
                completion_budget: budget,
            },
        )
    }
    .unwrap();
    (q, pool, s)
}
fn complete(s: &Rc<RefCell<State>>, token: u64, length: usize, failed: bool) {
    s.borrow_mut().completions.push_back(RxCompletion {
        token,
        length,
        failed,
    });
}
#[test]
fn ownership_headroom_capacity_and_cache_order() {
    let (mut q, pool, s) = setup(1, 1, 8);
    let mut p = pool.alloc().unwrap();
    p.as_mut_slice().fill(0xab);
    p.set_headroom(32);
    let mut frames = [p];
    assert_eq!(q.post(&mut frames).unwrap(), 1);
    assert_eq!(frames[0].capacity(), 0);
    assert_eq!(pool.available(), 0);
    assert_eq!(s.borrow().posts[0].capacity, 224);
    complete(&s, 1, 224, false);
    let mut out = Vec::new();
    assert_eq!(q.recv(1, &mut out).unwrap(), 1);
    assert_eq!(out[0].as_slice(), &[0x5a; 224]);
    assert_eq!(&out[0].as_mut_slice()[..32], &[0xab; 32]);
    assert_eq!(
        s.borrow().audit,
        ["map", "device", "post", "complete", "cpu", "unmap"]
    );
    drop(q);
    assert_eq!(pool.available(), 0);
    drop(out);
    assert_eq!(pool.available(), 1);
}
#[test]
fn malformed_and_failed_lengths_are_never_delivered() {
    for (length, failed, invalid) in [
        (0, false, 1),
        (193, false, 1),
        (usize::MAX, false, 1),
        (1, true, 0),
        (192, true, 0),
    ] {
        let (mut q, pool, s) = setup(1, 1, 8);
        q.post(&mut [pool.alloc().unwrap()]).unwrap();
        complete(&s, 1, length, failed);
        let mut out = Vec::new();
        assert_eq!(q.recv(1, &mut out).unwrap(), 0);
        assert_eq!(q.stats().invalid_lengths, invalid);
        assert_eq!(q.stats().completion_errors, u64::from(failed));
        assert_eq!(pool.available(), 1);
        assert_eq!(s.borrow().mapped, 0);
    }
}
#[test]
fn stale_reordered_completions_budget_and_ready_backpressure() {
    let (mut q, pool, s) = setup(3, 2, 1);
    q.post(&mut [pool.alloc().unwrap(), pool.alloc().unwrap()])
        .unwrap();
    complete(&s, 2, 1, false);
    complete(&s, 2, usize::MAX, false);
    complete(&s, 1, 2, false);
    q.poll_at(Duration::from_secs(1)).unwrap();
    assert_eq!((q.pending(), q.ready()), (1, 1));
    let mut extra = [pool.alloc().unwrap()];
    assert_eq!(q.post(&mut extra).unwrap(), 0);
    q.poll_at(Duration::from_secs(1)).unwrap();
    assert_eq!(q.stats().stale_completions, 1);
    assert!(q.poll_at(Duration::ZERO).is_err());
    let mut out = Vec::new();
    q.recv(1, &mut out).unwrap();
    assert_eq!(out[0].len(), 1);
    assert_eq!(q.post(&mut extra).unwrap(), 1);
    complete(&s, 2, 1, false);
    q.poll_at(Duration::from_secs(1)).unwrap();
    assert_eq!(q.pending(), 1);
    assert_eq!(q.stats().stale_completions, 2);
    q.recv(0, &mut out).unwrap();
    assert!(out.is_empty());
    q.recv(1, &mut out).unwrap();
    assert_eq!(out[0].len(), 2);
}
#[test]
fn failed_posts_preserve_buffers_and_tokens_are_not_reused() {
    let (mut q, pool, s) = setup(2, 2, 8);
    let mut frames = [pool.alloc().unwrap(), pool.alloc().unwrap()];
    s.borrow_mut().fail_map = true;
    assert!(q.post(&mut frames).is_err());
    assert_eq!(frames[0].capacity(), 256);
    s.borrow_mut().reject = true;
    assert!(q.post(&mut frames).is_err());
    assert_eq!(s.borrow().mapped, 0);
    frames[1].set_len(1);
    assert_eq!(q.post(&mut frames).unwrap(), 1);
    assert_eq!(frames[1].len(), 1);
    assert_eq!(s.borrow().posts[0].token, 3);
    assert_eq!(q.stats().mapping_errors, 1);
    assert_eq!(q.stats().posting_errors, 1);
}
#[test]
fn shutdown_retains_until_confirmed_and_completed_packets_survive() {
    let (mut q, pool, s) = setup(2, 2, 8);
    q.post(&mut [pool.alloc().unwrap(), pool.alloc().unwrap()])
        .unwrap();
    s.borrow_mut().deny_stop = true;
    assert!(!q.shutdown());
    assert_eq!(pool.available(), 0);
    assert!(q.post(&mut [Default::default()]).is_err());
    assert!(q.alloc().is_none());
    complete(&s, 1, 1, false);
    q.poll_at(Duration::ZERO).unwrap();
    s.borrow_mut().deny_stop = false;
    assert!(q.shutdown());
    assert!(q.shutdown());
    assert_eq!(q.stats().cancelled, 1);
    let mut out = Vec::new();
    q.recv(1, &mut out).unwrap();
    assert_eq!(out[0].as_slice(), &[0x5a]);
    drop(out);
    assert_eq!(pool.available(), 2);
}
#[test]
fn uncertain_drop_quarantines_mapping_and_packet() {
    let (mut q, pool, s) = setup(1, 1, 8);
    q.post(&mut [pool.alloc().unwrap()]).unwrap();
    s.borrow_mut().deny_stop = true;
    drop(q);
    assert_eq!(pool.available(), 0);
    assert_eq!(s.borrow().mapped, 1);
    assert!(!s.borrow().audit.contains(&"unmap"));
}
