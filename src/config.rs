//! Static device configuration, loaded from a TOML file at startup.
//! Things the owner changes from the dashboard (rates, portal name) live in
//! the encrypted database instead.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub network: Network,
    pub portal: Portal,
    pub admin: Admin,
    pub coin: Coin,
    pub storage: Storage,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Network {
    /// Customer-facing interface (to the access point).
    pub lan_iface: String,
    /// Internet uplink (to the ISP modem).
    pub wan_iface: String,
    /// This box's address on the LAN, e.g. 10.0.0.1.
    pub portal_ip: Ipv4Addr,
    /// LAN subnet, e.g. "10.0.0.0/19".
    pub lan_cidr: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Portal {
    pub listen: SocketAddr,
    #[serde(default = "default_max_conns_portal")]
    pub max_connections: usize,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Admin {
    /// HTTPS listener for the owner dashboard.
    pub listen: SocketAddr,
    /// If non-empty, only these device MACs may open the dashboard.
    #[serde(default)]
    pub allow_macs: Vec<String>,
    #[serde(default = "default_session_hours")]
    pub session_hours: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Coin {
    /// GPIO chip device, e.g. "/dev/gpiochip0".
    pub chip: String,
    /// Line offset of the coin signal (find it with `pisowifi coin-probe`).
    pub line: u32,
    /// The coin acceptor pulls the line low on each pulse (open collector).
    #[serde(default = "default_true")]
    pub active_low: bool,
    #[serde(default = "default_debounce")]
    pub debounce_ms: u64,
    /// Silence that ends one coin's pulse burst. ALLAN pulses are 20–100 ms
    /// apart; coins physically can't arrive faster than ~300 ms apart.
    #[serde(default = "default_coin_gap")]
    pub coin_gap_ms: u64,
    /// How many pesos one pulse is worth (ALLAN default: 1 pulse = ₱1).
    #[serde(default = "default_one")]
    pub pesos_per_pulse: u32,
    /// Seconds a customer has to drop coins after tapping "Insert coin".
    #[serde(default = "default_window")]
    pub insert_window_secs: u64,
    /// Optional output line that enables the acceptor only while someone
    /// is inserting (wired to the acceptor's inhibit input through a relay
    /// or transistor). Coins dropped while disabled are rejected.
    #[serde(default)]
    pub enable_line: Option<u32>,
    #[serde(default)]
    pub enable_active_low: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Storage {
    /// Directory for the encrypted database, salt and admin TLS certificate.
    pub data_dir: PathBuf,
}

fn default_true() -> bool {
    true
}
fn default_one() -> u32 {
    1
}
fn default_debounce() -> u64 {
    5
}
fn default_coin_gap() -> u64 {
    250
}
fn default_window() -> u64 {
    60
}
fn default_session_hours() -> u64 {
    8
}
fn default_max_conns_portal() -> usize {
    512
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        let cfg: Config = toml::from_str(&raw).context("parsing config")?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<()> {
        for iface in [&self.network.lan_iface, &self.network.wan_iface] {
            // Interface names end up in nft rules; keep them boring.
            if iface.is_empty()
                || iface.len() > 15
                || !iface.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
            {
                bail!("invalid interface name {iface:?}");
            }
        }
        let (net, prefix) = self.lan_net()?;
        if !in_subnet(self.network.portal_ip, net, prefix) {
            bail!("portal_ip is not inside lan_cidr");
        }
        if self.coin.pesos_per_pulse == 0 || self.coin.insert_window_secs < 10 {
            bail!("coin.pesos_per_pulse must be >= 1 and insert_window_secs >= 10");
        }
        if !(80..=2000).contains(&self.coin.coin_gap_ms) {
            bail!("coin.coin_gap_ms must be between 80 and 2000");
        }
        // Longer than a real pulse (20 ms and up) and no pulse is ever counted.
        if self.coin.debounce_ms > 15 {
            bail!("coin.debounce_ms must be 15 or less");
        }
        for m in &self.admin.allow_macs {
            crate::net::Mac::parse(m).with_context(|| format!("admin.allow_macs: {m}"))?;
        }
        Ok(())
    }

    pub fn lan_net(&self) -> Result<(Ipv4Addr, u8)> {
        let (ip, prefix) = self
            .network
            .lan_cidr
            .split_once('/')
            .context("lan_cidr must look like 10.0.0.0/19")?;
        let ip: Ipv4Addr = ip.parse().context("lan_cidr address")?;
        let prefix: u8 = prefix.parse().context("lan_cidr prefix")?;
        if !(8..=30).contains(&prefix) {
            bail!("lan_cidr prefix must be between /8 and /30");
        }
        Ok((ip, prefix))
    }

    /// Settings for running on a laptop: no GPIO, no firewall, localhost only.
    pub fn dev(data_dir: PathBuf) -> Self {
        Config {
            network: Network {
                lan_iface: "lo0".into(),
                wan_iface: "en0".into(),
                portal_ip: Ipv4Addr::new(127, 0, 0, 1),
                lan_cidr: "127.0.0.0/8".into(),
            },
            portal: Portal {
                listen: "127.0.0.1:8080".parse().unwrap(),
                max_connections: 64,
            },
            admin: Admin {
                listen: "127.0.0.1:8443".parse().unwrap(),
                allow_macs: vec![],
                session_hours: 8,
            },
            coin: Coin {
                chip: String::new(),
                line: 0,
                active_low: true,
                debounce_ms: 5,
                coin_gap_ms: 250,
                pesos_per_pulse: 1,
                insert_window_secs: 60,
                enable_line: None,
                enable_active_low: false,
            },
            storage: Storage { data_dir },
        }
    }
}

pub fn in_subnet(ip: Ipv4Addr, net: Ipv4Addr, prefix: u8) -> bool {
    let mask = if prefix == 0 { 0 } else { u32::MAX << (32 - prefix) };
    (u32::from(ip) & mask) == (u32::from(net) & mask)
}
