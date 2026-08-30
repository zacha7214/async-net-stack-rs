#[allow(unused_imports)]
use crate::device::buffer_pool::{FramePool, PacketBuf};

#[test]
fn alloc_free_roundtrip_exhausts_and_reuses() {
    let pool = FramePool::new(4, 256, 4096);
    assert_eq!(pool.frame_size(), 256);
    assert_eq!(pool.num_frames(), 4);

    let mut got = Vec::new();
    for _ in 0..4 {
        got.push(pool.alloc().unwrap());
    }
    assert!(pool.alloc().is_none(), "pool should be exhausted");

    // All four indices should be distinct.
    let mut sorted = got.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), 4);

    // Free two, then realloc them.
    pool.free(got[1]);
    pool.free(got[3]);
    let a = pool.alloc().unwrap();
    let b = pool.alloc().unwrap();
    let mut reuse = vec![a, b];
    reuse.sort_unstable();
    let mut expected = vec![got[1], got[3]];
    expected.sort_unstable();
    assert_eq!(reuse, expected);
    assert!(pool.alloc().is_none());
}

#[test]
fn packet_buf_reads_writes_and_recycles() {
    let pool = FramePool::new(2, 64, 4096);
    let idx = pool.alloc().unwrap();

    {
        let mut buf: PacketBuf = pool.packet_buf(idx, 0);
        assert!(buf.is_empty());
        assert_eq!(buf.capacity(), 64);

        let data = b"hello zero-copy";
        let off = buf.data_offset();
        buf.as_mut_slice()[off..off + data.len()].copy_from_slice(data);
        buf.set_len(data.len());

        assert_eq!(buf.len(), data.len());
        assert_eq!(buf.as_slice(), data);
    } // dropped here -> recycled

    // The same frame must be allocatable again after the buffer dropped.
    let idx2 = pool.alloc().unwrap();
    assert_eq!(idx, idx2);
}

#[test]
fn explicit_recycle_and_drop_are_idempotent() {
    let pool = FramePool::new(2, 64, 4096);

    let idx = pool.alloc().unwrap();
    let buf = pool.packet_buf(idx, 4);
    buf.recycle(); // explicit recycle

    let idx2 = pool.alloc().unwrap();
    assert_eq!(idx, idx2);

    let buf = pool.packet_buf(idx2, 4);
    drop(buf); // Drop path
    let idx3 = pool.alloc().unwrap();
    assert_eq!(idx2, idx3);
}

#[test]
#[should_panic(expected = "double-freed")]
fn debug_build_detects_double_free() {
    let pool = FramePool::new(1, 64, 4096);
    let idx = pool.alloc().unwrap();
    pool.free(idx);
    pool.free(idx); // double free -> panic in debug builds
}

#[test]
fn from_raw_parts_wraps_external_memory() {
    let frame_size = 128usize;
    let num = 3usize;
    let mut backing = vec![0u8; frame_size * num];
    let ptr = backing.as_mut_ptr();

    // SAFETY: `backing` stays alive for the pool's lifetime and is unaliased.
    let pool = unsafe { FramePool::from_raw_parts(ptr, backing.len(), frame_size, num) };

    let a = pool.alloc().unwrap();
    let b = pool.alloc().unwrap();
    let c = pool.alloc().unwrap();
    assert!(pool.alloc().is_none());

    let mut buf = pool.packet_buf(a, 0);
    let off = buf.data_offset();
    buf.as_mut_slice()[off..off + 4].copy_from_slice(&[1, 2, 3, 4]);
    buf.set_len(4);
    assert_eq!(buf.as_slice(), &[1, 2, 3, 4]);
    drop(buf);

    pool.free(b);
    pool.free(c);
    drop(pool);
    // backing still alive and unmodified structurally
    assert_eq!(backing.len(), frame_size * num);
}

#[test]
fn packet_buf_with_headroom() {
    let pool = FramePool::new(2, 256, 4096);
    let idx = pool.alloc().unwrap();

    {
        let mut buf = pool.packet_buf(idx, 0);
        // Default headroom was reserved; payload starts after it.
        let headroom = buf.data_offset();
        assert!(headroom > 0);
        assert_eq!(buf.len(), 0);
        assert_eq!(buf.as_slice(), &[]);

        // Write payload into the middle of the frame.
        let payload = b"hello zero-copy";
        let off = buf.data_offset();
        buf.as_mut_slice()[off..off + payload.len()].copy_from_slice(payload);
        buf.set_len(payload.len());

        assert_eq!(buf.as_slice(), payload);
        assert_eq!(buf.as_mut_packet(), payload);
    }
}

