//! What this vendo is made of, for Settings → System in the dashboard: the
//! software, the operating system and every piece of hardware it can see
//! (board, storage, network ports, USB devices, coin slot). It's for
//! whoever works on the vendo next; "Copy details" there pastes it as text.
//!
//! Everything is read from /proc, /sys and a few tools' version output.
//! Nothing is changed. A new row is one more `row(...)` in a section.

use crate::config::Config;
use serde::Serialize;
use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

#[derive(Serialize)]
pub struct SysInfo {
    /// Made-up hardware (dev mode on a laptop).
    pub sample: bool,
    /// The vendo's clock (Unix seconds), so the page can tell when it's off.
    pub clock: i64,
    pub sections: Vec<Section>,
}

#[derive(Serialize)]
pub struct Section {
    pub title: &'static str,
    pub rows: Vec<Row>,
}

#[derive(Serialize)]
pub struct Row {
    pub label: String,
    pub value: String,
    /// A smaller second line: model numbers, IDs, drivers.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub detail: String,
}

fn row(label: impl Into<String>, value: impl Into<String>, detail: impl Into<String>) -> Row {
    Row { label: label.into(), value: value.into(), detail: detail.into() }
}

/// What only the running service knows.
pub struct Facts {
    pub schema: i64,
    pub cipher: String,
    pub uptime_secs: u64,
    pub reader_ok: bool,
}

pub fn read(cfg: &Config, dev: bool, facts: &Facts) -> SysInfo {
    let mut sections = vec![software(cfg, dev, facts)];
    if dev {
        sections.extend(sample());
    } else {
        sections.extend([os(), hardware(), network(cfg), usb()]);
    }
    sections.push(coin(cfg, facts));
    SysInfo { sample: dev, clock: crate::db::now(), sections }
}

fn software(cfg: &Config, dev: bool, f: &Facts) -> Section {
    let key = match crate::release::trusted_key() {
        Some(k) => row("Release key", "Signed updates accepted", format!("Key {}", hex::encode(&k.as_bytes()[..8]))),
        None => row("Release key", "None: updates are refused", "Built without release-key.pub"),
    };
    let tool = |cmd: &str, arg: &str, keep: fn(&str) -> String| -> String {
        if dev {
            return "(not on the laptop)".into();
        }
        Command::new(cmd)
            .arg(arg)
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .and_then(|s| s.lines().next().map(keep))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "not found".into())
    };
    Section {
        title: "Software",
        rows: vec![
            row("Piso WiFi", env!("CARGO_PKG_VERSION"), env!("PISOWIFI_TARGET")),
            key,
            row("Database", format!("SQLCipher {}", f.cipher), format!("Schema v{} · {}", f.schema, cfg.storage.data_dir.display())),
            // "nftables v1.1.1 (Commodore Bullmoose #2)" → "nftables v1.1.1"
            row("Firewall", tool("nft", "--version", |l| l.split(" (").next().unwrap_or(l).trim().to_string()), "nftables, fail-closed"),
            // "Dnsmasq version 2.90  Copyright (c) …" → "dnsmasq 2.90"
            row(
                "DHCP and DNS",
                tool("dnsmasq", "--version", |l| l.split("  ").next().unwrap_or(l).replace("Dnsmasq version", "dnsmasq").trim().to_string()),
                "dnsmasq",
            ),
            row("Running for", duration(f.uptime_secs), "Since the service last started"),
        ],
    }
}

fn os() -> Section {
    let release = key_values(&text("/etc/os-release"));
    let armbian = key_values(&text("/etc/armbian-release"));
    let mut name = release.get("PRETTY_NAME").cloned().unwrap_or_else(|| "Linux".into());
    let mut detail = String::new();
    if let Some(v) = armbian.get("VERSION") {
        name = format!("Armbian {v} ({name})");
        detail = ["BRANCH", "IMAGE_TYPE"].iter().filter_map(|k| armbian.get(*k)).cloned().collect::<Vec<_>>().join(" · ");
    }
    let tz = text("/etc/timezone");
    let tz = if tz.is_empty() {
        std::fs::read_link("/etc/localtime")
            .ok()
            .map(|p| p.to_string_lossy().split("zoneinfo/").last().unwrap_or_default().to_string())
            .unwrap_or_default()
    } else {
        tz
    };
    let synced = Path::new("/run/systemd/timesync/synchronized").exists();
    let boot = text("/proc/uptime").split('.').next().and_then(|s| s.parse::<u64>().ok());
    Section {
        title: "Operating system",
        rows: vec![
            row("System", name, detail),
            row("Kernel", text("/proc/sys/kernel/osrelease"), machine()),
            row("Host name", text("/proc/sys/kernel/hostname"), ""),
            row("Time zone", if tz.is_empty() { "Not set".into() } else { tz }, ""),
            row(
                "Clock",
                chrono::Local::now().format("%a %b %-d %Y, %-I:%M %p").to_string(),
                if synced { "Synced over the internet" } else { "Not synced: no internet yet, and the board has no clock battery" },
            ),
            row("Board on for", boot.map(duration).unwrap_or_default(), "Since the last power-on or restart"),
        ],
    }
}

