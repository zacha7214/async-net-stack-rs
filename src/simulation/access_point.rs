//! Application-visible AP lifecycle, without 802.11 radio or management frames.
use super::{FabricStats, Link, Network, SimDevice};
use std::{io, net::Ipv4Addr, rc::Rc, time::Duration};

/// An isolated IPv4 broadcast domain owned by an application.
///
/// Starts stopped. `start` permits explicit association; `stop` (also Drop)
/// disconnects every endpoint and purges all fabric queues. Restarting never
/// silently reassociates endpoints. Each AP has its own virtual clock and routes.
/// This models reachability, not SSIDs, authentication, DHCP, RF or firmware.
#[derive(Default)]
pub struct AccessPoint {
    network: Network,
    running: bool,
}

impl AccessPoint {
    /// Allocate an initially disconnected endpoint, including firmware services.
    /// Addresses remain reserved while the device exists, even across restarts.
    pub fn port(
        &self,
        addresses: &[Ipv4Addr],
        frames: usize,
        queue: usize,
    ) -> io::Result<SimDevice> {
        let device = self.network.port(addresses, frames, queue)?;
        self.network.0.borrow_mut().ports[device.id].online = false;
        Ok(device)
    }

    /// Idempotent; does not alter existing associations when already running.
    pub fn start(&mut self) {
        self.running = true;
    }

    pub fn is_running(&self) -> bool {
        self.running
    }

    /// Idempotent. Application-owned RX/TX buffers and peer caches are untouched.
    pub fn stop(&mut self) {
        self.running = false;
        let mut state = self.network.0.borrow_mut();
        let mut dropped = 0;
        for port in &mut state.ports {
            port.online = false;
            dropped += port.queue.len() as u64;
            port.queue.clear();
        }
        state.stats.disconnected_drops += dropped;
    }

    /// Explicit successful association, with no simulated handshake or delay.
    /// Advance the clock before this call to model a reconnect delay.
    pub fn associate(&self, device: &SimDevice) -> io::Result<()> {
        self.check(device)?;
        if !self.running {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "access point is stopped",
            ));
        }
        self.network.0.borrow_mut().ports[device.id].online = true;
        Ok(())
    }

    /// Purge queued frames both to and from this endpoint, including broadcasts
    /// already copied into another endpoint's ingress queue.
    pub fn disassociate(&self, device: &SimDevice) -> io::Result<()> {
        self.check(device)?;
        self.network.0.borrow_mut().disconnect(device.id);
        Ok(())
    }

    pub fn set_link(&self, from: &SimDevice, to: &SimDevice, link: Link) -> io::Result<()> {
        self.network.set_link(from, to, link)
    }

    pub fn advance(&self, elapsed: Duration) -> io::Result<()> {
        self.network.advance(elapsed)
    }

    pub fn now(&self) -> Duration {
        self.network.now()
    }

    pub fn stats(&self) -> FabricStats {
        self.network.stats()
    }

    fn check(&self, device: &SimDevice) -> io::Result<()> {
        if !Rc::ptr_eq(&self.network.0, &device.network.0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "endpoint belongs to another access point",
            ));
        }
        Ok(())
    }
}

impl Drop for AccessPoint {
    fn drop(&mut self) {
        self.stop();
    }
}
