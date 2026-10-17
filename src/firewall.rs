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
//!
//! Every minute the kernel's state is checked against the daemon's: if
//! something else flushed the table (an nftables reload, a leftover LPB
//! script), it is reinstalled with every grant in one transaction; grants
//! that are missing, cut short or left over are fixed.
//!
//! Every value written into nft scripts is a parsed MAC/IPv4/integer or a
//! validated interface name, so no injection is possible.

use crate::config::Config;
use crate::net::Mac;
use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::future::Future;
use std::net::Ipv4Addr;
use std::pin::Pin;
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::mpsc;

/// Kernel-side grace period on top of the daemon's own expiry.
const KERNEL_GRACE_SECS: u64 = 30;
/// First wait before retrying a grant nft refused; doubles up to `RETRY_MAX`.
const RETRY_FIRST: Duration = Duration::from_secs(2);
const RETRY_MAX: Duration = Duration::from_secs(30);

/// A device allowed online, and how long the daemon will keep it there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Grant {
    pub mac: Mac,
    pub ip: Ipv4Addr,
    pub secs: u64,
}

enum Cmd {
    Allow(Grant),
    Revoke { mac: Mac, ip: Ipv4Addr },
    /// Make the kernel match these grants (every running customer).
    Reconcile(Vec<Grant>),
}

#[derive(Clone)]
pub struct Firewall {
    tx: Option<mpsc::UnboundedSender<Cmd>>,
}

/// Installs the ruleset once the engine knows which grants to restore.
/// Changes requested before that wait in the queue.
pub struct Starter {
    rx: Option<(mpsc::UnboundedReceiver<Cmd>, Config)>,
}

impl Firewall {
    pub fn new(cfg: &Config, dev: bool) -> (Firewall, Starter) {
        if dev {
            tracing::info!("dev mode: firewall changes are logged, not applied");
            return (Firewall { tx: None }, Starter { rx: None });
        }
        let (tx, rx) = mpsc::unbounded_channel();
        (Firewall { tx: Some(tx) }, Starter { rx: Some((rx, cfg.clone())) })
    }

    pub fn allow(&self, mac: Mac, ip: Ipv4Addr, secs: u64) {
        tracing::info!(%mac, %ip, secs, "grant internet");
        self.send(Cmd::Allow(Grant { mac, ip, secs }));
    }

    pub fn revoke(&self, mac: Mac, ip: Ipv4Addr) {
        tracing::info!(%mac, %ip, "revoke internet");
        self.send(Cmd::Revoke { mac, ip });
    }

    /// Queued behind every change already requested, so the kernel is
    /// compared with the grants as they were when this was called.
    pub fn reconcile(&self, grants: Vec<Grant>) {
        self.send(Cmd::Reconcile(grants));
    }

    fn send(&self, cmd: Cmd) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(cmd);
        }
    }
}

impl Starter {
    /// Replaces the table with the base ruleset plus `grants`, in one
    /// transaction, then starts applying changes. `on_restore` hears about
    /// a table someone else removed or emptied.
    pub async fn start(self, grants: &[Grant], on_restore: impl Fn(&str) + Send + 'static) -> Result<()> {
        let Some((rx, cfg)) = self.rx else { return Ok(()) };
        run_nft(&base_ruleset(&cfg, grants)?).await.context("installing nftables ruleset")?;
        tracing::info!("firewall installed with {} restored grants", grants.len());
        let worker = Worker::new(SystemNft, cfg, Box::new(on_restore));
        tokio::spawn(worker.run(rx));
        Ok(())
    }
}

type BoxFut<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// How the worker talks to nftables (a fake one in tests).
trait Nft: Send + 'static {
    fn run(&self, script: String) -> BoxFut<Result<()>>;
    /// `nft list table inet pisowifi`.
    fn list(&self) -> BoxFut<Result<String>>;
}

struct SystemNft;

impl Nft for SystemNft {
    fn run(&self, script: String) -> BoxFut<Result<()>> {
        Box::pin(async move { run_nft(&script).await })
    }

    fn list(&self) -> BoxFut<Result<String>> {
        Box::pin(async {
            let out = Command::new("nft")
                .args(["list", "table", "inet", "pisowifi"])
                .stdin(Stdio::null())
                .output()
                .await
                .context("running nft")?;
            if !out.status.success() {
                bail!("nft: {}", String::from_utf8_lossy(&out.stderr).trim());
            }
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        })
    }
}

