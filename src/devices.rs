//! Telling customers' devices apart for the owner: "Galaxy A53 5G" or
//! "Juans iPhone" instead of a MAC address.
//!
//! Everything detected here is what the device says about itself (its
//! browser's User-Agent and the name it gave the DHCP server), so it's for
//! display only and never decides who gets time. The owner's own nickname
//! for a device always wins.

use crate::db::{now, Db, DeviceRow};
use crate::net::Mac;
use anyhow::Result;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Mutex;

/// "Last seen" is written to the database at most this often per device
/// (in seconds), to spare the SD card.
const TOUCH_EVERY: i64 = 3600;
/// Longest User-Agent kept.
const MAX_UA: usize = 400;
/// Devices with no nickname that haven't been back for this long are forgotten.
const FORGET_AFTER: i64 = 365 * 86400;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Phone,
    Tablet,
    Computer,
    Tv,
    #[default]
    Unknown,
}

/// What a User-Agent tells us.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Guess {
    pub brand: Option<&'static str>,
    pub model: Option<String>,
    pub os: Option<String>,
    pub browser: Option<&'static str>,
    pub kind: Kind,
}

/// A device as the dashboard shows it.
#[derive(Debug, Clone, Serialize)]
pub struct DeviceInfo {
    /// Best name: the owner's nickname, the device's own name, its model,
    /// what kind of device it is, or failing all that "Device …A1B2C3".
    pub name: String,
    /// Model, system and browser, as far as we can tell.
    pub detail: String,
    pub kind: Kind,
    pub brand: Option<&'static str>,
    pub model: Option<String>,
    pub os: Option<String>,
    pub browser: Option<&'static str>,
    pub nickname: Option<String>,
    pub hostname: Option<String>,
    /// The phone uses a made-up address for this WiFi (most phones do).
    pub private_mac: bool,
    pub first_seen: Option<i64>,
    pub last_seen: Option<i64>,
}

#[derive(Clone, Default)]
struct Known {
    nickname: String,
    hostname: String,
    ua: String,
    first_seen: i64,
    last_seen: i64,
    /// `last_seen` as the database has it.
    saved_seen: i64,
}

impl Known {
    fn row(&self, mac: Mac) -> DeviceRow {
        DeviceRow {
            mac,
            nickname: self.nickname.clone(),
            hostname: self.hostname.clone(),
            user_agent: self.ua.clone(),
            first_seen: self.first_seen,
            last_seen: self.last_seen,
        }
    }
}

pub struct DeviceBook {
    known: Mutex<HashMap<Mac, Known>>,
}

impl DeviceBook {
    pub fn load(db: &Db) -> Result<DeviceBook> {
        db.forget_devices(now() - FORGET_AFTER)?;
        let known = db
            .load_devices()?
            .into_iter()
            .map(|r| {
                let k = Known {
                    nickname: r.nickname,
                    hostname: r.hostname,
                    ua: r.user_agent,
                    first_seen: r.first_seen,
                    last_seen: r.last_seen,
                    saved_seen: r.last_seen,
                };
                (r.mac, k)
            })
            .collect();
        Ok(DeviceBook { known: Mutex::new(known) })
    }

    /// A customer's browser checked in. Saved when something changed, or
    /// at most hourly for "last seen".
    pub fn saw_browser(&self, db: &Db, mac: Mac, ua: &str) {
        let ua = clean_ua(ua);
        let t = now();
        let row = {
            let mut known = self.known.lock().unwrap();
            let k = known.entry(mac).or_insert_with(|| Known { first_seen: t, ..Default::default() });
            k.last_seen = t;
            let changed = !ua.is_empty() && k.ua != ua;
            if changed {
                k.ua = ua;
            }
            if !changed && k.saved_seen != 0 && t - k.saved_seen < TOUCH_EVERY {
                return;
            }
            k.saved_seen = t;
            k.row(mac)
        };
        if let Err(e) = db.save_device(&row) {
            tracing::error!("saving device: {e:#}");
        }
    }

