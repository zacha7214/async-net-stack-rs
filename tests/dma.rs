use async_net_stack_rs::{
    device::{
        dma::{Completion, OwnershipEvent, TxConfig, TxQueue},
        Device, PacketBuf, PacketPool,
    },
    platform::io,
    simulation::dma::{backend, AuditEvent, DmaControl, SimEngine, SimMemory, TxFault},
};

use std::time::Duration;

type Queue = TxQueue<SimMemory, SimEngine>;
fn queue(frames: usize, slots: usize, descriptors: usize) -> (Queue, PacketPool, DmaControl) {
    let pool = PacketPool::new(frames, 256, 64);
    let (memory, engine, control) = backend(descriptors).unwrap();
    let queue = unsafe {
        TxQueue::new(
            pool.clone(),
            memory,
            engine,
            TxConfig {
                slots,
                completion_budget: 8,
                event_capacity: 8,
            },
        )
    }
    .unwrap();

    (queue, pool, control)
}

fn frame(pool: &PacketPool, data: &[u8]) -> PacketBuf {
    let mut frame = pool.alloc().unwrap();
    frame.set_len(data.len());
    frame.as_mut_packet().copy_from_slice(data);
    frame
}

fn complete(delay: u64) -> TxFault {
    TxFault::Complete {
        delay: Duration::from_millis(delay),
        failed: false,
        duplicate: false,
    }
}

#[test]
fn delayed_completion_pins_payload_and_orders_cache_hooks() {
    let (mut queue, pool, control) = queue(1, 1, 1);
    control.plan(complete(10)).unwrap();

    let mut tx = [frame(&pool, b"retained")];
    assert_eq!(queue.send(&mut tx).unwrap(), 1);
    assert!(tx[0].is_empty());
    assert!(pool.alloc().is_none());
    assert_eq!(queue.next_deadline(), Some(Duration::from_millis(10)));

    queue.poll_at(Duration::from_millis(9)).unwrap();
    assert_eq!(queue.pending(), 1);
    queue.poll_at(Duration::from_millis(10)).unwrap();
    assert_eq!(pool.available(), 1);
    assert_eq!(control.mapped(), 0);
    assert_eq!(queue.stats().completed, 1);
    assert_eq!(
        control.take_audit().into_iter().collect::<Vec<_>>(),
        vec![
            AuditEvent::Map(1),
            AuditEvent::DeviceSync(1),
            AuditEvent::Publish(1, 1),
            AuditEvent::Complete(1),
            AuditEvent::CpuSync(1),
            AuditEvent::Unmap(1),
        ]
    );
}

#[test]
fn partial_submission_and_mapping_failures_preserve_suffix() {
    let (mut queue, pool, control) = queue(3, 3, 3);
    control.plan(TxFault::Hold).unwrap();
    control.plan(TxFault::Reject).unwrap();

    let mut tx = [frame(&pool, b"a"), frame(&pool, b"b"), frame(&pool, b"c")];
    let pointer = tx[1].as_slice().as_ptr();
    assert_eq!(queue.send(&mut tx).unwrap(), 1);
    assert_eq!(tx[1].as_slice(), b"b");
    assert_eq!(tx[1].as_slice().as_ptr(), pointer);
    assert_eq!(control.mapped(), 1, "rejected descriptor was unmapped");

    control.fail_next_mapping();
    assert_eq!(
        queue.send(&mut tx[1..]).unwrap_err().kind(),
        io::ErrorKind::WouldBlock.into()
    );
    assert_eq!(queue.stats().mapping_errors, 1);
    assert_eq!(queue.stats().submission_errors, 1);
    assert_eq!(tx[1].as_slice(), b"b");
    assert!(queue.shutdown());
    assert_eq!(pool.available(), 1, "only accepted packet was reclaimed");
}
#[test]
fn reordered_failed_and_duplicate_completions_do_not_recycle_twice() {
    let (mut queue, pool, control) = queue(2, 2, 2);
    control.plan(complete(20)).unwrap();
    control
        .plan(TxFault::Complete {
            delay: Duration::from_millis(5),
            failed: true,
            duplicate: true,
        })
        .unwrap();

    let mut tx = [frame(&pool, b"a"), frame(&pool, b"b")];
    queue.send(&mut tx).unwrap();
    queue.poll_at(Duration::from_millis(5)).unwrap();
    assert_eq!(queue.pending(), 1);
    assert_eq!(pool.available(), 1);
    assert_eq!(queue.stats().completion_errors, 1);
    assert_eq!(queue.stats().stale_completions, 1);

    // Reuse the freed slot, but never its completion token.
    control.plan(TxFault::Hold).unwrap();
    queue.send(&mut [frame(&pool, b"c")]).unwrap();
    control
        .inject_completion(
            Duration::from_millis(6),
            Completion {
                token: 2,
                failed: false,
            },
        )
        .unwrap();

    queue.poll_at(Duration::from_millis(6)).unwrap();
    assert_eq!(pool.available(), 0);
    assert_eq!(queue.pending(), 2);

    queue.poll_at(Duration::from_millis(20)).unwrap();
    assert_eq!(queue.pending(), 1);
    assert_eq!(queue.stats().stale_completions, 2);
    assert!(queue.shutdown());
    assert_eq!(pool.available(), 2);
    assert_eq!(queue.stats().cancelled, 1);
}

