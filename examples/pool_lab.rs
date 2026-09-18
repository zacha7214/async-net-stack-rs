//! Deterministic multi-pool fan-out/fan-in with UDP discovery.
use async_net_stack_rs::{
    api::{Action, Service, UdpPool},
    simulation::{Link, Network},
};
use clap::Parser;
use std::{
    net::{Ipv4Addr, SocketAddrV4},
    time::{Duration, Instant},
};
#[derive(Parser)]
struct Args {
    #[arg(long, default_value_t = 8, value_parser = clap::value_parser!(u8).range(1..=200))]
    workers: u8,
    #[arg(long, default_value_t = 1000)]
    rounds: usize,
    #[arg(long, default_value_t = 64)]
    batch: usize,
    #[arg(long, default_value_t = 0)]
    drop_every: u64,
    #[arg(long, default_value_t = 0)]
    delay_us: u64,
    #[arg(long, default_value_t = 0)]
    reorder_us: u64,
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    if args.batch == 0 || args.batch > 256 {
        return Err("batch must be 1..=256".into());
    }
    let network = Network::default();
    let client_addr = SocketAddrV4::new(Ipv4Addr::new(10, 77, 0, 1), 8000);
    let client_dev = network.port(&[*client_addr.ip()], 1024, 256)?;
    let mut workers = Vec::new();
    for i in 0..args.workers {
        let address = SocketAddrV4::new(Ipv4Addr::new(10, 77, 0, i + 2), 9000);
        let device = network.port(&[*address.ip()], 256, 64)?;
        // Only worker -> client is impaired: isolate response incast.
        network.set_link(
            &device,
            &client_dev,
            Link {
                delay: Duration::from_micros(args.delay_us),
                alternating_delay: Duration::from_micros(args.reorder_us),
                drop_every: args.drop_every,
                ..Link::default()
            },
        )?;
        let mut worker = UdpPool::new(device, 128, 1)?;
        worker.bind(Service { address, id: 7 })?;
        workers.push(worker);
    }
    let mut client = UdpPool::new(client_dev, 256, args.workers as usize)?;
    client.bind(Service {
        address: client_addr,
        id: 0,
    })?;
    let lease = Duration::from_secs(3600);
    // Retry discovery to tolerate deterministic response loss.
    for _ in 0..3 {
        client.discover(client_addr, SocketAddrV4::new(Ipv4Addr::BROADCAST, 9000), 7)?;
        client.flush()?;
        for worker in &mut workers {
            worker.poll(network.now(), lease, 256, |_| Action::Echo)?;
            worker.flush()?;
        }
        network.advance(Duration::from_micros(
            args.delay_us.saturating_add(args.reorder_us),
        ))?;
        client.poll(network.now(), lease, 256, |_| Action::Ignore)?;
    }
    let peers: Vec<_> = client.peers().map(|p| p.service.address).collect();
    if peers.is_empty() {
        return Err("no peers discovered (try less discovery loss)".into());
    }
    let started = Instant::now();
    let mut sent = 0u64;
    let mut received = 0u64;
    let mut reordered = 0u64;
    let mut highest = 0;
    let mut payload = [0u8; 128];
    for round in 0..args.rounds + 1000 {
        if round < args.rounds {
            for _ in 0..args.batch {
                payload[..8].copy_from_slice(&sent.to_be_bytes());
                match client.send_to(client_addr, peers[sent as usize % peers.len()], &payload) {
                    Ok(()) => sent += 1,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(e) => return Err(e.into()),
                }
            }
        }
        client.poll(network.now(), lease, 256, |d| {
            let sequence = u64::from_be_bytes(d.payload[..8].try_into().unwrap());
            if sequence < highest {
                reordered += 1;
            }
            highest = highest.max(sequence);
            received += 1;
            Action::Ignore
        })?;
        for worker in &mut workers {
            worker.poll(network.now(), lease, 256, |_| Action::Echo)?;
        }
        network.advance(Duration::from_micros(100))?;
    }
    let elapsed = started.elapsed().as_secs_f64();
    println!(
        "{}",
        serde_json::json!({"mode":"userspace-simulation", "peers":peers.len(),
        "queued_requests":sent, "received_replies":received, "unanswered_at_stop":sent-received,
        "out_of_order":reordered, "wall_seconds":elapsed, "replies_per_second":received as f64/elapsed,
        "simulated_seconds":network.now().as_secs_f64(), "fabric":format!("{:?}",network.stats())})
    );
    Ok(())
}
