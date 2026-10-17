//! Signed releases.
//!
//! GitHub CI builds every release and signs it with an Ed25519 key kept as
//! a repository secret (scripts/make-release-key.sh makes it, with a backup
//! in the owner's Mac Keychain). Every vendo and hub has the public half
//! compiled in (release-key.pub) and refuses anything that doesn't verify.
//!
//! The signature travels inside the bundle: release-manifest.json lists the
//! SHA-256 of every file next to it, plus the product, board, version and
//! notes, and is signed. One file is all a box needs (downloaded, or
//! uploaded in the dashboard when the vendo has no internet), and
//! * only the owner can publish an update: whoever gets hold of the file,
//!   the download server or the network can change it, but can't sign it;
//! * every file is pinned, and nothing unsigned can ride along;
//! * a signature can't be reused for the other product or another board;
//! * versions only go up, so an older signed release (one with a known bug,
//!   say) can't be replayed to downgrade a box.
//!
//! CI also publishes a signed update list (channels/stable.json) in the
//! public releases repository; boxes check it to learn what's newest.

use anyhow::{bail, Context, Result};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fmt;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use zeroize::Zeroizing;

/// Domain separation: these signatures can't be confused with any other use of the key.
const CONTEXT: &[u8] = b"pisowifi release manifest v2\n";
pub const MANIFEST_NAME: &str = "release-manifest.json";
pub const MAX_MANIFEST_BYTES: u64 = 64 * 1024;
pub const MAX_BUNDLE_BYTES: u64 = 64 * 1024 * 1024;
/// Unpacked size cap (a bundle is a few MB; this only stops a runaway gzip stream).
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

    /// Files an update installs; the rest of a bundle (installer scripts,
    /// examples) is only for first installs.
    pub fn files(self) -> &'static [&'static str] {
        match self {
            Kind::Vendo => &["pisowifi", "pisowifi.service", "pisowifi-update.service", "pisowifi-update.path", "README.md"],
            Kind::Hub => &[
                "pisowifi",
                "pisowifi-hub.service",
                "pisowifi-hub-update.service",
                "pisowifi-hub-autoupdate.service",
                "pisowifi-hub-autoupdate.timer",
                "README.md",
            ],
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

// ---- versions ------------------------------------------------------------------

/// MAJOR.MINOR.PATCH, numbers only. Pre-release tags are refused so the
/// ordering is never ambiguous.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(pub u32, pub u32, pub u32);

impl Version {
    pub fn parse(s: &str) -> Result<Version> {
        let num = |p: &str| -> Option<u32> {
            (!p.is_empty() && p.len() <= 9 && p.bytes().all(|b| b.is_ascii_digit())).then(|| p.parse().ok()).flatten()
        };
        match s.trim().split('.').collect::<Vec<_>>().as_slice() {
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
    /// Boxes older than this must install an intermediate release first.
    pub min_from_version: String,
    #[serde(default)]
    pub notes: String,
    /// Unix time it was signed.
    pub created: i64,
    /// SHA-256 (hex) of every file in the bundle, by name.
    pub files: BTreeMap<String, String>,
}

/// What release-manifest.json holds.
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

/// Signs a manifest; returns the file contents.
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
    if !sig.is_some_and(|s| key.verify_strict(&signed_message(&env.manifest), &s).is_ok()) {
        let by = if env.key_id.len() == 16 && env.key_id.bytes().all(|b| b.is_ascii_hexdigit()) { env.key_id.as_str() } else { "unknown" };
        bail!("This update isn't signed with your release key (signed by {by}, expected {}). Nothing was changed.", key_id(key));
    }
    serde_json::from_str(&env.manifest).context("signed manifest doesn't parse")
}

/// What the installing box expects.
pub struct Expect<'a> {
    pub kind: Kind,
    pub target: &'a str,
    /// Installed version; `None` when it doesn't matter (signing, checking a file).
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
    if !m.files.contains_key("pisowifi") {
        bail!("The update has no pisowifi program in it.");
    }
    for (name, sha) in &m.files {
        if !plain_name(name) || name == MANIFEST_NAME || sha.len() != 64 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
            bail!("The update's file list is malformed.");
        }
    }
    Ok(v)
}

fn plain_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 100 && name != "." && name != ".." && !name.contains(['/', '\\', '\0'])
}

// ---- bundles ---------------------------------------------------------------------

struct Entry {
    name: String,
    mode: u32,
    mtime: u64,
    data: Vec<u8>,
}

/// A bundle read into memory: one top folder of plain files. Anything else
/// (links, devices, sub-folders, paths that climb out) is refused.
struct Archive {
    top: String,
    entries: Vec<Entry>,
}

