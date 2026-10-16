//! Vendo side of the hub connection: enrolment, signed calls, heartbeat.
//! Connections are outbound only, so vendos need no open ports.

use crate::db::Db;
use crate::hub::proto::*;
use crate::hub::signing_message;
use crate::keys::read_or_create_secret;
use anyhow::{bail, Context, Result};
use ed25519_dalek::{Signer, SigningKey};
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub struct HubClient {
    url: String,
    vendo_id: String,
    key: SigningKey,
    agent: ureq::Agent,
    status: Mutex<HubStatus>,
}

#[derive(Clone, Serialize, Default)]
pub struct HubStatus {
    pub url: String,
    pub name: String,
    pub vendo_id: String,
    pub online: bool,
    pub last_sync: Option<i64>,
    pub last_error: Option<String>,
    pub passes_here: usize,
    /// Wallet passes the hub can make, as of the last sync.
    pub wallet: WalletSupport,
}

#[derive(Debug)]
pub enum HubError {
    /// Unknown pass code.
    NotFound,
    /// Network trouble or hub down.
    Offline(String),
    /// The hub refused (e.g. this vendo was removed).
    Rejected(String),
}

impl std::fmt::Display for HubError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HubError::NotFound => write!(f, "not found"),
            HubError::Offline(e) => write!(f, "hub unreachable: {e}"),
            HubError::Rejected(e) => write!(f, "hub refused: {e}"),
        }
    }
}

fn signing_key(data_dir: &Path) -> Result<SigningKey> {
    let raw = read_or_create_secret(&data_dir.join("hub-signing.key"))?;
    let bytes: [u8; 32] = raw[..].try_into().context("hub-signing.key")?;
    Ok(SigningKey::from_bytes(&bytes))
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(5))
        .timeout(Duration::from_secs(10))
        .user_agent(concat!("pisowifi/", env!("CARGO_PKG_VERSION")))
        .build()
}

fn check_url(url: &str, dev: bool) -> Result<String> {
    let url = url.trim().trim_end_matches('/').to_string();
    if url.starts_with("https://") || (dev && url.starts_with("http://")) {
        Ok(url)
    } else {
        bail!("the hub URL must start with https://")
    }
}

/// `pisowifi join-hub`: registers this box with the hub (one-time code).
pub fn join(db: &Db, data_dir: &Path, url: &str, code: &str, dev: bool) -> Result<EnrollResp> {
    let url = check_url(url, dev)?;
    let key = signing_key(data_dir)?;
    let req = EnrollReq { code: code.trim().to_string(), pubkey: hex::encode(key.verifying_key().as_bytes()) };
    let resp: EnrollResp = match agent().post(&format!("{url}/v1/enroll")).send_json(&req) {
        Ok(r) => r.into_json()?,
        Err(ureq::Error::Status(_, r)) => bail!("hub refused: {}", r.into_string().unwrap_or_default()),
        Err(e) => bail!("can't reach the hub: {e}"),
    };
    db.set_setting("hub_url", &url)?;
    db.set_setting("hub_vendo_id", &resp.vendo_id)?;
    db.set_setting("hub_vendo_name", &resp.name)?;
    db.audit("shell", "join_hub", &format!("{url} as {}", resp.name));
    Ok(resp)
}

pub fn leave(db: &Db) -> Result<()> {
    for k in ["hub_url", "hub_vendo_id", "hub_vendo_name"] {
        db.delete_setting(k)?;
    }
    db.audit("shell", "leave_hub", "");
    Ok(())
}

impl HubClient {
    pub fn load(db: &Db, data_dir: &Path) -> Result<Option<Arc<HubClient>>> {
        let (Some(url), Some(vendo_id)) = (db.get_setting("hub_url")?, db.get_setting("hub_vendo_id")?) else {
            return Ok(None);
        };
        let name = db.get_setting("hub_vendo_name")?.unwrap_or_default();
        Ok(Some(Arc::new(HubClient {
            status: Mutex::new(HubStatus { url: url.clone(), name, vendo_id: vendo_id.clone(), ..Default::default() }),
            url,
            vendo_id,
            key: signing_key(data_dir)?,
            agent: agent(),
        })))
    }

