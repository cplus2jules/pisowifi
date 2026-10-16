//! Vendo side of the hub connection: enrolment, signed calls, heartbeat,
//! and the Piso Pass flows that move time between this vendo and the hub.
//! Connections are outbound only, so vendos need no open ports.
//!
//! Every flow that changes which passes this vendo holds (sync, claim, new
//! pass) runs under one lock, so a sync's snapshot never misses a pass that
//! is being claimed at the same moment.

use crate::db::Db;
use crate::engine::{Engine, Resume, MAX_GRANT_SECS};
use crate::hub::proto::*;
use crate::hub::{normalize_code, signing_message};
use crate::net::Mac;
use anyhow::{bail, Context, Result};
use ed25519_dalek::{Signer, SigningKey};
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::net::Ipv4Addr;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Sales reported per sync (the hub takes up to MAX_SYNC_CREDITS).
const CREDITS_PER_SYNC: u32 = 200;

pub struct HubClient {
    url: String,
    vendo_id: String,
    key: SigningKey,
    agent: ureq::Agent,
    db: Arc<Db>,
    status: Mutex<HubStatus>,
    /// Held across a hub call and the matching change to the engine.
    passes: tokio::sync::Mutex<()>,
    chain: Mutex<Option<String>>,
}

#[derive(Clone, Serialize, Default)]
pub struct HubStatus {
    pub url: String,
    pub name: String,
    pub vendo_id: String,
    pub online: bool,
    pub last_sync: Option<i64>,
    pub last_error: Option<String>,
    /// Phones holding pass time here.
    pub passes_here: usize,
    /// Wallet passes the hub can make, as of the last sync.
    pub wallet: WalletSupport,
}