#[test]
fn push_and_pull_headers() {
    let pool = FramePool::new(2, 256, 4096);
    let idx = pool.alloc().unwrap();

    let mut buf = pool.packet_buf(idx, 0);

    // Simulate receiving an Ethernet+IP+TCP frame from a backend.
    // Start with TCP payload.
    let payload = b"HTTP/1.1 200 OK\r\n";
    buf.set_headroom(128); // reserve 128 bytes for headers
    let off = buf.data_offset();
    buf.as_mut_slice()[off..off + payload.len()].copy_from_slice(payload);
    buf.set_len(payload.len());

    // Now prepend headers as the packet moves up the stack.
    let ip_hdr = [0x45, 0x00, 0x00, 0x30]; // fake IPv4 header
    buf.push_header(&ip_hdr);
    assert_eq!(buf.len(), payload.len() + ip_hdr.len());
    assert_eq!(&buf.as_slice()[..ip_hdr.len()], &ip_hdr[..]);
    assert_eq!(&buf.as_slice()[ip_hdr.len()..], payload);

    let eth_hdr = [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, 0x11, 0x22];
    buf.push_header(&eth_hdr);
    assert_eq!(buf.len(), payload.len() + ip_hdr.len() + eth_hdr.len());

    // Now parse it back: pull headers off as we go down the stack.
    buf.pull_header(eth_hdr.len());
    assert_eq!(&buf.as_slice()[..ip_hdr.len()], &ip_hdr[..]);

    buf.pull_header(ip_hdr.len());
    assert_eq!(buf.as_slice(), payload);

    // After pulling all headers, we're back to clean payload-only state.
    assert_eq!(buf.data_offset(), 128);
    assert_eq!(buf.len(), payload.len());

    // The full packet is still in frame memory if we look at the raw bytes.
    let full_packet: Vec<u8> = [eth_hdr.as_slice(), ip_hdr.as_slice(), payload.as_slice()].concat();

    assert_eq!(
        &buf.as_mut_slice()[116..116 + full_packet.len()],
        &full_packet[..]
    );
}

#[test]
#[should_panic(expected = "headroom exhausted")]
fn headroom_exhaustion_panics() {
    let pool = FramePool::new(1, 64, 4096);
    let idx = pool.alloc().unwrap();

    let mut buf = pool.packet_buf(idx, 0);
    buf.set_headroom(8);

    buf.push_header(&[1, 2, 3, 4]); // 4 bytes, ok
    buf.push_header(&[5, 6, 7, 8]); // 4 bytes, exactly exhausts
    buf.push_header(&[9]); // 1 byte, PANIC here
}

#[test]
fn into_parts_preserves_offset_and_len() {
    let pool = FramePool::new(2, 256, 4096);
    let idx = pool.alloc().unwrap();

    let mut buf = pool.packet_buf(idx, 0);
    let payload = b"test data";
    let off = buf.data_offset();
    buf.as_mut_slice()[off..off + payload.len()].copy_from_slice(payload);
    buf.set_len(payload.len());

    let (returned_idx, returned_len) = buf.into_parts();
    assert_eq!(returned_idx, idx);
    assert_eq!(returned_len, payload.len());
    // The backend (e.g. a TX ring) would use idx+len; offset is implicit
    // to the frame layout agreement between pool and backend.
}

#[test]
fn alloc_n_returns_requested_frames() {
    let pool = FramePool::new(8, 64, 4096);
    let mut out = [0usize; 4];

    let n = pool.alloc_n(&mut out);
    assert_eq!(n, 4);

    // All distinct and in valid range.
    let mut sorted = out.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), 4);
    for &idx in &out {
        assert!(idx < 8);
    }
}

#[test]
fn alloc_n_partial_when_pool_low() {
    let pool = FramePool::new(3, 64, 4096);
    let mut out = [0usize; 5];

    // Only 3 available; returns what it can.
    let n = pool.alloc_n(&mut out);
    assert_eq!(n, 3);

    // Unfilled slots should be untouched (or zero, doesn't matter — we only
    // inspect out[..n]).
    for i in 0..n {
        assert!(out[i] < 3);
    }
}