    /// Names devices gave the DHCP server, from the lease file.
    pub fn saw_hostnames<'a>(&self, db: &Db, names: impl IntoIterator<Item = (Mac, &'a str)>) {
        let t = now();
        let mut rows = vec![];
        {
            let mut known = self.known.lock().unwrap();
            for (mac, name) in names {
                let name = crate::brand::clean_text(name, 40);
                let k = known.entry(mac).or_insert_with(|| Known { first_seen: t, last_seen: t, ..Default::default() });
                if !name.is_empty() && k.hostname != name {
                    k.hostname = name;
                    k.saved_seen = k.last_seen;
                    rows.push(k.row(mac));
                }
            }
        }
        for row in rows {
            if let Err(e) = db.save_device(&row) {
                tracing::error!("saving device: {e:#}");
            }
        }
    }

    /// The owner's name for a device; empty clears it.
    pub fn set_nickname(&self, db: &Db, mac: Mac, name: &str) -> Result<()> {
        let t = now();
        let row = {
            let mut known = self.known.lock().unwrap();
            let k = known.entry(mac).or_insert_with(|| Known { first_seen: t, last_seen: t, ..Default::default() });
            k.nickname = name.to_string();
            k.saved_seen = k.last_seen;
            k.row(mac)
        };
        db.save_device(&row)
    }

    /// What the customer's own phone calls itself or its model, for showing
    /// to that customer. Never the owner's nickname for it.
    pub fn own_name(&self, mac: Mac) -> Option<String> {
        let k = self.known.lock().unwrap().get(&mac).cloned()?;
        let g = parse_ua(&k.ua);
        pretty_hostname(&k.hostname).or_else(|| g.model.as_deref().map(|m| display_model(g.brand, m)))
    }

    pub fn info(&self, mac: Mac) -> DeviceInfo {
        let k = self.known.lock().unwrap().get(&mac).cloned();
        describe(mac, k.as_ref())
    }

    /// Every device this vendo has seen.
    pub fn all(&self) -> Vec<(Mac, DeviceInfo)> {
        let known = self.known.lock().unwrap().clone();
        known.iter().map(|(m, k)| (*m, describe(*m, Some(k)))).collect()
    }
}

fn clean_ua(ua: &str) -> String {
    ua.chars().filter(|c| !c.is_control()).take(MAX_UA).collect::<String>().trim().to_string()
}

fn describe(mac: Mac, k: Option<&Known>) -> DeviceInfo {
    let g = k.map(|k| parse_ua(&k.ua)).unwrap_or_default();
    let nickname = k.map(|k| k.nickname.clone()).filter(|n| !n.is_empty());
    let hostname = k.map(|k| k.hostname.clone()).filter(|n| !n.is_empty());
    let own_name = hostname.as_deref().and_then(pretty_hostname);
    let model = g.model.as_deref().map(|m| display_model(g.brand, m));
    let name = nickname
        .clone()
        .or_else(|| own_name.clone())
        .or_else(|| model.clone())
        .or_else(|| kind_word(&g).map(String::from))
        .unwrap_or_else(|| mac.masked());
    let detail = [model.clone().filter(|m| *m != name), g.os.clone(), g.browser.map(String::from)]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · ");
    DeviceInfo {
        name,
        detail,
        kind: if g.kind == Kind::Unknown && own_name.as_deref().is_some_and(|n| n.contains("iPhone")) { Kind::Phone } else { g.kind },
        brand: g.brand,
        model,
        os: g.os,
        browser: g.browser,
        nickname,
        hostname,
        private_mac: mac.0[0] & 0x02 != 0,
        first_seen: k.map(|k| k.first_seen).filter(|t| *t > 0),
        last_seen: k.map(|k| k.last_seen).filter(|t| *t > 0),
    }
}

/// "Galaxy-A53-5G" → "Galaxy A53 5G". Made-up names ("android-6f1c2b…",
/// Windows' "DESKTOP-K3J9Q2") say nothing, so they're skipped.
fn pretty_hostname(h: &str) -> Option<String> {
    let lower = h.to_ascii_lowercase();
    let generic = ["localhost", "unknown", "iphone", "ipad", "android", "*"].contains(&lower.as_str())
        || ["android-", "android_", "desktop-", "laptop-", "esp_", "esp-"].iter().any(|p| lower.starts_with(p));
    if generic {
        return None;
    }
    let s = h.replace(['-', '_'], " ");
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    (!s.is_empty()).then_some(s)
}

