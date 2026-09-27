//! Neighbor state for one Ethernet interface. The owning interface bounds the
//! number of entries and queued packets; static entries never learn from ARP.
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NeighborState {
    Static {
        mac: [u8; 6],
    },
    Reachable {
        mac: [u8; 6],
        expires: Duration,
    },
    Resolving {
        attempts: u32,
        next_probe: Duration,
        expires: Duration,
    },
    Failed {
        retry_after: Duration,
    },
}

impl NeighborState {
    
    pub(crate) fn mac(self, now: Duration) -> Option<[u8; 6]> {
        match self {
            Self::Static { mac } => Some(mac),
            Self::Reachable { mac, expires } if now < expires => Some(mac),
            _ => None,
        }
    }
}
