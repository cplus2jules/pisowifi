//! Signed releases.
//!
//! The owner's Mac signs a small manifest for each bundle with an Ed25519
//! key that never leaves it (scripts/release.sh). Every vendo and hub has
//! the public half compiled in and refuses anything that doesn't verify:
//!
//! * only the owner can publish an update: whoever controls the download
//!   server, DNS or the GitHub account can serve files, but can't sign them;
//! * the manifest pins the bundle's SHA-256 and size, the product (vendo or
//!   hub) and the target board, so a valid signature can't be reused for a
//!   different file or the wrong machine;
//! * versions only go up: an older signed release (say, one with a known
//!   bug) can't be replayed to downgrade a box.

use anyhow::{bail, Context, Result};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};

/// Domain separation: these signatures can't be confused with any other use of the key.
const CONTEXT: &[u8] = b"pisowifi release manifest v1\n";
pub const MAX_MANIFEST_BYTES: u64 = 64 * 1024;
pub const MAX_BUNDLE_BYTES: u64 = 64 * 1024 * 1024;
/// Unpacked size cap (a bundle is ~6 MB; this only stops a runaway gzip stream).
const MAX_UNPACKED_BYTES: u64 = 256 * 1024 * 1024;
const MAX_NOTES_CHARS: usize = 4000;

/// The build target of this binary, e.g. armv7-unknown-linux-musleabihf.
pub const TARGET: &str = env!("PISOWIFI_TARGET");

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Vendo,
    Hub,
}

impl Kind {
    pub fn parse(s: &str) -> Result<Kind> {
        match s {
            "vendo" => Ok(Kind::Vendo),
            "hub" => Ok(Kind::Hub),
            _ => bail!("kind must be vendo or hub"),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Kind::Vendo => "a vendo",
            Kind::Hub => "the hub",
        }
    }

    /// Files taken from a bundle; everything else in it (installer scripts,
    /// examples) is only for first installs.
    pub fn files(self) -> &'static [&'static str] {
        match self {
            Kind::Vendo => &["pisowifi", "pisowifi.service", "pisowifi-update.service", "pisowifi-update.path", "README.md"],
            Kind::Hub => &["pisowifi", "pisowifi-hub.service", "pisowifi-hub-update.service", "README.md"],
        }
    }
}

/// Release asset name for a product and board, e.g. pisowifi-armv7.tar.gz.
pub fn asset_name(kind: Kind, target: &str) -> String {
    let arch = target.split('-').next().unwrap_or(target);
    match kind {
        Kind::Vendo => format!("pisowifi-{arch}.tar.gz"),
        Kind::Hub => format!("pisowifi-hub-{arch}.tar.gz"),
    }
}

/// The signed manifest sits next to its bundle: <bundle>.manifest
pub fn manifest_path(bundle: &Path) -> PathBuf {
    let mut s = bundle.as_os_str().to_owned();
    s.push(".manifest");
    PathBuf::from(s)
}

// ---- versions ------------------------------------------------------------------

/// MAJOR.MINOR.PATCH, numbers only. Pre-release tags are refused so the
/// ordering is never ambiguous.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(pub u32, pub u32, pub u32);

impl Version {
    pub fn parse(s: &str) -> Result<Version> {
        let parts: Vec<&str> = s.trim().split('.').collect();
        let num = |p: &str| -> Option<u32> {
            (!p.is_empty() && p.len() <= 9 && p.bytes().all(|b| b.is_ascii_digit())).then(|| p.parse().ok()).flatten()
        };
        match parts.as_slice() {
            [a, b, c] => match (num(a), num(b), num(c)) {
                (Some(a), Some(b), Some(c)) => Ok(Version(a, b, c)),
                _ => bail!("\"{s}\" is not a version like 1.2.3"),
            },
            _ => bail!("\"{s}\" is not a version like 1.2.3"),
        }
    }

