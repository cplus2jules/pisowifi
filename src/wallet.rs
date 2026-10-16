//! Wallet passes for Piso Pass: "Add to Google Wallet" and "Add to Apple
//! Wallet", so customers keep their pass with their cards and boarding passes.
//!
//! Flow
//! * The portal asks its vendo, and the vendo asks the hub with a signed
//!   `POST /v1/wallet`. The hub checks the code (HMAC lookup), builds the
//!   pass and keeps it under a random, single-use token for 10 minutes.
//! * The phone (online, since the customer has paid) opens
//!   `/v1/wallet/apple/<token>.pkpass` or `/v1/wallet/google/<token>` on the hub.
//! * The pass shows the code as a QR code and text, plus a "Use my time" link
//!   to the portal (`http://10.0.0.1/?pass=CODE`) that claims it at any vendo.
//!   Balances aren't shown: they change every second and would need push updates.
//!
//! Security
//! * Passes are only built on the hub. The Google service-account key and the
//!   Apple pass certificate never go on a vendo.
//! * Tokens are random and never contain the code. The built pass (which does)
//!   lives in memory only, until it's downloaded or expires.
//!
//! Not yet verified against the real services: this follows Google's and
//! Apple's published formats, but no issuer account or pass certificate existed
//! when it was written. See README → Wallet passes.

use crate::hub::proto::{WalletKind, WalletSupport};
use anyhow::{anyhow, bail, Context, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;
use base64::Engine as _;
use der::{Decode, Encode};
use rand::Rng;
use ring::signature::RsaKeyPair;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use serde::Deserialize;
use serde_json::{json, Value};
use sha1::Sha1;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;
use x509_cert::Certificate;
use zeroize::Zeroizing;

/// How long a download link works.
pub const TOKEN_TTL_SECS: i64 = 600;
/// Passes waiting to be downloaded, at most (memory guard: a .pkpass is ~30 KB).
const MAX_WAITING: usize = 1000;
/// Waiting links per pass: a new one pushes out the oldest, so one customer
/// tapping again and again can't fill the queue for everyone.
const MAX_WAITING_PER_PASS: usize = 2;
/// Longest brand name: wallets cut off longer ones on the pass's front.
pub const MAX_BRAND_CHARS: usize = 24;
/// Apple shows a pass on the lock screen near at most 10 places.
const MAX_LOCATIONS: usize = 10;

// ---- config -------------------------------------------------------------------

/// `[wallet]` in hub.toml. Each provider is on only when all its lines are set.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WalletConfig {
    /// Shown on the pass ("Juan's WiFi").
    #[serde(default = "default_brand")]
    pub brand_name: String,
    /// Pass colours, "#RRGGBB".
    #[serde(default = "default_background")]
    pub background: String,
    #[serde(default = "default_foreground")]
    pub foreground: String,
    #[serde(default = "default_label")]
    pub label: String,
    /// Where "Use my time" points: the portal address all your vendos share.
    #[serde(default = "default_portal_url")]
    pub portal_url: String,
    /// This hub's public HTTPS address. Lets Google show your logo, which it
    /// fetches from `<public_url>/v1/wallet/logo.png`.
    #[serde(default)]
    pub public_url: Option<String>,

    #[serde(default)]
    pub google_issuer_id: Option<String>,
    #[serde(default)]
    pub google_service_account_json: Option<PathBuf>,

    #[serde(default)]
    pub apple_pass_type_id: Option<String>,
    #[serde(default)]
    pub apple_team_id: Option<String>,
    #[serde(default)]
    pub apple_cert_pem: Option<PathBuf>,
    #[serde(default)]
    pub apple_key_pem: Option<PathBuf>,
    #[serde(default)]
    pub apple_wwdr_pem: Option<PathBuf>,

    /// Your vendos, so iPhones can suggest the pass when the customer is near one.
    #[serde(default)]
    pub locations: Vec<Location>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Location {
    pub name: String,
    pub latitude: f64,
    pub longitude: f64,
}

fn default_brand() -> String {
    "Piso WiFi".into()
}
fn default_background() -> String {
    "#5A43E6".into()
}
fn default_foreground() -> String {
    "#FFFFFF".into()
}
fn default_label() -> String {
    "#D4F04C".into()
}
fn default_portal_url() -> String {
    "http://10.0.0.1".into()
}

fn parse_color(s: &str, what: &str) -> Result<[u8; 3]> {
    let hex = s.strip_prefix('#').unwrap_or(s);
    let b = hex::decode(hex).ok().filter(|b| b.len() == 3).with_context(|| format!("wallet.{what} must look like \"#5A43E6\""))?;
    Ok([b[0], b[1], b[2]])
}

// ---- the wallet ---------------------------------------------------------------

/// A built pass waiting for the phone to fetch it.
struct Waiting {
    kind: WalletKind,
    expires: i64,
    pass_id: String,
    /// Issue order, to find a pass's oldest link.
    n: u64,
    /// The save URL (Google) or the .pkpass file (Apple). Holds the code.
    item: Zeroizing<Vec<u8>>,
}

pub struct Wallet {
    cfg: WalletConfig,
    colors: [[u8; 3]; 3],
    google: Option<Google>,
    apple: Option<Apple>,
    waiting: Mutex<HashMap<String, Waiting>>,
    issued: AtomicU64,
}

struct Google {
    issuer_id: String,
    email: String,
    key: RsaKeyPair,
}

struct Apple {
    pass_type_id: String,
    team_id: String,
    key: RsaKeyPair,
    cert: Certificate,
    wwdr: Certificate,
}

/// What goes on a pass. `code` is the bearer secret: never log it.
struct PassInfo<'a> {
    pass_id: &'a str,
    code: &'a str,
}