#[derive(Debug)]
pub enum HubError {
    /// The hub says this pass code doesn't exist.
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

/// A hub error status. Only a 404 whose body says `unknown_pass` means the
/// pass doesn't exist; any other 404 (a proxy pointing at the wrong place)
/// is a hub problem, so a customer's saved pass is never written off over it.
fn status_error(code: u16, body: &str) -> HubError {
    #[derive(serde::Deserialize)]
    struct Body {
        code: Option<String>,
    }
    let known = serde_json::from_str::<Body>(body).ok().and_then(|b| b.code);
    match (code, known.as_deref()) {
        (404, Some(UNKNOWN_PASS)) => HubError::NotFound,
        _ => {
            let mut body: String = body.chars().take(200).collect();
            body.retain(|c| !c.is_control());
            HubError::Rejected(format!("{code} {body}"))
        }
    }
}

const SIGNING_KEY: &str = "hub_signing_key";
/// Where older versions kept the key, unencrypted on the SD card.
const OLD_KEY_FILE: &str = "hub-signing.key";

/// This vendo's Ed25519 key for the hub. It lives in the encrypted database
/// (locked to the board's chip ID), so a copied SD card doesn't give it away,
/// and it's in passphrase backups, so a replacement board keeps its place in
/// the chain. A key file from an older version is moved in (same key, no
/// re-enrolment) and then removed.
fn signing_key(db: &Db, data_dir: &Path) -> Result<SigningKey> {
    let file = data_dir.join(OLD_KEY_FILE);
    let from_file = match std::fs::read(&file) {
        Ok(b) => Some(zeroize::Zeroizing::new(b)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).with_context(|| format!("reading {}", file.display())),
    };
    let raw = match (db.get_secret(SIGNING_KEY)?, &from_file) {
        (Some(k), _) => k,
        (None, Some(old)) => {
            if old.len() != 32 {
                bail!("{} is corrupt (expected 32 bytes)", file.display());
            }
            let k = db.put_secret_once(SIGNING_KEY, old)?;
            tracing::info!("hub signing key moved into the encrypted database");
            k
        }
        (None, None) => db.put_secret_once(SIGNING_KEY, &rand::random::<[u8; 32]>())?,
    };
    if let Some(old) = from_file {
        if *old == *raw {
            remove_key_file(&file);
        } else {
            tracing::warn!("{} doesn't match the key in the database; left in place (the database's key is used)", file.display());
        }
    }
    let bytes: [u8; 32] = raw[..].try_into().context("hub signing key in the database")?;
    Ok(SigningKey::from_bytes(&bytes))
}

/// Overwrites, then deletes. On flash the old blocks may survive wear
/// levelling, so this is best effort; the key no longer depends on it.
fn remove_key_file(file: &Path) {
    let wiped = std::fs::OpenOptions::new().write(true).open(file).and_then(|mut f| {
        use std::io::Write;
        f.write_all(&[0u8; 32])?;
        f.sync_all()
    });
    if let Err(e) = wiped.and_then(|_| std::fs::remove_file(file)) {
        tracing::warn!("couldn't remove {}: {e}", file.display());
    }
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
    let key = signing_key(db, data_dir)?;
    let req = EnrollReq { code: code.trim().to_string(), pubkey: hex::encode(key.verifying_key().as_bytes()) };
    let resp: EnrollResp = match agent().post(&format!("{url}/v1/enroll")).send_json(&req) {
        Ok(r) => r.into_json()?,
        Err(ureq::Error::Status(_, r)) => bail!("hub refused: {}", r.into_string().unwrap_or_default()),
        Err(e) => bail!("can't reach the hub: {e}"),
    };
    // Links and unsent sales from an earlier enrolment mean nothing to this
    // one. (A running service saves its links again on the way down; the
    // next start drops them, see Db::reset_pass_state_if_hub_changed.)
    db.clear_pass_state()?;
    db.set_setting("hub_url", &url)?;
    db.set_setting("hub_vendo_id", &resp.vendo_id)?;
    db.set_setting("hub_vendo_name", &resp.name)?;
    if resp.chain.is_empty() {
        db.delete_setting("hub_chain")?;
    } else {
        db.set_setting("hub_chain", &resp.chain)?;
    }
    db.audit("shell", "join_hub", &format!("{url} as {}", resp.name));
    Ok(resp)
}

/// Phones keep the time they have here; it just stops being a pass.
pub fn leave(db: &Db) -> Result<()> {
    for k in ["hub_url", "hub_vendo_id", "hub_vendo_name", "hub_chain"] {
        db.delete_setting(k)?;
    }
    db.clear_pass_state()?;
    db.audit("shell", "leave_hub", "");
    Ok(())
}

/// What happened to a pass after coins were paid at this vendo.
#[derive(Debug, PartialEq)]
pub enum AfterPurchase {
    /// The coins went to the pass this phone already holds here.
    AlreadyLinked,
    /// The phone's saved pass was claimed; its time (seconds) joined the coins.
    Claimed(u64),
    /// A new pass for a first-time customer.
    NewPass(String),
    /// The hub doesn't know the saved pass. The time stays on the phone.
    UnknownPass,
    /// The hub couldn't be reached. The time stays on the phone; the
    /// portal claims the saved pass later.
    Offline,
}

#[derive(Debug)]
pub enum PassError {
    /// The hub says this code doesn't exist.
    Unknown,
    /// This phone's pass time is already here.
    AlreadyHere,
    /// This phone already has a pass.
    HasPass,
    /// No time on this phone.
    NoTime,
    Hub(HubError),
}

#[derive(Debug)]
pub struct Claimed {
    /// Seconds the pass brought.
    pub secs: u64,
    /// This phone's time now.
    pub total: u64,
    pub running: bool,
}

impl HubClient {
    pub fn load(db: &Arc<Db>, data_dir: &Path) -> Result<Option<Arc<HubClient>>> {
        let (Some(url), Some(vendo_id)) = (db.get_setting("hub_url")?, db.get_setting("hub_vendo_id")?) else {
            return Ok(None);
        };
        let name = db.get_setting("hub_vendo_name")?.unwrap_or_default();
        Ok(Some(Arc::new(HubClient {
            status: Mutex::new(HubStatus { url: url.clone(), name, vendo_id: vendo_id.clone(), ..Default::default() }),
            url,
            vendo_id,
            key: signing_key(db, data_dir)?,
            agent: agent(),
            chain: Mutex::new(db.get_setting("hub_chain")?),
            db: db.clone(),
            passes: tokio::sync::Mutex::new(()),
        })))
    }

    pub fn status(&self) -> HubStatus {
        self.status.lock().unwrap().clone()
    }

    /// The hub's chain id: phones keep passes per chain.
    pub fn chain(&self) -> Option<String> {
        self.chain.lock().unwrap().clone()
    }

