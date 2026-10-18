//! Compare incast under different rates and byte queue limits, without sleeping.
use async_net_stack_rs::{
    device::Device,
    simulation::{Bandwidth, Network},
    transport::udp::build_ipv4,
};

use std::{io, net::Ipv4Addr};

fn main() -> io::Result<()> {
    for (rate, capacity) in [(1_000_000, 3000), (1_000_000, 6000), (10_000_000, 6000)] {
        run(rate, capacity)?;
    }

    Ok(())
}

fn run(rate: u64, capacity: usize) -> io::Result<()> {
    let net = Network::default();
    let destination = Ipv4Addr::new(10, 0, 0, 3);
    let mut receiver = net.port(&[destination], 16, 16)?;

    let mut senders = [
        net.port(&[Ipv4Addr::new(10, 0, 0, 1)], 8, 8)?,
        net.port(&[Ipv4Addr::new(10, 0, 0, 2)], 8, 8)?,
    ];

    let bottleneck = net.bottleneck(Bandwidth {
        bits_per_second: rate,
        queue_bytes: capacity,
        overhead_bytes: 0,
    })?;

    for sender in &senders {
        net.set_bottleneck(sender, &receiver, Some(&bottleneck))?;
    }

    // Interleave equal-time offers so each sender participates in the FIFO.
    for _ in 0..3 {
        for (i, sender) in senders.iter_mut().enumerate() {
            let mut frame = sender.alloc().unwrap();
            build_ipv4(
                &mut frame,
                ([10, 0, 0, i as u8 + 1], 9000),
                (destination.octets(), 9000),
                &[0; 972],
                0,
            )?;

            sender.send(&mut [frame])?;
        }
    }

    let mut rx = Vec::new();
    while let Some(deadline) = receiver.next_deadline() {
        net.advance(deadline.saturating_sub(net.now()))?;
        receiver.recv(16, &mut rx)?;
    }

    println!(
        "{}",
        serde_json::json!({
            "rate_bps": rate, "queue_limit_bytes": capacity, "offered_packets":6,
            "delivered_packets": net.stats().delivered,
            "bandwidth_drops":net.stats().bandwidth_drops,
            "last_arrival_ns":net.now().as_nanos(),
            "max_queue_delay_ns":bottleneck.stats().max_queue_delay.as_nanos(),
            "peak_queue_bytes":bottleneck.stats().peak_bytes,
        })
    );

    Ok(())
}
