//! The owner's customisation of the customer page: name, accent
//! colour, logo (and its home-screen icon), an advert poster, sound effects
//! and the page customers land on once online. Images live in the encrypted DB and
//! are cached in RAM so customers' phones don't trigger a decrypt each.

use crate::db::Db;
use anyhow::Result;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

/// "icon" is the logo on a white square, made by the dashboard when the
/// logo is uploaded (this box has no image decoders).
pub const ASSETS: &[&str] = &["logo", "poster", "icon"];
pub const MAX_ASSET_BYTES: usize = 1536 * 1024;
pub const DEFAULT_ACCENT: &str = "#5A43E6";
/// The big coin button (and small highlights), until the owner picks another.
pub const DEFAULT_BUTTON: &str = "#D4F04C";
/// Where customers land once online, until the owner picks another page (or none).
pub const DEFAULT_AFTER_URL: &str = "https://www.google.com/search?q=cats";
/// Philippine coins a coin slot can be set to take.
pub const COINS: [u32; 4] = [1, 5, 10, 20];
/// Looks the customer page can wear: the flat purple-and-lime one, or
/// frosted glass over soft colour. The first is the default.
pub const THEMES: [&str; 2] = ["classic", "glass"];

#[derive(Clone, Serialize)]
pub struct Brand {
    pub name: String,
    pub accent: String,
    /// Colour of the big coin button, `#RRGGBB`.
    pub button: String,
    pub wifi_name: String,
    pub logo: Option<String>,
    pub icon: Option<String>,
    pub poster: Option<Poster>,
    pub sounds: bool,
    /// Empty: customers stay on the portal.
    pub after_url: String,
    /// A short message for customers at the top of the page (empty: none).
    pub notice: String,
    /// Font for the name, a key of [`crate::fonts::FONTS`] ("": the phone's own).
    pub name_font: String,
    /// Thickness of the name, within that font's range.
    pub name_weight: u16,
    /// The font's size adjustment, in percent.
    pub name_size: u16,
    /// Coins this machine takes, smallest first (a non-empty subset of [`COINS`]).
    pub coins: Vec<u32>,
    /// One of [`THEMES`].
    pub theme: String,
}

#[derive(Clone, Serialize)]
pub struct Poster {
    pub url: String,
    pub alt: String,
}

pub struct Asset {
    pub mime: &'static str,
    pub data: bytes::Bytes,
    pub version: i64,
}

pub struct BrandStore {
    brand: RwLock<Brand>,
    assets: RwLock<HashMap<&'static str, Arc<Asset>>>,
}

/// Identifies an image by its magic bytes; anything else is refused.
pub fn sniff(data: &[u8]) -> Option<&'static str> {
    if data.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if data.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        Some("image/png")
    } else if data.len() > 12 && &data[0..4] == b"RIFF" && &data[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

pub fn clean_text(s: &str, max: usize) -> String {
    s.trim().chars().filter(|c| !c.is_control()).take(max).collect()
}

pub fn valid_accent(s: &str) -> bool {
    s.len() == 7 && s.starts_with('#') && s[1..].chars().all(|c| c.is_ascii_hexdigit())
}

/// The page customers land on once online, tidied: "google.com" becomes
/// "https://google.com". Only plain web pages; empty means none.
pub fn clean_after_url(s: &str) -> Option<String> {
    let s = s.trim();
    if s.is_empty() {
        return Some(String::new());
    }
    let url = if s.contains("://") { s.to_string() } else { format!("https://{s}") };
    let lower = url.to_ascii_lowercase();
    let rest = lower.strip_prefix("https://").or_else(|| lower.strip_prefix("http://"))?;
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    let (name, port) = host.split_once(':').unwrap_or((host, "0"));
    let ok = url.len() <= 300
        && !name.is_empty()
        && name.chars().all(|c| c.is_ascii_alphanumeric() || ".-".contains(c))
        && !port.is_empty()
        && port.chars().all(|c| c.is_ascii_digit())
        && !url.chars().any(|c| c.is_whitespace() || c.is_control() || matches!(c, '"' | '<' | '>' | '\\'));
    ok.then_some(url)
}

/// The owner's pick of coins, sorted and without repeats; `None` if it is
/// empty or names a coin that isn't one of [`COINS`].
pub fn clean_coins(coins: &[u32]) -> Option<Vec<u32>> {
    let list: Vec<u32> = COINS.iter().copied().filter(|c| coins.contains(c)).collect();
    (!list.is_empty() && coins.iter().all(|c| COINS.contains(c))).then_some(list)
}

/// The saved setting ("5,10"); all coins when unset or unreadable.
fn coins_of(setting: Option<&str>) -> Vec<u32> {
    let parsed: Option<Vec<u32>> = setting.map(|s| s.split(',').filter_map(|c| c.trim().parse().ok()).collect());
    parsed.and_then(|p| clean_coins(&p)).unwrap_or_else(|| COINS.to_vec())
}

/// The saved theme; the default when unset or unknown.
pub fn theme_of(setting: Option<&str>) -> &'static str {
    THEMES.iter().copied().find(|t| Some(*t) == setting).unwrap_or(THEMES[0])
}

