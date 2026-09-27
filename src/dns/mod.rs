//! Replaceable, poll-driven hostname resolution. The initial backend is a classic
//! UDP stub resolver, not an authoritative/recursive DNS server or OS resolver.
mod wire;
use crate::{
    api::{Action, DatagramId, SendOptions, Service, UdpConfig, UdpFailure, UdpOutcome, UdpPool},
    device::Device,
};

use std::{
    collections::{BTreeMap, VecDeque},
    fs::File,
    io::{self, Read},
    net::{Ipv4Addr, SocketAddrV4},
    time::Duration,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct QueryId(pub u64);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResolveError {
    InvalidName,
    Busy,
    Timeout,
    NotFound,
    NoData,
    ServerFailure,
    Refused,
    ProtocolError,
    TcpRequired,
    AliasLimit,
    Cancelled,
    NetworkChanged,
    Local(io::ErrorKind),
}

#[derive(Clone, Debug)]
pub struct Answer {
    pub canonical_name: String,
    pub addresses: Vec<Ipv4Addr>,
    /// Caller-owned monotonic time, including the shortest CNAME TTL.
    pub expires_at: Duration,
}

#[derive(Clone, Debug)]
pub struct Completion {
    pub id: QueryId,
    pub name: String,
    pub generation: u64,
    pub from_cache: bool,
    pub result: Result<Answer, ResolveError>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NetworkConfig {
    pub local_address: Ipv4Addr,
    /// Explicit recursive resolver endpoints, normally port 53. No implicit use
    /// of host resolv.conf, search domains, or management-network fallback.
    pub servers: Vec<SocketAddrV4>,
}

impl NetworkConfig {
    fn validate(&self) -> io::Result<()> {
        fn unicast(ip: Ipv4Addr) -> bool {
            !ip.is_unspecified() && !ip.is_multicast() && !ip.is_broadcast()
        }

        if !unicast(self.local_address)
            || self.servers.is_empty()
            || self.servers.len() > 8
            || self
                .servers
                .iter()
                .any(|s| s.port() == 0 || !unicast(*s.ip()))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid DNS network configuration",
            ));
        }

        Ok(())
    }
}

#[derive(Clone, Copy, Debug)]
pub struct ResolverConfig {
    /// Pending lookups plus undrained completions. Completions are never overwritten.
    pub capacity: usize,
    pub cache_capacity: usize,
    pub max_cache_ttl: Duration,
    pub lookup_timeout: Duration,
    pub attempt_timeout: Duration,
    pub attempts_per_name: u32,
    pub receive_budget: usize,
}

impl Default for ResolverConfig {
    fn default() -> Self {
        Self {
            capacity: 64,
            cache_capacity: 128,
            max_cache_ttl: Duration::from_secs(3600),
            lookup_timeout: Duration::from_secs(10),
            attempt_timeout: Duration::from_secs(1),
            attempts_per_name: 3,
            receive_budget: 64,
        }
    }
}

/// Application-facing substitution boundary. A future native, host-backed or
/// TCP-capable resolver can implement this without changing lookup consumers.
/// resolve() timestamps requests at the last poll time; poll once before enqueueing.
pub trait Resolver {
    fn resolve(&mut self, name: &str) -> Result<QueryId, ResolveError>;
    fn poll(&mut self, now: Duration) -> io::Result<()>;
    fn pop_result(&mut self) -> Option<Completion>;
    fn cancel(&mut self, id: QueryId) -> bool;
    fn next_deadline(&self) -> Option<Duration>;