#[test]
fn lost_completion_requires_quiescence_and_stop_can_be_retried() {
    let (mut queue, pool, control) = queue(1, 1, 1);
    control.plan(TxFault::Hold).unwrap();
    queue.send(&mut [frame(&pool, b"a")]).unwrap();
    control.allow_stop(false);
    queue.poll_at(Duration::from_secs(1000)).unwrap();
    assert!(!queue.shutdown());
    assert_eq!(pool.available(), 0);
    assert_eq!(control.in_flight(), 1);
    let mut empty = [PacketBuf::default()];
    assert_eq!(
        queue.send(&mut empty).unwrap_err().kind(),
        io::ErrorKind::NotConnected.into()
    );

    assert!(queue.alloc().is_none());
    control.allow_stop(true);
    assert!(queue.shutdown());
    assert!(queue.shutdown());
    assert_eq!(pool.available(), 1);
    assert_eq!(queue.stats().cancelled, 1);
}

#[test]
fn uncertain_drop_quarantines_instead_of_freeing_dma_memory() {
    let (mut queue, pool, control) = queue(1, 1, 1);
    control.plan(TxFault::Hold).unwrap();
    queue.send(&mut [frame(&pool, b"a")]).unwrap();
    control.allow_stop(false);
    drop(queue);
    // Intentional one-frame leak is the safe failure mode without proven stop.
    assert_eq!(pool.available(), 0);
    assert_eq!(control.mapped(), 1);
    assert_eq!(control.in_flight(), 1);
    assert!(!control
        .take_audit()
        .iter()
        .any(|e| matches!(e, AuditEvent::Unmap(_))));
}

#[test]
fn descriptor_pressure_uses_prefix_contract_and_stop_reclaims() {
    let (mut queue, pool, control) = queue(3, 1, 3);
    control.plan(TxFault::Hold).unwrap();

    let mut tx = [frame(&pool, b"a"), frame(&pool, b"b")];
    assert_eq!(queue.send(&mut tx).unwrap(), 1);
    assert_eq!(queue.send(&mut tx[1..]).unwrap(), 0);
    assert_eq!(tx[1].as_slice(), b"b");
    drop(queue);

    assert_eq!(pool.available(), 2);
    assert_eq!(control.mapped(), 0);
}

#[test]
fn event_and_completion_budgets_are_bounded_and_clock_is_monotonic() {
    let pool = PacketPool::new(2, 256, 64);
    let (memory, engine, control) = backend(2).unwrap();
    let mut queue = unsafe {
        TxQueue::new(
            pool.clone(),
            memory,
            engine,
            TxConfig {
                slots: 2,
                completion_budget: 1,
                event_capacity: 1,
            },
        )
    }
    .unwrap();
    queue
        .send(&mut [frame(&pool, b"a"), frame(&pool, b"b")])
        .unwrap();

    queue.poll_at(Duration::from_millis(1)).unwrap();
    assert_eq!(queue.pending(), 1);
    assert_eq!(
        queue.pop_event(),
        Some(OwnershipEvent::Completed(Completion {
            token: 1,
            failed: false
        }))
    );

    assert_eq!(queue.stats().event_overflow, 2);
    assert!(queue.poll_at(Duration::ZERO).is_err());
    assert_eq!(queue.pending(), 1);
    queue.poll_at(Duration::from_millis(1)).unwrap();
    assert_eq!(control.mapped(), 0);
}
#[test]
fn completion_after_failed_stop_can_still_release_packet() {
    let (mut queue, pool, control) = queue(1, 1, 1);
    control.plan(complete(10)).unwrap();
    queue.send(&mut [frame(&pool, b"a")]).unwrap();
    control.allow_stop(false);
    assert!(!queue.shutdown());
    queue.poll_at(Duration::from_millis(10)).unwrap();
    assert_eq!(queue.pending(), 0);
    assert_eq!(pool.available(), 1);
    control.allow_stop(true);
}

#[test]
fn rejection_before_any_acceptance_consumes_nothing() {
    let (mut queue, pool, control) = queue(1, 1, 1);
    control.plan(TxFault::Reject).unwrap();
    let mut tx = [frame(&pool, b"a")];
    let pointer = tx[0].as_slice().as_ptr();
    assert_eq!(
        queue.send(&mut tx).unwrap_err().kind(),
        io::ErrorKind::WouldBlock.into()
    );

    assert_eq!(tx[0].as_slice().as_ptr(), pointer);
    assert_eq!(control.mapped(), 0);
    assert_eq!(queue.pending(), 0);
    assert_eq!(queue.send(&mut tx).unwrap(), 1);
}

#[test]
fn timing_overflow_does_not_publish_or_leak_mapping() {
    let (mut queue, pool, control) = queue(1, 1, 1);
    control.plan(complete(1)).unwrap();
    queue.poll_at(Duration::MAX).unwrap();
    let mut tx = [frame(&pool, b"a")];
    assert!(queue.send(&mut tx).is_err());
    assert_eq!(tx[0].as_slice(), b"a");
    assert_eq!(control.mapped(), 0);
    assert_eq!(control.in_flight(), 0);
}