fn read_archive(bundle: &[u8]) -> Result<Archive> {
    let damaged = || anyhow::anyhow!("This isn't a Piso WiFi update file (or it's damaged).");
    if bundle.len() as u64 > MAX_BUNDLE_BYTES {
        bail!("The update file is too large.");
    }
    let gz = flate2::read::GzDecoder::new(bundle).take(MAX_UNPACKED_BYTES);
    let mut tar = tar::Archive::new(gz);
    let mut top: Option<String> = None;
    let mut entries: Vec<Entry> = Vec::new();
    for entry in tar.entries().map_err(|_| damaged())? {
        let mut entry = entry.map_err(|_| damaged())?;
        let path = entry.path().map_err(|_| damaged())?.into_owned();
        let mut parts = Vec::new();
        for c in path.components() {
            match c {
                Component::Normal(p) => parts.push(p.to_str().ok_or_else(damaged)?.to_string()),
                Component::CurDir => {}
                _ => bail!("The update file has an unsafe path in it ({}).", path.display()),
            }
        }
        let kind = entry.header().entry_type();
        let (dir, name) = match parts.as_slice() {
            [dir] if kind.is_dir() => (dir.clone(), None),
            [dir, name] if kind.is_file() && plain_name(name) => (dir.clone(), Some(name.clone())),
            _ => bail!("The update file contains something it shouldn't ({}).", path.display()),
        };
        if !matches!(dir.as_str(), "pisowifi" | "pisowifi-hub") || top.as_ref().is_some_and(|t| *t != dir) {
            bail!("The update file contains something it shouldn't ({}).", path.display());
        }
        top = Some(dir);
        let Some(name) = name else { continue };
        if entries.iter().any(|e| e.name == name) {
            bail!("{name} appears twice in the update file.");
        }
        let (mode, mtime) = (entry.header().mode().unwrap_or(0o644), entry.header().mtime().unwrap_or(0));
        let mut data = Vec::new();
        entry.read_to_end(&mut data).map_err(|_| damaged())?;
        entries.push(Entry { name, mode, mtime, data });
    }
    Ok(Archive { top: top.ok_or_else(damaged)?, entries })
}

/// A bundle whose signature and files all checked out.
pub struct Verified {
    pub manifest: Manifest,
    entries: Vec<Entry>,
}

/// Everything at once: signature, product, board, version, every file.
pub fn verify(bundle: &[u8], key: Option<&VerifyingKey>, e: &Expect) -> Result<Verified> {
    let key = key.ok_or_else(no_key_error)?;
    let archive = read_archive(bundle)?;
    let (signed, entries): (Vec<_>, Vec<_>) = archive.entries.into_iter().partition(|e| e.name == MANIFEST_NAME);
    let Some(signed) = signed.into_iter().next() else {
        bail!("This file isn't signed. Use the bundle from the releases page (github.com/cplus2jules/pisowifi-releases).");
    };
    let m = open(&signed.data, key)?;
    check(&m, e)?;
    let mismatch = || anyhow::anyhow!("The update file doesn't match what was signed (damaged or tampered with). Nothing was changed.");
    if entries.len() != m.files.len() {
        return Err(mismatch());
    }
    for entry in &entries {
        let want = m.files.get(&entry.name).ok_or_else(mismatch)?;
        if !hex::encode(Sha256::digest(&entry.data)).eq_ignore_ascii_case(want) {
            return Err(mismatch());
        }
    }
    Ok(Verified { manifest: m, entries })
}

impl Verified {
    /// Writes the files an update installs into `dest`.
    pub fn unpack(&self, kind: Kind, dest: &Path) -> Result<()> {
        for e in self.entries.iter().filter(|e| kind.files().contains(&e.name.as_str())) {
            write_file(&dest.join(&e.name), &e.data, if e.name == "pisowifi" { 0o755 } else { 0o644 })?;
        }
        Ok(())
    }
}

/// The installable files of an unpacked bundle folder (install.sh's upgrade mode).
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
    let mut f = opts.open(path).with_context(|| format!("creating {}", path.display()))?;
    // Exactly `mode`, whatever the umask (the hub's service user must be
    // able to run the binary).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        f.set_permissions(std::fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    let _ = mode;
    f.write_all(data)?;
    f.sync_all()?;
    Ok(())
}

pub fn no_key_error() -> anyhow::Error {
    anyhow::anyhow!("This copy was built without a release key, so it can't install updates. Reinstall it once from a signed bundle (github.com/cplus2jules/pisowifi-releases).")
}

/// The owner's release key, compiled in from release-key.pub.
pub fn trusted_key() -> Option<VerifyingKey> {
    parse_public(env!("PISOWIFI_RELEASE_PUBKEY")).ok()
}

pub fn parse_public(text: &str) -> Result<VerifyingKey> {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty() && !l.starts_with('#')).unwrap_or("");
    let bytes: [u8; 32] = hex::decode(line).ok().and_then(|b| b.try_into().ok()).context("not a 64-character hex key")?;
    VerifyingKey::from_bytes(&bytes).context("not a valid Ed25519 public key")
}

