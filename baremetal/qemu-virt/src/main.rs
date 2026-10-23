//! QEMU virt board: shared runtime plus portable-stack UDP echo over virtio-net.
#![no_std]
#![no_main]
use core::{
    arch::{asm, global_asm},
    fmt::Write,
    panic::PanicInfo,
};

extern crate alloc;
use runtime::{
    arch::aarch64::timer,
    memory::{self, dma::DmaAllocator, heap::LockedHeap},
    pl011::Uart,
    selftest,
    virtio::{
        mmio::Mmio,
        net::{Config as NetConfig, VirtioNet},
        probe,
        rng::{Config as RngConfig, VirtioRng},
    },
};

#[global_allocator]
static HEAP: LockedHeap<512> = LockedHeap::new();
mod board;
global_asm!(include_str!("boot.S"));

fn halt() -> ! {
    loop {
        unsafe {
            asm!(
            "wfe",
            options(nomem, nostack)
            )
        };
    }
}

#[panic_handler]
fn panic(info: &PanicInfo<'_>) -> ! {
    // Single core, masked interrupts; the previous owner never resumes.
    let mut uart = unsafe {
        board::console()
    };
    let _ = writeln!(uart, "PANIC: {info}");
    if cfg!(feature = "heap-oom-test") {
        let _ = writeln!(uart, "OOM HALT");
    }

    halt()
}

#[no_mangle]
extern "C" fn exception_fatal(syndrome: u64, address: u64) -> ! {
    let mut uart = unsafe { board::console() };
    let _ = writeln!(uart, "EXCEPTION: ESR={syndrome:#x} ELR={address:#x}");

    halt()
}

// The host runner sends ordinary UDP datagrams, verifies each echo, then sends
// a round-specific terminator. No descriptor or wire-format fixture lives here.
fn udp_smoke(
    base: usize,
    dma: &DmaAllocator,
    uart: &mut Uart,
    round: u8,
    entropy: Option<&VirtioRng<Mmio>>,
) {
    use async_net_stack_core::{
        api::{Action, Service, UdpPool},
        net::{EthernetIpv4, InterfaceConfig},
        platform::net::{Ipv4Addr, SocketAddrV4},
    };
    
    use core::time::Duration;

    // Previous rounds confirm shared reset and drop all owners before re-opening.
    let registers = unsafe { Mmio::new(base) };
    let net = unsafe { VirtioNet::new(registers, dma.clone(), NetConfig::default()) }.unwrap();
    let config = net.net_config();
    assert_eq!(net.queue_sizes(), [Some(64), Some(64)]);
    writeln!(
        uart,
        "PASS MMIO: round={round} RX=64 TX=64 DRIVER_OK; RX posted={}",
        net.posted_rx()
    )
    .unwrap();
    writeln!(
        uart,
        "MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} link={:?}",
        config.mac[0],
        config.mac[1],
        config.mac[2],
        config.mac[3],
        config.mac[4],
        config.mac[5],
        config.link_up
    )
    .unwrap();
    let address = Ipv4Addr::new(10, 0, 2, 15);
    let mut interface =
        EthernetIpv4::new(net, InterfaceConfig::new(address, 24, config.mac))
            .unwrap();

    interface.set_gateway(Ipv4Addr::new(10, 0, 2, 2)).unwrap();
    let mut stack = UdpPool::new(interface, 64, 8).unwrap();
    stack
        .bind(Service {
            address: SocketAddrV4::new(address, 9000),
            id: 1,
        })
        .unwrap();
    writeln!(uart, "NET READY: round={round} UDP=10.0.2.15:9000")
        .unwrap();

    let start = timer::ticks();
    let hz = timer::frequency_hz();
    let mut done = false;
    let mut datagrams = 0;
    loop {
        let elapsed = timer::ticks().wrapping_sub(start);
        let now = Duration::new(
            elapsed / hz,
            ((elapsed % hz) as u128 * 1_000_000_000 / hz as u128) as u32,
        );

        if let Some(rng) = entropy {
            rng.poll().unwrap();
        }

        stack
            .poll(now, Duration::from_secs(60), 32, |datagram| {
                if datagram.payload == [b'A', b'N', b'S', b'-', b'D', b'O', b'N', b'E', round] {
                    done = true;
                }
                else {
                    datagrams += 1;
                }

                Action::Echo
            })
            .unwrap();
        if done
            && stack.pending() == 0
            && stack.device_mut().pending() == 0
            && stack.device_mut().device_mut().pending_tx() == 0
        {
            break;
        }
        assert!(
            elapsed < hz * 60,
            "UDP peer timeout: round={round} received={datagrams}, nic={:?}",
            stack.device_mut().device_mut().stats()
        );
    }

    assert!(datagrams >= 128, "insufficient sustained UDP traffic");
    let stats = stack.stats();
    assert_eq!(stats.invalid, 0);
    assert_eq!(stats.response_drops, 0);

    let interface = stack.device_mut();
    let ethernet = interface.stats();
    assert!(ethernet.submitted_control > 0, "ARP did not run");
    assert_eq!(ethernet.malformed, 0);

    let net = interface.device_mut();
    let stats = net.stats();
    assert_eq!(stats.tx_submitted, stats.tx_completed);
    assert_eq!(stats.rx_invalid, 0);
    assert!(stats.rx_posted > 32, "RX was not replenished");
    writeln!(uart, "PASS UDP: round={round}; datagrams={datagrams}; TX={} RX={} replenished={}; portable ARP/IPv4/UDP echo",
        stats.tx_completed, stats.rx_completed, stats.rx_posted).unwrap();

    net.shutdown().unwrap();
    assert!(net.stats().rx_cancelled > 0);
    // Stack-owned packets, pools and rings all drop before the caller checks RAM.
}