/// "pass 7k3m…" → "PASS-7K3M-…", or None unless it's a well-formed pass code.
pub fn display_code(code: &str) -> Option<String> {
    let raw = crate::hub::normalize_code(code);
    (raw.len() == 16).then(|| format!("PASS-{}-{}-{}-{}", &raw[0..4], &raw[4..8], &raw[8..12], &raw[12..16]))
}

impl Wallet {
    /// Reads and checks every key and certificate up front, so a mistake
    /// shows when the hub starts rather than when a customer taps the button.
    pub fn load(cfg: WalletConfig, now: i64) -> Result<Wallet> {
        let colors = [
            parse_color(&cfg.background, "background")?,
            parse_color(&cfg.foreground, "foreground")?,
            parse_color(&cfg.label, "label")?,
        ];
        let brand = cfg.brand_name.chars().count();
        if brand == 0 || brand > MAX_BRAND_CHARS || cfg.brand_name.chars().any(char::is_control) {
            bail!("wallet.brand_name must be 1 to {MAX_BRAND_CHARS} characters (it is {brand})");
        }
        if !(cfg.portal_url.starts_with("http://") || cfg.portal_url.starts_with("https://")) {
            bail!("wallet.portal_url must start with http:// (usually \"http://10.0.0.1\")");
        }
        if cfg.public_url.as_deref().is_some_and(|u| !u.starts_with("https://")) {
            bail!("wallet.public_url must start with https://");
        }
        if cfg.locations.len() > MAX_LOCATIONS {
            bail!("wallet.locations: Apple Wallet allows at most {MAX_LOCATIONS} locations");
        }
        for l in &cfg.locations {
            if !(-90.0..=90.0).contains(&l.latitude) || !(-180.0..=180.0).contains(&l.longitude) {
                bail!("wallet.locations: \"{}\" has an impossible latitude/longitude", l.name);
            }
        }

        let google = match (&cfg.google_issuer_id, &cfg.google_service_account_json) {
            (Some(id), Some(path)) => Some(Google::load(id, path)?),
            (None, None) => None,
            _ => bail!("wallet: set both google_issuer_id and google_service_account_json, or neither"),
        };
        let apple = match (&cfg.apple_pass_type_id, &cfg.apple_team_id, &cfg.apple_cert_pem, &cfg.apple_key_pem, &cfg.apple_wwdr_pem) {
            (Some(id), Some(team), Some(cert), Some(key), Some(wwdr)) => Apple::load(id, team, cert, key, wwdr, now)?,
            (None, None, None, None, None) => None,
            _ => bail!("wallet: set all of apple_pass_type_id, apple_team_id, apple_cert_pem, apple_key_pem and apple_wwdr_pem, or none"),
        };
        Ok(Wallet { cfg, colors, google, apple, waiting: Mutex::new(HashMap::new()), issued: AtomicU64::new(0) })
    }

    pub fn support(&self) -> WalletSupport {
        WalletSupport { google: self.google.is_some(), apple: self.apple.is_some() }
    }

    /// Builds the pass now (so errors surface to the vendo, not the phone)
    /// and returns a token for downloading it once.
    pub fn issue(&self, kind: WalletKind, pass_id: &str, code: &str, now: i64) -> Result<String> {
        let code = display_code(code).context("bad pass code")?;
        let pass = PassInfo { pass_id, code: &code };
        let item = match kind {
            WalletKind::Google => {
                let g = self.google.as_ref().context("Google Wallet isn't set up on this hub")?;
                self.google_save_url(g, &pass, now)?.into_bytes()
            }
            WalletKind::Apple => {
                let a = self.apple.as_ref().context("Apple Wallet isn't set up on this hub")?;
                self.pkpass(a, &pass, now)?
            }
        };
        let token = hex::encode(rand::rngs::OsRng.gen::<[u8; 32]>());
        let mut w = self.waiting.lock().unwrap();
        w.retain(|_, x| x.expires > now);
        let mut mine: Vec<(u64, String)> = w.iter().filter(|(_, x)| x.pass_id == pass_id).map(|(t, x)| (x.n, t.clone())).collect();
        mine.sort();
        for (_, old) in mine.iter().take((mine.len() + 1).saturating_sub(MAX_WAITING_PER_PASS)) {
            w.remove(old);
        }
        if w.len() >= MAX_WAITING {
            bail!("too many wallet passes waiting to be downloaded");
        }
        let n = self.issued.fetch_add(1, Ordering::Relaxed);
        w.insert(
            token.clone(),
            Waiting { kind, expires: now + TOKEN_TTL_SECS, pass_id: pass_id.to_string(), n, item: Zeroizing::new(item) },
        );
        Ok(token)
    }