// ---- the update list -----------------------------------------------------------------

/// Vendos and the hub learn what's newest from a small signed file:
/// channels/stable.json in the public releases repository, with its
/// signature in stable.json.sig. It names each board's bundle by path,
/// SHA-256 and size, so a box downloads only its own file and knows exactly
/// what to expect before the bundle's own signature is even looked at.
const CHANNEL_CONTEXT: &[u8] = b"pisowifi release channel v1\n";
pub const MAX_CHANNEL_BYTES: u64 = 64 * 1024;
pub const MAX_CHANNEL_SIG_BYTES: u64 = 1024;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Channel {
    /// Always "pisowifi".
    pub product: String,
    /// "stable".
    pub channel: String,
    pub version: String,
    /// Unix time CI signed it. A box ignores a list older than the newest one
    /// it has seen, so an old list can't be replayed to hide an update.
    pub issued_at: i64,
    #[serde(default)]
    pub notes: String,
    pub files: Vec<ChannelFile>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChannelFile {
    pub kind: Kind,
    pub target: String,
    /// Relative to the list itself, e.g. ../v0.2.0/pisowifi-armv7.tar.gz.
    pub path: String,
    pub sha256: String,
    pub size: u64,
}

impl Channel {
    /// This box's bundle, if the release has one for it.
    pub fn file(&self, kind: Kind, target: &str) -> Option<&ChannelFile> {
        self.files.iter().find(|f| f.kind == kind && f.target == target)
    }
}

fn channel_message(json: &[u8]) -> Vec<u8> {
    [CHANNEL_CONTEXT, json].concat()
}

/// The .sig file for a list: the signature in hex.
pub fn sign_channel(key: &SigningKey, json: &[u8]) -> String {
    hex::encode(key.sign(&channel_message(json)).to_bytes()) + "\n"
}

/// Checks the signature, then parses. As with manifests, nothing in the list
/// is looked at before the signature is known to be good.
pub fn open_channel(json: &[u8], sig: &[u8], key: &VerifyingKey) -> Result<Channel> {
    if json.len() as u64 > MAX_CHANNEL_BYTES || sig.len() as u64 > MAX_CHANNEL_SIG_BYTES {
        bail!("The update list is too large.");
    }
    let sig = std::str::from_utf8(sig)
        .ok()
        .and_then(|s| hex::decode(s.trim()).ok())
        .and_then(|b| <[u8; 64]>::try_from(b).ok())
        .map(|b| Signature::from_bytes(&b));
    if !sig.is_some_and(|s| key.verify_strict(&channel_message(json), &s).is_ok()) {
        bail!("The update list isn't signed with your release key, so it was ignored.");
    }
    let c: Channel = serde_json::from_slice(json).context("signed update list doesn't parse")?;
    if c.product != "pisowifi" {
        bail!("This isn't a Piso WiFi update list.");
    }
    Version::parse(&c.version)?;
    if c.notes.chars().count() > MAX_NOTES_CHARS {
        bail!("The release notes are too long.");
    }
    for f in &c.files {
        let sha_ok = f.sha256.len() == 64 && f.sha256.bytes().all(|b| b.is_ascii_hexdigit());
        if !channel_path_ok(&f.path) || !sha_ok || f.size == 0 || f.size > MAX_BUNDLE_BYTES {
            bail!("The update list is malformed.");
        }
    }
    Ok(c)
}

/// Leading `..` segments, then plain names (letters, digits, `.`, `_`, `-`, `+`).
fn channel_path_ok(path: &str) -> bool {
    let mut parts = path.split('/').peekable();
    while parts.peek() == Some(&"..") {
        parts.next();
    }
    let rest: Vec<&str> = parts.collect();
    path.len() <= 200
        && !rest.is_empty()
        && rest.iter().all(|p| !p.is_empty() && *p != "." && *p != ".." && p.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-+".contains(&b)))
}

/// Where a file named in the list is: its path, relative to the list's own address.
pub fn channel_file_url(channel_url: &str, path: &str) -> Result<String> {
    if !channel_path_ok(path) {
        bail!("The update list is malformed.");
    }
    let (scheme, rest) = channel_url.split_once("://").context("not a web address")?;
    let rest = rest.split(['?', '#']).next().unwrap_or_default();
    // [host, dir, ..., list.json]
    let mut segs: Vec<&str> = rest.split('/').collect();
    segs.pop();
    for p in path.split('/') {
        if p == ".." {
            if segs.len() <= 1 {
                bail!("The update list points outside its server.");
            }
            segs.pop();
        } else {
            segs.push(p);
        }
    }
    Ok(format!("{scheme}://{}", segs.join("/")))
}

// ---- signing (CI, and the one-time key setup) ---------------------------------------

/// A new release key: (secret as hex, public as hex).
pub fn new_key() -> (Zeroizing<String>, String) {
    let k = SigningKey::generate(&mut rand::rngs::OsRng);
    (Zeroizing::new(hex::encode(k.to_bytes())), hex::encode(k.verifying_key().as_bytes()))
}

/// The secret release key from an environment variable (a CI secret), hex.
pub fn key_from_env(name: &str) -> Result<SigningKey> {
    let text = Zeroizing::new(std::env::var(name).map_err(|_| anyhow::anyhow!("{name} isn't set"))?);
    let mut seed = Zeroizing::new([0u8; 32]);
    hex::decode_to_slice(text.trim(), &mut seed[..]).map_err(|_| anyhow::anyhow!("{name} must be 64 hex characters"))?;
    Ok(SigningKey::from_bytes(&seed))
}

pub fn public_file(public_hex: &str) -> String {
    format!("# Piso WiFi release key (Ed25519, public half). Vendos and hubs built\n# with this file only install updates signed by the matching secret key.\n{public_hex}\n")
}

pub struct SignArgs<'a> {
    pub kind: Kind,
    pub target: &'a str,
    pub version: &'a str,
    pub min_from: &'a str,
    pub notes: &'a str,
}

/// Signs a bundle in place: release-manifest.json goes inside it, and
/// <bundle>.sha256 is refreshed if present.
/// Returns the signed bundle's SHA-256.
pub fn sign_bundle(key: &SigningKey, bundle: &Path, a: &SignArgs) -> Result<String> {
    let archive = read_archive(&std::fs::read(bundle).with_context(|| format!("reading {}", bundle.display()))?)?;
    // An older signature goes; so do macOS resource-fork files.
    let entries: Vec<Entry> = archive.entries.into_iter().filter(|e| e.name != MANIFEST_NAME && !e.name.starts_with("._")).collect();
    let m = Manifest {
        product: "pisowifi".into(),
        kind: a.kind,
        version: Version::parse(a.version)?.to_string(),
        target: a.target.into(),
        min_from_version: Version::parse(a.min_from)?.to_string(),
        notes: a.notes.trim().into(),
        created: chrono::Utc::now().timestamp(),
        files: entries.iter().map(|e| (e.name.clone(), hex::encode(Sha256::digest(&e.data)))).collect(),
    };
    // The same checks a box makes, so a bad release fails here, not in the field.
    check(&m, &Expect { kind: a.kind, target: a.target, current: None, allow_same: false })?;
    let signed = sign(key, &m)?;
    let mtime = entries.iter().map(|e| e.mtime).max().unwrap_or(0);
    let mut all = entries;
    all.push(Entry { name: MANIFEST_NAME.into(), mode: 0o644, mtime, data: signed.into_bytes() });
    let out = build_archive(&archive.top, &all, mtime)?;
    let tmp = bundle.with_extension("signing");
    std::fs::write(&tmp, &out)?;
    std::fs::rename(&tmp, bundle)?;
    let sha = hex::encode(Sha256::digest(&out));
    let mut sum = bundle.as_os_str().to_owned();
    sum.push(".sha256");
    let sum = PathBuf::from(sum);
    if sum.exists() {
        let name = bundle.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        std::fs::write(&sum, format!("{sha}  {name}\n"))?;
    }
    // Belt and braces: what was written must verify.
    verify(&out, Some(&key.verifying_key()), &Expect { kind: a.kind, target: a.target, current: None, allow_same: false })?;
    Ok(sha)
}

/// Writes <out_dir>/<name>.json and <name>.json.sig for a set of signed
/// bundles of one release. Each bundle is checked in full first; the list
/// points at them as ../v<version>/<file name>, which is where CI puts them
/// in the releases repository.
pub fn write_channel(key: &SigningKey, name: &str, bundles: &[PathBuf], out_dir: &Path) -> Result<Channel> {
    if name.is_empty() || !name.bytes().all(|b| b.is_ascii_lowercase()) {
        bail!("channel name must be lowercase letters");
    }
    let public = key.verifying_key();
    let mut files = Vec::new();
    let mut release: Option<(String, String)> = None;
    for path in bundles {
        let data = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let archive = read_archive(&data)?;
        let signed = archive.entries.iter().find(|e| e.name == MANIFEST_NAME).with_context(|| format!("{} isn't signed", path.display()))?;
        let m = open(&signed.data, &public)?;
        verify(&data, Some(&public), &Expect { kind: m.kind, target: &m.target, current: None, allow_same: false })?;
        match &release {
            None => release = Some((m.version.clone(), m.notes.clone())),
            Some((v, n)) if *v == m.version && *n == m.notes => {}
            Some(_) => bail!("{} is from a different release than the other bundles", path.display()),
        }
        if files.iter().any(|f: &ChannelFile| f.kind == m.kind && f.target == m.target) {
            bail!("two bundles for {:?} {}", m.kind, m.target);
        }
        let file_name = path.file_name().and_then(|n| n.to_str()).context("bundle file name")?;
        files.push(ChannelFile {
            kind: m.kind,
            target: m.target.clone(),
            path: format!("../v{}/{file_name}", m.version),
            sha256: hex::encode(Sha256::digest(&data)),
            size: data.len() as u64,
        });
    }
    let (version, notes) = release.context("no bundles given")?;
    let c = Channel { product: "pisowifi".into(), channel: name.into(), version, issued_at: chrono::Utc::now().timestamp(), notes, files };
    let json = serde_json::to_string_pretty(&c)? + "\n";
    let sig = sign_channel(key, json.as_bytes());
    std::fs::create_dir_all(out_dir)?;
    std::fs::write(out_dir.join(format!("{name}.json")), &json)?;
    std::fs::write(out_dir.join(format!("{name}.json.sig")), &sig)?;
    // What was written must open, the same way a box opens it.
    let back = open_channel(&std::fs::read(out_dir.join(format!("{name}.json")))?, &std::fs::read(out_dir.join(format!("{name}.json.sig")))?, &public)?;
    if back != c {
        bail!("the written update list doesn't read back the same");
    }
    Ok(c)
}

fn build_archive(top: &str, entries: &[Entry], mtime: u64) -> Result<Vec<u8>> {
    let mut b = tar::Builder::new(flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best()));
    let mut h = tar::Header::new_gnu();
    h.set_entry_type(tar::EntryType::Directory);
    h.set_mode(0o755);
    h.set_size(0);
    h.set_mtime(mtime);
    b.append_data(&mut h, format!("{top}/"), std::io::empty())?;
    for e in entries {
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Regular);
        h.set_mode(e.mode & 0o777);
        h.set_size(e.data.len() as u64);
        h.set_mtime(e.mtime);
        b.append_data(&mut h, format!("{top}/{}", e.name), &e.data[..])?;
    }
    Ok(b.into_inner()?.finish()?)
}