fn hardware() -> Section {
    let model = text("/proc/device-tree/model");
    let compatible = text("/proc/device-tree/compatible").split('\0').filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" · ");
    let cpuinfo = text("/proc/cpuinfo");
    let field = |name: &str| cpuinfo.lines().find(|l| l.starts_with(name)).and_then(|l| l.split(':').nth(1)).map(|s| s.trim().to_string());
    let cores = cpuinfo.lines().filter(|l| l.starts_with("processor")).count();
    let mhz = text("/sys/devices/system/cpu/cpu0/cpufreq/cpuinfo_max_freq").parse::<u64>().ok().map(|k| k / 1000);
    let mut cpu = format!("{cores} cores");
    if let Some(m) = mhz {
        cpu += &format!(", up to {:.2} GHz", m as f64 / 1000.0);
    }
    let cpu_detail = [field("Hardware"), field("model name")].into_iter().flatten().collect::<Vec<_>>().join(" · ");
    let mem = text("/proc/meminfo").lines().find(|l| l.starts_with("MemTotal:")).and_then(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok());
    let temp = text("/sys/class/thermal/thermal_zone0/temp").parse::<f64>().ok().map(|m| format!("{:.0} °C", m / 1000.0));
    let mut rows = vec![
        row("Board", if model.is_empty() { "Unknown".into() } else { model }, compatible),
        row("Processor", cpu, cpu_detail),
        row("Memory", mem.map(|k| format!("{} MB", k / 1024)).unwrap_or_default(), ""),
    ];
    rows.extend(storage());
    if let Some(t) = temp {
        rows.push(row("Temperature", t, "CPU"));
    }
    Section { title: "Hardware", rows }
}

/// SD cards, eMMC and USB drives.
fn storage() -> Vec<Row> {
    let mut out = vec![];
    let Ok(dir) = std::fs::read_dir("/sys/block") else { return out };
    let mut names: Vec<String> = dir.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    names.sort();
    for name in names {
        let real = name.starts_with("mmcblk") || name.starts_with("sd") || name.starts_with("nvme");
        if !real || name.contains("boot") || name.contains("rpmb") {
            continue;
        }
        let base = Path::new("/sys/block").join(&name);
        let bytes = text(base.join("size")).parse::<u64>().unwrap_or(0) * 512;
        if bytes == 0 {
            continue;
        }
        let dev = base.join("device");
        let (kind, detail) = if name.starts_with("mmcblk") {
            let kind = if text(dev.join("type")) == "MMC" { "eMMC" } else { "microSD card" };
            let made = text(dev.join("date"));
            let card = text(dev.join("name"));
            (kind.to_string(), [card, if made.is_empty() { made } else { format!("made {made}") }].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" · "))
        } else {
            let model = format!("{} {}", text(dev.join("vendor")), text(dev.join("model"))).trim().to_string();
            ("USB drive".to_string(), model)
        };
        out.push(row(format!("Storage ({name})"), format!("{kind}, {}", size(bytes)), detail));
    }
    out
}

