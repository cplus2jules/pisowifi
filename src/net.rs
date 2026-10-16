//! Identifying customers: we only trust the TCP peer address and the kernel's
//! neighbour (ARP) table. Nothing a client sends in headers is used.

use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::fmt;
use std::net::Ipv4Addr;
use std::path::Path;

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Mac(pub [u8; 6]);

impl Mac {
    pub fn parse(s: &str) -> Result<Mac> {
        let parts: Vec<&str> = s.trim().split([':', '-']).collect();
        if parts.len() != 6 {
            bail!("bad MAC address");
        }
        let mut out = [0u8; 6];
        for (i, p) in parts.iter().enumerate() {
            if p.len() != 2 {
                bail!("bad MAC address");
            }
            out[i] = u8::from_str_radix(p, 16)?;
        }
        if out == [0; 6] || out == [0xff; 6] || out[0] & 1 == 1 {
            bail!("not a unicast MAC address");
        }
        Ok(Mac(out))
    }

    /// Short form shown to customers so they can recognise their device.
    pub fn masked(&self) -> String {
        format!("Device …{:02X}{:02X}{:02X}", self.0[3], self.0[4], self.0[5])
    }
}

impl fmt::Display for Mac {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let b = self.0;
        write!(f, "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", b[0], b[1], b[2], b[3], b[4], b[5])
    }
}

impl fmt::Debug for Mac {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl Serialize for Mac {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> serde::Deserialize<'de> for Mac {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Mac, D::Error> {
        let s = String::deserialize(d)?;
        Mac::parse(&s).map_err(serde::de::Error::custom)
    }
}

/// Looks up the MAC of a LAN client from /proc/net/arp.
pub fn mac_for_ip(ip: Ipv4Addr, iface: &str, dev: bool) -> Option<Mac> {
    if dev {
        // Laptop testing: every request comes from 127.0.0.1, so fake one device.
        let o = ip.octets();
        return Some(Mac([0x02, 0x00, o[0], o[1], o[2], o[3]]));
    }
    let table = std::fs::read_to_string("/proc/net/arp").ok()?;
    parse_arp(&table, ip, iface)
}

/// The LAN address the kernel currently has for `mac` (reverse of `mac_for_ip`).
pub fn ip_for_mac(mac: Mac, iface: &str, dev: bool) -> Option<Ipv4Addr> {
    if dev {
        let b = mac.0;
        return (b[0] == 0x02 && b[1] == 0).then(|| Ipv4Addr::new(b[2], b[3], b[4], b[5]));
    }
    let table = std::fs::read_to_string("/proc/net/arp").ok()?;
    arp_entries(&table, iface).into_iter().find(|(m, _)| *m == mac).map(|(_, ip)| ip)
}

fn parse_arp(table: &str, ip: Ipv4Addr, iface: &str) -> Option<Mac> {
    arp_entries(table, iface).into_iter().find(|(_, i)| *i == ip).map(|(m, _)| m)
}

/// Complete neighbour entries on `iface`.
fn arp_entries(table: &str, iface: &str) -> Vec<(Mac, Ipv4Addr)> {
    let mut out = vec![];
    for line in table.lines().skip(1) {
        let cols: Vec<&str> = line.split_whitespace().collect();
        // Flags 0x0 means the entry is incomplete.
        if cols.len() < 6 || cols[5] != iface || cols[2] == "0x0" {
            continue;
        }
        if let (Ok(ip), Ok(mac)) = (cols[0].parse(), Mac::parse(cols[3])) {
            out.push((mac, ip));
        }
    }
    out
}

/// One line of dnsmasq's lease file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lease {
    pub mac: Mac,
    pub ip: Ipv4Addr,
    pub name: Option<String>,
}

/// Lines look like `<expiry> <mac> <ip> <hostname or *> <client id or *>`.
/// IPv6 lines and anything else unexpected are skipped.
pub fn parse_leases(text: &str) -> Vec<Lease> {
    let mut out = vec![];
    for line in text.lines() {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() < 4 {
            continue;
        }
        if let (Ok(mac), Ok(ip)) = (Mac::parse(cols[1]), cols[2].parse()) {
            let name = Some(crate::brand::clean_text(cols[3], 40)).filter(|n| !n.is_empty() && n != "*");
            out.push(Lease { mac, ip, name });
        }
    }
    out
}

pub fn read_leases(path: &Path) -> Result<Vec<Lease>> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(parse_leases(&text))
}

