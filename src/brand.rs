//! The owner's customisation of the customer page: name, accent
//! colour, logo and an advert poster. Images live in the encrypted DB and
//! are cached in RAM so customers' phones don't trigger a decrypt each.

use crate::db::Db;
use anyhow::Result;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

pub const ASSETS: &[&str] = &["logo", "poster"];
pub const MAX_ASSET_BYTES: usize = 1536 * 1024;
pub const DEFAULT_ACCENT: &str = "#5A43E6";

#[derive(Clone, Serialize)]
pub struct Brand {
    pub name: String,
    pub accent: String,
    pub wifi_name: String,
    pub logo: Option<String>,
    pub poster: Option<Poster>,
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
                wifi_name: String::new(),
                logo: None,
                poster: None,
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

    pub fn set_asset(&self, db: &Db, name: &str, data: Vec<u8>) -> Result<Result<(), &'static str>> {
        let Some(name) = asset_name(name) else { return Ok(Err("Unknown image")) };
        if data.len() > MAX_ASSET_BYTES {
            return Ok(Err("Image is too large (max 1.5 MB)"));
        }
        let Some(mime) = sniff(&data) else { return Ok(Err("Use a JPEG, PNG or WebP image")) };
        let version = db.put_asset(name, mime, &data)?;
        self.assets.write().unwrap().insert(name, Arc::new(Asset { mime, data: data.into(), version }));
        self.refresh(db)?;
        Ok(Ok(()))
    }

    pub fn remove_asset(&self, db: &Db, name: &str) -> Result<bool> {
        let Some(name) = asset_name(name) else { return Ok(false) };
        let removed = db.delete_asset(name)?;
        self.assets.write().unwrap().remove(name);
        self.refresh(db)?;
        Ok(removed)
    }

    fn refresh(&self, db: &Db) -> Result<()> {
        let assets = self.assets.read().unwrap();
        let url = |n: &str| assets.get(n).map(|a| format!("/brand/{n}?v={}", a.version));
        let accent = db.get_setting("accent")?.filter(|a| valid_accent(a)).unwrap_or_else(|| DEFAULT_ACCENT.into());
        let b = Brand {
            name: db.get_setting("portal_name")?.unwrap_or_else(|| "Piso WiFi".into()),
            accent,
            wifi_name: db.get_setting("wifi_name")?.unwrap_or_default(),
            logo: url("logo"),
            poster: url("poster").map(|u| Poster { url: u, alt: db.get_setting("poster_alt").ok().flatten().unwrap_or_default() }),
        };
        *self.brand.write().unwrap() = b;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniffs_only_real_images() {
        assert_eq!(sniff(&[0xFF, 0xD8, 0xFF, 0xE0, 0]), Some("image/jpeg"));
        assert_eq!(sniff(b"\x89PNG\r\n\x1a\nrest"), Some("image/png"));
        assert_eq!(sniff(b"RIFF\0\0\0\0WEBPVP8 "), Some("image/webp"));
        assert_eq!(sniff(b"<svg onload=alert(1)>"), None);
        assert_eq!(sniff(b"GIF89a"), None);
    }

    #[test]
    fn accent_must_be_hex() {
        assert!(valid_accent("#5A43E6"));
        assert!(!valid_accent("red"));
        assert!(!valid_accent("#5A43E6; background:url(x)"));
    }
}
