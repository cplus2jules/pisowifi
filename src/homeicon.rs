//! The home-screen icon: the portal's coin + WiFi mark in the owner's
//! accent colour, drawn here as a PNG (phones won't use SVG for this).
//! No image libraries: shapes are drawn from their distance fields and
//! written with a tiny deflate, which is plenty for flat colours.

use std::f32::consts::FRAC_1_SQRT_2;

const LIME: [f32; 3] = [212.0, 240.0, 76.0];

/// `round`: the favicon's rounded square with see-through corners.
/// Otherwise the square is filled edge to edge (iPhones round it themselves,
/// and Android masks it to its own shape: the mark sits inside the safe zone).
pub fn png(accent: &str, size: u32, round: bool) -> Vec<u8> {
    let accent = rgb(accent);
    let k = 64.0 / size as f32; // one pixel, in the SVG's 64-unit space
    let cover = |d: f32| (0.5 - d / k).clamp(0.0, 1.0);
    let mut px = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let p = ((x as f32 + 0.5) * k, (y as f32 + 0.5) * k);
            let mut c = accent;
            mix(&mut c, LIME, cover(dist(p, (32.0, 32.0)) - 20.0));
            mix(&mut c, accent, 0.16 * cover((dist(p, (32.0, 32.0)) - 16.5).abs() - 1.0));
            for r in [7.0, 13.0] {
                mix(&mut c, accent, cover(arc(p, r) - 2.0));
            }
            mix(&mut c, accent, cover(dist(p, (32.0, 38.0)) - 3.0));
            let alpha = if round { cover(rounded_square(p)) } else { 1.0 };
            px.extend(c.iter().map(|v| v.round() as u8));
            px.push((alpha * 255.0).round() as u8);
        }
    }
    encode(size, &px)
}

/// The side of a square PNG, or None for anything else.
pub fn square_png_side(data: &[u8]) -> Option<u32> {
    if data.len() < 24 || !data.starts_with(b"\x89PNG\r\n\x1a\n") || &data[12..16] != b"IHDR" {
        return None;
    }
    let w = u32::from_be_bytes(data[16..20].try_into().ok()?);
    let h = u32::from_be_bytes(data[20..24].try_into().ok()?);
    (w == h).then_some(w)
}

fn rgb(hex: &str) -> [f32; 3] {
    let hex = if crate::brand::valid_accent(hex) { hex } else { crate::brand::DEFAULT_ACCENT };
    let ch = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).unwrap_or(0) as f32;
    [ch(1), ch(3), ch(5)]
}

fn mix(c: &mut [f32; 3], top: [f32; 3], t: f32) {
    for i in 0..3 {
        c[i] += (top[i] - c[i]) * t;
    }
}

fn dist(p: (f32, f32), c: (f32, f32)) -> f32 {
    (p.0 - c.0).hypot(p.1 - c.1)
}

/// The WiFi arcs: the top quarter of a circle around (32, 37).
fn arc(p: (f32, f32), r: f32) -> f32 {
    let q = ((p.0 - 32.0).abs(), p.1 - 37.0);
    if q.0 <= -q.1 {
        (q.0.hypot(q.1) - r).abs()
    } else {
        dist(q, (r * FRAC_1_SQRT_2, -r * FRAC_1_SQRT_2)) // past the round cap
    }
}

/// The 64-unit square with 18-unit corners.
fn rounded_square(p: (f32, f32)) -> f32 {
    let q = ((p.0 - 32.0).abs() - 14.0, (p.1 - 32.0).abs() - 14.0);
    q.0.max(0.0).hypot(q.1.max(0.0)) + q.0.max(q.1).min(0.0) - 18.0
}

// ---- PNG --------------------------------------------------------------------