fn network(cfg: &Config) -> Section {
    let addrs = ipv4_addrs();
    let mut rows = vec![];
    let mut names: Vec<String> = std::fs::read_dir("/sys/class/net")
        .map(|d| d.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect())
        .unwrap_or_default();
    names.sort();
    for name in names.iter().filter(|n| *n != "lo") {
        let base = Path::new("/sys/class/net").join(name);
        let role = if *name == cfg.network.lan_iface {
            "Customer side"
        } else if *name == cfg.network.wan_iface {
            "Internet side"
        } else {
            "Not used"
        };
        let dev = base.join("device");
        let bus = std::fs::read_link(dev.join("subsystem")).ok().and_then(|p| p.file_name().map(|f| f.to_string_lossy().into_owned()));
        let kind = if base.join("wireless").exists() {
            "WiFi"
        } else if bus.as_deref() == Some("usb") {
            "USB Ethernet"
        } else if bus.is_some() {
            "Ethernet (built in)"
        } else {
            "Virtual"
        };
        let mut detail = vec![format!("MAC {}", text(base.join("address")))];
        let up = text(base.join("carrier")) == "1";
        let speed = text(base.join("speed")).parse::<i64>().ok().filter(|s| *s > 0);
        detail.push(match (up, speed) {
            (true, Some(s)) => format!("cable in, {s} Mb/s"),
            (true, None) => "connected".into(),
            (false, _) => "no cable".into(),
        });
        if let Some(a) = addrs.get(name.as_str()) {
            detail.push(a.join(", "));
        }
        if let Some(driver) = std::fs::read_link(dev.join("driver")).ok().and_then(|p| p.file_name().map(|f| f.to_string_lossy().into_owned())) {
            detail.push(format!("driver {driver}"));
        }
        if bus.as_deref() == Some("usb") {
            // The interface's parent folder is the USB device itself.
            let usb = dev.canonicalize().ok().and_then(|p| p.parent().map(Path::to_path_buf));
            if let Some(u) = usb {
                detail.push(usb_name(&u));
            }
        }
        rows.push(row(name.clone(), format!("{role} · {kind}"), detail.join(" · ")));
    }
    for (iface, role) in [(&cfg.network.lan_iface, "Customer side"), (&cfg.network.wan_iface, "Internet side")] {
        if !names.contains(iface) {
            let why = if iface == "nowan0" { "test layout: no internet port" } else { "not plugged in or not found" };
            rows.push(row(iface.clone(), format!("{role} · missing"), why));
        }
    }
    Section { title: "Network ports", rows }
}

fn usb() -> Section {
    let mut rows = vec![];
    let mut devices: Vec<_> = std::fs::read_dir("/sys/bus/usb/devices")
        .map(|d| d.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    devices.sort();
    for d in devices {
        let name = d.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
        // Interfaces ("1-1:1.0") and the board's own hubs ("usb1", class 09) aren't devices plugged in.
        if name.contains(':') || name.starts_with("usb") || text(d.join("bDeviceClass")) == "09" {
            continue;
        }
        let speed = text(d.join("speed"));
        let product: String = text(d.join("product")).chars().take(60).collect();
        rows.push(row(
            if product.is_empty() { "USB device".to_string() } else { product },
            text(d.join("manufacturer")),
            format!("ID {}:{} · {} Mb/s · port {name}", text(d.join("idVendor")), text(d.join("idProduct")), speed),
        ));
    }
    if rows.is_empty() {
        rows.push(row("Nothing plugged in", "", ""));
    }
    Section { title: "USB devices", rows }
}

fn coin(cfg: &Config, f: &Facts) -> Section {
    let c = &cfg.coin;
    let width = match c.pulse_width_ms {
        Some([lo, hi]) => format!("pulses {lo}–{hi} ms"),
        None => "any pulse width".into(),
    };
    Section {
        title: "Coin slot",
        rows: vec![
            row(
                "Coin signal",
                format!("Line {} on {}", c.line, c.chip),
                format!(
                    "{} · debounce {} ms · {} ms gap ends a coin · {width}",
                    if c.active_low { "active low" } else { "active high" },
                    c.debounce_ms,
                    c.coin_gap_ms
                ),
            ),
            row("Reader", if f.reader_ok { "Working" } else { "Not reading: check the Activity log" }, ""),
            row("Each pulse", format!("₱{}", c.pesos_per_pulse), ""),
            row(
                "Acceptor switch",
                c.enable_line.map_or("Not wired".into(), |l| format!("Line {l}")),
                if c.enable_line.is_some() { "Turns the acceptor on only while someone is inserting" } else { "" },
            ),
            row("Time to insert", format!("{} seconds", c.insert_window_secs), "After tapping Insert coin"),
        ],
    }
}

/// On a laptop there's no board to read: the vendo it's modelled on.
fn sample() -> Vec<Section> {
    vec![
        Section {
            title: "Operating system",
            rows: vec![
                row("System", "Armbian 26.8.1 (Debian GNU/Linux 13 (trixie))", "current · minimal"),
                row("Kernel", "6.18.2-current-sunxi", "armv7l"),
                row("Host name", "orangepione", ""),
                row("Time zone", "Asia/Manila", ""),
                row("Clock", chrono::Local::now().format("%a %b %-d %Y, %-I:%M %p").to_string(), "Synced over the internet"),
                row("Board on for", duration(3 * 86400 + 4 * 3600), "Since the last power-on or restart"),
            ],
        },
        Section {
            title: "Hardware",
            rows: vec![
                row("Board", "Xunlong Orange Pi One", "xunlong,orangepi-one · allwinner,sun8i-h3"),
                row("Processor", "4 cores, up to 1.37 GHz", "Allwinner sun8i Family · ARMv7 Processor rev 5 (v7l)"),
                row("Memory", "991 MB", ""),
                row("Storage (mmcblk0)", "microSD card, 15.9 GB", "SD16G · made 05/2021"),
                row("Temperature", "47 °C", "CPU"),
            ],
        },
        Section {
            title: "Network ports",
            rows: vec![
                row("end0", "Customer side · Ethernet (built in)", "MAC 02:42:ac:11:00:02 · cable in, 100 Mb/s · 10.0.0.1/19 · driver dwmac-sun8i"),
                row("enx02e04c000001", "Internet side · USB Ethernet", "MAC 02:e0:4c:00:00:01 · cable in, 100 Mb/s · 192.168.1.20/24 · driver r8152 · Realtek USB 10/100 LAN (0bda:8152)"),
            ],
        },
        Section {
            title: "USB devices",
            rows: vec![row("USB 10/100 LAN", "Realtek", "ID 0bda:8152 · 480 Mb/s · port 1-1")],
        },
    ]
}

// ---- helpers ----------------------------------------------------------------

/// A small /proc or /sys file, trimmed ("" when missing). Device-tree
/// strings end in NUL, which is dropped.
fn text(p: impl AsRef<Path>) -> String {
    std::fs::read(p).map(|b| String::from_utf8_lossy(&b).trim().trim_end_matches('\0').trim().to_string()).unwrap_or_default()
}

/// `KEY=value` and `KEY="value"` lines (os-release style).
fn key_values(s: &str) -> HashMap<String, String> {
    s.lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim().to_string(), v.trim().trim_matches('"').to_string()))
        .collect()
}