    pub fn current() -> Version {
        Version::parse(env!("CARGO_PKG_VERSION")).expect("Cargo.toml version is MAJOR.MINOR.PATCH")
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

// ---- manifests -------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    /// Always "pisowifi".
    pub product: String,
    pub kind: Kind,
    pub version: String,
    pub target: String,
    /// Bundle file name (informational; the checksum is what counts).
    pub file: String,
    pub size: u64,
    /// SHA-256 of the bundle, hex.
    pub sha256: String,
    /// Boxes older than this must install an intermediate release first.
    pub min_from_version: String,
    #[serde(default)]
    pub notes: String,
    /// Unix time it was signed.
    pub created: i64,
}

/// What goes in the .manifest file.
#[derive(Serialize, Deserialize)]
struct Envelope {
    /// The manifest JSON exactly as signed. Kept as text so nothing is
    /// re-serialized before the signature is checked.
    manifest: String,
    /// Ed25519 signature over CONTEXT + manifest, hex.
    signature: String,
    /// First 8 bytes of the signing key, hex (only for error messages).
    key_id: String,
}

fn key_id(k: &VerifyingKey) -> String {
    hex::encode(&k.as_bytes()[..8])
}

fn signed_message(manifest: &str) -> Vec<u8> {
    [CONTEXT, manifest.as_bytes()].concat()
}

/// Signs a manifest; returns the .manifest file contents.
pub fn sign(key: &SigningKey, m: &Manifest) -> Result<String> {
    let text = serde_json::to_string(m)?;
    let sig = key.sign(&signed_message(&text));
    let env = Envelope { manifest: text, signature: hex::encode(sig.to_bytes()), key_id: key_id(&key.verifying_key()) };
    Ok(serde_json::to_string_pretty(&env)? + "\n")
}

/// Checks the signature, then parses. Nothing in the manifest is looked at
/// before the signature is known to be good.
pub fn open(file: &[u8], key: &VerifyingKey) -> Result<Manifest> {
    if file.len() as u64 > MAX_MANIFEST_BYTES {
        bail!("The update's manifest is too large.");
    }
    let env: Envelope = serde_json::from_slice(file).map_err(|_| anyhow::anyhow!("The update's manifest is damaged."))?;
    let sig = hex::decode(&env.signature)
        .ok()
        .and_then(|b| <[u8; 64]>::try_from(b).ok())
        .map(|b| Signature::from_bytes(&b));
    let ok = sig.is_some_and(|s| key.verify_strict(&signed_message(&env.manifest), &s).is_ok());
    if !ok {
        bail!(
            "This update isn't signed with your release key (signed by {}, expected {}). Nothing was changed.",
            if env.key_id.len() == 16 && env.key_id.bytes().all(|b| b.is_ascii_hexdigit()) { &env.key_id } else { "unknown" },
            key_id(key)
        );
    }
    serde_json::from_str(&env.manifest).context("signed manifest doesn't parse")
}

/// What the installing box expects.
pub struct Expect<'a> {
    pub kind: Kind,
    pub target: &'a str,
    /// Installed version; `None` on a box with nothing installed yet.
    pub current: Option<Version>,
    /// Let the same version be installed again (only `install.sh`, run by root).
    pub allow_same: bool,
}

/// Checks a verified manifest against this box. Returns its version.
pub fn check(m: &Manifest, e: &Expect) -> Result<Version> {
    if m.product != "pisowifi" {
        bail!("This isn't a Piso WiFi update.");
    }
    if m.kind != e.kind {
        bail!("This update is for {}, not {}.", m.kind.label(), e.kind.label());
    }
    if m.target != e.target {
        bail!("This update is built for a different kind of board ({}; this one is {}).", m.target, e.target);
    }
    let v = Version::parse(&m.version)?;
    let min = Version::parse(&m.min_from_version)?;
    if let Some(cur) = e.current {
        if v < cur || (v == cur && !e.allow_same) {
            bail!("Version {v} is not newer than the installed {cur}. Nothing was changed.");
        }
        if cur < min {
            bail!("Install version {min} first; this update can't be installed over {cur} directly.");
        }
    }
    if m.notes.chars().count() > MAX_NOTES_CHARS {
        bail!("The release notes are too long.");
    }
    if m.sha256.len() != 64 || !m.sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("The update's checksum is malformed.");
    }
    if m.size == 0 || m.size > MAX_BUNDLE_BYTES {
        bail!("The update's size is out of range.");
    }
    Ok(v)
}

/// The bundle must be exactly the file that was signed.
pub fn check_bundle(m: &Manifest, bundle: &[u8]) -> Result<()> {
    let digest = hex::encode(Sha256::digest(bundle));
    if bundle.len() as u64 != m.size || !digest.eq_ignore_ascii_case(&m.sha256) {
        bail!("The update file doesn't match its signed checksum (damaged or tampered with). Nothing was changed.");
    }
    Ok(())
}