    fn set_chain(&self, chain: &str) {
        let mut c = self.chain.lock().unwrap();
        if chain.is_empty() || c.as_deref() == Some(chain) {
            return;
        }
        if let Err(e) = self.db.set_setting("hub_chain", chain) {
            tracing::error!("saving hub chain id: {e:#}");
        }
        *c = Some(chain.to_string());
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
            Err(ureq::Error::Status(code, r)) => {
                let body = r.into_string().unwrap_or_default();
                Err(status_error(code, &body))
            }
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

    async fn create_pass(self: &Arc<Self>) -> Result<PassResp, HubError> {
        self.blocking(|c| c.call("/v1/passes", &serde_json::json!({}))).await
    }

    async fn claim(self: &Arc<Self>, code: String) -> Result<ClaimResp, HubError> {
        self.blocking(move |c| c.call("/v1/claim", &ClaimReq { code })).await
    }

    /// One heartbeat: report every pass held here and the sales made for
    /// them, then drop what the hub says moved away and links whose time ran out.
    pub async fn sync_once(self: &Arc<Self>, engine: &Engine) -> Result<(), HubError> {
        let _held = self.passes.lock().await;
        let (leases, credits) = engine.sync_snapshot(CREDITS_PER_SYNC);
        let sent = leases.clone();
        let req = SyncReq { leases, credits };
        let resp: SyncResp = self.blocking(move |c| c.call("/v1/sync", &req)).await?;
        engine.after_sync(&sent, &resp.revoked);
        if let Err(e) = self.db.outbox_delete(&resp.acked) {
            tracing::error!("clearing reported sales: {e:#}");
        }
        self.set_chain(&resp.chain);
        let mut st = self.status.lock().unwrap();
        st.last_sync = Some(crate::db::now());
        st.passes_here = sent.iter().filter(|l| l.remaining_secs > 0).count();
        st.wallet = resp.wallet;
        Ok(())
    }

    /// Brings a pass's time to this phone. `start`: the customer asked for
    /// it (typed the code, confirmed a link), so the clock starts.
    pub async fn claim_pass(self: &Arc<Self>, engine: &Engine, mac: Mac, ip: Ipv4Addr, code: String, start: bool) -> Result<Claimed, PassError> {
        let _held = self.passes.lock().await;
        // Checked before asking the hub, so time is never moved and then lost:
        // a phone whose pass still has time here can't swap in another pass.
        if engine.holds_pass_time(mac) {
            return Err(PassError::AlreadyHere);
        }
        match self.claim(code).await {
            Ok(r) => {
                let secs = r.secs.min(MAX_GRANT_SECS);
                let (total, running) = engine.claim_into(mac, ip, r.pass_id, secs, start || r.running, 0);
                Ok(Claimed { secs, total, running })
            }
            Err(HubError::NotFound) => Err(PassError::Unknown),
            Err(e) => Err(PassError::Hub(e)),
        }
    }

    /// Coins were just credited to a phone (`pesos`). If it holds no pass
    /// here, claim the pass saved on the phone (never replacing it with a
    /// new one), or give a first-time customer a new pass.
    pub async fn after_purchase(self: &Arc<Self>, engine: &Engine, mac: Mac, ip: Ipv4Addr, saved: Option<String>, pesos: u32) -> AfterPurchase {
        let _held = self.passes.lock().await;
        if engine.pass_of(mac).is_some() {
            return AfterPurchase::AlreadyLinked;
        }
        match saved.filter(|c| normalize_code(c).len() == 16) {
            Some(code) => match self.claim(code).await {
                Ok(r) => {
                    let secs = r.secs.min(MAX_GRANT_SECS);
                    engine.claim_into(mac, ip, r.pass_id, secs, true, pesos);
                    AfterPurchase::Claimed(secs)
                }
                Err(HubError::NotFound) => AfterPurchase::UnknownPass,
                Err(e) => {
                    tracing::warn!("claiming saved pass: {e}");
                    AfterPurchase::Offline
                }
            },
            None => match self.create_pass().await {
                Ok(p) => {
                    engine.link_pass(mac, p.pass_id, pesos);
                    AfterPurchase::NewPass(p.code)
                }
                Err(e) => {
                    tracing::warn!("creating pass: {e}");
                    AfterPurchase::Offline
                }
            },
        }
    }

    /// "Save my time": puts a phone's time (bought while the hub was out of
    /// reach) on a new pass.
    pub async fn save_pass(self: &Arc<Self>, engine: &Engine, mac: Mac) -> Result<String, PassError> {
        let _held = self.passes.lock().await;
        if engine.pass_of(mac).is_some() {
            return Err(PassError::HasPass);
        }
        if !engine.has_time(mac) {
            return Err(PassError::NoTime);
        }
        let p = self.create_pass().await.map_err(PassError::Hub)?;
        engine.link_pass(mac, p.pass_id, 0);
        Ok(p.code)
    }

    /// Resume, checking with the hub first if paused pass time hasn't been
    /// confirmed lately (it may have been claimed at another location).
    pub async fn resume(self: &Arc<Self>, engine: &Engine, mac: Mac, ip: Ipv4Addr) -> Resume {
        match engine.resume(mac, ip) {
            Resume::NeedsHub => {
                if let Err(e) = self.sync_once(engine).await {
                    tracing::warn!("hub sync before resuming pass time: {e}");
                }
                engine.resume(mac, ip)
            }
            r => r,
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::CoinStart;
    use crate::hub::tests::{serve_test_hub, TestHub};
    use crate::keys::DbKey;
    use std::sync::atomic::AtomicBool;
    use zeroize::Zeroizing;

    const PHONE: &str = "02:11:22:33:44:55";
    const IP: Ipv4Addr = Ipv4Addr::new(10, 0, 0, 9);

    struct Vendo {
        dir: std::path::PathBuf,
        db: Arc<Db>,
        engine: Arc<Engine>,
        hub: Arc<HubClient>,
    }

    impl Drop for Vendo {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// A vendo enrolled with `h` through the real HTTP API.
    async fn vendo(h: &TestHub, name: &str) -> Vendo {
        let dir = std::env::temp_dir().join(format!("pisowifi-vendo-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = Arc::new(Db::open(&dir.join("t.db"), &DbKey(Zeroizing::new([7u8; 32]))).unwrap());
        let code = h.st.db.add_enroll_code(name).unwrap();
        let (db2, dir2, url) = (db.clone(), dir.clone(), h.url.clone());
        tokio::task::spawn_blocking(move || join(&db2, &dir2, &url, &code, true)).await.unwrap().unwrap();
        let fw = crate::firewall::Firewall::start(&crate::config::Config::dev(dir.clone()), true).await.unwrap();
        let engine = Engine::new(db.clone(), fw, None, 1, 60, Arc::new(AtomicBool::new(true))).unwrap();
        let hub = HubClient::load(&db, &dir).unwrap().unwrap();
        Vendo { dir, db, engine, hub }
    }

    impl Vendo {
        /// Coins through the portal's flow; returns (seconds bought, what the pass did).
        async fn pay(&self, mac: Mac, pesos: u32, saved: Option<&str>) -> (u64, AfterPurchase) {
            assert!(matches!(self.engine.coin_start(mac, IP), CoinStart::Ok));
            self.engine.test_coin(pesos);
            let (pesos, secs) = self.engine.coin_done(mac).unwrap();
            (secs, self.hub.after_purchase(&self.engine, mac, IP, saved.map(str::to_string), pesos).await)
        }
        fn left(&self, mac: Mac) -> (u64, bool) {
            let me = self.engine.me(mac, IP);
            (me.remaining_secs, me.running)
        }
        /// This vendo's view of the hub, but pointed somewhere nothing answers.
        fn cut_off(&self) -> Arc<HubClient> {
            self.db.set_setting("hub_url", "http://127.0.0.1:9").unwrap();
            HubClient::load(&self.db, &self.dir).unwrap().unwrap()
        }
    }

    fn hub_lease(h: &TestHub, pass_id: &str) -> (Option<String>, i64) {
        let (_, vendo, secs) = h.st.db.lease_for_tests(pass_id);
        (vendo, secs)
    }

    #[test]
    fn only_the_hubs_own_unknown_pass_answer_means_no_such_pass() {
        assert!(matches!(status_error(404, r#"{"error":"unknown pass","code":"unknown_pass"}"#), HubError::NotFound));
        // A proxy sending 404 for everything, another site, a bare 404.
        assert!(matches!(status_error(404, "<html>404 page not found</html>"), HubError::Rejected(_)));
        assert!(matches!(status_error(404, r#"{"error":"not found"}"#), HubError::Rejected(_)));
        assert!(matches!(status_error(404, ""), HubError::Rejected(_)));
        assert!(matches!(status_error(410, r#"{"code":"unknown_pass"}"#), HubError::Rejected(_)));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn passes_move_between_vendos_end_to_end() {
        let h = serve_test_hub().await;
        let (a, b) = (vendo(&h, "Branch A").await, vendo(&h, "Branch B").await);
        let mac = Mac::parse(PHONE).unwrap();
        assert_eq!(a.hub.chain().as_deref(), Some(h.st.db.chain()), "chain id from enrolment");

        // First purchase at A: a new pass, reported with its sale.
        let (bought, r) = a.pay(mac, 5, None).await;
        let AfterPurchase::NewPass(code) = r else { panic!("{r:?}") };
        let pass_id = a.engine.pass_of(mac).unwrap();
        assert!(a.engine.pause(mac));
        a.hub.sync_once(&a.engine).await.unwrap();
        let (vendo, secs) = hub_lease(&h, &pass_id);
        assert!(vendo == Some(a.hub.status().vendo_id) && bought.abs_diff(secs as u64) <= 1);

        // At B: the saved pass is claimed automatically and stays paused.
        let c = b.hub.claim_pass(&b.engine, mac, IP, code.clone(), false).await.unwrap();
        assert!(c.secs.abs_diff(bought) <= 1 && !c.running);
        assert!(!b.left(mac).1);
        // Its time can't be swapped out for another pass.
        assert!(matches!(b.hub.claim_pass(&b.engine, mac, IP, code.clone(), true).await, Err(PassError::AlreadyHere)));
        // A hears the pass moved and cuts the phone off.
        a.hub.sync_once(&a.engine).await.unwrap();
        assert_eq!(a.left(mac), (0, false));
        // B hasn't synced yet: resuming checks with the hub first.
        assert!(matches!(b.hub.resume(&b.engine, mac, IP).await, Resume::Resumed));
        // Unknown codes are told apart from hub trouble.
        let wrong = "PASS-AAAA-BBBB-CCCC-DDDD".to_string();
        assert!(matches!(b.hub.claim_pass(&b.engine, Mac::parse("02:00:00:00:00:07").unwrap(), IP, wrong, true).await, Err(PassError::Unknown)));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn buying_coins_brings_the_saved_pass_instead_of_making_a_new_one() {
        let h = serve_test_hub().await;
        let (a, b) = (vendo(&h, "Branch A").await, vendo(&h, "Branch B").await);
        let mac = Mac::parse(PHONE).unwrap();
        let (_, r) = a.pay(mac, 5, None).await;
        let AfterPurchase::NewPass(code) = r else { panic!("{r:?}") };
        let pass_id = a.engine.pass_of(mac).unwrap();
        assert!(a.engine.pause(mac));
        a.hub.sync_once(&a.engine).await.unwrap();
        let held = a.left(mac).0;
        let passes = |h: &TestHub| h.st.db.summary().unwrap().0;
        assert_eq!(passes(&h), 1);

        // At B the auto-claim didn't happen (hub blip), then the phone pays.
        let offline = b.cut_off();
        assert!(matches!(offline.after_purchase(&b.engine, mac, IP, Some(code.clone()), 0).await, AfterPurchase::Offline));
        assert_eq!(passes(&h), 1, "no new pass while the saved one couldn't be claimed");
        let (bought, r) = b.pay(mac, 10, Some(&code)).await;
        assert!(matches!(r, AfterPurchase::Claimed(s) if s.abs_diff(held) <= 1), "{r:?}");
        assert_eq!(passes(&h), 1, "still one pass");
        assert_eq!(b.engine.pass_of(mac), Some(pass_id.clone()));
        let (total, running) = b.left(mac);
        assert!(running && total.abs_diff(held + bought) <= 2);
        // The coins paid at B count towards the pass on the hub (not capped away).
        b.hub.sync_once(&b.engine).await.unwrap();
        let (vendo, secs) = hub_lease(&h, &pass_id);
        assert_eq!(vendo, Some(b.hub.status().vendo_id));
        assert!((secs as u64).abs_diff(total) <= 2, "hub {secs} vs phone {total}");

        // A saved pass the hub doesn't know: the time stays, no pass is made.
        let other = Mac::parse("02:00:00:00:00:02").unwrap();
        let (_, r) = b.pay(other, 5, Some("PASS-AAAA-BBBB-CCCC-DDDD")).await;
        assert_eq!(r, AfterPurchase::UnknownPass);
        assert!(b.engine.pass_of(other).is_none() && b.engine.has_time(other));
        assert_eq!(passes(&h), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_sync_waits_for_a_claim_in_progress() {
        let h = serve_test_hub().await;
        let a = vendo(&h, "Branch A").await;
        let held = a.hub.passes.lock().await;
        let (hub, engine) = (a.hub.clone(), a.engine.clone());
        let sync = tokio::spawn(async move { hub.sync_once(&engine).await });
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!sync.is_finished(), "the snapshot waits until the claim has landed in the engine");
        drop(held);
        sync.await.unwrap().unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn spent_passes_stop_being_reported() {
        let h = serve_test_hub().await;
        let a = vendo(&h, "Branch A").await;
        for i in 0..5u8 {
            let mac = Mac::parse(&format!("02:00:00:00:00:{i:02x}")).unwrap();
            a.pay(mac, 1, None).await;
            a.engine.kick(mac); // time used up
        }
        a.hub.sync_once(&a.engine).await.unwrap();
        a.hub.sync_once(&a.engine).await.unwrap();
        assert!(a.engine.sync_snapshot(100).0.is_empty(), "links don't pile up");
        assert!(a.db.load_links().unwrap().is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn paused_pass_time_wont_restart_while_the_hub_is_unreachable() {
        let h = serve_test_hub().await;
        let a = vendo(&h, "Branch A").await;
        let mac = Mac::parse(PHONE).unwrap();
        a.pay(mac, 5, None).await;
        assert!(a.engine.pause(mac));
        let offline = a.cut_off();
        assert!(matches!(offline.resume(&a.engine, mac, IP).await, Resume::NeedsHub));
        assert!(!a.left(mac).1);
        // Back in touch: fine.
        assert!(matches!(a.hub.resume(&a.engine, mac, IP).await, Resume::Resumed));
    }

    #[test]
    fn the_signing_key_moves_from_the_sd_card_into_the_database() {
        let dir = std::env::temp_dir().join(format!("pisowifi-sigkey-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&dir).unwrap();
        let key = DbKey(Zeroizing::new([7u8; 32]));
        let open = || Db::open(&dir.join("t.db"), &key).unwrap();
        let file = dir.join(OLD_KEY_FILE);
        // An older install: the key is a plain file next to the database.
        std::fs::write(&file, [9u8; 32]).unwrap();
        let enrolled = SigningKey::from_bytes(&[9u8; 32]).verifying_key();
        let db = open();
        assert_eq!(signing_key(&db, &dir).unwrap().verifying_key(), enrolled, "same key: no re-enrolment");
        assert!(!file.exists(), "the plain copy is gone");
        let on_disk = |name: &str| std::fs::read(dir.join(name)).unwrap_or_default();
        assert!(!on_disk("t.db-wal").is_empty(), "the write is in the WAL");
        for name in ["t.db", "t.db-wal"] {
            assert!(!on_disk(name).windows(32).any(|w| w == [9u8; 32]), "{name} holds it encrypted");
        }
        drop(db);
        assert_eq!(signing_key(&open(), &dir).unwrap().verifying_key(), enrolled, "kept across restarts");

        // A stray file that isn't this vendo's key is left alone, and unused.
        std::fs::write(&file, [1u8; 32]).unwrap();
        assert_eq!(signing_key(&open(), &dir).unwrap().verifying_key(), enrolled);
        assert!(file.exists());
        let _ = std::fs::remove_dir_all(&dir);

        // A new vendo: a random key, never written to a file.
        let dir2 = std::env::temp_dir().join(format!("pisowifi-sigkey-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&dir2).unwrap();
        let db = Db::open(&dir2.join("t.db"), &key).unwrap();
        let a = signing_key(&db, &dir2).unwrap().verifying_key();
        assert_eq!(signing_key(&db, &dir2).unwrap().verifying_key(), a);
        assert!(!dir2.join(OLD_KEY_FILE).exists());
        let _ = std::fs::remove_dir_all(dir2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn leaving_the_hub_forgets_pass_links_but_not_the_time() {
        let h = serve_test_hub().await;
        let a = vendo(&h, "Branch A").await;
        let mac = Mac::parse(PHONE).unwrap();
        a.pay(mac, 5, None).await;
        a.engine.persist();
        assert_eq!(a.db.load_links().unwrap().len(), 1);
        assert_eq!(a.db.outbox_list(10).unwrap().len(), 1);
        leave(&a.db).unwrap();
        assert!(a.db.load_links().unwrap().is_empty() && a.db.outbox_list(10).unwrap().is_empty());
        assert!(a.db.get_setting("hub_chain").unwrap().is_none());
        assert_eq!(a.db.load_clients().unwrap().len(), 1, "the phone keeps its time");
        assert!(a.db.get_secret(SIGNING_KEY).unwrap().is_some(), "and the vendo its key, to join again");
    }
}
