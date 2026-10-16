//! Identifying customers: we only trust the TCP peer address and the kernel's
//! neighbour (ARP) table. Nothing a client sends in headers is used.

use anyhow::{bail, Result};
use std::fmt;
use std::net::Ipv4Addr;

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

fn parse_arp(table: &str, ip: Ipv4Addr, iface: &str) -> Option<Mac> {
    for line in table.lines().skip(1) {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() < 6 || cols[5] != iface {
            continue;
        }
        // Flags 0x0 means the entry is incomplete.
        if cols[2] == "0x0" || cols[0].parse::<Ipv4Addr>().ok() != Some(ip) {
            continue;
        }
        return Mac::parse(cols[3]).ok();
    }
    None
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
    fn rejects_bad_macs() {
        assert!(Mac::parse("ff:ff:ff:ff:ff:ff").is_err());
        assert!(Mac::parse("01:00:5e:00:00:01").is_err());
        assert!(Mac::parse("a4:5e:60:11:22").is_err());
        assert!(Mac::parse("a4:5e:60:11:22:3g").is_err());
    }
}