/// Applies changes strictly in order and keeps retrying grants nft refused.
struct Worker<N: Nft> {
    nft: N,
    cfg: Config,
    on_restore: Box<dyn Fn(&str) + Send>,
    /// Grants nft refused, with when the daemon's own clock ends them.
    failed: HashMap<(Mac, Ipv4Addr), Instant>,
    backoff: Duration,
    next_retry: Option<tokio::time::Instant>,
}

impl<N: Nft> Worker<N> {
    fn new(nft: N, cfg: Config, on_restore: Box<dyn Fn(&str) + Send>) -> Self {
        Worker { nft, cfg, on_restore, failed: HashMap::new(), backoff: RETRY_FIRST, next_retry: None }
    }

    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Cmd>) {
        loop {
            let retry_at = self.next_retry;
            let cmd = tokio::select! {
                c = rx.recv() => match c {
                    Some(c) => Some(c),
                    None => return,
                },
                _ = tokio::time::sleep_until(retry_at.unwrap_or_else(tokio::time::Instant::now)), if retry_at.is_some() => None,
            };
            match cmd {
                Some(cmd) => self.handle(cmd).await,
                None => self.retry().await,
            }
        }
    }

    async fn handle(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Allow(g) => {
                let deadline = Instant::now() + Duration::from_secs(g.secs);
                match self.nft.run(allow_script(&g)).await {
                    Ok(()) => {
                        self.failed.remove(&(g.mac, g.ip));
                    }
                    Err(e) => {
                        tracing::error!(mac = %g.mac, ip = %g.ip, "firewall refused a grant (retrying): {e:#}");
                        self.failed.insert((g.mac, g.ip), deadline);
                        self.schedule_retry();
                    }
                }
            }
            Cmd::Revoke { mac, ip } => {
                self.failed.remove(&(mac, ip));
                if let Err(e) = self.nft.run(revoke_script(mac, ip)).await {
                    // Left over in the kernel: the next reconcile removes it.
                    tracing::error!(%mac, %ip, "firewall: revoking: {e:#}");
                }
            }
            Cmd::Reconcile(grants) => self.reconcile(grants).await,
        }
    }

    async fn retry(&mut self) {
        self.next_retry = None;
        let now = Instant::now();
        self.failed.retain(|_, end| *end > now);
        let pending: Vec<_> = self.failed.iter().map(|(&(mac, ip), end)| (mac, ip, *end)).collect();
        for (mac, ip, end) in pending {
            let g = Grant { mac, ip, secs: end.saturating_duration_since(now).as_secs() };
            match self.nft.run(allow_script(&g)).await {
                Ok(()) => {
                    tracing::info!(%mac, %ip, "firewall accepted the grant on retry");
                    self.failed.remove(&(mac, ip));
                }
                Err(e) => tracing::warn!(%mac, %ip, "firewall still refuses the grant: {e:#}"),
            }
        }
        if self.failed.is_empty() {
            self.backoff = RETRY_FIRST;
        } else {
            self.backoff = (self.backoff * 2).min(RETRY_MAX);
            self.schedule_retry();
        }
    }

    fn schedule_retry(&mut self) {
        if self.next_retry.is_none() {
            self.next_retry = Some(tokio::time::Instant::now() + self.backoff);
        }
    }

    async fn reconcile(&mut self, grants: Vec<Grant>) {
        let listing = match self.nft.list().await {
            Ok(text) => Some(text),
            Err(e) => {
                tracing::debug!("listing the firewall: {e:#}");
                None
            }
        };
        let result = match plan(listing.as_deref(), &grants) {
            Plan::InSync => Ok(()),
            Plan::Reinstall(why) => {
                tracing::warn!("firewall: {why}; reinstalling it with {} grants", grants.len());
                let r = match base_ruleset(&self.cfg, &grants) {
                    Ok(script) => self.nft.run(script).await,
                    Err(e) => Err(e),
                };
                if r.is_ok() {
                    (self.on_restore)(&format!("{why}; reinstalled with {} customers", grants.len()));
                }
                r
            }
            Plan::Fix { add, remove } => {
                tracing::warn!("firewall: fixing {} missing or cut-short grants, removing {} left over", add.len(), remove.len());
                let mut script: String = add.iter().map(allow_script).collect();
                script.extend(remove.iter().map(|&(mac, ip)| revoke_script(mac, ip)));
                self.nft.run(script).await
            }
        };
        match result {
            // Every grant the daemon holds is in the kernel now, including
            // any that were waiting for a retry.
            Ok(()) => {
                self.failed.clear();
                self.backoff = RETRY_FIRST;
            }
            Err(e) => tracing::error!("firewall: reconcile failed (trying again in a minute): {e:#}"),
        }
    }
}