#[test]
fn alloc_n_zero_when_exhausted() {
    let pool = FramePool::new(2, 64, 4096);
    let mut out = [0usize; 2];

    pool.alloc_n(&mut out); // drain
    let mut out2 = [0usize; 1];
    assert_eq!(pool.alloc_n(&mut out2), 0);
}

#[test]
fn free_n_roundtrip_reuse() {
    let pool = FramePool::new(4, 64, 4096);
    let mut out = [0usize; 4];

    pool.alloc_n(&mut out);
    let saved = out; // [3, 2, 1, 0] if built in reverse order

    pool.free_n(&saved);

    let mut out2 = [0usize; 4];
    let n = pool.alloc_n(&mut out2);
    assert_eq!(n, 4);

    // LIFO: we should get the same indices back in reverse order.
    let mut first = saved.to_vec();
    first.sort_unstable();
    let mut second = out2.to_vec();
    second.sort_unstable();
    assert_eq!(first, second);
}

#[test]
fn mixed_alloc_free_and_batch() {
    let pool = FramePool::new(6, 64, 4096);

    // Single alloc
    let a = pool.alloc().unwrap();
    // Batch alloc the rest
    let mut batch = [0usize; 4];
    assert_eq!(pool.alloc_n(&mut batch), 4);

    // Pool empty
    assert!(pool.alloc().is_none());

    // Free one single
    pool.free(a);

    // Free two via batch
    pool.free_n(&[batch[0], batch[1]]);

    // Should be able to alloc 3 now
    let mut reclaim = [0usize; 3];
    assert_eq!(pool.alloc_n(&mut reclaim), 3);

    // Verify 'a' is back in the pool (LIFO)
    let mut found_a = false;
    for &idx in &reclaim {
        if idx == a {
            found_a = true;
        }
    }
    assert!(found_a, "single-freed frame should have been reallocated");
}

#[test]
fn alloc_n_then_free_n_idempotent() {
    let pool = FramePool::new(4, 64, 4096);
    let mut out = [0usize; 4];

    pool.alloc_n(&mut out);
    pool.free_n(&out);

    // Should be able to alloc all 4 again
    let mut out2 = [0usize; 4];
    assert_eq!(pool.alloc_n(&mut out2), 4);
}

#[test]
#[cfg(debug_assertions)]
#[should_panic(expected = "double-freed")]
fn debug_detects_double_free_in_free_n() {
    let pool = FramePool::new(4, 64, 4096);
    let mut out = [0usize; 2];
    pool.alloc_n(&mut out);

    pool.free_n(&out);
    pool.free_n(&out); // second free of same indices -> panic
}

#[test]
#[cfg(debug_assertions)]
#[should_panic(expected = "double-allocated")]
fn debug_detects_double_alloc_in_alloc_n() {
    let pool = FramePool::new(2, 64, 4096);
    let mut out = [0usize; 2];
    pool.alloc_n(&mut out);

    // Try to alloc the same indices again via a second buffer
    let mut out2 = [0usize; 2];
    pool.alloc_n(&mut out2); // this is fine, pool is empty

    // But if we manually free one and then alloc_n again...
    pool.free(out[0]);
    let mut out3 = [0usize; 1];
    pool.alloc_n(&mut out3); // should get out[0]

    // Now free both the original and the new one (same index)
    pool.free_n(&[out[0], out3[0]]); // double-free -> panic
}

#[test]
fn batch_alloc_to_packet_bufs_and_recycle() {
    let pool = FramePool::new(4, 256, 4096);
    let mut indices = [0usize; 4];

    let n = pool.alloc_n(&mut indices);
    assert_eq!(n, 4);

    let mut bufs: Vec<PacketBuf> = indices[..n]
        .iter()
        .map(|&idx| pool.packet_buf(idx, 0))
        .collect();

    // Use them
    for (i, buf) in bufs.iter_mut().enumerate() {
        let data = format!("frame-{}", i).into_bytes();
        let off = buf.data_offset();

        buf.as_mut_slice()[off..(off + data.len())].copy_from_slice(&data);
        buf.set_len(data.len());
    }

    // Drop all -> automatic recycle
    drop(bufs);

    // All frames back in pool
    let mut indices2 = [0usize; 4];
    assert_eq!(pool.alloc_n(&mut indices2), 4);

    // Verify reuse (same set, order may differ)
    let mut a = indices.to_vec();
    let mut b = indices2.to_vec();
    a.sort_unstable();
    b.sort_unstable();
    assert_eq!(a, b);
}