/// Everything at once: signature, product, board, version, checksum.
pub fn verify(manifest: &[u8], bundle: &[u8], key: Option<&VerifyingKey>, e: &Expect) -> Result<Manifest> {
    let key = key.ok_or_else(no_key_error)?;
    let m = open(manifest, key)?;
    check(&m, e)?;
    check_bundle(&m, bundle)?;
    Ok(m)
}

pub fn no_key_error() -> anyhow::Error {
    anyhow::anyhow!("This copy was built without a release key, so it can't install updates. Reinstall from a bundle made with scripts/release.sh.")
}

/// The owner's release key, compiled in from release-key.pub.
pub fn trusted_key() -> Option<VerifyingKey> {
    parse_public(env!("PISOWIFI_RELEASE_PUBKEY")).ok()
}

pub fn parse_public(hex_key: &str) -> Result<VerifyingKey> {
    let line = hex_key.lines().map(str::trim).find(|l| !l.is_empty() && !l.starts_with('#')).unwrap_or("");
    let bytes: [u8; 32] = hex::decode(line).ok().and_then(|b| b.try_into().ok()).context("not a 64-character hex key")?;
    VerifyingKey::from_bytes(&bytes).context("not a valid Ed25519 public key")
}

// ---- unpacking ---------------------------------------------------------------------

/// Unpacks the allowed files of a (verified) bundle into `dest`. Only plain
/// files directly under the bundle's top folder are taken: no links, no
/// paths that climb out, nothing not on the list.
pub fn unpack(bundle: &[u8], kind: Kind, dest: &Path) -> Result<()> {
    let gz = flate2::read::GzDecoder::new(bundle).take(MAX_UNPACKED_BYTES);
    let mut tar = tar::Archive::new(gz);
    let wanted = kind.files();
    let mut seen = Vec::new();
    for entry in tar.entries().context("reading the bundle")? {
        let mut entry = entry.context("reading the bundle")?;
        let path = entry.path().context("bad path in bundle")?.into_owned();
        let parts: Vec<&str> = path.iter().map(|p| p.to_str().unwrap_or("\0")).filter(|p| *p != ".").collect();
        let [top, name] = parts.as_slice() else { continue };
        if !matches!(*top, "pisowifi" | "pisowifi-hub") || !wanted.contains(name) {
            continue;
        }
        if !entry.header().entry_type().is_file() {
            bail!("{name} in the bundle is not a plain file");
        }
        if seen.contains(&name.to_string()) {
            bail!("{name} appears twice in the bundle");
        }
        let mut data = Vec::new();
        entry.read_to_end(&mut data)?;
        write_file(&dest.join(name), &data, if *name == "pisowifi" { 0o755 } else { 0o644 })?;
        seen.push(name.to_string());
    }
    if !seen.iter().any(|n| n == "pisowifi") {
        bail!("the bundle has no pisowifi binary");
    }
    Ok(())
}

/// Same, from an unpacked bundle folder (install.sh's upgrade mode).
pub fn copy_dir(src: &Path, kind: Kind, dest: &Path) -> Result<()> {
    for name in kind.files() {
        let from = src.join(name);
        let meta = match std::fs::symlink_metadata(&from) {
            Ok(m) => m,
            Err(_) if *name != "pisowifi" => continue,
            Err(e) => return Err(e).with_context(|| format!("{} not found", from.display())),
        };
        if !meta.is_file() {
            bail!("{} is not a plain file", from.display());
        }
        let data = std::fs::read(&from)?;
        write_file(&dest.join(name), &data, if *name == "pisowifi" { 0o755 } else { 0o644 })?;
    }
    Ok(())
}

pub fn write_file(path: &Path, data: &[u8], mode: u32) -> Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(mode);
    }
    #[cfg(not(unix))]
    let _ = mode;
    let mut f = opts.open(path).with_context(|| format!("creating {}", path.display()))?;
    // Exactly `mode`, whatever the umask (the hub's service user must be
    // able to run the binary).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        f.set_permissions(std::fs::Permissions::from_mode(mode))?;
    }
    f.write_all(data)?;
    f.sync_all()?;
    Ok(())
}

// ---- the owner's side ------------------------------------------------------------

