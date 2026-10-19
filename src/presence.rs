//! Auto-pause: notices phones that left the WiFi, and phones that came back.
//!
//! Every 20 s the vendo sends each watched phone an empty UDP packet. That
//! makes the kernel check the phone's neighbour (ARP) entry: a phone still
//! on the WiFi answers the kernel's ARP probe, even asleep in a pocket, and
//! one that left doesn't, so its entry turns incomplete a few seconds later.
//! Nothing is read from the phone, and customers notice nothing.
//!
//! A phone must miss every check for the owner's whole auto-pause time
//! (minutes) before its time pauses, so one slow answer never pauses anyone.

use crate::engine::Engine;
use crate::net::{self, Mac};
use std::collections::{HashMap, HashSet};
use std::net::{Ipv4Addr, UdpSocket};
use std::sync::Arc;
use std::time::Duration;

/// How often watched phones are checked.
const EVERY: Duration = Duration::from_secs(20);
/// The kernel checks a stale entry in about 8 s: a 5 s delay, then three
/// probes a second apart.
const SETTLE: Duration = Duration::from_secs(10);

pub fn spawn(engine: Arc<Engine>, iface: String, dev: bool) {
    tokio::spawn(async move {
        let sock = match UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).and_then(|s| s.set_nonblocking(true).map(|()| s)) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("auto-pause can't check phones: {e}");
                return;
            }
        };
        let mut tick = tokio::time::interval(if dev { Duration::from_secs(5) } else { EVERY });
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let watched: HashMap<Mac, Ipv4Addr> = engine.watched().into_iter().collect();
            if watched.is_empty() {
                continue;
            }
            if dev {
                // A laptop has no customers' neighbour entries: the only sign
                // of life is a phone's page asking for its time (Engine::me).
                engine.presence(&HashMap::new());
                continue;
            }
            // Its own address, and any other one the kernel has for it (DHCP may have moved it).
            let mut ips: HashSet<Ipv4Addr> = watched.values().copied().collect();
            ips.extend(net::neighbours(&iface).into_iter().filter(|(m, _)| watched.contains_key(m)).map(|(_, ip)| ip));
            for ip in ips {
                net::nudge(&sock, ip);
            }
            tokio::time::sleep(SETTLE).await;
            engine.presence(&here(&watched, net::neighbours(&iface)));
        }
    });
}

/// Watched phones with a complete neighbour entry; at their own address if
/// the kernel has several for one.
fn here(watched: &HashMap<Mac, Ipv4Addr>, table: Vec<(Mac, Ipv4Addr)>) -> HashMap<Mac, Ipv4Addr> {
    let mut out = HashMap::new();
    for (mac, ip) in table {
        let Some(own) = watched.get(&mac) else { continue };
        if ip == *own || !out.contains_key(&mac) {
            out.insert(mac, ip);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_watched_phones_count_and_their_own_address_wins() {
        let (a, b, other) = (Mac([2, 0, 0, 0, 0, 1]), Mac([2, 0, 0, 0, 0, 2]), Mac([2, 0, 0, 0, 0, 3]));
        let ip = |n| Ipv4Addr::new(10, 0, 0, n);
        let watched = HashMap::from([(a, ip(5)), (b, ip(6))]);
        let here = here(&watched, vec![(a, ip(9)), (a, ip(5)), (b, ip(7)), (other, ip(8))]);
        assert_eq!(here.get(&a), Some(&ip(5)));
        assert_eq!(here.get(&b), Some(&ip(7)), "moved to a new address");
        assert!(!here.contains_key(&other));
    }
}