    /// Replaces resolver settings, not device routes/addresses. Identical settings
    /// are a no-op. Call invalidate() for a changed network with identical settings.
    fn configure(&mut self, network: NetworkConfig) -> io::Result<()>;
    fn invalidate(&mut self);
    fn generation(&self) -> u64;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ResolverStats {
    pub queries_submitted: u64,
    pub cache_hits: u64,
    pub ignored_responses: u64,
    pub malformed_responses: u64,
    pub completed: u64,
}

struct Pending {
    original: String,
    question: String,
    aliases: Vec<String>,
    alias_expires: Option<Duration>,
    deadline: Duration,
    next: Duration,
    attempts: u32,
    local: Option<SocketAddrV4>,
    server: SocketAddrV4,
    wire_id: u16,
    datagram: Option<DatagramId>,
}

struct Cached {
    expires: Duration,
    result: Result<Answer, ResolveError>,
}

struct Incoming {
    source: SocketAddrV4,
    destination: SocketAddrV4,
    payload: Vec<u8>,
}

/// Owns the device receive stream. Do not independently poll another UdpPool or
/// TcpPool over that device. Future shared transport dispatch can live behind Resolver.
pub struct UdpResolver<D> {
    pool: UdpPool<D>,
    config: ResolverConfig,
    network: NetworkConfig,
    entropy: File,
    pending: BTreeMap<QueryId, Pending>,
    results: VecDeque<Completion>,
    cache: BTreeMap<String, Cached>,
    incoming: Vec<Incoming>,
    now: Duration,
    generation: u64,
    next_id: u64,
    stats: ResolverStats,
}

impl<D: Device> UdpResolver<D> {
    pub fn new(device: D, network: NetworkConfig, config: ResolverConfig) -> io::Result<Self> {
        network.validate()?;
        if config.capacity == 0
            || config.capacity > 4096
            || config.cache_capacity > 4096
            || config.lookup_timeout.is_zero()
            || config.attempt_timeout.is_zero()
            || config.attempts_per_name == 0
            || config.attempts_per_name > 16
            || config.receive_budget == 0
            || config.receive_budget > 256
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid DNS limits",
            ));
        }

        let entropy = File::open("/dev/urandom")?;
        let pool = UdpPool::with_config(
            device,
            UdpConfig {
                queue_capacity: config.capacity,
                service_capacity: config.capacity,
                max_tx_peers: 8,
                per_peer_capacity: config.capacity,
                tx_budget: config.capacity,
                queue_lifetime: Some(config.attempt_timeout),
                event_capacity: config.capacity * 4,
                fair_queue: true,
                ..UdpConfig::default()
            },
        )?;

        Ok(Self {
            pool,
            config,
            network,
            entropy,
            pending: BTreeMap::new(),
            results: VecDeque::with_capacity(config.capacity),
            cache: BTreeMap::new(),
            incoming: Vec::with_capacity(config.receive_budget),
            now: Duration::ZERO,
            generation: 0,
            next_id: 1,
            stats: ResolverStats::default(),
        })
    }

    pub fn device_mut(&mut self) -> &mut D {
        self.pool.device_mut()
    }

    pub fn stats(&self) -> ResolverStats {
        self.stats
    }

    pub fn network(&self) -> &NetworkConfig {
        &self.network
    }

    fn close(&mut self, q: &mut Pending) {
        if let Some(local) = q.local.take() {
            self.pool.unbind(local);
        }
        q.datagram = None;
    }

    fn finish(&mut self, id: QueryId, result: Result<Answer, ResolveError>, ttl: Option<Duration>) {
        let Some(mut q) = self.pending.remove(&id) else {
            return;
        };
        self.close(&mut q);

        if let Some(ttl) = ttl {
            let expires = self.now.saturating_add(ttl.min(self.config.max_cache_ttl));

            if expires > self.now && self.config.cache_capacity > 0 {
                if self.cache.len() >= self.config.cache_capacity {
                    if let Some(old) = self
                        .cache
                        .iter()
                        .min_by_key(|(_, c)| c.expires)
                        .map(|(n, _)| n.clone())
                    {
                        self.cache.remove(&old);
                    }
                }

                self.cache.insert(
                    q.original.clone(),
                    Cached {
                        expires,
                        result: result.clone(),
                    },
                );
            }
        }

        self.stats.completed += 1;
        self.results.push_back(Completion {
            id,
            name: q.original,
            generation: self.generation,
            from_cache: false,
            result,
        });
    }

    fn attempt(&mut self, id: QueryId) -> io::Result<()> {
        let mut q = self.pending.remove(&id).unwrap();
        self.close(&mut q);
        let result = (|| {
            for _ in 0..32 {
                let mut random = [0; 4];
                self.entropy.read_exact(&mut random)?;
                let port = u16::from_be_bytes([random[2], random[3]]);
                if port < 1024 {
                    continue;
                }

                let local = SocketAddrV4::new(self.network.local_address, port);
                match self.pool.bind(Service {
                    address: local,
                    id: 0,
                }) {
                    Err(e) if e.kind() == io::ErrorKind::AddrInUse => continue,
                    Err(e) => return Err(e),
                    Ok(()) => {}
                }

                q.local = Some(local);
                q.wire_id = u16::from_be_bytes([random[0], random[1]]);
                q.server = self.network.servers[q.attempts as usize % self.network.servers.len()];
                let next = self
                    .now
                    .saturating_add(
                        self.config
                            .attempt_timeout
                            .saturating_mul(1u32 << q.attempts),
                    )
                    .min(q.deadline);

                q.datagram = Some(self.pool.send_with_options(
                    local,
                    q.server,
                    &wire::query(q.wire_id, &q.question),
                    SendOptions {
                        deadline: Some(next),
                    },
                )?);

                q.attempts += 1;
                q.next = next;
                self.stats.queries_submitted += 1;
                return Ok(());
            }

            Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "DNS source-port allocation exhausted",
            ))
        })();

        if result.is_err() {
            self.close(&mut q);
        }

        self.pending.insert(id, q);
        result
    }

    fn receive(&mut self, input: Incoming) {
        if input.payload.len() < 2 {
            self.stats.malformed_responses += 1;
            return;
        }

        let wire_id = u16::from_be_bytes([input.payload[0], input.payload[1]]);
        let id = self
            .pending
            .iter()
            .find(|(_, q)| {
                q.local == Some(input.destination)
                    && q.server == input.source
                    && q.wire_id == wire_id
                    && q.datagram.is_some()
            })
            .map(|(&id, _)| id);

        let Some(id) = id else {
            self.stats.ignored_responses += 1;
            return;
        };

        let q = &self.pending[&id];
        if self.now >= q.deadline || self.now >= q.next {
            self.stats.ignored_responses += 1;
            return;
        }

        let parsed = wire::response(
            &input.payload,
            wire_id,
            &q.question,
            8usize.saturating_sub(q.aliases.len() - 1),
        );

        let chain_expiry = q.alias_expires;
        let expiry = |ttl: u32| {
            let expires = self
                .now
                .saturating_add(Duration::from_secs(ttl as u64).min(self.config.max_cache_ttl));
            chain_expiry.map_or(expires, |old| old.min(expires))
        };

        match parsed {
            Err(()) => self.stats.malformed_responses += 1,
            Ok(wire::Answer::Addresses {
                canonical,
                addresses,
                ttl,
            }) => {
                let expires = expiry(ttl);
                let ttl = expires.saturating_sub(self.now);
                self.finish(
                    id,
                    Ok(Answer {
                        canonical_name: canonical,
                        addresses,
                        expires_at: expires,
                    }),
                    Some(ttl),
                );
            }

            Ok(wire::Answer::Alias { target, ttl, hops }) => {
                let expires = expiry(ttl);
                let q = self.pending.get_mut(&id).unwrap();

                // Limit total alias chasing, including aliases within earlier responses.
                if q.aliases.contains(&target) || q.aliases.len() - 1 + hops > 8 {
                    self.finish(id, Err(ResolveError::AliasLimit), None);
                    return;
                }

                for _ in 0..hops.max(1) {
                    q.aliases.push(target.clone());
                }

                q.question = target;
                q.alias_expires = Some(expires);
                q.attempts = 0;
                q.next = self.now;
                // Make old responses ineligible until the next attempt.
                q.datagram = None;
            }

            Ok(wire::Answer::Negative { error, ttl }) => {
                let lifetime = ttl.map(|ttl| expiry(ttl).saturating_sub(self.now));
                self.finish(id, Err(error), lifetime);
            }

            Ok(wire::Answer::Failure(error)) => {
                if matches!(error, ResolveError::ServerFailure | ResolveError::Refused)
                    && self.pending[&id].attempts < self.config.attempts_per_name
                {
                    let q = self.pending.get_mut(&id).unwrap();
                    q.next = self.now;
                    q.datagram = None;
                } else {
                    self.finish(id, Err(error), None);
                }
            }
        }
    }
}

