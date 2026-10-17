//! nftables control. One table, `inet pisowifi`, owns everything:
//!
//! * forward policy is DROP: only devices in the `allowed` set (MAC *and* IP
//!   must both match) reach the internet;
//! * unpaid HTTP is redirected to the portal, and every DNS query is forced
//!   to the local resolver;
//! * customers can never reach the ISP modem, other private ranges, SSH, or
//!   anything on this box besides DHCP, DNS, the portal and the dashboard;
//! * IPv6 from customers is dropped so it can't be used to bypass the portal;
//! * unpaid phones get a small DNS allowance (enough to find the portal, too
//!   little for DNS-tunnel "free internet" apps).
//!
//! It fails closed: if the daemon dies, the table stays and nobody new gets
//! online; existing grants expire by themselves via kernel set timeouts.
//! On start the table is replaced together with the saved grants, in one
//! transaction, so a restart or an update never cuts off paying customers.
//!
//! Every value written into nft scripts is a parsed MAC/IPv4/integer or a
//! validated interface name, so no injection is possible.

use crate::config::Config;
use crate::net::Mac;
use anyhow::{bail, Context, Result};
use std::net::Ipv4Addr;
use std::process::Stdio;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::mpsc;

/// Kernel-side grace period on top of the daemon's own expiry.
const KERNEL_GRACE_SECS: u64 = 30;

enum Cmd {
    Allow { mac: Mac, ip: Ipv4Addr, secs: u64 },
    Revoke { mac: Mac, ip: Ipv4Addr },
}

#[derive(Clone)]
pub struct Firewall {
    tx: Option<mpsc::UnboundedSender<Cmd>>,
}

impl Firewall {
    /// `grants`: customers with time running (MAC, IP, seconds left).
    pub async fn start(cfg: &Config, dev: bool, grants: &[(Mac, Ipv4Addr, u64)]) -> Result<Firewall> {
        if dev {
            tracing::info!("dev mode: firewall changes are logged, not applied");
            return Ok(Firewall { tx: None });
        }
        run_nft(&(base_ruleset(cfg)? + &grant_elements(grants))).await.context("installing nftables ruleset")?;
        let (tx, mut rx) = mpsc::unbounded_channel();
        // A single worker keeps grant/revoke operations strictly ordered.
        tokio::spawn(async move {
            while let Some(cmd) = rx.recv().await {
                let res = match cmd {
                    Cmd::Allow { mac, ip, secs } => allow(mac, ip, secs).await,
                    Cmd::Revoke { mac, ip } => run_nft(&del_element(mac, ip)).await.or(Ok(())),
                };
                if let Err(e) = res {
                    tracing::error!("firewall: {e:#}");
                }
            }
        });
        Ok(Firewall { tx: Some(tx) })
    }

    pub fn allow(&self, mac: Mac, ip: Ipv4Addr, secs: u64) {
        tracing::info!(%mac, %ip, secs, "grant internet");
        if let Some(tx) = &self.tx {
            let _ = tx.send(Cmd::Allow { mac, ip, secs });
        }
    }

    pub fn revoke(&self, mac: Mac, ip: Ipv4Addr) {
        tracing::info!(%mac, %ip, "revoke internet");
        if let Some(tx) = &self.tx {
            let _ = tx.send(Cmd::Revoke { mac, ip });
        }
    }
}

async fn allow(mac: Mac, ip: Ipv4Addr, secs: u64) -> Result<()> {
    // Re-adding an element doesn't refresh its timeout, so it's replaced. All
    // in one transaction: a customer topping up never loses connectivity in
    // between (the first add makes sure the delete has something to delete).
    run_nft(&format!(
        "add element inet pisowifi allowed {{ {mac} . {ip} }}\n{}add element inet pisowifi allowed {{ {mac} . {ip} timeout {}s }}\n",
        del_element(mac, ip),
        secs + KERNEL_GRACE_SECS
    ))
    .await
}

/// Re-adds saved grants. Appended to the ruleset, so the old table (whose
/// `allowed` set kept customers online while the daemon was down) is
/// swapped for the new one with no gap in between.
fn grant_elements(grants: &[(Mac, Ipv4Addr, u64)]) -> String {
    if grants.is_empty() {
        return String::new();
    }
    let items: Vec<String> = grants.iter().map(|(mac, ip, secs)| format!("{mac} . {ip} timeout {}s", secs + KERNEL_GRACE_SECS)).collect();
    format!("add element inet pisowifi allowed {{ {} }}\n", items.join(", "))
}

fn del_element(mac: Mac, ip: Ipv4Addr) -> String {
    format!("delete element inet pisowifi allowed {{ {mac} . {ip} }}\n")
}