fn asset_name(name: &str) -> Option<&'static str> {
    ASSETS.iter().copied().find(|n| *n == name)
}

impl BrandStore {
    pub fn load(db: &Db) -> Result<BrandStore> {
        let mut assets = HashMap::new();
        for name in ASSETS {
            if let Some((mime, data, version)) = db.get_asset(name)? {
                if let Some(mime) = sniff(&data).filter(|m| *m == mime) {
                    assets.insert(*name, Arc::new(Asset { mime, data: data.into(), version }));
                }
            }
        }
        let store = BrandStore {
            brand: RwLock::new(Brand {
                name: String::new(),
                accent: DEFAULT_ACCENT.into(),
                button: DEFAULT_BUTTON.into(),
                wifi_name: String::new(),
                logo: None,
                icon: None,
                poster: None,
                sounds: true,
                after_url: String::new(),
                notice: String::new(),
                name_font: String::new(),
                name_weight: crate::fonts::SYSTEM.2,
                name_size: 100,
                coins: COINS.to_vec(),
                theme: THEMES[0].into(),
            }),
            assets: RwLock::new(assets),
        };
        store.refresh(db)?;
        Ok(store)
    }

    pub fn get(&self) -> Brand {
        self.brand.read().unwrap().clone()
    }

    pub fn asset(&self, name: &str) -> Option<Arc<Asset>> {
        self.assets.read().unwrap().get(asset_name(name)?).cloned()
    }

    pub fn set_text(&self, db: &Db, name: &str, accent: &str, poster_alt: &str, wifi_name: &str) -> Result<()> {
        db.set_setting("wifi_name", wifi_name)?;
        db.set_setting("portal_name", name)?;
        db.set_setting("accent", accent)?;
        db.set_setting("poster_alt", poster_alt)?;
        self.refresh(db)
    }

    /// `notice` must already be cleaned by [`clean_text`].
    pub fn set_notice(&self, db: &Db, notice: &str) -> Result<()> {
        db.set_setting("notice", notice)?;
        self.refresh(db)
    }

    /// `hex` must pass [`valid_accent`].
    pub fn set_button(&self, db: &Db, hex: &str) -> Result<()> {
        db.set_setting("button_color", hex)?;
        self.refresh(db)
    }

    /// `font` must be "" or a key of [`crate::fonts::FONTS`].
    pub fn set_name_font(&self, db: &Db, font: &str, weight: Option<u16>) -> Result<()> {
        db.set_setting("name_font", font)?;
        db.set_setting("name_weight", &crate::fonts::weight_for(font, weight).to_string())?;
        self.refresh(db)
    }

    /// `coins` must be a non-empty subset of [`COINS`] (see [`clean_coins`]).
    pub fn set_coins(&self, db: &Db, coins: &[u32]) -> Result<()> {
        let list: Vec<String> = coins.iter().map(u32::to_string).collect();
        db.set_setting("coins", &list.join(","))?;
        self.refresh(db)
    }