    /// Hands out a waiting pass, once.
    pub fn take(&self, kind: WalletKind, token: &str, now: i64) -> Option<Zeroizing<Vec<u8>>> {
        let mut w = self.waiting.lock().unwrap();
        if w.get(token)?.kind != kind {
            return None;
        }
        let x = w.remove(token)?;
        (x.expires > now).then_some(x.item)
    }

    /// For HEAD requests (link previews, download managers): doesn't use the token up.
    pub fn is_waiting(&self, kind: WalletKind, token: &str, now: i64) -> bool {
        self.waiting.lock().unwrap().get(token).is_some_and(|x| x.kind == kind && x.expires > now)
    }

    fn use_link(&self, code: &str) -> String {
        // Codes are A-Z, 0-9 and dashes only, so nothing needs escaping.
        format!("{}/?pass={code}", self.cfg.portal_url.trim_end_matches('/'))
    }

    fn brand(&self) -> &str {
        &self.cfg.brand_name
    }

    // ---- Google -----------------------------------------------------------------

    /// The JWT claims for a "Save to Google Wallet" link: a Generic pass,
    /// with its class included so it's created on first save.
    fn google_claims(&self, g: &Google, pass: &PassInfo, iat: i64) -> Value {
        let lang = |s: &str| json!({ "defaultValue": { "language": "en", "value": s } });
        let class_id = format!("{}.piso_pass", g.issuer_id);
        let [bg, ..] = self.colors;
        let mut object = json!({
            "id": format!("{}.{}", g.issuer_id, pass.pass_id),
            "classId": class_id,
            "state": "ACTIVE",
            "cardTitle": lang(self.brand()),
            "header": lang("Piso Pass"),
            "hexBackgroundColor": format!("#{}", hex::encode(bg)),
            "barcode": { "type": "QR_CODE", "value": pass.code, "alternateText": pass.code },
            "textModulesData": [
                { "id": "where", "header": "Works at", "body": format!("Every {} location", self.brand()) },
                { "id": "private", "header": "Keep it private", "body": "Anyone with this code can use your time." },
            ],
            "linksModuleData": { "uris": [
                { "id": "use", "uri": self.use_link(pass.code), "description": "Use my time" },
            ] },
        });
        if let Some(url) = &self.cfg.public_url {
            object["logo"] = json!({
                "sourceUri": { "uri": format!("{}/v1/wallet/logo.png", url.trim_end_matches('/')) },
                "contentDescription": lang(self.brand()),
            });
        }
        json!({
            "iss": g.email,
            "aud": "google",
            "typ": "savetowallet",
            "iat": iat,
            "origins": [],
            "payload": {
                "genericClasses": [{ "id": class_id }],
                "genericObjects": [object],
            },
        })
    }

    fn google_save_url(&self, g: &Google, pass: &PassInfo, now: i64) -> Result<String> {
        let jwt = jwt_rs256(&g.key, &self.google_claims(g, pass, now))?;
        Ok(format!("https://pay.google.com/gp/v/save/{jwt}"))
    }

    // ---- Apple ------------------------------------------------------------------

    fn pass_json(&self, a: &Apple, pass: &PassInfo) -> Value {
        let rgb = |c: [u8; 3]| format!("rgb({}, {}, {})", c[0], c[1], c[2]);
        let [bg, fg, label] = self.colors;
        let link = self.use_link(pass.code);
        let mut p = json!({
            "formatVersion": 1,
            "passTypeIdentifier": a.pass_type_id,
            "teamIdentifier": a.team_id,
            "serialNumber": pass.pass_id,
            "organizationName": self.brand(),
            "description": format!("{} Piso Pass", self.brand()),
            "logoText": self.brand(),
            "backgroundColor": rgb(bg),
            "foregroundColor": rgb(fg),
            "labelColor": rgb(label),
            // The code is a bearer secret: don't offer AirDrop/Messages sharing.
            "sharingProhibited": true,
            "barcodes": [{
                "format": "PKBarcodeFormatQR",
                "message": pass.code,
                "messageEncoding": "iso-8859-1",
                "altText": pass.code,
            }],
            "storeCard": {
                "primaryFields": [{ "key": "title", "label": "PISO PASS", "value": "WiFi time" }],
                "secondaryFields": [{ "key": "code", "label": "CODE", "value": pass.code }],
                "auxiliaryFields": [{ "key": "where", "label": "WORKS AT", "value": format!("Every {} location", self.brand()) }],
                "backFields": [
                    {
                        "key": "use",
                        "label": "Use my time",
                        "value": format!("Connect to the WiFi, then open {link}"),
                        "attributedValue": format!("<a href=\"{link}\">Use my time</a>"),
                    },
                    {
                        "key": "how",
                        "label": "How it works",
                        "value": format!(
                            "Your time is kept on this pass, not on one machine. At any {} location, \
                             connect to the WiFi and tap Use my time, or type the code under \
                             \"Have a voucher or Piso Pass?\".",
                            self.brand()
                        ),
                    },
                    { "key": "private", "label": "Keep it private", "value": "Anyone with this code can use your time." },
                ],
            },
        });
        if !self.cfg.locations.is_empty() {
            p["locations"] = self
                .cfg
                .locations
                .iter()
                .map(|l| json!({ "latitude": l.latitude, "longitude": l.longitude, "relevantText": format!("{}: your Piso Pass works here", l.name) }))
                .collect();
        }
        p
    }

