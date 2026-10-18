//! Deterministic TX ownership experiment, with no NIC or OS clock.
use async_net_stack_rs::{
    device::{dma::TxConfig, Device, PacketPool},
    platform::io,
    simulation::dma::{tx_queue, TxFault},
    transport::udp::build_ipv4,
};

use std::time::Duration;

fn main() -> io::Result<()> {
    run()
}

fn run() -> io::Result<()> {
    let pool = PacketPool::new(3, 2048, 64);
    let (mut queue, faults) = tx_queue(
        pool.clone(),
        TxConfig {
            slots: 2,
            ..TxConfig::default()
        },
        3,
    )?;

    faults.plan(TxFault::Complete {
        delay: Duration::from_millis(20),
        failed: false,
        duplicate: true,
    })?;

    faults.plan(TxFault::Hold)?;
    let mut packets = Vec::new();
    for _ in 0..3 {
        let mut packet = queue.alloc().unwrap();
        build_ipv4(
            &mut packet,
            ([10, 0, 0, 1], 9000),
            ([10, 0, 0, 2], 9000),
            b"ownership probe",
            0,
        )?;

        packets.push(packet);
    }

    assert_eq!(queue.send(&mut packets)?, 2);
    assert_eq!(pool.available(), 0);
    println!("t=0ms: accepted=2, retained_by_caller=1, free_frames=0");
    queue.poll_at(Duration::from_millis(20))?;

    assert_eq!(queue.stats().stale_completions, 1);
    assert_eq!(pool.available(), 1);
    println!("t=20ms: completed=1, duplicate_rejected=1, free_frames=1");

    // The suffix can now be submitted; its default immediate completion is
    // independent of the withheld completion for token 2.
    assert_eq!(queue.send(&mut packets[2..])?, 1);

    queue.poll_at(Duration::from_millis(20))?;
    assert_eq!(queue.pending(), 1);

    faults.allow_stop(false);
    assert!(!queue.shutdown());
    assert_eq!(pool.available(), 2);
    println!("stop unconfirmed: outstanding=1, free_frames=2");

    faults.allow_stop(true);
    assert!(queue.shutdown());
    assert_eq!(pool.available(), 3);
    println!(
        "stop confirmed: outstanding=0, free_frames=3, stats={:?}",
        queue.stats()
    );

    Ok(())
}

#[test]
fn completion_and_teardown_faults_preserve_ownership() {
    run().unwrap();
}
