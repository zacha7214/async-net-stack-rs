//! Run without privileges: cargo run --locked --example ap_lifecycle
use async_net_stack_rs::{
    api::{Action, Service, UdpPool},
    simulation::{AccessPoint, Link, SimDevice},
};
use std::{io, net::SocketAddrV4, time::Duration};

const LEASE: Duration = Duration::from_secs(1);

fn discover(
    ap: &AccessPoint,
    client: &mut UdpPool<SimDevice>,
    firmware: &mut UdpPool<SimDevice>,
    source: SocketAddrV4,
) -> io::Result<()> {
    client.discover(source, "255.255.255.255:9000".parse().unwrap(), 7)?;
    client.flush()?;

    firmware.poll(ap.now(), LEASE, 8, |_| Action::Echo)?;
    firmware.flush()?;
    client.poll(ap.now(), LEASE, 8, |_| Action::Ignore)?;

    println!(
        "t={:?}: discovered {} firmware service(s)",
        ap.now(),
        client.peers().count()
    );

    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut ap = AccessPoint::default();
    let client_addr: SocketAddrV4 = "10.77.0.1:8000".parse()?;
    let firmware_addr: SocketAddrV4 = "10.77.0.2:9000".parse()?;
    let mut client = UdpPool::new(ap.port(&[*client_addr.ip()], 16, 8)?, 8, 8)?;
    let mut firmware = UdpPool::new(ap.port(&[*firmware_addr.ip()], 16, 8)?, 8, 8)?;

    client.bind(Service {
        address: client_addr,
        id: 0,
    })?;
    firmware.bind(Service {
        address: firmware_addr,
        id: 7,
    })?;

    ap.start();
    ap.associate(client.device_mut())?;
    ap.associate(firmware.device_mut())?;
    discover(&ap, &mut client, &mut firmware, client_addr)?;

    // Firmware disappears while a response is in flight.
    ap.set_link(
        firmware.device_mut(),
        client.device_mut(),
        Link {
            delay: Duration::from_millis(500),
            ..Link::default()
        },
    )?;

    client.send_to(client_addr, firmware_addr, b"before restart")?;
    client.flush()?;
    firmware.poll(ap.now(), LEASE, 8, |_| Action::Echo)?;
    firmware.flush()?;

    println!("queued reply before stop: {}", client.device_mut().queued());
    ap.stop();
    println!(
        "after stop: online={}, queued={}, cached peers={}",
        client.device_mut().is_online(),
        client.device_mut().queued(),
        client.peers().count()
    );

    ap.advance(LEASE)?;
    // No pending application TX here, so poll can expire leases while offline.
    client.poll(ap.now(), LEASE, 8, |_| Action::Ignore)?;
    println!(
        "after lease expiry: cached peers={}",
        client.peers().count()
    );

    ap.start();
    println!(
        "after start: client online={}",
        client.device_mut().is_online()
    );

    ap.associate(client.device_mut())?;
    ap.associate(firmware.device_mut())?;
    ap.set_link(firmware.device_mut(), client.device_mut(), Link::default())?;
    discover(&ap, &mut client, &mut firmware, client_addr)?;
    client.send_to(client_addr, firmware_addr, b"after restart")?;
    client.flush()?;
    firmware.poll(ap.now(), LEASE, 8, |_| Action::Echo)?;
    firmware.flush()?;

    client.poll(ap.now(), LEASE, 8, |d| {
        println!("echo: {}", String::from_utf8_lossy(d.payload));
        Action::Ignore
    })?;
    println!("fabric: {:?}", ap.stats());

    Ok(())
}