    fn pkpass(&self, a: &Apple, pass: &PassInfo, now: i64) -> Result<Vec<u8>> {
        let pass_json = serde_json::to_vec_pretty(&self.pass_json(a, pass))?;
        let mut files: Vec<(&str, &[u8])> = vec![("pass.json", &pass_json)];
        files.extend_from_slice(PASS_IMAGES);
        let manifest = manifest(&files);
        let signature = cms_sign(a, &manifest, now)?;
        files.push(("manifest.json", &manifest));
        files.push(("signature", &signature));
        zip_store(&files)
    }
}

/// Built-in images, drawn from web/shared/icon.svg. `icon.png` is required.
const PASS_IMAGES: &[(&str, &[u8])] = &[
    ("icon.png", include_bytes!("../web/wallet/icon.png")),
    ("icon@2x.png", include_bytes!("../web/wallet/icon@2x.png")),
    ("icon@3x.png", include_bytes!("../web/wallet/icon@3x.png")),
    ("logo.png", include_bytes!("../web/wallet/logo.png")),
    ("logo@2x.png", include_bytes!("../web/wallet/logo@2x.png")),
    ("logo@3x.png", include_bytes!("../web/wallet/logo@3x.png")),
];

/// Logo Google fetches for the pass (see `public_url`).
pub const GOOGLE_LOGO: &[u8] = include_bytes!("../web/wallet/google-logo.png");

/// manifest.json: every file's SHA-1, hex.
fn manifest(files: &[(&str, &[u8])]) -> Vec<u8> {
    let map: serde_json::Map<String, Value> =
        files.iter().map(|(name, data)| (name.to_string(), Value::String(hex::encode(Sha1::digest(data))))).collect();
    serde_json::to_vec_pretty(&Value::Object(map)).expect("json")
}

fn zip_store(files: &[(&str, &[u8])]) -> Result<Vec<u8>> {
    let mut z = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for (name, data) in files {
        z.start_file(*name, opts)?;
        z.write_all(data)?;
    }
    Ok(z.finish()?.into_inner())
}

// ---- keys and signing -----------------------------------------------------------

fn read(path: &Path, what: &str) -> Result<Zeroizing<Vec<u8>>> {
    Ok(Zeroizing::new(std::fs::read(path).with_context(|| format!("wallet: reading {what} {}", path.display()))?))
}

fn rsa_key(pem: &[u8], what: &str) -> Result<RsaKeyPair> {
    if pem.windows(9).any(|w| w == b"ENCRYPTED") {
        bail!("wallet: {what} is password-protected; export it without a password (openssl … -nodes)");
    }
    let key = PrivateKeyDer::from_pem_slice(pem).map_err(|e| anyhow!("wallet: no private key in {what} ({e})"))?;
    let pair = match &key {
        PrivateKeyDer::Pkcs8(k) => RsaKeyPair::from_pkcs8(k.secret_pkcs8_der()),
        PrivateKeyDer::Pkcs1(k) => RsaKeyPair::from_der(k.secret_pkcs1_der()),
        _ => bail!("wallet: {what} must be an RSA key"),
    };
    pair.map_err(|e| anyhow!("wallet: {what} isn't a usable RSA key ({e})"))
}

/// First certificate in a PEM file (text such as openssl's "Bag Attributes"
/// is skipped), or a raw DER .cer as Apple serves it.
fn certificate(bytes: &[u8], what: &str) -> Result<Certificate> {
    let der = match CertificateDer::pem_slice_iter(bytes).next() {
        Some(c) => c.map_err(|e| anyhow!("wallet: reading {what} ({e})"))?.to_vec(),
        None => bytes.to_vec(),
    };
    Certificate::from_der(&der).map_err(|e| anyhow!("wallet: {what} isn't a certificate ({e})"))
}

fn rsa_sign(key: &RsaKeyPair, msg: &[u8]) -> Result<Vec<u8>> {
    let mut sig = vec![0; key.public().modulus_len()];
    key.sign(&ring::signature::RSA_PKCS1_SHA256, &ring::rand::SystemRandom::new(), msg, &mut sig)
        .map_err(|_| anyhow!("RSA signing failed"))?;
    Ok(sig)
}