/// Re-adding an element doesn't refresh its timeout, so it's replaced. All
/// in one transaction: a customer topping up never loses connectivity in
/// between (the first add makes sure the delete has something to delete).
fn allow_script(g: &Grant) -> String {
    let (mac, ip) = (g.mac, g.ip);
    format!(
        "add element inet pisowifi allowed {{ {mac} . {ip} }}\n{}add element inet pisowifi allowed {{ {mac} . {ip} timeout {}s }}\n",
        del_element(mac, ip),
        g.secs + KERNEL_GRACE_SECS
    )
}

/// Removes an element whether or not it's still there, so a real failure
/// isn't hidden among "no such element" errors.
fn revoke_script(mac: Mac, ip: Ipv4Addr) -> String {
    format!("add element inet pisowifi allowed {{ {mac} . {ip} }}\n{}", del_element(mac, ip))
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

#[derive(Debug, PartialEq)]
enum Plan {
    InSync,
    Reinstall(&'static str),
    Fix { add: Vec<Grant>, remove: Vec<(Mac, Ipv4Addr)> },
}

/// Compares `nft list table inet pisowifi` output (None: it failed, so the
/// table is gone) with the grants the daemon holds.
fn plan(listing: Option<&str>, grants: &[Grant]) -> Plan {
    let Some(text) = listing else { return Plan::Reinstall("the pisowifi table was missing") };
    // A flushed table keeps its chains but loses every rule.
    if !text.contains("chain forward") || !text.contains("@allowed") || !text.contains("masquerade") {
        return Plan::Reinstall("the pisowifi rules were missing");
    }
    let Some(kernel) = parse_allowed(text) else { return Plan::Reinstall("the allowed set was missing") };
    let mut add = vec![];
    for g in grants.iter().filter(|g| g.secs > 0) {
        let fine = match kernel.get(&(g.mac, g.ip)) {
            None => false,
            // No timeout at all: replace it with the right one.
            Some(None) => false,
            // Normally the kernel ends a grant 30 s after the daemon does.
            Some(Some(left)) => *left + 5 >= g.secs && *left <= g.secs + KERNEL_GRACE_SECS + 120,
        };
        if !fine {
            add.push(*g);
        }
    }
    let mut remove: Vec<(Mac, Ipv4Addr)> =
        kernel.keys().filter(|k| !grants.iter().any(|g| (g.mac, g.ip) == **k)).copied().collect();
    remove.sort();
    if add.is_empty() && remove.is_empty() {
        Plan::InSync
    } else {
        Plan::Fix { add, remove }
    }
}

/// Elements of `set allowed`, with the seconds until each expires (None:
/// it has no timeout). None if the set isn't in the listing.
fn parse_allowed(listing: &str) -> Option<HashMap<(Mac, Ipv4Addr), Option<u64>>> {
    let start = listing.find("set allowed {")?;
    let block = &listing[start..];
    let end = block.find('}')?;
    let block = &block[..=end];
    let mut out = HashMap::new();
    // "elements = { a . b timeout 1h expires 59m58s, ... }" (may wrap lines).
    let Some(el) = block.find("elements = {") else { return Some(out) };
    let items = &block[el + "elements = {".len()..block.len() - 1];
    for item in items.split(',') {
        let tok: Vec<&str> = item.split_whitespace().collect();
        if tok.len() < 3 || tok[1] != "." {
            continue;
        }
        let (Ok(mac), Ok(ip)) = (Mac::parse(tok[0]), tok[2].parse::<Ipv4Addr>()) else { continue };
        let expires = tok.iter().position(|t| *t == "expires").and_then(|i| tok.get(i + 1)).and_then(|d| parse_nft_duration(d));
        out.insert((mac, ip), expires);
    }
    Some(out)
}

/// nft's durations: "1d2h3m4s", "59m58s120ms", "30s". Rounded down to seconds.
fn parse_nft_duration(s: &str) -> Option<u64> {
    let (mut total_ms, mut num, mut any) = (0u64, String::new(), false);
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c.is_ascii_digit() {
            num.push(c);
            continue;
        }
        let n: u64 = num.parse().ok()?;
        num.clear();
        let unit_ms = match c {
            'd' => 86_400_000,
            'h' => 3_600_000,
            'm' if chars.peek() == Some(&'s') => {
                chars.next();
                1
            }
            'm' => 60_000,
            's' => 1000,
            _ => return None,
        };
        total_ms += n * unit_ms;
        any = true;
    }
    (any && num.is_empty()).then_some(total_ms / 1000)
}

/// The whole table. `grants` go straight into the `allowed` set, so a
/// restart (or a reinstall) restores everyone in the same transaction.
pub fn base_ruleset(cfg: &Config, grants: &[Grant]) -> Result<String> {
    let elements: Vec<String> = grants
        .iter()
        .filter(|g| g.secs > 0)
        .map(|g| format!("{} . {} timeout {}s", g.mac, g.ip, g.secs + KERNEL_GRACE_SECS))
        .collect();
    let elements = if elements.is_empty() {
        String::new()
    } else {
        format!("\n        elements = {{ {} }}", elements.join(", "))
    };
    let lan = &cfg.network.lan_iface;
    let wan = &cfg.network.wan_iface;
    let portal = cfg.network.portal_ip;
    let portal_port = cfg.portal.listen.port();
    let admin_port = cfg.admin.listen.port();
    let ssh_ports = cfg.network.ssh_ports.iter().map(u16::to_string).collect::<Vec<_>>().join(", ");
    // Test layout: the owner's own device may SSH in from the customer side.
    let owner_macs: Vec<String> = cfg
        .admin
        .allow_macs
        .iter()
        .filter_map(|m| Mac::parse(m).ok())
        .map(|m| m.to_string())
        .collect();
    let owner_ssh = if cfg.admin.owner_ssh_from_lan && !owner_macs.is_empty() {
        format!("\n        ether saddr {{ {} }} tcp dport {{ {ssh_ports} }} accept", owner_macs.join(", "))
    } else {
        String::new()
    };
    let (net, prefix) = cfg.lan_net()?;
    Ok(format!(
        r#"# Generated by pisowifi. Replaced atomically on every start.
table inet pisowifi
delete table inet pisowifi
table inet pisowifi {{
    set allowed {{
        type ether_addr . ipv4_addr
        flags timeout{elements}
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
        tcp dport {{ {portal_port}, {admin_port} }} accept{owner_ssh}
        tcp dport 443 reject with tcp reset
        drop
    }}

    chain wan_in {{
        ct state invalid drop
        ct state established,related accept
        icmp type echo-request limit rate 5/second accept
        udp sport 67 udp dport 68 accept
        ip saddr {{ 10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16 }} tcp dport {{ {ssh_ports} }} accept
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
        let r = base_ruleset(&cfg, &[]).unwrap();
        assert!(r.contains(r#"iifname "eth1" oifname "eth0" ether saddr . ip saddr @allowed accept"#));
        assert!(r.contains("dnat ip to 10.0.0.1:80"));
        assert!(r.contains("ip saddr != 10.0.0.0/19"));
        assert!(r.contains("policy drop"));
        // Flood limits count new connections, not every packet of a page load.
        assert!(r.contains("tcp dport 80 ct state new meter portal_flood"));
        // Paid phones' DNS is accepted before the unpaid allowance applies.
        let paid = r.find("th dport 53 ether saddr . ip saddr @allowed accept").unwrap();
        assert!(paid < r.find("meter dns_unpaid").unwrap());
        assert!(!r.contains("elements = { 02"));
        assert!(r.contains("192.168.0.0/16 } tcp dport { 22 } accept"));
        // SSH on another port (LPB's images used 320) stays reachable from the modem side.
        cfg.network.ssh_ports = vec![22, 320];
        let r = base_ruleset(&cfg, &[]).unwrap();
        assert!(r.contains("192.168.0.0/16 } tcp dport { 22, 320 } accept"));
        // Customers can't SSH in; in the test layout the owner's device can.
        assert!(!r.contains("ether saddr {"));
        cfg.admin.allow_macs = vec!["00:E0:4C:36:52:91".into()];
        assert!(!base_ruleset(&cfg, &[]).unwrap().contains("ether saddr {"), "needs the test-layout switch");
        cfg.admin.owner_ssh_from_lan = true;
        let r = base_ruleset(&cfg, &[]).unwrap();
        assert!(r.contains("ether saddr { 00:e0:4c:36:52:91 } tcp dport { 22, 320 } accept"));
    }

    fn mac(s: &str) -> Mac {
        Mac::parse(s).unwrap()
    }

    fn grant(m: &str, ip: &str, secs: u64) -> Grant {
        Grant { mac: mac(m), ip: ip.parse().unwrap(), secs }
    }

    #[test]
    fn restored_grants_go_into_the_same_transaction() {
        let cfg = Config::dev(std::env::temp_dir());
        let r = base_ruleset(&cfg, &[grant("02:11:22:33:44:55", "10.0.0.9", 600), grant("02:11:22:33:44:56", "10.0.0.10", 0)]).unwrap();
        assert!(r.contains("flags timeout\n        elements = { 02:11:22:33:44:55 . 10.0.0.9 timeout 630s }\n"), "{r}");
        assert!(!r.contains("10.0.0.10"), "nothing left: not granted");
    }

    #[test]
    fn reads_nft_durations() {
        assert_eq!(parse_nft_duration("30s"), Some(30));
        assert_eq!(parse_nft_duration("59m58s120ms"), Some(3598));
        assert_eq!(parse_nft_duration("1d2h3m4s"), Some(86400 + 7200 + 184));
        assert_eq!(parse_nft_duration("500ms"), Some(0));
        assert_eq!(parse_nft_duration("12"), None);
        assert_eq!(parse_nft_duration("3x"), None);
    }

    /// Trimmed `nft list table inet pisowifi` output.
    const LISTING: &str = "table inet pisowifi {
\tset allowed {
\t\ttype ether_addr . ipv4_addr
\t\tflags timeout
\t\telements = { 02:11:22:33:44:55 . 10.0.0.9 timeout 10m30s expires 10m20s500ms,
\t\t\t     02:11:22:33:44:56 . 10.0.0.10 timeout 1h expires 2m,
\t\t\t     02:11:22:33:44:57 . 10.0.0.11 }
\t}

\tset blocked_dst {
\t\ttype ipv4_addr
\t\tflags interval
\t\telements = { 10.0.0.0/8, 192.168.0.0/16 }
\t}

\tchain prerouting {
\t\ttype nat hook prerouting priority dstnat; policy accept;
\t\tiifname \"eth1\" ether saddr . ip saddr @allowed return
\t}

\tchain forward {
\t\ttype filter hook forward priority filter; policy drop;
\t\tiifname \"eth1\" oifname \"eth0\" ether saddr . ip saddr @allowed accept
\t}

\tchain postrouting {
\t\ttype nat hook postrouting priority srcnat; policy accept;
\t\toifname \"eth0\" masquerade
\t}
}
";

    #[test]
    fn reads_the_allowed_set() {
        let set = parse_allowed(LISTING).unwrap();
        assert_eq!(set.len(), 3);
        assert_eq!(set[&(mac("02:11:22:33:44:55"), "10.0.0.9".parse().unwrap())], Some(620));
        assert_eq!(set[&(mac("02:11:22:33:44:56"), "10.0.0.10".parse().unwrap())], Some(120));
        assert_eq!(set[&(mac("02:11:22:33:44:57"), "10.0.0.11".parse().unwrap())], None);
        assert!(parse_allowed("table inet pisowifi {\n\tset allowed {\n\t\ttype ether_addr . ipv4_addr\n\t}\n}").unwrap().is_empty());
    }

    #[test]
    fn reconcile_plans() {
        let a = grant("02:11:22:33:44:55", "10.0.0.9", 590);
        // Missing table or rules: reinstall everything.
        assert!(matches!(plan(None, &[a]), Plan::Reinstall(_)));
        let flushed = "table inet pisowifi {\n\tset allowed {\n\t}\n\tchain forward {\n\t}\n}";
        assert!(matches!(plan(Some(flushed), &[a]), Plan::Reinstall(_)));
        // Kernel has a, a cut-short b and a no-timeout c; daemon has a, b and d.
        let b = grant("02:11:22:33:44:56", "10.0.0.10", 3000);
        let d = grant("02:11:22:33:44:58", "10.0.0.12", 60);
        match plan(Some(LISTING), &[a, b, d]) {
            Plan::Fix { add, remove } => {
                assert_eq!(add, vec![b, d]);
                assert_eq!(remove, vec![(mac("02:11:22:33:44:57"), "10.0.0.11".parse().unwrap())]);
            }
            p => panic!("{p:?}"),
        }
        let b_ok = grant("02:11:22:33:44:56", "10.0.0.10", 100);
        let c = grant("02:11:22:33:44:57", "10.0.0.11", 100);
        assert!(matches!(plan(Some(LISTING), &[a, b_ok, c]), Plan::Fix { ref add, ref remove } if add == &vec![c] && remove.is_empty()));
        let in_sync = LISTING.replace(",\n\t\t\t     02:11:22:33:44:57 . 10.0.0.11 }", " }");
        assert_eq!(plan(Some(&in_sync), &[a, b_ok]), Plan::InSync);
    }

    /// Records scripts; refuses everything while `fail` is set.
    #[derive(Clone, Default)]
    struct FakeNft {
        fail: std::sync::Arc<std::sync::atomic::AtomicBool>,
        scripts: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        listing: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    }

    impl Nft for FakeNft {
        fn run(&self, script: String) -> BoxFut<Result<()>> {
            let fail = self.fail.load(std::sync::atomic::Ordering::SeqCst);
            if !fail {
                self.scripts.lock().unwrap().push(script);
            }
            Box::pin(async move { if fail { bail!("nft: busy") } else { Ok(()) } })
        }
        fn list(&self) -> BoxFut<Result<String>> {
            let l = self.listing.lock().unwrap().clone();
            Box::pin(async move { l.context("no table") })
        }
    }

    fn worker(nft: &FakeNft) -> Worker<FakeNft> {
        Worker::new(nft.clone(), Config::dev(std::env::temp_dir()), Box::new(|_| {}))
    }

    #[tokio::test]
    async fn refused_grants_are_retried_until_they_stick() {
        let nft = FakeNft::default();
        let mut w = worker(&nft);
        let a = grant("02:11:22:33:44:55", "10.0.0.9", 600);
        nft.fail.store(true, std::sync::atomic::Ordering::SeqCst);
        w.handle(Cmd::Allow(a)).await;
        assert!(w.failed.contains_key(&(a.mac, a.ip)));
        assert!(w.next_retry.is_some());
        // Still failing: kept, and the wait grows.
        w.retry().await;
        assert!(w.failed.contains_key(&(a.mac, a.ip)));
        assert_eq!(w.backoff, RETRY_FIRST * 2);
        nft.fail.store(false, std::sync::atomic::Ordering::SeqCst);
        w.retry().await;
        assert!(w.failed.is_empty());
        let scripts = nft.scripts.lock().unwrap();
        assert!(scripts[0].contains("02:11:22:33:44:55 . 10.0.0.9 timeout 6"), "{}", scripts[0]);
    }

    #[tokio::test]
    async fn a_revoke_cancels_a_pending_retry() {
        let nft = FakeNft::default();
        let mut w = worker(&nft);
        let a = grant("02:11:22:33:44:55", "10.0.0.9", 600);
        nft.fail.store(true, std::sync::atomic::Ordering::SeqCst);
        w.handle(Cmd::Allow(a)).await;
        w.handle(Cmd::Revoke { mac: a.mac, ip: a.ip }).await;
        assert!(w.failed.is_empty(), "time ran out or the customer paused: never grant it again");
    }

    #[tokio::test]
    async fn a_missing_table_is_reinstalled_with_every_grant() {
        let nft = FakeNft::default();
        let restored = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let r2 = restored.clone();
        let mut w = Worker::new(nft.clone(), Config::dev(std::env::temp_dir()), Box::new(move |why: &str| *r2.lock().unwrap() = why.into()));
        let a = grant("02:11:22:33:44:55", "10.0.0.9", 600);
        w.handle(Cmd::Reconcile(vec![a])).await;
        let scripts = nft.scripts.lock().unwrap();
        assert_eq!(scripts.len(), 1, "one transaction");
        assert!(scripts[0].contains("table inet pisowifi {") && scripts[0].contains("02:11:22:33:44:55 . 10.0.0.9 timeout 630s"));
        assert!(restored.lock().unwrap().contains("missing"));
    }
}
