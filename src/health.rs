//! How the board itself is doing, for the dashboard: temperature, memory,
//! storage and the internet cable. Read straight from /proc and /sys.

use serde::Serialize;
use std::path::Path;

#[derive(Debug, Serialize, Default)]
pub struct Health {
    /// CPU temperature in °C.
    pub temp_c: Option<f32>,
    pub mem_used_pct: Option<u8>,
    pub mem_total_mb: Option<u64>,
    pub disk_used_pct: Option<u8>,
    pub disk_free_mb: Option<u64>,
    /// 1-minute load per CPU core, as a percentage.
    pub cpu_pct: Option<u8>,
    /// Since the board last booted.
    pub boot_secs: Option<u64>,
    /// The internet port has a cable and a link (None: no such port).
    pub wan_up: Option<bool>,
}

pub fn read(data_dir: &Path, wan_iface: &str, dev: bool) -> Health {
    if dev {
        return simulated();
    }
    let (mem_total_mb, mem_used_pct) = memory().unzip();
    let (disk_free_mb, disk_used_pct) = disk(data_dir).unzip();
    Health {
        temp_c: temperature(),
        mem_used_pct,
        mem_total_mb,
        disk_used_pct,
        disk_free_mb,
        cpu_pct: cpu(),
        boot_secs: std::fs::read_to_string("/proc/uptime").ok().and_then(|s| s.split('.').next()?.parse().ok()),
        wan_up: wan(wan_iface),
    }
}

/// A laptop isn't the board: plausible numbers that drift a little.
fn simulated() -> Health {
    let t = crate::db::now();
    let wobble = |period: i64, span: f32| ((t % period) as f32 / period as f32 * std::f32::consts::TAU).sin() * span;
    Health {
        temp_c: Some(47.0 + wobble(300, 4.0)),
        mem_used_pct: Some((31.0 + wobble(240, 3.0)) as u8),
        mem_total_mb: Some(991),
        disk_used_pct: Some(18),
        disk_free_mb: Some(11_840),
        cpu_pct: Some((12.0 + wobble(60, 8.0)).max(1.0) as u8),
        boot_secs: Some(3 * 86400 + 4 * 3600),
        wan_up: Some(true),
    }
}

fn temperature() -> Option<f32> {
    let raw = std::fs::read_to_string("/sys/class/thermal/thermal_zone0/temp").ok()?;
    let milli: f32 = raw.trim().parse().ok()?;
    Some(milli / 1000.0)
}

/// (total MB, % used) from /proc/meminfo.
fn memory() -> Option<(u64, u8)> {
    let info = std::fs::read_to_string("/proc/meminfo").ok()?;
    let field = |name: &str| -> Option<u64> {
        let line = info.lines().find(|l| l.starts_with(name))?;
        line.split_whitespace().nth(1)?.parse().ok()
    };
    let (total, avail) = (field("MemTotal:")?, field("MemAvailable:")?);
    (total > 0).then(|| (total / 1024, (100 * total.saturating_sub(avail) / total) as u8))
}

/// (free MB, % used) of the filesystem holding the database.
fn disk(dir: &Path) -> Option<(u64, u8)> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(dir.as_os_str().as_bytes()).ok()?;
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: a valid C string and a zeroed struct for the kernel to fill.
    if unsafe { libc::statvfs(path.as_ptr(), &mut s) } != 0 {
        return None;
    }
    let frag = s.f_frsize as u64;
    let (total, free, avail) = (s.f_blocks as u64 * frag, s.f_bfree as u64 * frag, s.f_bavail as u64 * frag);
    (total > 0).then(|| (avail / (1024 * 1024), (100 * total.saturating_sub(free) / total) as u8))
}

fn cpu() -> Option<u8> {
    let load: f32 = std::fs::read_to_string("/proc/loadavg").ok()?.split_whitespace().next()?.parse().ok()?;
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1) as f32;
    Some((100.0 * load / cores).clamp(0.0, 100.0) as u8)
}

fn wan(iface: &str) -> Option<bool> {
    if iface.is_empty() || iface.contains('/') || iface.contains("..") {
        return None;
    }
    let base = Path::new("/sys/class/net").join(iface);
    if !base.exists() {
        return None;
    }
    let carrier = std::fs::read_to_string(base.join("carrier")).map(|s| s.trim() == "1").unwrap_or(false);
    Some(carrier)
}