/// A device on the customer network, from the neighbour table or a DHCP lease.
#[derive(Debug, Clone, Serialize)]
pub struct LanDevice {
    pub mac: Mac,
    pub ip: Ipv4Addr,
    /// The name the device gave the DHCP server (its own choice: display only).
    pub name: Option<String>,
}

/// Devices seen on the LAN: present now (neighbour table) or holding a lease.
pub fn lan_devices(iface: &str, leases: &Path, dev: bool) -> Vec<LanDevice> {
    if dev {
        return vec![];
    }
    let arp = std::fs::read_to_string("/proc/net/arp").unwrap_or_default();
    merge_devices(arp_entries(&arp, iface), read_leases(leases).unwrap_or_default())
}

fn merge_devices(arp: Vec<(Mac, Ipv4Addr)>, leases: Vec<Lease>) -> Vec<LanDevice> {
    let mut out: Vec<LanDevice> = vec![];
    for (mac, ip) in arp {
        let name = leases.iter().find(|l| l.mac == mac).and_then(|l| l.name.clone());
        out.push(LanDevice { mac, ip, name });
    }
    for l in leases {
        if !out.iter().any(|d| d.mac == l.mac) {
            out.push(LanDevice { mac: l.mac, ip: l.ip, name: l.name });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const ARP: &str = "IP address       HW type     Flags       HW address            Mask     Device
10.0.0.23        0x1         0x2         a4:5e:60:11:22:33     *        eth1
10.0.0.24        0x1         0x0         00:00:00:00:00:00     *        eth1
192.168.1.1      0x1         0x2         10:20:30:40:50:60     *        eth0
";

    #[test]
    fn finds_lan_client() {
        let m = parse_arp(ARP, "10.0.0.23".parse().unwrap(), "eth1").unwrap();
        assert_eq!(m.to_string(), "a4:5e:60:11:22:33");
    }

    #[test]
    fn ignores_incomplete_and_wrong_iface() {
        assert!(parse_arp(ARP, "10.0.0.24".parse().unwrap(), "eth1").is_none());
        assert!(parse_arp(ARP, "192.168.1.1".parse().unwrap(), "eth1").is_none());
    }

    #[test]
    fn finds_ip_of_a_mac() {
        let mac = Mac::parse("a4:5e:60:11:22:33").unwrap();
        let found = arp_entries(ARP, "eth1").into_iter().find(|(m, _)| *m == mac).map(|(_, ip)| ip);
        assert_eq!(found, Some("10.0.0.23".parse().unwrap()));
        // Dev mode's fake MACs map back to the address they came from.
        let fake = mac_for_ip("127.0.0.1".parse().unwrap(), "lo0", true).unwrap();
        assert_eq!(ip_for_mac(fake, "lo0", true), Some("127.0.0.1".parse().unwrap()));
    }

    const LEASES: &str = "1760600000 a4:5e:60:11:22:33 10.0.0.23 Juans-iPhone 01:a4:5e:60:11:22:33
1760600100 02:aa:bb:cc:dd:ee 10.0.0.40 * *
duid 00:01:00:01:2c:aa:bb:cc:dd:ee:ff:00
1760600200 not-a-mac 10.0.0.41 x *
";

    #[test]
    fn reads_dnsmasq_leases() {
        let l = parse_leases(LEASES);
        assert_eq!(l.len(), 2);
        assert_eq!(l[0].mac.to_string(), "a4:5e:60:11:22:33");
        assert_eq!(l[0].ip, "10.0.0.23".parse::<Ipv4Addr>().unwrap());
        assert_eq!(l[0].name.as_deref(), Some("Juans-iPhone"));
        assert_eq!(l[1].name, None);
    }

    #[test]
    fn lan_devices_merge_neighbours_and_leases() {
        let d = merge_devices(arp_entries(ARP, "eth1"), parse_leases(LEASES));
        assert_eq!(d.len(), 2);
        assert_eq!(d[0].name.as_deref(), Some("Juans-iPhone"));
        assert_eq!(d[1].ip, "10.0.0.40".parse::<Ipv4Addr>().unwrap());
    }

    #[test]
    fn rejects_bad_macs() {
        assert!(Mac::parse("ff:ff:ff:ff:ff:ff").is_err());
        assert!(Mac::parse("01:00:5e:00:00:01").is_err());
        assert!(Mac::parse("a4:5e:60:11:22").is_err());
        assert!(Mac::parse("a4:5e:60:11:22:3g").is_err());
    }
}