/// Words we can always put a brand in front of without repeating it.
const FAMILIES: &[&str] = &["Galaxy", "Redmi", "POCO", "Mi ", "Pixel", "iPhone", "iPad", "Mac", "Chromebook", "moto", "Nokia", "Xiaomi"];

fn display_model(brand: Option<&str>, model: &str) -> String {
    match brand {
        Some(b) if !model.to_ascii_lowercase().starts_with(&b.to_ascii_lowercase()) && !FAMILIES.iter().any(|f| model.starts_with(f)) => {
            format!("{b} {model}")
        }
        _ => model.to_string(),
    }
}

fn kind_word(g: &Guess) -> Option<&'static str> {
    let os = g.os.as_deref().unwrap_or("");
    Some(match g.kind {
        Kind::Phone if os.starts_with("Android") => "Android phone",
        Kind::Phone => "Phone",
        Kind::Tablet if os.starts_with("Android") => "Android tablet",
        Kind::Tablet => "Tablet",
        Kind::Computer if os.starts_with("Windows") => "Windows computer",
        Kind::Computer if os.starts_with("Linux") => "Linux computer",
        Kind::Computer => "Computer",
        Kind::Tv => "Smart TV",
        Kind::Unknown => return None,
    })
}

// ---- User-Agent ---------------------------------------------------------

pub fn parse_ua(ua: &str) -> Guess {
    let mut g = Guess { browser: browser(ua), ..Default::default() };
    // The first (...) holds the platform: "Linux; Android 14; SM-A536E Build/UP1A; wv".
    // Models can have brackets of their own: "moto g(30)".
    let inner = ua.split_once('(').map(|(_, rest)| {
        let mut depth = 0;
        let end = rest
            .char_indices()
            .find(|&(_, c)| {
                depth += match c { '(' => 1, ')' => -1, _ => 0 };
                depth < 0
            })
            .map_or(rest.len(), |(i, _)| i);
        &rest[..end]
    });
    let parts: Vec<&str> = inner.unwrap_or("").split(';').map(str::trim).collect();
    if ua.contains("iPhone") || ua.contains("iPod") {
        g.kind = Kind::Phone;
        g.brand = Some("Apple");
        g.model = Some("iPhone".into());
        g.os = Some(apple_os("iOS", ua));
    } else if ua.contains("iPad") {
        g.kind = Kind::Tablet;
        g.brand = Some("Apple");
        g.model = Some("iPad".into());
        g.os = Some(apple_os("iPadOS", ua));
    } else if let Some(i) = parts.iter().position(|p| p.starts_with("Android")) {
        let version = parts[i]["Android".len()..].trim();
        g.os = Some(if version.is_empty() { "Android".into() } else { format!("Android {version}") });
        g.kind = if ua.contains("Mobile") { Kind::Phone } else { Kind::Tablet };
        if let Some(m) = parts[i + 1..].iter().map(|p| strip_build(p)).find(|p| plausible_model(p)) {
            let (brand, model) = android_model(m);
            g.brand = brand;
            g.model = Some(model);
        }
    } else if ua.contains("Windows NT") {
        g.kind = Kind::Computer;
        g.os = Some("Windows".into());
    } else if ua.contains("CrOS") {
        g.kind = Kind::Computer;
        g.model = Some("Chromebook".into());
        g.os = Some("ChromeOS".into());
    } else if ua.contains("Macintosh") {
        g.kind = Kind::Computer;
        g.brand = Some("Apple");
        g.model = Some("Mac".into());
        g.os = Some("macOS".into());
    } else if ua.contains("Linux") {
        g.kind = Kind::Computer;
        g.os = Some("Linux".into());
    }
    if ["SmartTV", "SMART-TV", "Tizen", "Web0S", "WebOS", "BRAVIA", "Android TV", "GoogleTV", "AFTM", "AFTS", "AFTK"]
        .iter()
        .any(|m| ua.contains(m))
    {
        g.kind = Kind::Tv;
    }
    g
}

/// "iOS 17.5" from "CPU iPhone OS 17_5 like Mac OS X".
fn apple_os(name: &str, ua: &str) -> String {
    let version = ua.split_once(" OS ").map(|(_, r)| r.split(' ').next().unwrap_or("")).unwrap_or("");
    let parts: Vec<&str> = version.split('_').filter(|p| p.chars().all(|c| c.is_ascii_digit()) && !p.is_empty()).take(2).collect();
    if parts.is_empty() {
        name.to_string()
    } else {
        format!("{name} {}", parts.join("."))
    }
}