    /// `theme` must be one of [`THEMES`].
    pub fn set_theme(&self, db: &Db, theme: &str) -> Result<()> {
        db.set_setting("portal_theme", theme)?;
        self.refresh(db)
    }

    /// `after_url` must already be cleaned by [`clean_after_url`].
    pub fn set_online(&self, db: &Db, sounds: bool, after_url: &str) -> Result<()> {
        db.set_setting("portal_sounds", if sounds { "1" } else { "0" })?;
        db.set_setting("after_url", after_url)?;
        self.refresh(db)
    }

    pub fn set_asset(&self, db: &Db, name: &str, data: Vec<u8>) -> Result<Result<(), &'static str>> {
        let Some(name) = asset_name(name) else { return Ok(Err("Unknown image")) };
        if data.len() > MAX_ASSET_BYTES {
            return Ok(Err("Image is too large (max 1.5 MB)"));
        }
        let Some(mime) = sniff(&data) else { return Ok(Err("Use a JPEG, PNG or WebP image")) };
        if name == "icon" {
            if !crate::homeicon::square_png_side(&data).is_some_and(|s| (96..=1024).contains(&s)) {
                return Ok(Err("The home-screen icon must be a square PNG"));
            }
            if self.asset("logo").is_none() {
                return Ok(Err("Upload a logo first"));
            }
        }
        let version = db.put_asset(name, mime, &data)?;
        let mut assets = self.assets.write().unwrap();
        assets.insert(name, Arc::new(Asset { mime, data: data.into(), version }));
        if name == "logo" {
            // Made from the old logo: the dashboard sends a new one next.
            db.delete_asset("icon")?;
            assets.remove("icon");
        }
        drop(assets);
        self.refresh(db)?;
        Ok(Ok(()))
    }

    pub fn remove_asset(&self, db: &Db, name: &str) -> Result<bool> {
        let Some(name) = asset_name(name) else { return Ok(false) };
        let mut removed = db.delete_asset(name)?;
        let mut assets = self.assets.write().unwrap();
        assets.remove(name);
        if name == "logo" {
            removed |= db.delete_asset("icon")?;
            assets.remove("icon");
        }
        drop(assets);
        self.refresh(db)?;
        Ok(removed)
    }

    fn refresh(&self, db: &Db) -> Result<()> {
        let assets = self.assets.read().unwrap();
        let url = |n: &str| assets.get(n).map(|a| format!("/brand/{n}?v={}", a.version));
        let accent = db.get_setting("accent")?.filter(|a| valid_accent(a)).unwrap_or_else(|| DEFAULT_ACCENT.into());
        let font = db.get_setting("name_font")?.and_then(|k| crate::fonts::find(&k));
        let key = font.map_or("", |f| f.key);
        let weight = crate::fonts::weight_for(key, db.get_setting("name_weight")?.and_then(|w| w.parse().ok()));
        let b = Brand {
            name: db.get_setting("portal_name")?.unwrap_or_else(|| "Piso WiFi".into()),
            accent,
            button: db.get_setting("button_color")?.filter(|a| valid_accent(a)).unwrap_or_else(|| DEFAULT_BUTTON.into()),
            wifi_name: db.get_setting("wifi_name")?.unwrap_or_default(),
            logo: url("logo"),
            icon: url("icon"),
            poster: url("poster").map(|u| Poster { url: u, alt: db.get_setting("poster_alt").ok().flatten().unwrap_or_default() }),
            sounds: db.get_setting("portal_sounds")?.is_none_or(|v| v != "0"),
            after_url: db.get_setting("after_url")?.unwrap_or_else(|| DEFAULT_AFTER_URL.into()),
            notice: db.get_setting("notice")?.unwrap_or_default(),
            name_font: key.to_string(),
            name_weight: weight,
            name_size: font.map_or(100, |f| f.size),
            coins: coins_of(db.get_setting("coins")?.as_deref()),
            theme: theme_of(db.get_setting("portal_theme")?.as_deref()).into(),
        };
        *self.brand.write().unwrap() = b;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_themes_fall_back_to_classic() {
        assert_eq!(theme_of(Some("glass")), "glass");
        assert_eq!(theme_of(Some("classic")), "classic");
        assert_eq!(theme_of(Some("neon")), "classic");
        assert_eq!(theme_of(None), "classic");
    }

    #[test]
    fn coins_are_a_sorted_subset() {
        assert_eq!(clean_coins(&[10, 5]), Some(vec![5, 10]));
        assert_eq!(clean_coins(&[5, 5, 20]), Some(vec![5, 20]));
        assert_eq!(clean_coins(&[]), None);
        assert_eq!(clean_coins(&[5, 25]), None, "not a coin");
        assert_eq!(coins_of(None), COINS.to_vec());
        assert_eq!(coins_of(Some("5,10")), vec![5, 10]);
        assert_eq!(coins_of(Some("")), COINS.to_vec());
        assert_eq!(coins_of(Some("3")), COINS.to_vec());
    }

    #[test]
    fn sniffs_only_real_images() {
        assert_eq!(sniff(&[0xFF, 0xD8, 0xFF, 0xE0, 0]), Some("image/jpeg"));
        assert_eq!(sniff(b"\x89PNG\r\n\x1a\nrest"), Some("image/png"));
        assert_eq!(sniff(b"RIFF\0\0\0\0WEBPVP8 "), Some("image/webp"));
        assert_eq!(sniff(b"<svg onload=alert(1)>"), None);
        assert_eq!(sniff(b"GIF89a"), None);
    }

    #[test]
    fn the_home_icon_follows_the_logo() {
        let dir = std::env::temp_dir().join(format!("pisowifi-brand-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&dir).unwrap();
        let key = crate::keys::DbKey(zeroize::Zeroizing::new([7u8; 32]));
        let db = Db::open(&dir.join("t.db"), &key).unwrap();
        let store = BrandStore::load(&db).unwrap();
        let icon = crate::homeicon::png("#5A43E6", 128, false);
        let logo = crate::homeicon::png("#E63946", 64, true);

        assert_eq!(store.set_asset(&db, "icon", icon.clone()).unwrap(), Err("Upload a logo first"));
        store.set_asset(&db, "logo", logo.clone()).unwrap().unwrap();
        for bad in [b"\xFF\xD8\xFF\xE0 a jpeg".to_vec(), logo[..20].to_vec(), crate::homeicon::png("#5A43E6", 64, false)] {
            assert_eq!(store.set_asset(&db, "icon", bad).unwrap(), Err("The home-screen icon must be a square PNG"));
        }
        store.set_asset(&db, "icon", icon.clone()).unwrap().unwrap();
        assert!(store.get().icon.is_some());

        // A new logo drops the icon made from the old one; so does removing it.
        store.set_asset(&db, "logo", logo.clone()).unwrap().unwrap();
        assert!(store.get().icon.is_none() && store.asset("icon").is_none());
        store.set_asset(&db, "icon", icon).unwrap().unwrap();
        assert!(store.remove_asset(&db, "logo").unwrap());
        assert!(store.get().icon.is_none() && store.get().logo.is_none());
        assert!(BrandStore::load(&db).unwrap().asset("icon").is_none(), "gone from the database too");
        drop(db);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn after_url_is_a_plain_web_page() {
        let c = |s: &str| clean_after_url(s);
        assert_eq!(c("  ").as_deref(), Some(""));
        assert_eq!(c("google.com/search?q=cats").as_deref(), Some("https://google.com/search?q=cats"));
        assert_eq!(c("http://10.0.0.1:8080/promo").as_deref(), Some("http://10.0.0.1:8080/promo"));
        assert_eq!(c(DEFAULT_AFTER_URL).as_deref(), Some(DEFAULT_AFTER_URL));
        for bad in ["javascript:alert(1)", "javascript://x%0aalert(1)", "data:text/html,hi", "https://", "https://a b.com", "https://x.com/\"<p>", "ftp://x.com"] {
            assert_eq!(c(bad), None, "{bad}");
        }
    }

    #[test]
    fn accent_must_be_hex() {
        assert!(valid_accent("#5A43E6"));
        assert!(!valid_accent("red"));
        assert!(!valid_accent("#5A43E6; background:url(x)"));
    }
}
