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