/// Loads the release key, creating it (mode 0600) the first time.
pub fn load_or_create_key(path: &Path) -> Result<SigningKey> {
    if let Some(dir) = path.parent() {
        if !dir.exists() {
            std::fs::create_dir_all(dir)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
            }
        }
    }
    #[cfg(unix)]
    if path.exists() {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path)?.permissions().mode();
        if mode & 0o077 != 0 {
            bail!("{} can be read by other users (mode {:o}); run: chmod 600 {}", path.display(), mode & 0o777, path.display());
        }
    }
    let raw = crate::keys::read_or_create_secret(path)?;
    let seed: [u8; 32] = raw[..].try_into().context("release key")?;
    Ok(SigningKey::from_bytes(&seed))
}

pub struct SignArgs<'a> {
    pub kind: Kind,
    pub target: &'a str,
    pub version: &'a str,
    pub min_from: &'a str,
    pub notes: &'a str,
}

/// Writes <bundle>.manifest for `bundle`.
pub fn sign_bundle(key: &SigningKey, bundle: &Path, a: &SignArgs) -> Result<PathBuf> {
    let data = std::fs::read(bundle).with_context(|| format!("reading {}", bundle.display()))?;
    let m = Manifest {
        product: "pisowifi".into(),
        kind: a.kind,
        version: Version::parse(a.version)?.to_string(),
        target: a.target.into(),
        file: bundle.file_name().and_then(|n| n.to_str()).unwrap_or_default().into(),
        size: data.len() as u64,
        sha256: hex::encode(Sha256::digest(&data)),
        min_from_version: Version::parse(a.min_from)?.to_string(),
        notes: a.notes.trim().into(),
        created: chrono::Utc::now().timestamp(),
    };
    // Same checks a box makes, so a bad manifest is caught here, not in the field.
    check(&m, &Expect { kind: a.kind, target: a.target, current: None, allow_same: false })?;
    let out = manifest_path(bundle);
    std::fs::write(&out, sign(key, &m)?)?;
    Ok(out)
}

#[cfg(test)]
pub mod tests {
    use super::*;

    pub const TARGET_ARM: &str = "armv7-unknown-linux-musleabihf";