/// "Realtek USB 10/100 LAN (0bda:8152)" from a USB device's folder.
fn usb_name(dir: &Path) -> String {
    let name = format!("{} {}", text(dir.join("manufacturer")), text(dir.join("product"))).trim().to_string();
    format!("{name} ({}:{})", text(dir.join("idVendor")), text(dir.join("idProduct")))
}

/// IPv4 addresses per interface, from `ip -j -4 addr`.
fn ipv4_addrs() -> HashMap<String, Vec<String>> {
    let Ok(out) = Command::new("ip").args(["-j", "-4", "addr", "show"]).output() else { return HashMap::new() };
    let list: Vec<serde_json::Value> = serde_json::from_slice(&out.stdout).unwrap_or_default();
    list.iter()
        .filter_map(|i| {
            let name = i["ifname"].as_str()?.to_string();
            let addrs = i["addr_info"].as_array()?.iter().filter_map(|a| Some(format!("{}/{}", a["local"].as_str()?, a["prefixlen"]))).collect();
            Some((name, addrs))
        })
        .collect()
}

/// The kernel's machine name, e.g. "armv7l".
fn machine() -> String {
    // SAFETY: a zeroed struct for the kernel to fill; the field it fills is NUL-terminated.
    unsafe {
        let mut u: libc::utsname = std::mem::zeroed();
        if libc::uname(&mut u) != 0 {
            return String::new();
        }
        std::ffi::CStr::from_ptr(u.machine.as_ptr()).to_string_lossy().into_owned()
    }
}

fn size(bytes: u64) -> String {
    format!("{:.1} GB", bytes as f64 / 1e9)
}

fn duration(secs: u64) -> String {
    let (d, h, m) = (secs / 86400, secs % 86400 / 3600, secs % 3600 / 60);
    match (d, h) {
        (0, 0) => format!("{m} min"),
        (0, _) => format!("{h} hr {m} min"),
        _ => format!("{d} days {h} hr"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_os_release_style_files() {
        let kv = key_values("NAME=\"Debian GNU/Linux\"\nVERSION_ID=\"13\"\nBOARD_NAME=\"Orange Pi One\"\n");
        assert_eq!(kv["NAME"], "Debian GNU/Linux");
        assert_eq!(kv["BOARD_NAME"], "Orange Pi One");
        assert_eq!(duration(3 * 86400 + 4 * 3600 + 59), "3 days 4 hr");
        assert_eq!(duration(3720), "1 hr 2 min");
    }

    #[test]
    fn a_laptop_gets_the_sample_with_real_software_facts() {
        let cfg = Config::dev(std::env::temp_dir());
        let f = Facts { schema: 7, cipher: "4.6.1 community".into(), uptime_secs: 90, reader_ok: true };
        let s = read(&cfg, true, &f);
        assert!(s.sample);
        let titles: Vec<_> = s.sections.iter().map(|s| s.title).collect();
        assert_eq!(titles, ["Software", "Operating system", "Hardware", "Network ports", "USB devices", "Coin slot"]);
        assert_eq!(s.sections[0].rows[0].value, env!("CARGO_PKG_VERSION"));
    }
}