impl<D: Device> Resolver for UdpResolver<D> {
    fn resolve(&mut self, input: &str) -> Result<QueryId, ResolveError> {
        let name = wire::normalize(input)?;
        if self.pending.len() + self.results.len() >= self.config.capacity {
            return Err(ResolveError::Busy);
        }

        let next = self.next_id.checked_add(1).ok_or(ResolveError::Busy)?;
        let id = QueryId(self.next_id);
        self.next_id = next;
        if let Some(cached) = self.cache.get(&name).filter(|c| c.expires > self.now) {
            self.results.push_back(Completion {
                id,
                name,
                generation: self.generation,
                from_cache: true,
                result: cached.result.clone(),
            });

            self.stats.cache_hits += 1;
            self.stats.completed += 1;
        } else {
            self.pending.insert(
                id,
                Pending {
                    original: name.clone(),
                    question: name.clone(),
                    aliases: vec![name],
                    alias_expires: None,
                    deadline: self.now.saturating_add(self.config.lookup_timeout),
                    next: self.now,
                    attempts: 0,
                    local: None,
                    server: self.network.servers[0],
                    wire_id: 0,
                    datagram: None,
                },
            );
        }
        Ok(id)
    }

    fn poll(&mut self, now: Duration) -> io::Result<()> {
        if now < self.now {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "DNS time went backwards",
            ));
        }

        self.now = now;
        self.cache.retain(|_, c| c.expires > now);
        let mut incoming = std::mem::take(&mut self.incoming);
        let result = self
            .pool
            .poll(now, Duration::ZERO, self.config.receive_budget, |d| {
                if d.payload.len() <= 512 {
                    incoming.push(Incoming {
                        source: d.source,
                        destination: d.destination,
                        payload: d.payload.to_vec(),
                    });
                }
                Action::Ignore
            });

        if let Err(error) = result {
            incoming.clear();
            self.incoming = incoming;
            return Err(error);
        }

        for input in incoming.drain(..) {
            self.receive(input);
        }

        self.incoming = incoming;
        while let Some(event) = self.pool.pop_event() {
            if let UdpOutcome::Dropped(reason) = event.outcome {
                let id = self
                    .pending
                    .iter()
                    .find(|(_, q)| q.datagram.is_some() && q.datagram == event.id)
                    .map(|(&id, _)| id);

                if let Some(id) = id {
                    if let UdpFailure::Device(kind) = reason {
                        self.finish(id, Err(ResolveError::Local(kind)), None);
                    } else {
                        self.pending.get_mut(&id).unwrap().next = now;
                    }
                }
            }
        }

        let ids: Vec<_> = self.pending.keys().copied().collect();
        for id in ids {
            let q = &self.pending[&id];
            if now >= q.deadline || (now >= q.next && q.attempts >= self.config.attempts_per_name) {
                self.finish(id, Err(ResolveError::Timeout), None);
            } else if now >= q.next {
                match self.attempt(id) {
                    Ok(()) => {}
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        self.pending.get_mut(&id).unwrap().next =
                            now.saturating_add(Duration::from_millis(10))
                    }
                    Err(e) => self.finish(id, Err(ResolveError::Local(e.kind())), None),
                }
            }
        }

        self.pool.flush()?;
        Ok(())
    }

    fn pop_result(&mut self) -> Option<Completion> {
        self.results.pop_front()
    }

    fn cancel(&mut self, id: QueryId) -> bool {
        if !self.pending.contains_key(&id) {
            return false;
        }
        self.finish(id, Err(ResolveError::Cancelled), None);
        true
    }

    fn next_deadline(&self) -> Option<Duration> {
        self.pending
            .values()
            .map(|q| q.next.min(q.deadline))
            .chain(self.pool.next_deadline())
            .min()
    }

    fn configure(&mut self, network: NetworkConfig) -> io::Result<()> {
        network.validate()?;
        if self.network != network {
            self.invalidate();
            self.network = network;
        }
        Ok(())
    }

    fn generation(&self) -> u64 {
        self.generation
    }

    fn invalidate(&mut self) {
        let ids: Vec<_> = self.pending.keys().copied().collect();
        for id in ids {
            self.finish(id, Err(ResolveError::NetworkChanged), None);
        }
        // Already completed results retain their old generation so consumers can
        // identify snapshots produced before reconfiguration.
        self.cache.clear();
        self.generation = self.generation.wrapping_add(1);
    }
}