fn strip_build(p: &str) -> &str {
    let p = p.split(" Build/").next().unwrap_or(p).trim();
    let p = if p.starts_with("Build/") { "" } else { p };
    p.strip_prefix("SAMSUNG ").or_else(|| p.strip_prefix("samsung ")).unwrap_or(p).trim()
}

fn plausible_model(p: &str) -> bool {
    let locale = p.len() == 5 && matches!(p.as_bytes()[2], b'-' | b'_');
    !p.is_empty()
        && p.len() <= 40
        && !locale
        && !["K", "U", "wv", "Mobile", "Linux"].contains(&p)
        && !p.starts_with("Android")
        && !p.starts_with("rv:")
}

/// Brand from the model code, and the Samsung marketing name when we know it.
fn android_model(m: &str) -> (Option<&'static str>, String) {
    const PREFIXES: &[(&str, &str, bool)] = &[
        // (prefix, brand, strip the prefix from the model)
        ("HUAWEI ", "Huawei", true),
        ("HONOR ", "Honor", true),
        ("Honor ", "Honor", true),
        ("OPPO ", "OPPO", true),
        ("vivo ", "vivo", true),
        ("Infinix ", "Infinix", true),
        ("TECNO ", "Tecno", true),
        ("itel ", "itel", true),
        ("realme ", "realme", true),
        ("Lenovo ", "Lenovo", true),
        ("TCL ", "TCL", true),
        ("Cherry ", "Cherry Mobile", true),
        ("MYPHONE ", "MyPhone", true),
        ("myPhone ", "MyPhone", true),
        ("Nokia", "Nokia", false),
        ("Redmi", "Xiaomi", false),
        ("POCO", "Xiaomi", false),
        ("Mi ", "Xiaomi", false),
        ("Xiaomi", "Xiaomi", false),
        ("Pixel", "Google", false),
        ("moto", "Motorola", false),
        ("motorola", "Motorola", false),
        ("CPH", "OPPO", false),
        ("RMX", "realme", false),
        ("LM-", "LG", false),
        ("ASUS", "ASUS", false),
        ("SM-", "Samsung", false),
        ("GT-", "Samsung", false),
    ];
    for (prefix, brand, strip) in PREFIXES {
        if let Some(rest) = m.strip_prefix(prefix) {
            let model = if *strip { rest.trim() } else { m };
            if *brand == "Samsung" {
                if let Some(name) = samsung_name(m) {
                    return (Some(brand), name.to_string());
                }
            }
            return (Some(brand), model.to_string());
        }
    }
    let b = m.as_bytes();
    // vivo: V2111, V2250A. Xiaomi: M2101K6G, 2201117TG, 23021RAA2Y.
    if b.len() >= 5 && b[0] == b'V' && b[1..5].iter().all(u8::is_ascii_digit) {
        return (Some("vivo"), m.to_string());
    }
    let xiaomi_code = (b.len() >= 8 && b[..4].iter().all(u8::is_ascii_digit) && b.iter().all(u8::is_ascii_alphanumeric))
        || (b.len() >= 8 && b[0] == b'M' && b[1..5].iter().all(u8::is_ascii_digit));
    if xiaomi_code {
        return (Some("Xiaomi"), m.to_string());
    }
    // Huawei: JNY-LX2, MAR-LX1M.
    if b.len() >= 7 && b[..3].iter().all(u8::is_ascii_uppercase) && b[3] == b'-' && matches!(b[4], b'L' | b'A') {
        return (Some("Huawei"), m.to_string());
    }
    (None, m.to_string())
}

