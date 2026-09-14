//! Guest half of docs/vhost-user-lab.md; validates data and sampled UMEM GPAs.
#[cfg(target_os = "linux")]
#[path = "support/vm_packet.rs"]
mod vm_packet;
#[cfg(target_os = "linux")]
mod guest {
    use super::vm_packet;
    use async_net_stack_rs::device::{Device, PacketBuf, XdpConfig, XdpDevice, XdpMode};
    use clap::Parser;
    use serde_json::json;
    use std::fs::File;
    use std::io;
    use std::os::unix::fs::FileExt;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
    type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
    #[derive(Parser)]
    struct Args {
        #[arg(long)]
        iface: String,
        #[arg(long, default_value="zero-copy", value_parser=["zero-copy", "copy"])]
        mode: String,
        #[arg(long, default_value_t = 10000)]
        packets: u64,
        #[arg(long, default_value_t = 64)]
        batch: usize,
        #[arg(long, default_value_t = 8)]
        samples: usize,
        #[arg(long, default_value_t = 30)]
        timeout: u64,
        /// Use only when pagemap PFNs are restricted; this weakens the evidence.
        #[arg(long)]
        skip_address_check: bool,
    }
    fn physical_address(pagemap: &File, pointer: *const u8, page: u64) -> io::Result<u64> {
        let address = pointer as u64;
        let mut raw = [0; 8];
        pagemap.read_exact_at(&mut raw, (address / page) * 8)?;
        let entry = u64::from_ne_bytes(raw);
        let pfn = entry & ((1u64 << 55) - 1);
        if entry & (1 << 63) == 0 || entry & (1 << 62) != 0 || pfn == 0 {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied,
                "pagemap PFN unavailable: run as guest root with CAP_SYS_ADMIN, or explicitly --skip-address-check"));
        }
        Ok(pfn * page + address % page)
    }
    fn control(
        dev: &mut XdpDevice,
        kind: [u8; 2],
        session: u64,
        count: u64,
        timeout: u64,
    ) -> Result<()> {
        let mut packet = dev.alloc().ok_or("no TX frame available")?;
        let h = vm_packet::header(kind, 0, count, 64, session);
        packet.as_mut_slice()[..64].fill(0);
        packet.as_mut_slice()[..h.len()].copy_from_slice(&h);
        packet.set_len(64);
        let mut frames = [packet];
        let deadline = Instant::now() + Duration::from_secs(timeout);
        while dev.send(&mut frames)? == 0 {
            dev.progress();
            if Instant::now() >= deadline {
                return Err("timed out sending control packet".into());
            }
            std::thread::yield_now();
        }
        Ok(())
    }
    pub fn main() -> Result<()> {
        let a = Args::parse();
        if a.packets == 0
            || a.packets > 1_000_000_000
            || !(1..=256).contains(&a.batch)
            || a.samples == 0
            || a.samples > 64
            || a.timeout == 0
            || a.timeout > 3600
        {
            return Err("invalid count, batch, samples or timeout".into());
        }
        let zero = a.mode == "zero-copy";
        let cfg = XdpConfig {
            mode: if zero {
                XdpMode::ZeroCopy
            } else {
                XdpMode::Copy
            },
            batch_size: a.batch,
            tx_reserve: 64,
            headroom: 32,
            ..XdpConfig::default()
        };
        let mut dev = XdpDevice::with_config(&a.iface, 0, &cfg)?;
        if dev.is_zero_copy() != zero {
            return Err("kernel XDP_OPTIONS disagrees with requested mode".into());
        }
        let pagemap = if a.skip_address_check {
            None
        } else {
            Some(File::open("/proc/self/pagemap")?)
        };
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        if page <= 0 {
            return Err("cannot read guest page size".into());
        }
        let session = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos() as u64;
        println!(
            "{}",
            json!({"event":"guest_ready", "session":session, "zero_copy":dev.is_zero_copy(),
            "attach":format!("{:?}", dev.attach_mode()), "page_size":page, "address_check":!a.skip_address_check})
        );
        control(&mut dev, *b"VS", session, a.packets, a.timeout)?;
        let mut frames: Vec<PacketBuf> = Vec::with_capacity(a.batch);
        let mut received = 0u64;
        let mut checked = 0usize;
        let mut matched = 0usize;
        let start = Instant::now();
        let mut last = start;
        let result: Result<()> = (|| {
            while received < a.packets {
                if last.elapsed() > Duration::from_secs(a.timeout) {
                    return Err(format!(
                        "no progress: received {received}/{}; inspect backend/IOTLB logs",
                        a.packets
                    )
                    .into());
                }
                if dev.recv(a.batch, &mut frames)? == 0 {
                    std::thread::yield_now();
                    continue;
                }
                for frame in &frames {
                    let b = frame.as_slice();
                    if !vm_packet::identify(b, b"VD") || vm_packet::u64_at(b, 40) != session {
                        continue;
                    }
                    let seq = vm_packet::u64_at(b, 16);
                    if seq != received {
                        return Err(format!(
                            "sequence gap: expected {received}, got {seq}; lower backend --pps"
                        )
                        .into());
                    }
                    let size = u32::from_le_bytes(b[32..36].try_into().unwrap()) as usize;
                    if size != b.len()
                        || b[36..40] != [0; 4]
                        || b[vm_packet::HEADER..]
                            .iter()
                            .enumerate()
                            .any(|(i, &v)| v != vm_packet::payload_byte(seq, i))
                    {
                        return Err(format!("packet {seq}: length/payload mismatch").into());
                    }
                    if checked < a.samples {
                        let expected = vm_packet::u64_at(b, 24);
                        let actual = pagemap
                            .as_ref()
                            .map(|p| physical_address(p, b.as_ptr(), page as u64))
                            .transpose()?;
                        if actual == Some(expected) {
                            matched += 1;
                        }
                        println!(
                            "{}",
                            json!({"event":"guest_sample", "session":session, "sequence":seq,
                            "backend_frame_gpa":format!("{expected:#x}"), "umem_frame_gpa":actual.map(|g| format!("{g:#x}")),
                            "same_frame":actual.map(|g| g == expected), "data_offset":frame.data_offset()})
                        );
                        if zero && actual.is_some() && actual != Some(expected) {
                            return Err(
                                "zero-copy sample did not land in the same physical frame".into()
                            );
                        }
                        checked += 1;
                    }
                    received += 1;
                    last = Instant::now();
                }
                // Drop the application leases, then refill FILL and drive the
                // driver's NEED_WAKEUP path. This is the final step of the lab.
                frames.clear();
                dev.progress();
            }
            Ok(())
        })();
        frames.clear();
        if result.is_err() {
            let _ = control(&mut dev, *b"VE", session, 0, 1);
        }
        let drain = Instant::now() + Duration::from_millis(250);
        while dev.pending_tx() > 0 && Instant::now() < drain {
            dev.progress();
            std::thread::yield_now();
        }
        let stats = dev.stats()?;
        println!(
            "{}",
            json!({"event":"guest_result", "ok":result.is_ok(), "session":session,
            "requested":a.packets, "received":received, "zero_copy":dev.is_zero_copy(), "samples":checked,
            "physical_matches":matched, "address_check":!a.skip_address_check, "seconds":start.elapsed().as_secs_f64(),
            "rx_dropped":stats.rx_dropped, "rx_invalid":stats.rx_invalid_descs,
            "pending_tx":dev.pending_tx(), "tx_completed":dev.counters().tx_completed})
        );
        result
    }
}
#[cfg(target_os = "linux")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    guest::main()
}
#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("xdp_vm_rx runs inside the Linux guest; vhost_user_net runs on the host.");
    std::process::exit(1);
}