fn encode(size: u32, rgba: &[u8]) -> Vec<u8> {
    // Each row is "Sub"-filtered: flat colour becomes runs of zeros.
    let row = size as usize * 4;
    let mut raw = Vec::with_capacity(rgba.len() + size as usize);
    for line in rgba.chunks(row) {
        raw.push(1);
        raw.extend(line.iter().enumerate().map(|(i, &v)| if i < 4 { v } else { v.wrapping_sub(line[i - 4]) }));
    }
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend(size.to_be_bytes());
    ihdr.extend(size.to_be_bytes());
    ihdr.extend([8, 6, 0, 0, 0]); // 8-bit RGBA
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &zlib(&raw));
    chunk(&mut out, b"IEND", &[]);
    out
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend((data.len() as u32).to_be_bytes());
    let start = out.len();
    out.extend(kind);
    out.extend(data);
    let crc = crc32(&out[start..]);
    out.extend(crc.to_be_bytes());
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

const LEN_BASE: [u32; 29] = [3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131, 163, 195, 227, 258];
const LEN_EXTRA: [u32; 29] = [0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0];

struct Bits {
    out: Vec<u8>,
    acc: u32,
    n: u32,
}

impl Bits {
    fn put(&mut self, v: u32, len: u32) {
        self.acc |= v << self.n;
        self.n += len;
        while self.n >= 8 {
            self.out.push(self.acc as u8);
            self.acc >>= 8;
            self.n -= 8;
        }
    }
    /// Huffman codes go most significant bit first.
    fn code(&mut self, code: u32, len: u32) {
        self.put(code.reverse_bits() >> (32 - len), len);
    }
    /// A literal byte, end of block (256) or length symbol, in the fixed code.
    fn sym(&mut self, s: u32) {
        match s {
            0..=143 => self.code(0x30 + s, 8),
            144..=255 => self.code(0x190 + s - 144, 9),
            256..=279 => self.code(s - 256, 7),
            _ => self.code(0xC0 + s - 280, 8),
        }
    }
    /// Repeat the previous byte `len` (3..=258) times.
    fn repeat(&mut self, len: u32) {
        let i = LEN_BASE.iter().rposition(|&b| b <= len).unwrap();
        self.sym(257 + i as u32);
        self.put(len - LEN_BASE[i], LEN_EXTRA[i]);
        self.code(0, 5); // distance 1
    }
}

