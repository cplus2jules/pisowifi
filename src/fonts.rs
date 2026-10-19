//! Fonts the owner can pick for the WiFi's name on the customer page.
//! They're built in because customers' phones have no internet until they
//! pay. Latin subsets of variable fonts, so any thickness in a font's range
//! works from one small file. All are under the SIL Open Font License,
//! served next to them as /fonts/OFL.txt.

use axum::extract::Path;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Serialize;

#[derive(Serialize)]
pub struct Font {
    pub key: &'static str,
    pub name: &'static str,
    /// Thickness range of the font's weight axis, and the one it starts at.
    pub min: u16,
    pub max: u16,
    pub default: u16,
    /// Shown this much bigger or smaller (percent), so every font looks
    /// about as big as the others in the same space.
    pub size: u16,
    #[serde(skip)]
    data: &'static [u8],
}

pub const FONTS: &[Font] = &[
    Font { key: "bricolage", name: "Bricolage Grotesque", min: 200, max: 800, default: 800, size: 100, data: include_bytes!("../web/fonts/bricolage.woff2") },
    Font { key: "fredoka", name: "Fredoka", min: 300, max: 700, default: 600, size: 104, data: include_bytes!("../web/fonts/fredoka.woff2") },
    Font { key: "unbounded", name: "Unbounded", min: 200, max: 900, default: 700, size: 84, data: include_bytes!("../web/fonts/unbounded.woff2") },
    Font { key: "playfair", name: "Playfair Display", min: 400, max: 900, default: 800, size: 106, data: include_bytes!("../web/fonts/playfair.woff2") },
    Font { key: "dancing", name: "Dancing Script", min: 400, max: 700, default: 700, size: 118, data: include_bytes!("../web/fonts/dancing.woff2") },
    Font { key: "caveat", name: "Caveat", min: 400, max: 700, default: 700, size: 128, data: include_bytes!("../web/fonts/caveat.woff2") },
];

/// The phone's own font (no download): the thickness range and start.
pub const SYSTEM: (u16, u16, u16) = (300, 900, 800);

pub fn find(key: &str) -> Option<&'static Font> {
    FONTS.iter().find(|f| f.key == key)
}

/// A thickness that `key` ("" is the phone's own font) can show; `None`
/// asks for the font's own default.
pub fn weight_for(key: &str, weight: Option<u16>) -> u16 {
    let (min, max, default) = find(key).map_or(SYSTEM, |f| (f.min, f.max, f.default));
    weight.map_or(default, |w| w.clamp(min, max))
}

/// GET /fonts/{file}: `fredoka.woff2`, or `OFL.txt` for the licences.
pub async fn serve(Path(file): Path<String>) -> Response {
    if file == "OFL.txt" {
        return ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], include_str!("../web/fonts/OFL.txt")).into_response();
    }
    match file.strip_suffix(".woff2").and_then(find) {
        Some(f) => (
            [(header::CONTENT_TYPE, "font/woff2"), (header::CACHE_CONTROL, "public, max-age=31536000, immutable")],
            f.data,
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_font_is_a_real_woff2_in_range() {
        for f in FONTS {
            assert!(f.data.starts_with(b"wOF2"), "{}", f.key);
            assert!(f.min <= f.default && f.default <= f.max, "{}", f.key);
        }
        assert_eq!(weight_for("caveat", Some(900)), 700);
        assert_eq!(weight_for("caveat", None), 700);
        assert_eq!(weight_for("", Some(100)), SYSTEM.0);
        assert_eq!(weight_for("nope", None), SYSTEM.2, "unknown: the phone's own");
    }
}
