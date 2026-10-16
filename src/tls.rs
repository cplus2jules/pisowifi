//! HTTPS for the owner dashboard, so the admin password and session cookie
//! never cross the open customer WiFi in plain text.
//!
//! A self-signed certificate is generated on first start. Your browser will
//! warn once; compare the SHA-256 fingerprint it shows with the one printed
//! by `pisowifi admin-info` before trusting it.
//!
//! iPhones and Macs refuse (with no "visit anyway") server certificates that
//! lack the serverAuth usage or are valid for more than 825 days, so ours is
//! valid for 800 days and renewed a month before it expires.

use anyhow::{Context, Result};
use chrono::Datelike;
use der::Decode;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio_rustls::TlsAcceptor;

const VALID_DAYS: u64 = 800;
const RENEW_DAYS: u64 = 30;
const DAY: u64 = 86400;
/// The board has no clock battery: until NTP syncs, the date can be years
/// off. Certificates never start before this, and expiry isn't judged by a
/// clock that reads earlier.
const CLOCK_FLOOR: (i32, u8, u8) = (2026, 10, 1);

pub struct AdminTls {
    pub acceptor: TlsAcceptor,
    pub fingerprint: String,
}

pub fn load_or_create(data_dir: &Path, names: Vec<String>) -> Result<AdminTls> {
    let cert_path = data_dir.join("admin-cert.pem");
    let key_path = data_dir.join("admin-key.pem");
    let (certs, key) = match load(&cert_path, &key_path) {
        Ok((certs, key, None)) => (certs, key),
        found => {
            match found {
                Ok((_, _, Some(why))) => tracing::warn!("renewing the dashboard certificate: {why}"),
                Err(e) if cert_path.exists() || key_path.exists() => {
                    tracing::warn!("dashboard certificate unreadable ({e:#}); making a new one")
                }
                _ => {}
            }
            create(&cert_path, &key_path, names)?;
            let (certs, key, _) = load(&cert_path, &key_path)?;
            tracing::warn!(
                "new dashboard certificate, fingerprint SHA-256 {}. Your browser will warn once again.",
                fingerprint(&certs[0])
            );
            (certs, key)
        }
    };
    let fingerprint = fingerprint(&certs[0]);
    Ok(AdminTls { acceptor: acceptor(certs, key)?, fingerprint })
}