    pub fn test_key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    /// A gzipped tar shaped like build-pi.sh's output.
    pub fn bundle_with(top: &str, files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut b = tar::Builder::new(flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast()));
        for (name, data) in files {
            let mut h = tar::Header::new_gnu();
            h.set_size(data.len() as u64);
            h.set_mode(0o755);
            h.set_cksum();
            b.append_data(&mut h, format!("{top}/{name}"), *data).unwrap();
        }
        b.into_inner().unwrap().finish().unwrap()
    }

    pub fn manifest_for(bundle: &[u8], kind: Kind, target: &str, version: &str, min_from: &str) -> Manifest {
        Manifest {
            product: "pisowifi".into(),
            kind,
            version: version.into(),
            target: target.into(),
            file: asset_name(kind, target),
            size: bundle.len() as u64,
            sha256: hex::encode(Sha256::digest(bundle)),
            min_from_version: min_from.into(),
            notes: "Faster coin counting.".into(),
            created: 1_760_000_000,
        }
    }

    fn expect(current: &str) -> Expect<'static> {
        Expect { kind: Kind::Vendo, target: TARGET_ARM, current: Some(Version::parse(current).unwrap()), allow_same: false }
    }

    fn signed(key: &SigningKey, m: &Manifest) -> Vec<u8> {
        sign(key, m).unwrap().into_bytes()
    }

    #[test]
    fn versions_order_numerically_and_reject_junk() {
        assert!(Version::parse("0.10.0").unwrap() > Version::parse("0.9.9").unwrap());
        assert!(Version::parse("1.0.0").unwrap() > Version::parse("0.99.99").unwrap());
        for bad in ["1.2", "1.2.3.4", "1.2.x", "1.2.3-rc1", "", "01.2.+3", " . . "] {
            assert!(Version::parse(bad).is_err(), "{bad}");
        }
        assert_eq!(Version::current().to_string(), env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn a_good_bundle_verifies() {
        let key = test_key(1);
        let bundle = bundle_with("pisowifi", &[("pisowifi", b"new binary")]);
        let m = manifest_for(&bundle, Kind::Vendo, TARGET_ARM, "0.2.0", "0.1.0");
        let got = verify(&signed(&key, &m), &bundle, Some(&key.verifying_key()), &expect("0.1.0")).unwrap();
        assert_eq!(got, m);
    }

    #[test]
    fn a_bundle_signed_by_another_key_is_refused() {
        let (owner, attacker) = (test_key(1), test_key(2));
        let bundle = bundle_with("pisowifi", &[("pisowifi", b"evil")]);
        let m = manifest_for(&bundle, Kind::Vendo, TARGET_ARM, "9.0.0", "0.0.0");
        let e = verify(&signed(&attacker, &m), &bundle, Some(&owner.verifying_key()), &expect("0.1.0")).unwrap_err();
        assert!(e.to_string().contains("isn't signed with your release key"), "{e}");
        // Garbage, an empty signature, or no key compiled in: refused too.
        assert!(verify(b"not json", &bundle, Some(&owner.verifying_key()), &expect("0.1.0")).is_err());
        let mut env: serde_json::Value = serde_json::from_slice(&signed(&owner, &m)).unwrap();
        env["signature"] = "".into();
        assert!(verify(env.to_string().as_bytes(), &bundle, Some(&owner.verifying_key()), &expect("0.1.0")).is_err());
        assert!(verify(&signed(&owner, &m), &bundle, None, &expect("0.1.0")).is_err());
    }

    #[test]
    fn tampering_with_the_manifest_or_the_bundle_is_caught() {
        let key = test_key(1);
        let bundle = bundle_with("pisowifi", &[("pisowifi", b"new binary")]);
        let m = manifest_for(&bundle, Kind::Vendo, TARGET_ARM, "0.2.0", "0.1.0");
        let good = signed(&key, &m);

        // Edit a signed field (the version) without re-signing.
        let mut env: serde_json::Value = serde_json::from_slice(&good).unwrap();
        env["manifest"] = env["manifest"].as_str().unwrap().replace("0.2.0", "0.3.0").into();
        let e = verify(env.to_string().as_bytes(), &bundle, Some(&key.verifying_key()), &expect("0.1.0")).unwrap_err();
        assert!(e.to_string().contains("isn't signed"), "{e}");

        // Swap the bundle for another file.
        let other = bundle_with("pisowifi", &[("pisowifi", b"backdoored binary")]);
        let e = verify(&good, &other, Some(&key.verifying_key()), &expect("0.1.0")).unwrap_err();
        assert!(e.to_string().contains("doesn't match its signed checksum"), "{e}");

        // Flip one byte, keep the length.
        let mut flipped = bundle.clone();
        let mid = flipped.len() / 2;
        flipped[mid] ^= 1;
        assert!(verify(&good, &flipped, Some(&key.verifying_key()), &expect("0.1.0")).is_err());
    }

    #[test]
    fn the_wrong_board_or_product_is_refused() {
        let key = test_key(1);
        let bundle = bundle_with("pisowifi-hub", &[("pisowifi", b"x86 binary")]);
        let m = manifest_for(&bundle, Kind::Vendo, "x86_64-unknown-linux-musl", "0.2.0", "0.1.0");
        let e = verify(&signed(&key, &m), &bundle, Some(&key.verifying_key()), &expect("0.1.0")).unwrap_err();
        assert!(e.to_string().contains("different kind of board"), "{e}");

        let m = manifest_for(&bundle, Kind::Hub, TARGET_ARM, "0.2.0", "0.1.0");
        let e = verify(&signed(&key, &m), &bundle, Some(&key.verifying_key()), &expect("0.1.0")).unwrap_err();
        assert!(e.to_string().contains("for the hub, not a vendo"), "{e}");
    }

    #[test]
    fn older_or_same_versions_are_refused() {
        let key = test_key(1);
        let bundle = bundle_with("pisowifi", &[("pisowifi", b"old binary")]);
        for (v, current) in [("0.1.0", "0.2.0"), ("0.2.0", "0.2.0"), ("0.9.0", "0.10.0")] {
            let m = manifest_for(&bundle, Kind::Vendo, TARGET_ARM, v, "0.0.0");
            let e = verify(&signed(&key, &m), &bundle, Some(&key.verifying_key()), &expect(current)).unwrap_err();
            assert!(e.to_string().contains("not newer"), "{v} over {current}: {e}");
        }
        // install.sh may reinstall the same version, never an older one.
        let m = manifest_for(&bundle, Kind::Vendo, TARGET_ARM, "0.2.0", "0.0.0");
        let same = Expect { allow_same: true, ..expect("0.2.0") };
        assert!(verify(&signed(&key, &m), &bundle, Some(&key.verifying_key()), &same).is_ok());
        let older = Expect { allow_same: true, ..expect("0.3.0") };
        assert!(verify(&signed(&key, &m), &bundle, Some(&key.verifying_key()), &older).is_err());
    }

    #[test]
    fn a_too_old_box_must_step_through_min_from_version() {
        let key = test_key(1);
        let bundle = bundle_with("pisowifi", &[("pisowifi", b"bin")]);
        let m = manifest_for(&bundle, Kind::Vendo, TARGET_ARM, "0.5.0", "0.3.0");
        let e = verify(&signed(&key, &m), &bundle, Some(&key.verifying_key()), &expect("0.2.0")).unwrap_err();
        assert!(e.to_string().contains("Install version 0.3.0 first"), "{e}");
        assert!(verify(&signed(&key, &m), &bundle, Some(&key.verifying_key()), &expect("0.3.0")).is_ok());
    }

    #[test]
    fn unpacking_takes_only_listed_plain_files() {
        let dir = std::env::temp_dir().join(format!("pisowifi-unpack-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&dir).unwrap();
        let bundle = bundle_with(
            "pisowifi",
            &[("pisowifi", b"bin"), ("pisowifi.service", b"[Unit]"), ("install.sh", b"#!/bin/sh"), ("config.example.toml", b"x")],
        );
        unpack(&bundle, Kind::Vendo, &dir).unwrap();
        let mut names: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
        names.sort();
        assert_eq!(names, ["pisowifi", "pisowifi.service"]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(dir.join("pisowifi")).unwrap().permissions().mode() & 0o777, 0o755);
            assert_eq!(std::fs::metadata(dir.join("pisowifi.service")).unwrap().permissions().mode() & 0o777, 0o644);
        }
        let _ = std::fs::remove_dir_all(&dir);

        // A symlink posing as the binary, or no binary at all, is refused.
        std::fs::create_dir_all(&dir).unwrap();
        let mut b = tar::Builder::new(flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast()));
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Symlink);
        h.set_size(0);
        b.append_link(&mut h, "pisowifi/pisowifi", "/bin/sh").unwrap();
        let linky = b.into_inner().unwrap().finish().unwrap();
        assert!(unpack(&linky, Kind::Vendo, &dir).is_err());
        assert!(unpack(&bundle_with("pisowifi", &[("README.md", b"hi")]), Kind::Vendo, &dir).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sign_bundle_writes_a_manifest_that_verifies() {
        let dir = std::env::temp_dir().join(format!("pisowifi-sign-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&dir).unwrap();
        let key_path = dir.join("keys/release.key");
        let key = load_or_create_key(&key_path).unwrap();
        assert_eq!(load_or_create_key(&key_path).unwrap().to_bytes(), key.to_bytes(), "the key is created once");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&key_path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        let bundle_path = dir.join("pisowifi-armv7.tar.gz");
        let bundle = bundle_with("pisowifi", &[("pisowifi", b"bin")]);
        std::fs::write(&bundle_path, &bundle).unwrap();
        let args = SignArgs { kind: Kind::Vendo, target: TARGET_ARM, version: "0.2.0", min_from: "0.1.0", notes: " Notes. " };
        let out = sign_bundle(&key, &bundle_path, &args).unwrap();
        assert_eq!(out, dir.join("pisowifi-armv7.tar.gz.manifest"));
        let m = verify(&std::fs::read(&out).unwrap(), &bundle, Some(&key.verifying_key()), &expect("0.1.0")).unwrap();
        assert_eq!((m.version.as_str(), m.notes.as_str(), m.file.as_str()), ("0.2.0", "Notes.", "pisowifi-armv7.tar.gz"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn asset_names_match_build_pi() {
        assert_eq!(asset_name(Kind::Vendo, TARGET_ARM), "pisowifi-armv7.tar.gz");
        assert_eq!(asset_name(Kind::Hub, "x86_64-unknown-linux-musl"), "pisowifi-hub-x86_64.tar.gz");
    }
}