/// Samsung's model codes for phones popular in the Philippines.
fn samsung_name(code: &str) -> Option<&'static str> {
    const NAMES: &[(&str, &str)] = &[
        ("SM-A013", "Galaxy A01 Core"), ("SM-A015", "Galaxy A01"), ("SM-A022", "Galaxy A02"), ("SM-A025", "Galaxy A02s"),
        ("SM-A032", "Galaxy A03 Core"), ("SM-A035", "Galaxy A03"), ("SM-A037", "Galaxy A03s"), ("SM-A042", "Galaxy A04e"),
        ("SM-A045", "Galaxy A04"), ("SM-A047", "Galaxy A04s"), ("SM-A055", "Galaxy A05"), ("SM-A057", "Galaxy A05s"),
        ("SM-A065", "Galaxy A06"), ("SM-A105", "Galaxy A10"), ("SM-A107", "Galaxy A10s"), ("SM-A125", "Galaxy A12"),
        ("SM-A127", "Galaxy A12"), ("SM-A135", "Galaxy A13"), ("SM-A137", "Galaxy A13"), ("SM-A145", "Galaxy A14"),
        ("SM-A146", "Galaxy A14 5G"), ("SM-A155", "Galaxy A15"), ("SM-A156", "Galaxy A15 5G"), ("SM-A165", "Galaxy A16"),
        ("SM-A166", "Galaxy A16 5G"), ("SM-A205", "Galaxy A20"), ("SM-A207", "Galaxy A20s"), ("SM-A235", "Galaxy A23"),
        ("SM-A236", "Galaxy A23 5G"), ("SM-A245", "Galaxy A24"), ("SM-A256", "Galaxy A25 5G"), ("SM-A266", "Galaxy A26 5G"),
        ("SM-A305", "Galaxy A30"), ("SM-A307", "Galaxy A30s"), ("SM-A325", "Galaxy A32"), ("SM-A326", "Galaxy A32 5G"),
        ("SM-A336", "Galaxy A33 5G"), ("SM-A346", "Galaxy A34 5G"), ("SM-A356", "Galaxy A35 5G"), ("SM-A366", "Galaxy A36 5G"),
        ("SM-A505", "Galaxy A50"), ("SM-A507", "Galaxy A50s"), ("SM-A515", "Galaxy A51"), ("SM-A525", "Galaxy A52"),
        ("SM-A526", "Galaxy A52 5G"), ("SM-A528", "Galaxy A52s 5G"), ("SM-A536", "Galaxy A53 5G"), ("SM-A546", "Galaxy A54 5G"),
        ("SM-A556", "Galaxy A55 5G"), ("SM-A566", "Galaxy A56 5G"), ("SM-A715", "Galaxy A71"), ("SM-A725", "Galaxy A72"),
        ("SM-A736", "Galaxy A73 5G"), ("SM-G780", "Galaxy S20 FE"), ("SM-G981", "Galaxy S20"), ("SM-G990", "Galaxy S21 FE"),
        ("SM-G991", "Galaxy S21"), ("SM-G996", "Galaxy S21+"), ("SM-G998", "Galaxy S21 Ultra"), ("SM-S901", "Galaxy S22"),
        ("SM-S906", "Galaxy S22+"), ("SM-S908", "Galaxy S22 Ultra"), ("SM-S711", "Galaxy S23 FE"), ("SM-S911", "Galaxy S23"),
        ("SM-S916", "Galaxy S23+"), ("SM-S918", "Galaxy S23 Ultra"), ("SM-S721", "Galaxy S24 FE"), ("SM-S921", "Galaxy S24"),
        ("SM-S926", "Galaxy S24+"), ("SM-S928", "Galaxy S24 Ultra"), ("SM-S931", "Galaxy S25"), ("SM-S936", "Galaxy S25+"),
        ("SM-S938", "Galaxy S25 Ultra"), ("SM-F711", "Galaxy Z Flip3"), ("SM-F721", "Galaxy Z Flip4"), ("SM-F731", "Galaxy Z Flip5"),
        ("SM-F741", "Galaxy Z Flip6"), ("SM-F926", "Galaxy Z Fold3"), ("SM-F936", "Galaxy Z Fold4"), ("SM-F946", "Galaxy Z Fold5"),
        ("SM-F956", "Galaxy Z Fold6"), ("SM-N970", "Galaxy Note10"), ("SM-N975", "Galaxy Note10+"), ("SM-N980", "Galaxy Note20"),
        ("SM-N981", "Galaxy Note20"), ("SM-N985", "Galaxy Note20 Ultra"), ("SM-N986", "Galaxy Note20 Ultra"), ("SM-T220", "Galaxy Tab A7 Lite"),
        ("SM-T225", "Galaxy Tab A7 Lite"), ("SM-T500", "Galaxy Tab A7"), ("SM-T505", "Galaxy Tab A7"), ("SM-X200", "Galaxy Tab A8"),
        ("SM-X205", "Galaxy Tab A8"), ("SM-X110", "Galaxy Tab A9"), ("SM-X115", "Galaxy Tab A9"), ("SM-X210", "Galaxy Tab A9+"),
        ("SM-X215", "Galaxy Tab A9+"), ("SM-X216", "Galaxy Tab A9+"),
    ];
    let key = code.get(..7)?;
    NAMES.iter().find(|(c, _)| *c == key).map(|(_, n)| *n)
}