type Loaded = (Vec<CertificateDer<'static>>, PrivateKeyDer<'static>, Option<&'static str>);

/// The stored certificate and key, plus a reason to renew them, if any.
fn load(cert_path: &Path, key_path: &Path) -> Result<Loaded> {
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(cert_path)
        .context("reading admin-cert.pem")?
        .collect::<Result<_, _>>()?;
    let first = certs.first().context("admin-cert.pem holds no certificate")?;
    let key = PrivateKeyDer::from_pem_file(key_path).context("reading admin-key.pem")?;
    let validity = x509_cert::Certificate::from_der(first).context("parsing admin-cert.pem")?.tbs_certificate.validity;
    let (from, until) = (validity.not_before.to_unix_duration().as_secs(), validity.not_after.to_unix_duration().as_secs());
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
    let (y, m, d) = CLOCK_FLOOR;
    let floor = chrono::NaiveDate::from_ymd_opt(y, m as u32, d as u32).and_then(|d| d.and_hms_opt(0, 0, 0)).map_or(0, |t| t.and_utc().timestamp() as u64);
    let why = if until.saturating_sub(from) > 825 * DAY {
        Some("valid for too long for iPhones")
    } else if now >= floor && until < now + RENEW_DAYS * DAY {
        Some("expires within a month")
    } else {
        None
    };
    Ok((certs, key, why))
}

fn create(cert_path: &Path, key_path: &Path, names: Vec<String>) -> Result<()> {
    let today = chrono::Utc::now().date_naive();
    let floor = rcgen::date_time_ymd(CLOCK_FLOOR.0, CLOCK_FLOOR.1, CLOCK_FLOOR.2);
    let start = rcgen::date_time_ymd(today.year(), today.month() as u8, today.day() as u8).max(floor);
    let mut p = rcgen::CertificateParams::new(names).context("certificate names")?;
    p.distinguished_name = rcgen::DistinguishedName::new();
    p.distinguished_name.push(rcgen::DnType::CommonName, "Piso WiFi dashboard");
    p.not_before = start - Duration::from_secs(DAY);
    p.not_after = start + Duration::from_secs(VALID_DAYS * DAY);
    p.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
    p.key_usages = vec![rcgen::KeyUsagePurpose::DigitalSignature];
    let key = rcgen::KeyPair::generate().context("generating key")?;
    let cert = p.self_signed(&key).context("generating certificate")?;
    // Written whole and renamed into place: a power cut can't leave half a file.
    write_atomic(key_path, key.serialize_pem().as_bytes())?;
    write_atomic(cert_path, cert.pem().as_bytes())?;
    tracing::info!("created dashboard TLS certificate");
    Ok(())
}

/// TLS from existing PEM files (e.g. a Let's Encrypt certificate for the hub).
pub fn from_files(cert: &Path, key: &Path) -> Result<TlsAcceptor> {
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(cert)
        .with_context(|| format!("reading {}", cert.display()))?
        .collect::<Result<_, _>>()?;
    let key = PrivateKeyDer::from_pem_file(key).with_context(|| format!("reading {}", key.display()))?;
    acceptor(certs, key)
}

fn acceptor(certs: Vec<CertificateDer<'static>>, key: PrivateKeyDer<'static>) -> Result<TlsAcceptor> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut cfg = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])?
        .with_no_client_auth()
        .with_single_cert(certs, key)?;
    cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(TlsAcceptor::from(Arc::new(cfg)))
}

pub fn fingerprint(cert: &CertificateDer<'_>) -> String {
    Sha256::digest(cert.as_ref()).iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(":")
}

fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    use std::io::Write;
    let tmp = path.with_extension("tmp");
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(&tmp).with_context(|| format!("writing {}", tmp.display()))?;
    f.write_all(data)?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dashboard_certificate_suits_iphones_and_old_ones_are_replaced() {
        let dir = std::env::temp_dir().join(format!("pisowifi-tls-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&dir).unwrap();
        let (cert, key) = (dir.join("admin-cert.pem"), dir.join("admin-key.pem"));
        // A certificate made the old way (valid 1975–4096, no serverAuth) is renewed.
        let old = rcgen::generate_simple_self_signed(vec!["10.0.0.1".into()]).unwrap();
        std::fs::write(&cert, old.cert.pem()).unwrap();
        std::fs::write(&key, old.key_pair.serialize_pem()).unwrap();
        assert!(load(&cert, &key).unwrap().2.is_some());
        let first = load_or_create(&dir, vec!["10.0.0.1".into()]).unwrap().fingerprint;
        let (certs, _, why) = load(&cert, &key).unwrap();
        assert!(why.is_none());
        let c = x509_cert::Certificate::from_der(&certs[0]).unwrap();
        let v = &c.tbs_certificate.validity;
        assert!(v.not_after.to_unix_duration() - v.not_before.to_unix_duration() <= Duration::from_secs(825 * DAY));
        let eku = c.tbs_certificate.extensions.unwrap().iter().any(|e| e.extn_id.to_string() == "2.5.29.37");
        assert!(eku, "has extendedKeyUsage");
        // Kept as long as it's good.
        assert_eq!(load_or_create(&dir, vec!["10.0.0.1".into()]).unwrap().fingerprint, first);
        // A damaged file is replaced instead of stopping the vendo.
        std::fs::write(&cert, "garbage").unwrap();
        assert_ne!(load_or_create(&dir, vec!["10.0.0.1".into()]).unwrap().fingerprint, first);
        let _ = std::fs::remove_dir_all(dir);
    }
}