fn jwt_rs256(key: &RsaKeyPair, claims: &Value) -> Result<String> {
    let head = B64URL.encode(br#"{"alg":"RS256","typ":"JWT"}"#);
    let body = B64URL.encode(serde_json::to_vec(claims)?);
    let signed = format!("{head}.{body}");
    let sig = rsa_sign(key, signed.as_bytes())?;
    Ok(format!("{signed}.{}", B64URL.encode(sig)))
}

impl Google {
    fn load(issuer_id: &str, path: &Path) -> Result<Google> {
        #[derive(Deserialize)]
        struct ServiceAccount {
            #[serde(rename = "type")]
            kind: String,
            client_email: String,
            private_key: String,
        }
        let issuer_id = issuer_id.trim().to_string();
        if issuer_id.is_empty() || !issuer_id.bytes().all(|b| b.is_ascii_digit()) {
            bail!("wallet.google_issuer_id is the number shown in the Google Pay & Wallet Console");
        }
        let raw = read(path, "google_service_account_json")?;
        let sa: ServiceAccount = serde_json::from_slice(&raw).context("wallet: google_service_account_json isn't a service-account key file")?;
        if sa.kind != "service_account" {
            bail!("wallet: google_service_account_json must be a service-account key (\"type\": \"service_account\")");
        }
        let pem = Zeroizing::new(sa.private_key);
        let key = rsa_key(pem.as_bytes(), "the service-account key")?;
        Ok(Google { issuer_id, email: sa.client_email, key })
    }
}

mod oid {
    use der::oid::ObjectIdentifier as Oid;
    pub const DATA: Oid = Oid::new_unwrap("1.2.840.113549.1.7.1");
    pub const SIGNED_DATA: Oid = Oid::new_unwrap("1.2.840.113549.1.7.2");
    pub const CONTENT_TYPE: Oid = Oid::new_unwrap("1.2.840.113549.1.9.3");
    pub const MESSAGE_DIGEST: Oid = Oid::new_unwrap("1.2.840.113549.1.9.4");
    pub const SIGNING_TIME: Oid = Oid::new_unwrap("1.2.840.113549.1.9.5");
    pub const SHA256: Oid = Oid::new_unwrap("2.16.840.1.101.3.4.2.1");
    pub const RSA: Oid = Oid::new_unwrap("1.2.840.113549.1.1.1");
    pub const UID: Oid = Oid::new_unwrap("0.9.2342.19200300.100.1.1");
    pub const OU: Oid = Oid::new_unwrap("2.5.4.11");
}

/// A subject field of a certificate, as text (UTF8String / PrintableString).
fn subject_field(cert: &Certificate, field: der::oid::ObjectIdentifier) -> Option<String> {
    cert.tbs_certificate
        .subject
        .0
        .iter()
        .flat_map(|rdn| rdn.0.iter())
        .find(|atv| atv.oid == field)
        .and_then(|atv| String::from_utf8(atv.value.value().to_vec()).ok())
}

impl Apple {
    /// None (with a warning) when the certificate has expired: passes would be
    /// refused by iPhones, and the hub must keep running for Piso Pass itself.
    fn load(pass_type_id: &str, team_id: &str, cert: &Path, key: &Path, wwdr: &Path, now: i64) -> Result<Option<Apple>> {
        let cert = certificate(&read(cert, "apple_cert_pem")?, "apple_cert_pem")?;
        let wwdr = certificate(&read(wwdr, "apple_wwdr_pem")?, "apple_wwdr_pem")?;
        let key = rsa_key(&read(key, "apple_key_pem")?, "apple_key_pem")?;
        let spki = &cert.tbs_certificate.subject_public_key_info.subject_public_key;
        if spki.raw_bytes() != key.public().as_ref() {
            bail!("wallet: apple_key_pem is not the key for apple_cert_pem");
        }
        if let Some(uid) = subject_field(&cert, oid::UID) {
            if uid != pass_type_id {
                bail!("wallet: apple_cert_pem is for {uid}, but apple_pass_type_id is {pass_type_id}");
            }
        }
        if let Some(ou) = subject_field(&cert, oid::OU) {
            if ou != team_id {
                bail!("wallet: apple_cert_pem belongs to team {ou}, but apple_team_id is {team_id}");
            }
        }
        if cert.tbs_certificate.issuer != wwdr.tbs_certificate.subject {
            bail!("wallet: apple_wwdr_pem isn't the certificate that issued apple_cert_pem (download the matching WWDR intermediate)");
        }
        let expires = cert.tbs_certificate.validity.not_after.to_unix_duration().as_secs() as i64;
        if expires <= now {
            tracing::error!("wallet: the Apple pass certificate has expired; Apple Wallet is off until you renew it");
            return Ok(None);
        }
        if expires - now < 30 * 86400 {
            tracing::warn!("wallet: the Apple pass certificate expires in {} days; renew it soon", (expires - now) / 86400);
        }
        Ok(Some(Apple { pass_type_id: pass_type_id.to_string(), team_id: team_id.to_string(), key, cert, wwdr }))
    }
}

/// Detached PKCS#7 / CMS signature over `content` (the manifest), with the
/// pass certificate and the WWDR intermediate, as Apple Wallet requires.
fn cms_sign(a: &Apple, content: &[u8], now: i64) -> Result<Vec<u8>> {
    use cms::cert::{CertificateChoices, IssuerAndSerialNumber};
    use cms::content_info::{CmsVersion, ContentInfo};
    use cms::signed_data::{CertificateSet, EncapsulatedContentInfo, SignedData, SignerIdentifier, SignerInfo, SignerInfos};
    use der::asn1::{Any, OctetString, SetOfVec, UtcTime};
    use x509_cert::attr::Attribute;
    use x509_cert::spki::AlgorithmIdentifierOwned;
    use x509_cert::time::Time;

    let attr = |oid, value: Any| -> Result<Attribute> { Ok(Attribute { oid, values: SetOfVec::try_from(vec![value])? }) };
    let when = Time::UtcTime(UtcTime::from_unix_duration(Duration::from_secs(now.max(0) as u64))?);
    let signed_attrs = SetOfVec::try_from(vec![
        attr(oid::CONTENT_TYPE, Any::encode_from(&oid::DATA)?)?,
        attr(oid::SIGNING_TIME, Any::encode_from(&when)?)?,
        attr(oid::MESSAGE_DIGEST, Any::encode_from(&OctetString::new(Sha256::digest(content).to_vec())?)?)?,
    ])?;
    // What's signed is the attributes' DER encoding as a SET (RFC 5652 §5.4).
    let signature = rsa_sign(&a.key, &signed_attrs.to_der()?)?;
    let sha256 = AlgorithmIdentifierOwned { oid: oid::SHA256, parameters: None };
    let signer = SignerInfo {
        version: CmsVersion::V1,
        sid: SignerIdentifier::IssuerAndSerialNumber(IssuerAndSerialNumber {
            issuer: a.cert.tbs_certificate.issuer.clone(),
            serial_number: a.cert.tbs_certificate.serial_number.clone(),
        }),
        digest_alg: sha256.clone(),
        signed_attrs: Some(signed_attrs),
        signature_algorithm: AlgorithmIdentifierOwned { oid: oid::RSA, parameters: Some(Any::null()) },
        signature: OctetString::new(signature)?,
        unsigned_attrs: None,
    };
    let signed_data = SignedData {
        version: CmsVersion::V1,
        digest_algorithms: SetOfVec::try_from(vec![sha256])?,
        encap_content_info: EncapsulatedContentInfo { econtent_type: oid::DATA, econtent: None },
        certificates: Some(CertificateSet(SetOfVec::try_from(vec![
            CertificateChoices::Certificate(a.cert.clone()),
            CertificateChoices::Certificate(a.wwdr.clone()),
        ])?)),
        crls: None,
        signer_infos: SignerInfos(SetOfVec::try_from(vec![signer])?),
    };
    Ok(ContentInfo { content_type: oid::SIGNED_DATA, content: Any::encode_from(&signed_data)? }.to_der()?)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::Read as _;

    const CODE: &str = "PASS-7K3M-ABCD-EFGH-JKMN";
    const NOW: i64 = 1_790_000_000; // 2026-09

    pub fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/wallet").join(name)
    }

    pub fn test_config() -> WalletConfig {
        let f = |n| fixture(n).display().to_string();
        toml::from_str(&format!(
            r#"
            brand_name = "Juan's WiFi"
            public_url = "https://hub.example.ph"
            google_issuer_id = "3388000000012345678"
            google_service_account_json = "{}"
            apple_pass_type_id = "pass.ph.pisowifi.test"
            apple_team_id = "TEAMID1234"
            apple_cert_pem = "{}"
            apple_key_pem = "{}"
            apple_wwdr_pem = "{}"
            [[locations]]
            name = "Barangay A"
            latitude = 14.5995
            longitude = 120.9842
            "#,
            f("test-service-account.json"),
            f("test-cert.pem"),
            f("test-key.pem"),
            f("test-wwdr.pem"),
        ))
        .unwrap()
    }

    pub fn test_wallet() -> Wallet {
        Wallet::load(test_config(), NOW).unwrap()
    }

    fn b64(s: &str) -> Vec<u8> {
        B64URL.decode(s).unwrap()
    }

    /// The RSA public key (PKCS#1) inside the test certificate.
    fn test_public_key() -> Vec<u8> {
        let cert = certificate(&std::fs::read(fixture("test-cert.pem")).unwrap(), "test").unwrap();
        cert.tbs_certificate.subject_public_key_info.subject_public_key.raw_bytes().to_vec()
    }

    fn verify_rs256(msg: &[u8], sig: &[u8]) -> bool {
        ring::signature::UnparsedPublicKey::new(&ring::signature::RSA_PKCS1_2048_8192_SHA256, test_public_key())
            .verify(msg, sig)
            .is_ok()
    }

    #[test]
    fn google_jwt_has_the_save_claims_and_verifies() {
        let w = test_wallet();
        let token = w.issue(WalletKind::Google, "p_0123456789abcdef", "pass-7k3m abcd efgh jkmn", NOW).unwrap();
        let url = String::from_utf8(w.take(WalletKind::Google, &token, NOW).unwrap().to_vec()).unwrap();
        let jwt = url.strip_prefix("https://pay.google.com/gp/v/save/").expect("save URL");
        // Browsers and Google cope with long URLs, but keep it compact.
        assert!(url.len() < 2000, "save URL is {} chars", url.len());

        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3);
        let head: Value = serde_json::from_slice(&b64(parts[0])).unwrap();
        assert_eq!(head, json!({ "alg": "RS256", "typ": "JWT" }));
        assert!(verify_rs256(format!("{}.{}", parts[0], parts[1]).as_bytes(), &b64(parts[2])), "RS256 signature");

        let c: Value = serde_json::from_slice(&b64(parts[1])).unwrap();
        assert_eq!(c["iss"], "wallet-test@test-only.iam.gserviceaccount.com");
        assert_eq!(c["aud"], "google");
        assert_eq!(c["typ"], "savetowallet");
        assert_eq!(c["iat"], NOW);
        assert_eq!(c["payload"]["genericClasses"][0]["id"], "3388000000012345678.piso_pass");
        let o = &c["payload"]["genericObjects"][0];
        assert_eq!(o["id"], "3388000000012345678.p_0123456789abcdef");
        assert_eq!(o["classId"], "3388000000012345678.piso_pass");
        assert_eq!(o["state"], "ACTIVE");
        assert_eq!(o["cardTitle"]["defaultValue"]["value"], "Juan's WiFi");
        assert_eq!(o["hexBackgroundColor"], "#5a43e6");
        assert_eq!(o["barcode"], json!({ "type": "QR_CODE", "value": CODE, "alternateText": CODE }));
        assert_eq!(o["linksModuleData"]["uris"][0]["uri"], format!("http://10.0.0.1/?pass={CODE}"));
        assert_eq!(o["linksModuleData"]["uris"][0]["description"], "Use my time");
        assert_eq!(o["logo"]["sourceUri"]["uri"], "https://hub.example.ph/v1/wallet/logo.png");
    }

    fn unzip(bytes: &[u8]) -> HashMap<String, Vec<u8>> {
        let mut z = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
        (0..z.len())
            .map(|i| {
                let mut f = z.by_index(i).unwrap();
                let mut data = Vec::new();
                f.read_to_end(&mut data).unwrap();
                (f.name().unwrap().to_string(), data)
            })
            .collect()
    }

    #[test]
    fn pkpass_manifest_hashes_every_file_and_is_signed() {
        let w = test_wallet();
        let token = w.issue(WalletKind::Apple, "p_0123456789abcdef", CODE, NOW).unwrap();
        let files = unzip(&w.take(WalletKind::Apple, &token, NOW).unwrap());

        // manifest.json lists every other file with its SHA-1.
        let manifest: HashMap<String, String> = serde_json::from_slice(&files["manifest.json"]).unwrap();
        let mut listed: Vec<_> = manifest.keys().cloned().collect();
        listed.sort();
        let mut present: Vec<_> = files.keys().filter(|n| *n != "manifest.json" && *n != "signature").cloned().collect();
        present.sort();
        assert_eq!(listed, present);
        assert!(listed.contains(&"icon.png".to_string()), "icon.png is required");
        for (name, hash) in &manifest {
            assert_eq!(*hash, hex::encode(Sha1::digest(&files[name])), "{name}");
        }

        let p: Value = serde_json::from_slice(&files["pass.json"]).unwrap();
        assert_eq!(p["formatVersion"], 1);
        assert_eq!(p["passTypeIdentifier"], "pass.ph.pisowifi.test");
        assert_eq!(p["teamIdentifier"], "TEAMID1234");
        assert_eq!(p["serialNumber"], "p_0123456789abcdef");
        assert_eq!(p["barcodes"][0]["message"], CODE);
        assert_eq!(p["storeCard"]["secondaryFields"][0]["value"], CODE);
        assert!(p["storeCard"]["backFields"][0]["attributedValue"].as_str().unwrap().contains(&format!("http://10.0.0.1/?pass={CODE}")));
        assert_eq!(p["locations"][0]["latitude"], 14.5995);
        assert_eq!(p["sharingProhibited"], true);

        // The detached signature covers the manifest: check its digest and RSA signature.
        let ci = cms::content_info::ContentInfo::from_der(&files["signature"]).unwrap();
        assert_eq!(ci.content_type, oid::SIGNED_DATA);
        let sd: cms::signed_data::SignedData = ci.content.decode_as().unwrap();
        assert!(sd.encap_content_info.econtent.is_none(), "detached");
        assert_eq!(sd.certificates.as_ref().unwrap().0.len(), 2, "pass cert + WWDR");
        let si = sd.signer_infos.0.get(0).unwrap();
        let attrs = si.signed_attrs.as_ref().unwrap();
        let digest = attrs.iter().find(|a| a.oid == oid::MESSAGE_DIGEST).unwrap();
        let digest: OctetString = digest.values.get(0).unwrap().decode_as().unwrap();
        assert_eq!(digest.as_bytes(), Sha256::digest(&files["manifest.json"]).as_slice());
        assert!(attrs.iter().any(|a| a.oid == oid::SIGNING_TIME));
        assert!(verify_rs256(&attrs.to_der().unwrap(), si.signature.as_bytes()), "CMS signature");
    }
    use der::asn1::OctetString;

    #[test]
    fn tokens_are_single_use_and_expire() {
        let w = test_wallet();
        let t = w.issue(WalletKind::Apple, "p_1", CODE, NOW).unwrap();
        assert_eq!(t.len(), 64);
        assert!(!t.contains("7K3M"), "token doesn't carry the code");
        assert!(w.is_waiting(WalletKind::Apple, &t, NOW), "HEAD doesn't use it up");
        assert!(w.take(WalletKind::Google, &t, NOW).is_none(), "wrong kind");
        assert!(w.take(WalletKind::Apple, &t, NOW + 5).is_some());
        assert!(w.take(WalletKind::Apple, &t, NOW + 6).is_none(), "single use");

        let t = w.issue(WalletKind::Google, "p_1", CODE, NOW).unwrap();
        assert!(w.take(WalletKind::Google, &t, NOW + TOKEN_TTL_SECS).is_none(), "expired after 10 min");
        assert!(w.take(WalletKind::Google, &t, NOW).is_none(), "and gone");
        assert!(w.issue(WalletKind::Google, "p_1", "PASS-SHORT", NOW).is_err());
    }

    #[test]
    fn one_pass_cant_fill_the_download_queue() {
        let w = test_wallet();
        let tokens: Vec<String> = (0..5).map(|_| w.issue(WalletKind::Google, "p_1", CODE, NOW).unwrap()).collect();
        // Only the two newest links for this pass still work.
        let live: Vec<bool> = tokens.iter().map(|t| w.is_waiting(WalletKind::Google, t, NOW)).collect();
        assert_eq!(live, [false, false, false, true, true]);
        assert_eq!(w.waiting.lock().unwrap().len(), 2);
        // Other passes are untouched.
        let other = w.issue(WalletKind::Google, "p_2", CODE, NOW).unwrap();
        assert!(w.is_waiting(WalletKind::Google, &other, NOW));
        assert!(w.is_waiting(WalletKind::Google, &tokens[4], NOW));
    }

    #[test]
    fn config_mistakes_are_caught_at_start() {
        let mut c = test_config();
        c.apple_team_id = Some("OTHERTEAM1".into());
        assert!(Wallet::load(c, NOW).err().unwrap().to_string().contains("team"));

        let mut c = test_config();
        c.apple_wwdr_pem = Some(fixture("test-cert.pem"));
        assert!(Wallet::load(c, NOW).err().unwrap().to_string().contains("WWDR"));

        let mut c = test_config();
        c.google_service_account_json = None;
        assert!(Wallet::load(c, NOW).is_err(), "half-configured Google");

        let mut c = test_config();
        c.brand_name = "Juan's Super Fast Piso WiFi Station".into();
        assert!(Wallet::load(c, NOW).err().unwrap().to_string().contains("brand_name"), "too long for a pass");
        let mut c = test_config();
        c.brand_name = "Juan's WiFi Barangay 123".into(); // 24 characters
        assert!(Wallet::load(c, NOW).is_ok());

        let mut c = test_config();
        c.locations = (0..11).map(|i| Location { name: format!("L{i}"), latitude: 14.0, longitude: 121.0 }).collect();
        assert!(Wallet::load(c, NOW).is_err(), "more than 10 locations");

        // Nothing configured: wallet off.
        let off: WalletConfig = toml::from_str("").unwrap();
        assert_eq!(Wallet::load(off, NOW).unwrap().support(), WalletSupport { google: false, apple: false });

        // An expired Apple certificate turns Apple off but keeps the hub (and Google) up.
        let w = Wallet::load(test_config(), 5_000_000_000).unwrap(); // after the test cert's 2126 expiry
        assert_eq!(w.support(), WalletSupport { google: true, apple: false });
    }

    #[test]
    fn keys_exported_by_openssl_pkcs12_load() {
        // `openssl pkcs12 -nodes` prefixes each block with "Bag Attributes" text.
        let pem = std::fs::read_to_string(fixture("test-key.pem")).unwrap();
        let bagged = format!("Bag Attributes\n    localKeyID: 01 02 03\nKey Attributes: <No Attributes>\n{pem}");
        assert!(rsa_key(bagged.as_bytes(), "key").is_ok());
        let cert = std::fs::read_to_string(fixture("test-cert.pem")).unwrap();
        assert!(certificate(format!("Bag Attributes\n    friendlyName: Pass Type ID\n{cert}").as_bytes(), "cert").is_ok());
        assert!(rsa_key(b"-----BEGIN ENCRYPTED PRIVATE KEY-----\nAAAA\n-----END ENCRYPTED PRIVATE KEY-----\n", "key").is_err());
    }
}