async fn run_nft(script: &str) -> Result<()> {
    let mut child = Command::new("nft")
        // /dev/stdin rather than "-": older nft releases don't know "-".
        .args(["-f", "/dev/stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("running nft (is nftables installed?)")?;
    child.stdin.take().unwrap().write_all(script.as_bytes()).await?;
    let out = child.wait_with_output().await?;
    if !out.status.success() {
        bail!("nft: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

pub fn base_ruleset(cfg: &Config) -> Result<String> {
    let lan = &cfg.network.lan_iface;
    let wan = &cfg.network.wan_iface;
    let portal = cfg.network.portal_ip;
    let portal_port = cfg.portal.listen.port();
    let admin_port = cfg.admin.listen.port();
    let (net, prefix) = cfg.lan_net()?;
    Ok(format!(
        r#"# Generated by pisowifi. Replaced atomically on every start.
table inet pisowifi
delete table inet pisowifi
table inet pisowifi {{
    set allowed {{
        type ether_addr . ipv4_addr
        flags timeout
    }}

    set blocked_dst {{
        type ipv4_addr
        flags interval
        elements = {{ 10.0.0.0/8, 100.64.0.0/10, 169.254.0.0/16, 172.16.0.0/12, 192.168.0.0/16, 224.0.0.0/3 }}
    }}

    chain prerouting {{
        type nat hook prerouting priority -100; policy accept;
        iifname "{lan}" ip daddr != {portal} udp dport 53 dnat ip to {portal}:53
        iifname "{lan}" ip daddr != {portal} tcp dport 53 dnat ip to {portal}:53
        iifname "{lan}" ether saddr . ip saddr @allowed return
        iifname "{lan}" ip daddr != {portal} tcp dport 80 dnat ip to {portal}:{portal_port}
    }}

    chain input {{
        type filter hook input priority 0; policy accept;
        iifname "lo" accept
        iifname "{lan}" jump lan_in
        iifname "{wan}" jump wan_in
    }}

    chain lan_in {{
        meta nfproto ipv6 drop
        ip saddr != {net}/{prefix} udp dport != 67 drop
        ct state invalid drop
        ct state established,related accept
        icmp type echo-request limit rate 5/second accept
        udp dport 67 accept
        udp dport 53 meter dns_flood {{ ip saddr limit rate over 50/second burst 100 packets }} drop
        meta l4proto {{ tcp, udp }} th dport 53 ether saddr . ip saddr @allowed accept
        meta l4proto {{ tcp, udp }} th dport 53 meter dns_unpaid {{ ip saddr limit rate over 10/second burst 30 packets }} drop
        meta l4proto {{ tcp, udp }} th dport 53 accept
        tcp dport {portal_port} ct state new meter portal_flood {{ ip saddr limit rate over 20/second burst 40 packets }} drop
        tcp dport {admin_port} ct state new meter admin_flood {{ ip saddr limit rate over 3/second burst 12 packets }} drop
        tcp dport {{ {portal_port}, {admin_port} }} accept
        tcp dport 443 reject with tcp reset
        drop
    }}

    chain wan_in {{
        ct state invalid drop
        ct state established,related accept
        icmp type echo-request limit rate 5/second accept
        udp sport 67 udp dport 68 accept
        ip saddr {{ 10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16 }} tcp dport 22 accept
        drop
    }}

    chain forward {{
        type filter hook forward priority 0; policy drop;
        ct state invalid drop
        iifname "{wan}" oifname "{lan}" ct state established,related accept
        iifname "{lan}" meta nfproto ipv6 drop
        iifname "{lan}" ip daddr @blocked_dst drop
        iifname "{lan}" oifname "{wan}" ether saddr . ip saddr @allowed accept
        iifname "{lan}" meta l4proto tcp reject with tcp reset
        iifname "{lan}" reject with icmpx type admin-prohibited
    }}

    chain postrouting {{
        type nat hook postrouting priority 100; policy accept;
        oifname "{wan}" masquerade
    }}
}}
"#
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ruleset_mentions_both_interfaces_and_ports() {
        let mut cfg = Config::dev(std::env::temp_dir());
        cfg.network.lan_iface = "eth1".into();
        cfg.network.wan_iface = "eth0".into();
        cfg.network.portal_ip = "10.0.0.1".parse().unwrap();
        cfg.network.lan_cidr = "10.0.0.0/19".into();
        cfg.portal.listen = "10.0.0.1:80".parse().unwrap();
        let r = base_ruleset(&cfg).unwrap();
        assert!(r.contains(r#"iifname "eth1" oifname "eth0" ether saddr . ip saddr @allowed accept"#));
        assert!(r.contains("dnat ip to 10.0.0.1:80"));
        assert!(r.contains("ip saddr != 10.0.0.0/19"));
        assert!(r.contains("policy drop"));
        // Flood limits count new connections, not every packet of a page load.
        assert!(r.contains("tcp dport 80 ct state new meter portal_flood"));
        // Paid phones' DNS is accepted before the unpaid allowance applies.
        let paid = r.find("th dport 53 ether saddr . ip saddr @allowed accept").unwrap();
        assert!(paid < r.find("meter dns_unpaid").unwrap());
    }

    #[test]
    fn saved_grants_load_with_the_table() {
        assert_eq!(grant_elements(&[]), "");
        let a = (Mac::parse("02:11:22:33:44:55").unwrap(), Ipv4Addr::new(10, 0, 0, 9), 600);
        let b = (Mac::parse("02:00:00:00:00:02").unwrap(), Ipv4Addr::new(10, 0, 0, 10), 60);
        assert_eq!(
            grant_elements(&[a, b]),
            "add element inet pisowifi allowed { 02:11:22:33:44:55 . 10.0.0.9 timeout 630s, 02:00:00:00:00:02 . 10.0.0.10 timeout 90s }\n"
        );
    }
}
