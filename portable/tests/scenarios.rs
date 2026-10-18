// Run the host scenarios against the no_std engines, without copying tests.
extern crate async_net_stack_core as async_net_stack_rs;
#[path = "../../tests/ap_lifecycle.rs"]
mod ap_lifecycle;
#[path = "../../tests/bandwidth.rs"]
mod bandwidth;

#[allow(dead_code)]
#[path = "../../examples/coordinator_lab.rs"]
mod composed_tcp_udp;

#[path = "../../tests/coordinator.rs"]
mod coordinator;

#[path = "../../tests/ethernet_ipv4.rs"]
mod ethernet_ipv4;

#[path = "../../tests/network_lab.rs"]
mod network_lab;

#[test]
fn portable_tcp_requires_explicit_entropy() {
    use async_net_stack_core::{
        api::tcp::{TcpConfig, TcpPool},
        device::LoopbackDevice,
        platform::io,
    };
    
    assert!(
        matches!(TcpPool::new(LoopbackDevice::new(), TcpConfig::default()),
        Err(e) if e.kind() == io::ErrorKind::Unsupported.into())
    );
}

#[test]
fn packet_arena_ownership_survives_pool_drop() {
    use async_net_stack_core::device::PacketPool;
    
    let pool = PacketPool::new(1, 256, 64);
    
    let mut packet = pool.alloc().unwrap();
    assert!(pool.alloc().is_none());
    
    packet.set_len(3);
    packet.as_mut_packet().copy_from_slice(b"abc");
    
    let retained = pool.clone();
    drop(pool);
    assert_eq!(packet.as_slice(), b"abc");
    drop(packet);
    assert_eq!(retained.available(), 1);
    
    let packet = retained
        .alloc()
        .unwrap();
    
    drop(retained);
    assert_eq!(packet.capacity(), 256);
}

#[path = "../../tests/dma.rs"]
mod dma;

#[path = "../../tests/dma_rx.rs"]
mod dma_rx;
