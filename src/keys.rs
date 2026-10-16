//! Database key derivation.
//!
//! The SQLCipher key is never stored. It is derived on every boot from:
//!   * the Allwinner SoC's factory-burned unique ID (SID), which lives in the
//!     chip, not on the SD card, and
//!   * a random 32-byte salt stored next to the database.
//!
//! So a stolen or cloned SD card cannot be opened on another board, and two
//! boards never share a key. Boards without a readable SID fall back to a
//! random key file (weaker: card theft then exposes the key) with a warning.
//!
//! The SID is not a strong secret (U-Boot derives the eth0 MAC from a CRC
//! of it, and many bits are shared across a model), so v2 stretches it
//! with Argon2id (64 MiB, 3 passes) before HKDF. That makes each guess
//! cost about a second on the board instead of nanoseconds. Stealing the
//! whole box (chip and card together) still opens the database; the
//! Argon2id-hashed admin password is the protection at that point.

use anyhow::{bail, Context, Result};
use hkdf::Hkdf;
use rand::RngCore;
use sha2::Sha256;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use zeroize::Zeroizing;

const SID_PATHS: &[&str] = &[
    "/sys/bus/nvmem/devices/sunxi-sid0/nvmem",
    "/sys/bus/nvmem/devices/sunxi-sid/nvmem",
];

pub struct DbKey(pub Zeroizing<[u8; 32]>);

impl DbKey {
    /// SQLCipher raw-key syntax: x'<64 hex chars>'. Skips SQLCipher's own
    /// passphrase KDF since our key is already uniformly random.
    pub fn pragma_value(&self) -> Zeroizing<String> {
        Zeroizing::new(format!("x'{}'", hex::encode(*self.0)))
    }
}

pub struct DerivedKeys {
    /// Current key.
    pub key: DbKey,
    /// The pre-Argon2 key, only to migrate databases created by v0.1.
    pub legacy: DbKey,
    pub source: &'static str,
}

/// Which secret a database is locked to. It's recorded next to the database
/// (`db.keysource`) the first time it opens, so a later boot never silently
/// switches: a kernel that hides the chip ID, or starts showing it, must not
/// make the database unreadable or start a new, empty one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Chip,
    File,
}

const MARKER: &str = "db.keysource";

pub fn chip_id_readable(dev: bool) -> bool {
    !dev && read_sid().is_some()
}

pub fn recorded_source(data_dir: &Path) -> Option<Source> {
    match fs::read_to_string(data_dir.join(MARKER)).ok()?.trim() {
        "chip" => Some(Source::Chip),
        "file" => Some(Source::File),
        _ => None,
    }
}

pub fn record_source(data_dir: &Path, source: Source) -> Result<()> {
    let text = match source {
        Source::Chip => "chip\n",
        Source::File => "file\n",
    };
    if recorded_source(data_dir) != Some(source) {
        fs::write(data_dir.join(MARKER), text).context("writing db.keysource")?;
    }
    Ok(())
}

/// Derives the key for `source`. With an existing database (`db_exists`),
/// missing key material is an error, never silently replaced.
pub fn derive(data_dir: &Path, source: Source, db_exists: bool) -> Result<DerivedKeys> {
    for f in ["db.salt"].into_iter().chain((source == Source::File).then_some("db.key")) {
        if db_exists && !data_dir.join(f).exists() {
            bail!("{} is missing, so the database can't be opened. Restore it, or restore from a backup.", data_dir.join(f).display());
        }
    }
    let salt = read_or_create_secret(&data_dir.join("db.salt"))?;
    let (ikm, source) = match source {
        Source::Chip => match read_sid() {
            Some(sid) => (sid, "SoC unique ID + salt"),
            None => bail!(
                "this database is locked to the board's chip ID, but the chip ID can't be read now \
                 ({} is missing: a kernel change?). Nothing was changed. Boot the previous kernel, \
                 or restore from a backup with `pisowifi restore`.",
                SID_PATHS[0]
            ),
        },
        Source::File => (read_or_create_secret(&data_dir.join("db.key"))?, "key file + salt"),
    };
    // v1: HKDF only (kept to open and upgrade old databases).
    let legacy = hkdf32(&salt, &ikm, b"pisowifi sqlcipher key v1")?;
    // v2: Argon2id first, so guessing the SID is slow.
    let mut stretched = Zeroizing::new([0u8; 32]);
    let params = argon2::Params::new(64 * 1024, 3, 1, Some(32)).map_err(|e| anyhow::anyhow!("{e}"))?;
    argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params)
        .hash_password_into(&ikm, &salt, &mut stretched[..])
        .map_err(|e| anyhow::anyhow!("argon2: {e}"))?;
    let key = hkdf32(&salt, &stretched[..], b"pisowifi sqlcipher key v2")?;
    Ok(DerivedKeys { key, legacy, source })
}

fn hkdf32(salt: &[u8], ikm: &[u8], info: &[u8]) -> Result<DbKey> {
    let hk = Hkdf::<Sha256>::new(Some(salt), ikm);
    let mut okm = Zeroizing::new([0u8; 32]);
    hk.expand(info, &mut okm[..]).map_err(|_| anyhow::anyhow!("hkdf expand"))?;
    Ok(DbKey(okm))
}

fn read_sid() -> Option<Zeroizing<Vec<u8>>> {
    for p in SID_PATHS {
        if let Ok(bytes) = fs::read(p) {
            // The first 16 bytes are the chip ID; reject blank/unprogrammed parts.
            let id = &bytes[..bytes.len().min(16)];
            if id.len() == 16 && id.iter().any(|&b| b != 0 && b != 0xff) {
                return Some(Zeroizing::new(id.to_vec()));
            }
        }
    }
    None
}

/// Reads a 32-byte secret, creating it with mode 0600 if missing.
pub fn read_or_create_secret(path: &Path) -> Result<Zeroizing<Vec<u8>>> {
    if path.exists() {
        let b = Zeroizing::new(fs::read(path).with_context(|| format!("reading {}", path.display()))?);
        if b.len() != 32 {
            bail!("{} is corrupt (expected 32 bytes)", path.display());
        }
        return Ok(b);
    }
    let mut b = Zeroizing::new(vec![0u8; 32]);
    rand::rngs::OsRng.fill_bytes(&mut b);
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path).with_context(|| format!("creating {}", path.display()))?;
    f.write_all(&b)?;
    f.sync_all()?;
    Ok(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_key_source_sticks_and_missing_material_is_an_error() {
        let dir = std::env::temp_dir().join(format!("pisowifi-keys-{}", rand::random::<u64>()));
        fs::create_dir_all(&dir).unwrap();
        assert_eq!(recorded_source(&dir), None);
        record_source(&dir, Source::File).unwrap();
        assert_eq!(recorded_source(&dir), Some(Source::File));
        // A database that exists but whose key file is gone is never re-keyed.
        let e = derive(&dir, Source::File, true).err().unwrap();
        assert!(e.to_string().contains("missing"), "{e}");
        // Fresh: key material is created, and derivation is repeatable.
        let a = derive(&dir, Source::File, false).unwrap();
        let b = derive(&dir, Source::File, true).unwrap();
        assert_eq!(*a.key.0, *b.key.0);
        let _ = fs::remove_dir_all(dir);
    }
}