#[cfg(test)]
pub mod tests {
    use super::*;

    pub const TARGET_ARM: &str = "armv7-unknown-linux-musleabihf";

    pub fn test_key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    /// An unsigned bundle shaped like build-pi.sh's output.
    pub fn bundle_with(top: &str, files: &[(&str, &[u8])]) -> Vec<u8> {
        let entries: Vec<Entry> = files.iter().map(|(n, d)| Entry { name: n.to_string(), mode: 0o755, mtime: 1, data: d.to_vec() }).collect();
        build_archive(top, &entries, 1).unwrap()
    }

    /// A bundle signed the way CI signs it.
    pub fn signed_bundle(key: &SigningKey, kind: Kind, target: &str, version: &str, min_from: &str, files: &[(&str, &[u8])]) -> Vec<u8> {
        let dir = std::env::temp_dir().join(format!("pisowifi-sign-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(asset_name(kind, target));
        let top = if kind == Kind::Hub { "pisowifi-hub" } else { "pisowifi" };
        std::fs::write(&path, bundle_with(top, files)).unwrap();
        let args = SignArgs { kind, target, version, min_from, notes: "Faster coin counting." };
        sign_bundle(key, &path, &args).unwrap();
        let out = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        out
    }

    /// Re-packs a signed bundle with `f` applied to its entries (to tamper).
    fn repack(bundle: &[u8], f: impl FnOnce(&mut Vec<Entry>)) -> Vec<u8> {
        let mut a = read_archive(bundle).unwrap();
        f(&mut a.entries);
        build_archive(&a.top, &a.entries, 1).unwrap()
    }

    fn expect(current: &str) -> Expect<'static> {
        Expect { kind: Kind::Vendo, target: TARGET_ARM, current: Some(Version::parse(current).unwrap()), allow_same: false }
    }

    const BIN: &[u8] = b"new binary";

    fn good(key: &SigningKey, version: &str) -> Vec<u8> {
        signed_bundle(key, Kind::Vendo, TARGET_ARM, version, "0.1.0", &[("pisowifi", BIN), ("pisowifi.service", b"[Unit]"), ("install.sh", b"#!/bin/sh")])
    }

    fn err(r: Result<Verified>) -> String {
        match r {
            Ok(_) => panic!("accepted"),
            Err(e) => e.to_string(),
        }
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
    fn a_good_bundle_verifies_and_unpacks_only_installable_files() {
        let key = test_key(1);
        let v = verify(&good(&key, "0.2.0"), Some(&key.verifying_key()), &expect("0.1.0")).unwrap();
        assert_eq!(v.manifest.version, "0.2.0");
        assert_eq!(v.manifest.files.keys().collect::<Vec<_>>(), ["install.sh", "pisowifi", "pisowifi.service"]);
        let dir = std::env::temp_dir().join(format!("pisowifi-unpack-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&dir).unwrap();
        v.unpack(Kind::Vendo, &dir).unwrap();
        let mut names: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
        names.sort();
        assert_eq!(names, ["pisowifi", "pisowifi.service"]);
        assert_eq!(std::fs::read(dir.join("pisowifi")).unwrap(), BIN);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(dir.join("pisowifi")).unwrap().permissions().mode() & 0o777, 0o755);
            assert_eq!(std::fs::metadata(dir.join("pisowifi.service")).unwrap().permissions().mode() & 0o777, 0o644);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unsigned_or_foreign_signed_bundle_is_refused() {
        let (owner, attacker) = (test_key(1), test_key(2));
        let unsigned = bundle_with("pisowifi", &[("pisowifi", b"evil")]);
        assert!(err(verify(&unsigned, Some(&owner.verifying_key()), &expect("0.1.0"))).contains("isn't signed"));
        let foreign = good(&attacker, "9.0.0");
        assert!(err(verify(&foreign, Some(&owner.verifying_key()), &expect("0.1.0"))).contains("isn't signed with your release key"));
        assert!(err(verify(&good(&owner, "0.2.0"), None, &expect("0.1.0"))).contains("without a release key"));
        assert!(err(verify(b"not a bundle", Some(&owner.verifying_key()), &expect("0.1.0"))).contains("isn't a Piso WiFi update"));
    }

    #[test]
    fn tampering_with_any_file_or_the_manifest_is_caught() {
        let key = test_key(1);
        let pk = key.verifying_key();
        let bundle = good(&key, "0.2.0");
        let swapped = repack(&bundle, |es| es.iter_mut().find(|e| e.name == "pisowifi").unwrap().data = b"backdoor".to_vec());
        assert!(err(verify(&swapped, Some(&pk), &expect("0.1.0"))).contains("doesn't match what was signed"));
        let scripted = repack(&bundle, |es| es.iter_mut().find(|e| e.name == "install.sh").unwrap().data.push(b'!'));
        assert!(err(verify(&scripted, Some(&pk), &expect("0.1.0"))).contains("doesn't match"));
        let extra = repack(&bundle, |es| es.push(Entry { name: "extra.sh".into(), mode: 0o755, mtime: 1, data: b"x".to_vec() }));
        assert!(err(verify(&extra, Some(&pk), &expect("0.1.0"))).contains("doesn't match"));
        let missing = repack(&bundle, |es| es.retain(|e| e.name != "pisowifi.service"));
        assert!(err(verify(&missing, Some(&pk), &expect("0.1.0"))).contains("doesn't match"));
        // Raise the version inside the signed manifest without re-signing.
        let bumped = repack(&bundle, |es| {
            let m = es.iter_mut().find(|e| e.name == MANIFEST_NAME).unwrap();
            m.data = String::from_utf8(m.data.clone()).unwrap().replace("0.2.0", "0.3.0").into_bytes();
        });
        assert!(err(verify(&bumped, Some(&pk), &expect("0.1.0"))).contains("isn't signed with your release key"));
        // Flip a byte of the compressed file.
        let mut flipped = bundle.clone();
        let mid = flipped.len() / 2;
        flipped[mid] ^= 1;
        assert!(verify(&flipped, Some(&pk), &expect("0.1.0")).is_err());
    }

    #[test]
    fn links_and_odd_paths_are_refused() {
        let key = test_key(1);
        let mut b = tar::Builder::new(flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast()));
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Symlink);
        h.set_size(0);
        b.append_link(&mut h, "pisowifi/pisowifi", "/bin/sh").unwrap();
        let linky = b.into_inner().unwrap().finish().unwrap();
        assert!(err(verify(&linky, Some(&key.verifying_key()), &expect("0.1.0"))).contains("shouldn't"));
        for path in ["pisowifi/sub/pisowifi", "other/pisowifi", "pisowifi"] {
            let mut b = tar::Builder::new(flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast()));
            let mut h = tar::Header::new_gnu();
            h.set_size(1);
            h.set_mode(0o644);
            b.append_data(&mut h, path, &b"x"[..]).unwrap();
            let odd = b.into_inner().unwrap().finish().unwrap();
            assert!(err(verify(&odd, Some(&key.verifying_key()), &expect("0.1.0"))).contains("shouldn't"), "{path}");
        }
    }

    #[test]
    fn the_wrong_board_or_product_is_refused() {
        let key = test_key(1);
        let x86 = signed_bundle(&key, Kind::Vendo, "x86_64-unknown-linux-musl", "0.2.0", "0.1.0", &[("pisowifi", BIN)]);
        assert!(err(verify(&x86, Some(&key.verifying_key()), &expect("0.1.0"))).contains("different kind of board"));
        let hub = signed_bundle(&key, Kind::Hub, TARGET_ARM, "0.2.0", "0.1.0", &[("pisowifi", BIN)]);
        assert!(err(verify(&hub, Some(&key.verifying_key()), &expect("0.1.0"))).contains("for the hub, not a vendo"));
    }

    #[test]
    fn older_or_same_versions_are_refused() {
        let key = test_key(1);
        for (v, current) in [("0.1.0", "0.2.0"), ("0.2.0", "0.2.0"), ("0.9.0", "0.10.0")] {
            let b = signed_bundle(&key, Kind::Vendo, TARGET_ARM, v, "0.0.0", &[("pisowifi", BIN)]);
            assert!(err(verify(&b, Some(&key.verifying_key()), &expect(current))).contains("not newer"), "{v} over {current}");
        }
        // install.sh may reinstall the same version, never an older one.
        let b = signed_bundle(&key, Kind::Vendo, TARGET_ARM, "0.2.0", "0.0.0", &[("pisowifi", BIN)]);
        assert!(verify(&b, Some(&key.verifying_key()), &Expect { allow_same: true, ..expect("0.2.0") }).is_ok());
        assert!(verify(&b, Some(&key.verifying_key()), &Expect { allow_same: true, ..expect("0.3.0") }).is_err());
    }

    #[test]
    fn a_too_old_box_must_step_through_min_from_version() {
        let key = test_key(1);
        let b = signed_bundle(&key, Kind::Vendo, TARGET_ARM, "0.5.0", "0.3.0", &[("pisowifi", BIN)]);
        assert!(err(verify(&b, Some(&key.verifying_key()), &expect("0.2.0"))).contains("Install version 0.3.0 first"));
        assert!(verify(&b, Some(&key.verifying_key()), &expect("0.3.0")).is_ok());
    }

    #[test]
    fn signing_in_place_replaces_the_signature_and_checksum() {
        let dir = std::env::temp_dir().join(format!("pisowifi-sign-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("pisowifi-armv7.tar.gz");
        std::fs::write(&path, bundle_with("pisowifi", &[("pisowifi", BIN), ("._pisowifi", b"mac junk")])).unwrap();
        std::fs::write(dir.join("pisowifi-armv7.tar.gz.sha256"), "stale").unwrap();
        let key = test_key(3);
        let args = SignArgs { kind: Kind::Vendo, target: TARGET_ARM, version: "0.2.0", min_from: "0.1.0", notes: " Notes. " };
        let sha = sign_bundle(&key, &path, &args).unwrap();
        // Signing again replaces the old signature rather than signing it.
        let sha2 = sign_bundle(&key, &path, &args).unwrap();
        let bundle = std::fs::read(&path).unwrap();
        assert_eq!(hex::encode(Sha256::digest(&bundle)), sha2);
        assert_ne!(sha, "");
        let v = verify(&bundle, Some(&key.verifying_key()), &expect("0.1.0")).unwrap();
        assert_eq!((v.manifest.notes.as_str(), v.manifest.files.len()), ("Notes.", 1), "the resource fork was dropped");
        assert_eq!(std::fs::read_to_string(dir.join("pisowifi-armv7.tar.gz.sha256")).unwrap(), format!("{sha2}  pisowifi-armv7.tar.gz\n"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn keys_come_from_hex_and_the_public_file_parses() {
        let (secret, public) = new_key();
        std::env::set_var("PISOWIFI_TEST_RELEASE_KEY", secret.as_str());
        let k = key_from_env("PISOWIFI_TEST_RELEASE_KEY").unwrap();
        assert_eq!(hex::encode(k.verifying_key().as_bytes()), public);
        assert_eq!(parse_public(&public_file(&public)).unwrap(), k.verifying_key());
        std::env::set_var("PISOWIFI_TEST_RELEASE_KEY", "not hex");
        assert!(key_from_env("PISOWIFI_TEST_RELEASE_KEY").is_err());
        assert!(key_from_env("PISOWIFI_TEST_UNSET_KEY").is_err());
    }

    #[test]
    fn asset_names_match_build_pi() {
        assert_eq!(asset_name(Kind::Vendo, TARGET_ARM), "pisowifi-armv7.tar.gz");
        assert_eq!(asset_name(Kind::Hub, "x86_64-unknown-linux-musl"), "pisowifi-hub-x86_64.tar.gz");
    }

    pub const TARGET_HUB: &str = "x86_64-unknown-linux-musl";

    /// A release the way CI lays it out: both bundles in dist/, and the
    /// update list written next to them. Returns (dir, list, sig).
    pub fn signed_release(key: &SigningKey, version: &str, vendo_target: &str) -> (PathBuf, Vec<u8>, Vec<u8>) {
        let dir = std::env::temp_dir().join(format!("pisowifi-channel-{}", rand::random::<u64>()));
        std::fs::create_dir_all(dir.join("dist")).unwrap();
        let mut paths = Vec::new();
        for (kind, target, unit) in [(Kind::Vendo, vendo_target, "pisowifi.service"), (Kind::Hub, TARGET_HUB, "pisowifi-hub.service")] {
            let data = signed_bundle(key, kind, target, version, "0.1.0", &[("pisowifi", b"#!/bin/sh\n"), (unit, b"[Unit]\n")]);
            let path = dir.join("dist").join(asset_name(kind, target));
            std::fs::write(&path, data).unwrap();
            paths.push(path);
        }
        write_channel(key, "stable", &paths, &dir.join("channels")).unwrap();
        let list = std::fs::read(dir.join("channels/stable.json")).unwrap();
        let sig = std::fs::read(dir.join("channels/stable.json.sig")).unwrap();
        (dir, list, sig)
    }

    #[test]
    fn the_update_list_is_signed_and_names_each_bundle() {
        let key = test_key(7);
        let (dir, list, sig) = signed_release(&key, "1.2.0", TARGET_ARM);
        let c = open_channel(&list, &sig, &key.verifying_key()).unwrap();
        assert_eq!((c.product.as_str(), c.channel.as_str(), c.version.as_str()), ("pisowifi", "stable", "1.2.0"));
        assert_eq!(c.notes, "Faster coin counting.");
        let vendo = c.file(Kind::Vendo, TARGET_ARM).unwrap();
        let bundle = std::fs::read(dir.join("dist/pisowifi-armv7.tar.gz")).unwrap();
        assert_eq!(vendo.path, "../v1.2.0/pisowifi-armv7.tar.gz");
        assert_eq!(vendo.sha256, hex::encode(Sha256::digest(&bundle)));
        assert_eq!(vendo.size, bundle.len() as u64);
        assert_eq!(c.file(Kind::Hub, TARGET_HUB).unwrap().path, "../v1.2.0/pisowifi-hub-x86_64.tar.gz");
        assert!(c.file(Kind::Hub, TARGET_ARM).is_none());
        let url = "https://raw.githubusercontent.com/cplus2jules/pisowifi-releases/main/channels/stable.json";
        assert_eq!(
            channel_file_url(url, &vendo.path).unwrap(),
            "https://raw.githubusercontent.com/cplus2jules/pisowifi-releases/main/v1.2.0/pisowifi-armv7.tar.gz"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_changed_or_foreign_update_list_is_refused() {
        let key = test_key(7);
        let (dir, list, sig) = signed_release(&key, "1.2.0", TARGET_ARM);
        let pk = key.verifying_key();
        // Every byte is covered: a changed version, size or path is caught.
        let text = String::from_utf8(list.clone()).unwrap();
        for changed in [text.replace("1.2.0", "9.9.9"), text.replace("\"size\": ", "\"size\": 1"), text.replace("../v1.2.0/", "../v1.1.0/")] {
            assert!(open_channel(changed.as_bytes(), &sig, &pk).unwrap_err().to_string().contains("isn't signed"));
        }
        // Another key, a damaged or missing signature.
        assert!(open_channel(&list, &sig, &test_key(8).verifying_key()).is_err());
        assert!(open_channel(&list, b"00", &pk).is_err());
        assert!(open_channel(&list, b"", &pk).is_err());
        // A manifest signature can't stand in for the list's (domain separation).
        let as_manifest = hex::encode(key.sign(&signed_message(std::str::from_utf8(&list).unwrap())).to_bytes());
        assert!(open_channel(&list, as_manifest.as_bytes(), &pk).is_err());
        // Even signed, a path may only climb out with leading `..` and never leave the server.
        for bad in ["/etc/passwd", "v1/../../x", "v1//x", "", "v1/./x", "v1/x y"] {
            assert!(!channel_path_ok(bad), "{bad}");
        }
        assert!(channel_file_url("https://host/stable.json", "../x.tar.gz").is_err());
        assert_eq!(channel_file_url("https://host/a/stable.json?x=1", "b.tar.gz").unwrap(), "https://host/a/b.tar.gz");
        let mut c = open_channel(&list, &sig, &pk).unwrap();
        c.files[0].path = "../../../../etc/passwd/x".into();
        let json = serde_json::to_vec(&c).unwrap();
        let resigned = sign_channel(&key, &json);
        let c = open_channel(&json, resigned.as_bytes(), &pk).unwrap();
        assert!(channel_file_url("https://host/a/stable.json", &c.files[0].path).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_update_list_covers_one_release_only() {
        let key = test_key(7);
        let dir = std::env::temp_dir().join(format!("pisowifi-channel-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.tar.gz");
        let b = dir.join("b.tar.gz");
        std::fs::write(&a, signed_bundle(&key, Kind::Vendo, TARGET_ARM, "1.2.0", "0.1.0", &[("pisowifi", b"x")])).unwrap();
        std::fs::write(&b, signed_bundle(&key, Kind::Hub, TARGET_HUB, "1.3.0", "0.1.0", &[("pisowifi", b"x")])).unwrap();
        assert!(write_channel(&key, "stable", &[a.clone(), b], &dir).is_err());
        // Only bundles signed with the same key.
        assert!(write_channel(&test_key(8), "stable", &[a], &dir).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