    pub fn status(&self) -> HubStatus {
        self.status.lock().unwrap().clone()
    }

    fn call<T: Serialize, R: DeserializeOwned>(&self, path: &str, body: &T) -> Result<R, HubError> {
        let body = serde_json::to_vec(body).map_err(|e| HubError::Offline(e.to_string()))?;
        let ts = crate::db::now();
        let nonce = hex::encode(rand::random::<[u8; 16]>());
        let sig = self.key.sign(&signing_message("POST", path, &self.vendo_id, ts, &nonce, &body));
        let res = self
            .agent
            .post(&format!("{}{path}", self.url))
            .set("content-type", "application/json")
            .set("x-vendo-id", &self.vendo_id)
            .set("x-ts", &ts.to_string())
            .set("x-nonce", &nonce)
            .set("x-sig", &hex::encode(sig.to_bytes()))
            .send_bytes(&body);
        let out = match res {
            Ok(r) => r.into_json::<R>().map_err(|e| HubError::Offline(e.to_string())),
            Err(ureq::Error::Status(404, _)) => Err(HubError::NotFound),
            Err(ureq::Error::Status(code, r)) => Err(HubError::Rejected(format!("{code} {}", r.into_string().unwrap_or_default()))),
            Err(e) => Err(HubError::Offline(e.to_string())),
        };
        let mut st = self.status.lock().unwrap();
        match &out {
            Ok(_) | Err(HubError::NotFound) => {
                st.online = true;
                st.last_error = None;
            }
            Err(e) => {
                st.online = false;
                st.last_error = Some(e.to_string());
            }
        }
        out
    }

    async fn blocking<R: Send + 'static>(
        self: &Arc<Self>,
        f: impl FnOnce(&HubClient) -> Result<R, HubError> + Send + 'static,
    ) -> Result<R, HubError> {
        let me = self.clone();
        tokio::task::spawn_blocking(move || f(&me)).await.map_err(|e| HubError::Offline(e.to_string()))?
    }

    pub async fn create_pass(self: &Arc<Self>) -> Result<PassResp, HubError> {
        self.blocking(|c| c.call("/v1/passes", &serde_json::json!({}))).await
    }

    pub async fn claim(self: &Arc<Self>, code: String) -> Result<ClaimResp, HubError> {
        self.blocking(move |c| c.call("/v1/claim", &ClaimReq { code })).await
    }

    pub async fn sync(self: &Arc<Self>, req: SyncReq) -> Result<SyncResp, HubError> {
        let n = req.leases.len();
        let r: Result<SyncResp, HubError> = self.blocking(move |c| c.call("/v1/sync", &req)).await;
        if let Ok(resp) = &r {
            let mut st = self.status.lock().unwrap();
            st.last_sync = Some(crate::db::now());
            st.passes_here = n;
            st.wallet = resp.wallet;
        }
        r
    }

    /// A single-use link on the hub for adding a pass to Google/Apple Wallet.
    pub async fn wallet(self: &Arc<Self>, code: String, kind: WalletKind) -> Result<String, HubError> {
        let r: WalletResp = self.blocking(move |c| c.call("/v1/wallet", &WalletReq { code, kind })).await?;
        // Only ever send the customer to a path on our own hub (no "@host" tricks).
        let ok = r.path.starts_with("/v1/wallet/") && r.path.bytes().all(|b| b.is_ascii_alphanumeric() || b"/._-".contains(&b));
        if !ok {
            return Err(HubError::Rejected("unexpected wallet link".into()));
        }
        Ok(format!("{}{}", self.url, r.path))
    }
}