// Boot-only readiness waits have a deadline. The packet loop and TCP entropy
// callback never wait; the main loop explicitly services the shared RNG handle.
fn wait_for_entropy(rng: &VirtioRng<Mmio>, bytes: usize) {
    let start = timer::ticks();
    while rng.available() < bytes {
        rng.poll().unwrap();
        assert!(
            timer::ticks().wrapping_sub(start) < timer::frequency_hz() * 5,
            "virtio-rng readiness timeout"
        );
    }
}

fn entropy_start(dma: &DmaAllocator, uart: &mut Uart) -> Option<(usize, VirtioRng<Mmio>)> {
    use async_net_stack_core::{
        api::tcp::{TcpConfig, TcpPool, TcpState},
        device::LoopbackDevice,
        platform::{
            io,
            net::{Ipv4Addr, SocketAddrV4},
        },
    };

    // The portable default still refuses construction without an explicit source.
    assert!(
        matches!(TcpPool::new(LoopbackDevice::new(), TcpConfig::default()),
        Err(e) if e.kind() == io::ErrorKind::Unsupported.into())
    );

    let base = match unsafe { probe::find(board::VIRTIO_MMIO, probe::Kind::Entropy) } {
        Some(device) if device.version == 2 => device.base,
        Some(device) => {
            writeln!(
                uart,
                "UNSUPPORTED entropy MMIO version {}; TCP unavailable",
                device.version
            )
            .unwrap();
            return None;
        }
        None => {
            writeln!(
                uart,
                "NO virtio-rng device; TCP unavailable; UDP remains usable"
            )
            .unwrap();
            return None;
        }
    };

    let rng =
        unsafe {
            VirtioRng::new(Mmio::new(base), dma.clone(), RngConfig::default())
        }.unwrap();

    // Completed descriptors have not been polled yet, regardless of device speed.
    assert!(
        matches!(rng.tcp_sequence_source(),
                 Err(e) if e.kind() == io::ErrorKind::WouldBlock)
    );

    let mut untouched = [0xa5; 4];
    assert!(rng.try_fill(&mut untouched).is_err());
    assert_eq!(untouched, [0xa5; 4]);

    // Exercise repeated refills without probabilistic assertions about byte values.
    let mut bytes = [0; 128];
    for _ in 0..32 {
        wait_for_entropy(&rng, bytes.len());
        rng.try_fill(&mut bytes).unwrap();
    }
    assert_eq!(rng.stats().bytes_delivered, 4096);
    wait_for_entropy(&rng, 4);

    let before = rng.stats().bytes_delivered;
    {
        let mut tcp = TcpPool::with_sequence_source(
            LoopbackDevice::new(),
            TcpConfig::default(),
            rng.tcp_sequence_source().unwrap(),
        ).unwrap();

        let connection = tcp
            .connect(
                SocketAddrV4::new(Ipv4Addr::new(10, 0, 2, 15), 30000),
                SocketAddrV4::new(Ipv4Addr::new(10, 0, 2, 2), 9001),
            )
            .unwrap();
        assert_eq!(tcp.status(connection).unwrap().state, TcpState::SynSent);
        assert_eq!(rng.stats().bytes_delivered, before + 4);
    }

    writeln!(uart, "PASS entropy: modern MMIO at {base:#x}; 4096 bytes, bounded refill, explicit TCP sequence source").unwrap();
    writeln!(
        uart,
        "PASS entropy short reads: {}",
        rng.stats().short_reads
    )
    .unwrap();

    Some((base, rng))
}

