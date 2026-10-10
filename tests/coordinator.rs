use async_net_stack_rs::{
    coordinator::{Coordinator, PortConfig},
    device::{Device, LoopbackDevice, PacketBuf},
    transport::udp,
};
use std::{io, time::Duration};

fn packet(dev: &mut impl Device, value: u8) -> PacketBuf {
    let mut frame = dev.alloc().unwrap();
    udp::build_ipv4(
        &mut frame,
        ([10, 0, 0, 1], 1234),
        ([10, 0, 0, 2], 9000),
        &[value],
        0,
    )
    .unwrap();
    frame
}
#[test]
fn full_subscription_does_not_block_another_protocol() {
    let mut device = LoopbackDevice::with_capacity(16, 16);
    let mut frames = vec![
        packet(&mut device, 1),
        packet(&mut device, 2),
        packet(&mut device, 3),
    ];
    // A custom protocol is dispatched without transport interpretation.
    frames[2].as_mut_packet()[9] = 99;
    frames[2].as_mut_packet()[10..12].fill(0);
    let sum = async_net_stack_rs::net::checksum(&frames[2].as_slice()[..20]);
    frames[2].as_mut_packet()[10..12].copy_from_slice(&sum.to_be_bytes());
    device.send(&mut frames).unwrap();
    let mut coordinator = Coordinator::new(device);
    let cfg = PortConfig {
        rx_capacity: 1,
        tx_budget: 1,
    };
    let mut udp = coordinator.port(17, cfg).unwrap();
    let mut custom = coordinator.port(99, cfg).unwrap();
    assert_eq!(coordinator.poll(Duration::ZERO, 3).unwrap(), 3);
    assert_eq!(udp.stats().queue_drops, 1);
    let mut rx = Vec::new();
    assert_eq!(custom.recv(8, &mut rx).unwrap(), 1);
    assert_eq!(rx[0].as_slice()[28], 3);
    assert_eq!(udp.recv(8, &mut rx).unwrap(), 1);
    assert_eq!(rx[0].as_slice()[28], 1);
}
#[test]
fn budgets_preserve_unsent_suffix_and_renew_only_at_coordinator() {
    let mut coordinator = Coordinator::new(LoopbackDevice::with_capacity(8, 8));
    let cfg = PortConfig {
        rx_capacity: 4,
        tx_budget: 1,
    };
    let mut a = coordinator.port(17, cfg).unwrap();
    let mut b = coordinator.port(6, cfg).unwrap();
    coordinator.poll(Duration::ZERO, 8).unwrap();
    let mut tx = vec![packet(&mut a, 1), packet(&mut a, 2)];
    assert_eq!(a.send(&mut tx).unwrap(), 1);
    assert!(tx[0].is_empty());
    assert_eq!(tx[1].as_slice()[28], 2);
    a.poll_at(Duration::ZERO).unwrap();
    assert_eq!(a.send(&mut tx[1..]).unwrap(), 0);
    let mut other = vec![packet(&mut b, 3)];
    assert_eq!(b.send(&mut other).unwrap(), 1);
    coordinator.poll(Duration::ZERO, 8).unwrap();
    assert_eq!(a.send(&mut tx[1..]).unwrap(), 1);
    assert_eq!(a.stats().submitted, 2);
}
#[test]
fn subscription_lifetime_and_clock_contract() {
    let mut coordinator = Coordinator::new(LoopbackDevice::with_capacity(4, 4));
    let cfg = PortConfig::default();
    let mut port = coordinator.port(17, cfg).unwrap();
    assert!(coordinator.port(17, cfg).is_err());
    assert!(coordinator
        .port(
            6,
            PortConfig {
                rx_capacity: 0,
                ..cfg
            }
        )
        .is_err());
    let now = Duration::from_secs(1);
    assert!(port.poll_at(now).is_err());
    coordinator.poll(now, 4).unwrap();
    port.poll_at(now).unwrap();
    assert!(coordinator.poll(Duration::ZERO, 4).is_err());
    let mut tx = vec![packet(&mut port, 1)];
    port.send(&mut tx).unwrap();
    coordinator.poll(now, 4).unwrap();
    assert_eq!(port.queued(), 1);
    drop(port);
    let mut replacement = coordinator.port(17, cfg).unwrap();
    assert_eq!(replacement.queued(), 0);
    // All buffers from the removed subscription are available again.
    let held: Vec<_> = (0..4).map(|_| replacement.alloc().unwrap()).collect();
    assert!(replacement.alloc().is_none());
    drop(held);
}
struct FailingDevice {
    inner: LoopbackDevice,
}
impl Device for FailingDevice {
    fn recv(&mut self, max: usize, out: &mut Vec<PacketBuf>) -> io::Result<usize> {
        self.inner.recv(max, out)
    }
    fn send(&mut self, _: &mut [PacketBuf]) -> io::Result<usize> {
        Err(io::Error::new(io::ErrorKind::WouldBlock, "injected"))
    }
    fn alloc(&mut self) -> Option<PacketBuf> {
        self.inner.alloc()
    }
    fn frame_size(&self) -> usize {
        self.inner.frame_size()
    }
    fn next_deadline(&self) -> Option<Duration> {
        Some(Duration::from_secs(2))
    }
}
#[test]
fn backend_error_preserves_ownership_and_adapter_deadlines_are_combined() {
    let mut coordinator = Coordinator::new(FailingDevice {
        inner: LoopbackDevice::new(),
    });
    let mut port = coordinator.port(17, PortConfig::default()).unwrap();
    coordinator.poll(Duration::ZERO, 4).unwrap();
    let mut tx = vec![packet(&mut port, 7)];
    assert_eq!(
        port.send(&mut tx).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    assert_eq!(tx[0].as_slice()[28], 7);
    assert_eq!(port.stats().submitted, 0);
    assert_eq!(
        coordinator.next_deadline([None, Some(Duration::from_secs(3))]),
        Some(Duration::from_secs(2))
    );
    assert_eq!(
        coordinator.next_deadline([Some(Duration::from_secs(1))]),
        Some(Duration::from_secs(1))
    );
}
#[test]
fn malformed_and_unclaimed_are_distinct() {
    let mut device = LoopbackDevice::new();
    let mut tx = vec![packet(&mut device, 1), packet(&mut device, 2)];
    tx[0].as_mut_packet()[0] = 0;
    device.send(&mut tx).unwrap();
    let mut coordinator = Coordinator::new(device);
    coordinator.poll(Duration::ZERO, 8).unwrap();
    assert_eq!(coordinator.stats().malformed, 1);
    assert_eq!(coordinator.stats().unclaimed, 1);
}

#[test]
fn simulated_dispatch_moves_the_original_packet_handle() {
    use async_net_stack_rs::simulation::Network;
    let net = Network::default();
    let mut sender = net.port(&[[10, 0, 0, 1].into()], 4, 4).unwrap();
    let receiver = net.port(&[[10, 0, 0, 2].into()], 4, 4).unwrap();
    let mut tx = vec![packet(&mut sender, 42)];
    let original = tx[0].as_slice().as_ptr();
    sender.send(&mut tx).unwrap();
    let mut coordinator = Coordinator::new(receiver);
    let mut port = coordinator.port(17, PortConfig::default()).unwrap();
    coordinator.poll(Duration::ZERO, 4).unwrap();
    let mut rx = Vec::new();
    port.recv(4, &mut rx).unwrap();
    assert_eq!(rx[0].as_slice().as_ptr(), original);
    drop(sender);
    drop(port);
    drop(coordinator);
    assert_eq!(rx[0].as_slice()[28], 42);
}
