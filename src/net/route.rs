//! Small bounded IPv4 routing table. Gateways must be directly reachable on
//! the adapter's Ethernet link; recursive routes and policy routing are absent.
use std::{io, net::Ipv4Addr};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Route {
    network: Ipv4Addr,
    prefix_len: u8,
    gateway: Option<Ipv4Addr>,
}

impl Route {
    /// Host bits are normalized away. `None` means an on-link route.
    pub fn new(address: Ipv4Addr, prefix_len: u8, gateway: Option<Ipv4Addr>) -> io::Result<Self> {
        if prefix_len > 32 || gateway.is_some_and(|ip| !unicast(ip)) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid IPv4 route",
            ));
        }
        Ok(Self {
            network: Ipv4Addr::from(u32::from(address) & mask(prefix_len)),
            prefix_len,
            gateway,
        })
    }

    pub fn network(self) -> Ipv4Addr {
        self.network
    }
    pub fn prefix_len(self) -> u8 {
        self.prefix_len
    }
    pub fn gateway(self) -> Option<Ipv4Addr> {
        self.gateway
    }
    pub fn contains(self, ip: Ipv4Addr) -> bool {
        u32::from(ip) & mask(self.prefix_len) == u32::from(self.network)
    }
    pub fn next_hop(self, destination: Ipv4Addr) -> Ipv4Addr {
        self.gateway.unwrap_or(destination)
    }
}

#[derive(Debug)]
pub struct RouteTable {
    entries: Vec<Route>,
    limit: usize,
}

impl RouteTable {
    pub fn new(limit: usize) -> Self {
        Self {
            entries: Vec::with_capacity(limit),
            limit,
        }
    }

    /// Replace an identical prefix or insert a route. Equal-prefix multipath
    /// routing is intentionally unsupported.
    pub fn insert(&mut self, route: Route) -> io::Result<()> {
        if let Some(existing) = self
            .entries
            .iter_mut()
            .find(|r| r.network == route.network && r.prefix_len == route.prefix_len)
        {
            *existing = route;
            return Ok(());
        }
        if self.entries.len() == self.limit {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "route table full",
            ));
        }
        self.entries.push(route);
        Ok(())
    }

    pub fn lookup(&self, destination: Ipv4Addr) -> Option<Route> {
        self.entries
            .iter()
            .filter(|r| r.contains(destination))
            .max_by_key(|r| r.prefix_len)
            .copied()
    }

    pub fn entries(&self) -> impl Iterator<Item = &Route> {
        self.entries.iter()
    }
}

pub(crate) fn mask(prefix: u8) -> u32 {
    if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix)
    }
}

pub(crate) fn unicast(ip: Ipv4Addr) -> bool {
    !ip.is_unspecified() && !ip.is_broadcast() && !ip.is_multicast() && !ip.is_loopback()
}
