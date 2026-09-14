//! Integration tests exercising the public [`Device`] API through the in-memory
//! [`LoopbackDevice`]. These run unprivileged and verify the ownership/recycling
//! contract end-to-end.

use async_net_stack_rs::device::{Device, LoopbackDevice, PacketBuf};

#[test]
fn public_api_send_recv_roundtrip() {
    let mut dev = LoopbackDevice::with_capacity(16, 16);

    let mut tx = Vec::new();
    for i in 0..8u8 {
        let mut buf = dev.alloc().unwrap();
        let off = buf.data_offset();
        let payload = [i; 1500];
        buf.as_mut_slice()[off..off + payload.len()].copy_from_slice(&payload);
        buf.set_len(payload.len());
        tx.push(buf);
    }

    assert_eq!(dev.send(&mut tx).unwrap(), 8);

    let mut rx: Vec<PacketBuf> = Vec::new();
    assert_eq!(dev.recv(8, &mut rx).unwrap(), 8);

    for (i, buf) in rx.iter().enumerate() {
        assert_eq!(buf.len(), 1500);
        assert!(buf.as_slice().iter().all(|&b| b == i as u8));
    }
}

#[test]
fn public_api_alloc_recycles_on_drop() {
    let mut dev = LoopbackDevice::with_capacity(2, 2);

    let a = dev.alloc().unwrap();
    let b = dev.alloc().unwrap();
    assert!(dev.alloc().is_none(), "pool should be exhausted");

    drop(a);
    drop(b);
    assert!(dev.alloc().is_some(), "dropped buffers should be recycled");
}

#[test]
fn packets_survive_device_move_and_drop() {
    let mut dev = LoopbackDevice::new();
    let mut packet = dev.alloc().unwrap();
    packet.set_len(5);
    packet.as_mut_packet().copy_from_slice(b"alive");
    let mut moved = Box::new(dev);
    let another = moved.alloc().unwrap();
    drop(moved);
    assert_eq!(packet.as_slice(), b"alive");
    drop(another);
    drop(packet);
}

#[test]
fn partial_send_can_be_retried_and_slots_are_safe() {
    let mut dev = LoopbackDevice::with_capacity(4, 1);
    let mut tx = Vec::new();
    for value in [7, 9] {
        let mut buf = dev.alloc().unwrap();
        buf.set_len(1);
        buf.as_mut_packet()[0] = value;
        tx.push(buf);
    }
    assert_eq!(dev.send(&mut tx).unwrap(), 1);
    assert!(tx[0].as_slice().is_empty());
    assert!(tx[0].as_mut_slice().is_empty());
    assert_eq!(tx[1].as_slice(), &[9]);
    let mut rx = Vec::new();
    assert_eq!(dev.recv(1, &mut rx).unwrap(), 1);
    assert_eq!(rx[0].as_slice(), &[7]);
    assert_eq!(dev.send(&mut tx[1..]).unwrap(), 1);
    assert_eq!(dev.recv(1, &mut rx).unwrap(), 1);
    assert_eq!(rx[0].as_slice(), &[9]);
}

#[test]
#[should_panic(expected = "available capacity")]
fn length_includes_headroom_in_bounds_check() {
    let mut dev = LoopbackDevice::new();
    let mut packet = dev.alloc().unwrap();
    packet.set_len(packet.capacity());
}

#[test]
fn exhausted_receive_does_not_lose_queued_packets() {
    let mut dev = LoopbackDevice::with_capacity(1, 1);
    let mut tx = vec![dev.alloc().unwrap()];
    tx[0].set_len(1);
    tx[0].as_mut_packet()[0] = 42;
    dev.send(&mut tx).unwrap();
    let held = dev.alloc().unwrap();
    let mut rx = Vec::new();
    assert_eq!(dev.recv(1, &mut rx).unwrap(), 0);
    assert_eq!(dev.queued(), 1);
    drop(held);
    assert_eq!(dev.recv(1, &mut rx).unwrap(), 1);
    assert_eq!(rx[0].as_slice(), &[42]);
}