/// One fixed-Huffman deflate block; runs of a byte become back-references.
fn zlib(data: &[u8]) -> Vec<u8> {
    let mut b = Bits { out: vec![0x78, 0x01], acc: 0, n: 0 };
    b.put(0b011, 3); // final block, fixed codes
    let mut i = 0;
    while i < data.len() {
        let v = data[i];
        b.sym(v as u32);
        let mut j = i + 1;
        while j < data.len() && data[j] == v {
            j += 1;
        }
        let mut n = (j - i - 1) as u32;
        while n >= 3 {
            let len = n.min(258);
            b.repeat(len);
            n -= len;
        }
        for _ in 0..n {
            b.sym(v as u32);
        }
        i = j;
    }
    b.sym(256);
    if b.n > 0 {
        b.out.push(b.acc as u8);
    }
    let (mut s1, mut s2) = (1u32, 0u32);
    for &v in data {
        s1 = (s1 + v as u32) % 65521;
        s2 = (s2 + s1) % 65521;
    }
    b.out.extend(((s2 << 16) | s1).to_be_bytes());
    b.out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Just enough inflate for what `zlib` writes.
    fn inflate(z: &[u8]) -> Vec<u8> {
        let d = &z[2..z.len() - 4];
        let mut pos = 0;
        let mut bit = || {
            let v = (d[pos / 8] >> (pos % 8)) & 1;
            pos += 1;
            v as u32
        };
        assert_eq!((bit(), bit(), bit()), (1, 1, 0), "one final fixed block");
        let mut out: Vec<u8> = vec![];
        loop {
            let mut c = 0;
            for _ in 0..7 {
                c = c << 1 | bit();
            }
            let s = if c <= 0x17 {
                256 + c
            } else {
                c = c << 1 | bit();
                match c {
                    0x30..=0xBF => c - 0x30,
                    0xC0..=0xC7 => 280 + c - 0xC0,
                    _ => 144 + (c << 1 | bit()) - 0x190,
                }
            };
            match s {
                0..=255 => out.push(s as u8),
                256 => break,
                _ => {
                    let i = (s - 257) as usize;
                    let mut extra = 0;
                    for k in 0..LEN_EXTRA[i] {
                        extra |= bit() << k;
                    }
                    let mut dc = 0;
                    for _ in 0..5 {
                        dc = dc << 1 | bit();
                    }
                    assert_eq!(dc, 0, "distance 1");
                    let last = *out.last().unwrap();
                    out.extend(std::iter::repeat_n(last, (LEN_BASE[i] + extra) as usize));
                }
            }
        }
        out
    }

    /// RGBA pixels back out of a PNG from `encode`.
    fn decode(png: &[u8]) -> (u32, Vec<u8>) {
        let side = square_png_side(png).unwrap();
        let len = u32::from_be_bytes(png[33..37].try_into().unwrap()) as usize;
        assert_eq!(&png[37..41], b"IDAT");
        let z = &png[41..41 + len];
        let crc = u32::from_be_bytes(png[41 + len..45 + len].try_into().unwrap());
        assert_eq!(crc, crc32(&png[37..41 + len]));
        let raw = inflate(z);
        let row = side as usize * 4;
        let mut px = vec![];
        for line in raw.chunks(row + 1) {
            assert_eq!(line[0], 1);
            let start = px.len();
            for (i, &v) in line[1..].iter().enumerate() {
                let left = if i < 4 { 0 } else { px[start + i - 4] };
                px.push(v.wrapping_add(left));
            }
        }
        assert!(png.ends_with(b"IEND\xae\x42\x60\x82"));
        (side, px)
    }

    fn at(px: &[u8], side: u32, x: u32, y: u32) -> &[u8] {
        let i = ((y * side + x) * 4) as usize;
        &px[i..i + 4]
    }

    #[test]
    fn draws_the_mark_in_the_owners_colour() {
        let (side, px) = decode(&png("#E63946", 64, true));
        assert_eq!(side, 64);
        assert_eq!(px.len(), 64 * 64 * 4);
        assert_eq!(at(&px, 64, 0, 0)[3], 0, "rounded corner is see-through");
        assert_eq!(at(&px, 64, 32, 2), [0xE6, 0x39, 0x46, 255], "accent background");
        assert_eq!(at(&px, 64, 32, 20), [212, 240, 76, 255], "lime coin");
        assert_eq!(at(&px, 64, 32, 37)[..3], [0xE6, 0x39, 0x46], "WiFi dot");
        let (_, px) = decode(&png("#E63946", 64, false));
        assert_eq!(at(&px, 64, 0, 0), [0xE6, 0x39, 0x46, 255], "full square for phones to shape");
    }

    #[test]
    fn flat_colours_compress_well() {
        let big = png("#5A43E6", 512, false);
        assert!(big.len() < 64 * 1024, "{} bytes", big.len());
        let (side, px) = decode(&big);
        assert_eq!((side, px.len()), (512, 512 * 512 * 4));
    }

    #[test]
    fn bad_accent_falls_back_to_the_default() {
        let (_, px) = decode(&png("red", 64, false));
        assert_eq!(at(&px, 64, 0, 0), [0x5A, 0x43, 0xE6, 255]);
    }

    #[test]
    fn square_png_side_reads_the_header() {
        assert_eq!(square_png_side(&png("#5A43E6", 48, true)), Some(48));
        let mut wide = png("#5A43E6", 48, true);
        wide[16..20].copy_from_slice(&96u32.to_be_bytes());
        assert_eq!(square_png_side(&wide), None);
        assert_eq!(square_png_side(b"\xFF\xD8\xFF\xE0 not a png at all"), None);
    }
}