fn browser(ua: &str) -> Option<&'static str> {
    // In-app browsers first: they also say "Chrome" or "Safari".
    const MARKERS: &[(&str, &str)] = &[
        ("FBAN/Messenger", "Messenger"),
        ("MessengerForiOS", "Messenger"),
        ("Orca-Android", "Messenger"),
        ("FB_IAB", "Facebook"),
        ("FBAN", "Facebook"),
        ("FBAV", "Facebook"),
        ("Instagram", "Instagram"),
        ("musical_ly", "TikTok"),
        ("BytedanceWebview", "TikTok"),
        ("TikTok", "TikTok"),
        ("Line/", "LINE"),
        ("Viber", "Viber"),
        ("Telegram", "Telegram"),
        ("GSA/", "Google app"),
        ("SamsungBrowser", "Samsung Internet"),
        ("MiuiBrowser", "Mi Browser"),
        ("HeyTapBrowser", "HeyTap Browser"),
        ("VivoBrowser", "vivo Browser"),
        ("UCBrowser", "UC Browser"),
        ("EdgA", "Edge"),
        ("EdgiOS", "Edge"),
        ("Edg/", "Edge"),
        ("OPR/", "Opera"),
        ("Opera", "Opera"),
        ("FxiOS", "Firefox"),
        ("Firefox/", "Firefox"),
        ("CriOS", "Chrome"),
        // Android's own WebView, which the "Sign in to WiFi" sheet uses.
        ("; wv)", "In-app browser"),
        ("Chrome/", "Chrome"),
    ];
    if let Some((_, name)) = MARKERS.iter().find(|(m, _)| ua.contains(m)) {
        return Some(name);
    }
    if ua.contains("Safari/") && ua.contains("Version/") {
        return Some("Safari");
    }
    // iOS's captive-portal sheet: WebKit with no browser name.
    if ua.contains("AppleWebKit") && ua.contains("Mobile/") {
        return Some("In-app browser");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name_of(ua: &str, hostname: &str, nickname: &str) -> DeviceInfo {
        let k = Known { ua: ua.into(), hostname: hostname.into(), nickname: nickname.into(), first_seen: 1, last_seen: 2, saved_seen: 2 };
        describe(Mac::parse("da:a1:19:00:11:22").unwrap(), Some(&k))
    }

    #[test]
    fn samsung_in_the_wifi_sign_in_sheet() {
        let ua = "Mozilla/5.0 (Linux; Android 14; SM-A536E Build/UP1A.231005.007; wv) AppleWebKit/537.36 (KHTML, like Gecko) Version/4.0 Chrome/128.0.6613.146 Mobile Safari/537.36";
        let g = parse_ua(ua);
        assert_eq!(g.brand, Some("Samsung"));
        assert_eq!(g.model.as_deref(), Some("Galaxy A53 5G"));
        assert_eq!(g.os.as_deref(), Some("Android 14"));
        assert_eq!(g.browser, Some("In-app browser"));
        assert_eq!(g.kind, Kind::Phone);
        let d = name_of(ua, "", "");
        assert_eq!(d.name, "Galaxy A53 5G");
        assert_eq!(d.detail, "Android 14 · In-app browser");
        assert!(d.private_mac);
    }

    #[test]
    fn reduced_chrome_user_agent_still_says_android() {
        let ua = "Mozilla/5.0 (Linux; Android 10; K) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/129.0.0.0 Mobile Safari/537.36";
        let d = name_of(ua, "", "");
        assert_eq!(d.model, None);
        assert_eq!(d.name, "Android phone");
        assert_eq!(d.detail, "Android 10 · Chrome");
    }

    #[test]
    fn brands_from_model_codes() {
        let model = |m: &str| {
            let g = parse_ua(&format!("Mozilla/5.0 (Linux; Android 13; {m}) AppleWebKit/537.36 Chrome/120 Mobile Safari/537.36"));
            display_model(g.brand, g.model.as_deref().unwrap())
        };
        assert_eq!(model("CPH2591"), "OPPO CPH2591");
        assert_eq!(model("RMX3710"), "realme RMX3710");
        assert_eq!(model("V2250"), "vivo V2250");
        assert_eq!(model("Infinix X6831"), "Infinix X6831");
        assert_eq!(model("TECNO KJ5"), "Tecno KJ5");
        assert_eq!(model("Redmi Note 13"), "Redmi Note 13");
        assert_eq!(model("23021RAA2Y"), "Xiaomi 23021RAA2Y");
        assert_eq!(model("SM-X999B"), "Samsung SM-X999B");
        assert_eq!(model("JNY-LX2"), "Huawei JNY-LX2");
        assert_eq!(model("moto g(30)"), "moto g(30)");
        // Old UAs put a locale before the model.
        let g = parse_ua("Mozilla/5.0 (Linux; U; Android 4.4.2; en-us; GT-I9505 Build/KOT49H) AppleWebKit/534.30 Version/4.0 Mobile Safari/534.30");
        assert_eq!(g.model.as_deref(), Some("GT-I9505"));
        // Samsung Internet's own UA.
        let g = parse_ua("Mozilla/5.0 (Linux; Android 13; SAMSUNG SM-A146P) AppleWebKit/537.36 (KHTML, like Gecko) SamsungBrowser/23.0 Chrome/115.0 Mobile Safari/537.36");
        assert_eq!((g.model.as_deref(), g.browser), (Some("Galaxy A14 5G"), Some("Samsung Internet")));
    }

    #[test]
    fn iphones_and_computers() {
        let ios = "Mozilla/5.0 (iPhone; CPU iPhone OS 17_5_1 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.5 Mobile/15E148 Safari/604.1";
        let d = name_of(ios, "Juans-iPhone", "");
        assert_eq!(d.name, "Juans iPhone");
        assert_eq!(d.detail, "iPhone · iOS 17.5 · Safari");
        let sheet = "Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Mobile/15E148";
        assert_eq!(parse_ua(sheet).browser, Some("In-app browser"));
        let fb = "Mozilla/5.0 (iPhone; CPU iPhone OS 17_1 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Mobile/15E148 [FBAN/FBIOS;FBAV/440.0]";
        assert_eq!(parse_ua(fb).browser, Some("Facebook"));
        let win = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/129.0 Safari/537.36 Edg/129.0";
        let d = name_of(win, "DESKTOP-K3J9Q2", "");
        assert_eq!((d.name.as_str(), d.detail.as_str()), ("Windows computer", "Windows · Edge"));
        let mac = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.0 Safari/605.1.15";
        assert_eq!(name_of(mac, "", "").name, "Mac");
    }

    #[test]
    fn nickname_beats_everything_and_unknowns_fall_back_to_the_address() {
        let ua = "Mozilla/5.0 (Linux; Android 14; SM-A156E) AppleWebKit/537.36 Chrome/128 Mobile Safari/537.36";
        let d = name_of(ua, "Galaxy-A15-5G", "Aling Nena");
        assert_eq!(d.name, "Aling Nena");
        assert_eq!(d.detail, "Galaxy A15 5G · Android 14 · Chrome");
        let d = describe(Mac::parse("a4:5e:60:11:22:33").unwrap(), None);
        assert_eq!(d.name, "Device …112233");
        assert!(!d.private_mac);
        assert_eq!(pretty_hostname("android-6f1c2b3a4d5e"), None);
    }

    #[test]
    fn smart_tvs() {
        let tv = "Mozilla/5.0 (Linux; Tizen 6.0) AppleWebKit/537.36 (KHTML, like Gecko) SamsungBrowser/4.0 Chrome/76 TV Safari/537.36 SmartTV";
        assert_eq!(name_of(tv, "", "").name, "Smart TV");
    }
}
