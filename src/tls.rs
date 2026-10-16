//! HTTPS for the owner dashboard, so the admin password and session cookie
//! never cross the open customer WiFi in plain text.
//!
//! A self-signed certificate is generated on first start. Your browser will
//! warn once; compare the SHA-256 fingerprint it shows with the one printed
//! by `pisowifi admin-info` before trusting it.

use anyhow::{Context, Result};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::sync::Arc;
use tokio_rustls::TlsAcceptor;

pub struct AdminTls {
    pub acceptor: TlsAcceptor,
    pub fingerprint: String,
}

pub fn load_or_create(data_dir: &Path, names: Vec<String>) -> Result<AdminTls> {
    let cert_path = data_dir.join("admin-cert.pem");
    let key_path = data_dir.join("admin-key.pem");
    if !cert_path.exists() || !key_path.exists() {
        let ck = rcgen::generate_simple_self_signed(names).context("generating certificate")?;
        write_private(&key_path, ck.key_pair.serialize_pem().as_bytes())?;
        std::fs::write(&cert_path, ck.cert.pem())?;
        tracing::info!("created dashboard TLS certificate");
    }
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(&cert_path)
        .context("reading admin-cert.pem")?
        .collect::<Result<_, _>>()?;
    let key = PrivateKeyDer::from_pem_file(&key_path).context("reading admin-key.pem")?;
    let fingerprint = fingerprint(&certs[0]);
    Ok(AdminTls { acceptor: acceptor(certs, key)?, fingerprint })
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

fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)?.write_all(data)?;
    Ok(())
}