#[no_mangle]
pub extern "C" fn kernel_main(_boot_arg: usize) -> ! {
    // Startup and the checked-in runner establish the fixed board contract.
    let mut uart = unsafe { Uart::new(board::UART0) };
    writeln!(uart, "net-boot: EL1, one CPU, polling, caches/MMU off").unwrap();

    let (heap_start, heap_bytes) = memory::heap_region();
    assert!(unsafe { HEAP.init(heap_start, heap_bytes) });
    selftest::heap(&HEAP, &mut uart);

    if cfg!(feature = "heap-oom-test") {
        // Exercise Rust's default no_std allocation-error path, which reaches
        // our non allocating panic handler. This must never return.
        let impossible = alloc::vec![0u8; heap_bytes + 1];
        core::hint::black_box(impossible);
        panic!("unexpected heap allocation success");
    }

    selftest::clock(&mut uart);
    // No DMA owner exists yet; the self-test returns every page.
    unsafe { selftest::pages::<{ board::PAGE_POOL_WORDS }>(&mut uart) };

    // Caches/MMU off: the pool is coherent and identity mapped for the device.
    let (cpu, bytes) = memory::page_pool_region();
    let dma = unsafe { DmaAllocator::new(cpu, cpu.as_ptr() as usize, bytes) }.unwrap();
    let original_free = dma.free_pages();
    selftest::dma(&dma, &mut uart);

    let entropy = entropy_start(&dma, &mut uart);
    let network_baseline = dma.free_pages();
    match unsafe { probe::find(board::VIRTIO_MMIO, probe::Kind::Network) } {
        Some(device) if device.version == 2 => {
            writeln!(uart, "PASS virtio-net: modern MMIO at {:#x}", device.base).unwrap();
            for round in 0..2 {
                udp_smoke(
                    device.base,
                    &dma,
                    &mut uart,
                    round,
                    entropy.as_ref().map(|(_, rng)| rng),
                );

                assert_eq!(dma.free_pages(), network_baseline);
                if let Some((_, rng)) = &entropy {
                    wait_for_entropy(rng, 4);
                    rng
                        .try_fill(&mut [0; 4])
                        .unwrap();
                    writeln!(uart, "PASS entropy isolation: NIC reset round={round}")
                        .unwrap();
                }
                writeln!(
                    uart,
                    "PASS MMIO reset: round={round}; rings and packet/header arenas reclaimed"
                ).unwrap();
            }
        }
        Some(device) => {
            writeln!(uart, "UNSUPPORTED virtio MMIO version {}", device.version).unwrap();
        }
        None => writeln!(uart, "NO virtio-net device").unwrap(),
    }

    if let Some((base, rng)) = entropy {
        rng.shutdown().unwrap();
        drop(rng);
        assert_eq!(dma.free_pages(), original_free);
        // Re-open the same device only after confirmed reset and all handles drop.
        let rng =
            unsafe {
                VirtioRng::new(Mmio::new(base), dma.clone(), RngConfig::default())
            }.unwrap();

        wait_for_entropy(&rng, 4);
        rng.try_fill(&mut [0; 4]).unwrap();
        rng.shutdown().unwrap();
        drop(rng);
        assert_eq!(dma.free_pages(), original_free);
        writeln!(
            uart,
            "PASS entropy reset: reinitialized; all DMA pages reclaimed"
        )
        .unwrap();
    }
    writeln!(uart, "BOOT DONE: portable UDP smoke complete when present").unwrap();

    halt()
}

