//! The hub: one small server (a cloud VPS) that keeps Piso Pass balances
//! for all of an owner's vendos, so time bought at one location works at
//! every other.
//!
//! Model
//! * A pass's time is either in the hub (`balance_secs`) or "leased" to the
//!   vendo where the customer currently is. Only one vendo holds a lease.
//! * Vendos report their leases every 30 s (remaining time, running or not).
//! * Claiming a pass at vendo B refunds A's lease (last report minus time
//!   that has run since), moves everything to B, and A drops that phone on
//!   its next report.
//! * A lease a vendo stops reporting goes back to the pass's balance (a
//!   vendo drops a pass once its time there has run out and it reported
//!   zero). Leases granted in the last [`LEASE_GRACE_SECS`] are left alone:
//!   the report may have been taken before the grant reached the vendo.
//! * A report can only lower a lease, or raise it by time the vendo has
//!   reported selling for that pass, so a stolen vendo key can't mint time.
//!
//! Security
//! * Vendos enrol with a one-time code and an Ed25519 key that never leaves
//!   the box. Every request is signed over method, path, time, nonce and
//!   body hash; stale (±5 min) or replayed requests are refused.
//! * Pass codes are bearer secrets: only an HMAC of each code is stored.
//! * The hub's own database is SQLCipher-encrypted like the vendos'.

use crate::app::ApiError;
use crate::db::apply_key;
use crate::keys::{read_or_create_secret, DbKey};
use crate::wallet::Wallet;
use anyhow::{bail, Context, Result};
use axum::body::{Body, Bytes};
use axum::extract::{ConnectInfo, DefaultBodyLimit, Path as AxPath, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use hmac::{Hmac, Mac as _};
use rand::Rng;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use zeroize::Zeroizing;

pub const MAX_SKEW_SECS: i64 = 300;
/// Most leases and credits one sync may carry. A vendo reports only phones
/// holding pass time there, so real vendos stay far below this; a bigger
/// sync is refused whole rather than cut short.
pub const MAX_SYNC_LEASES: usize = 2000;
pub const MAX_SYNC_CREDITS: usize = 500;
/// An unreported lease younger than this may have been granted after the
/// vendo took its snapshot, so it isn't taken back yet.
pub const LEASE_GRACE_SECS: i64 = 90;
/// Request bodies: a sync is the only big one (see MAX_SYNC_*).
const SMALL_BODY: usize = 4 * 1024;
const SYNC_BODY: usize = 256 * 1024;
/// Unambiguous characters for codes people type (no 0/O, 1/I/L).
const ALPHABET: &[u8] = b"23456789ABCDEFGHJKMNPQRSTUVWXYZ";

// ---- wire protocol (shared with the vendo client) ---------------------------

pub mod proto {
    use serde::{Deserialize, Serialize};

    /// `code` in the hub's 404 body when a pass code doesn't exist. Only this
    /// means "no such pass": a 404 from anything else (a misconfigured proxy)
    /// is a hub problem.
    pub const UNKNOWN_PASS: &str = "unknown_pass";

    #[derive(Serialize, Deserialize)]
    pub struct EnrollReq {
        pub code: String,
        pub pubkey: String,
    }
    #[derive(Serialize, Deserialize)]
    pub struct EnrollResp {
        pub vendo_id: String,
        pub name: String,
        /// This hub's chain id (see SyncResp::chain).
        #[serde(default)]
        pub chain: String,
    }
    #[derive(Serialize, Deserialize)]
    pub struct PassResp {
        pub pass_id: String,
        pub code: String,
    }
    #[derive(Serialize, Deserialize)]
    pub struct ClaimReq {
        pub code: String,
    }
    #[derive(Serialize, Deserialize)]
    pub struct ClaimResp {
        pub pass_id: String,
        pub secs: u64,
        /// The time was running where it was (false: paused, or waiting on
        /// the hub), so a claim the customer didn't ask for keeps it paused.
        #[serde(default)]
        pub running: bool,
    }
    #[derive(Serialize, Deserialize, Clone, Debug)]
    pub struct LeaseReport {
        pub pass_id: String,
        pub remaining_secs: u64,
        pub running: bool,
    }
    #[derive(Serialize, Deserialize, Clone, Debug)]
    pub struct CreditReport {
        pub id: i64,
        pub pass_id: String,
        pub pesos: u32,
        pub secs: u64,
        pub ts: i64,
    }
    #[derive(Serialize, Deserialize, Default)]
    pub struct SyncReq {
        pub leases: Vec<LeaseReport>,
        pub credits: Vec<CreditReport>,
    }
    #[derive(Serialize, Deserialize, Default)]
    pub struct SyncResp {
        /// Passes this vendo must drop: the customer moved elsewhere.
        pub revoked: Vec<String>,
        /// Credit ids stored; the vendo can forget them.
        pub acked: Vec<i64>,
        /// Random id of this hub (its database). Phones keep passes per
        /// chain, since every owner's vendos share the address 10.0.0.1.
        #[serde(default)]
        pub chain: String,
        /// Wallet passes the hub can make (absent from older hubs).
        #[serde(default)]
        pub wallet: WalletSupport,
    }
    #[derive(Serialize, Deserialize, Clone, Copy, Default, Debug, PartialEq, Eq)]
    pub struct WalletSupport {
        pub google: bool,
        pub apple: bool,
    }
    impl WalletSupport {
        pub fn has(&self, kind: WalletKind) -> bool {
            match kind {
                WalletKind::Google => self.google,
                WalletKind::Apple => self.apple,
            }
        }
    }
    #[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
    #[serde(rename_all = "lowercase")]
    pub enum WalletKind {
        Google,
        Apple,
    }
    #[derive(Serialize, Deserialize)]
    pub struct WalletReq {
        pub code: String,
        pub kind: WalletKind,
    }
    #[derive(Serialize, Deserialize)]
    pub struct WalletResp {
        /// Download path on the hub, single use.
        pub path: String,
        pub expires_in: i64,
    }
}
use proto::*;

/// Canonical string a vendo signs for each request.
pub fn signing_message(method: &str, path: &str, vendo_id: &str, ts: i64, nonce: &str, body: &[u8]) -> Vec<u8> {
    format!("pisowifi-hub-v1\n{method}\n{path}\n{vendo_id}\n{ts}\n{nonce}\n{}", hex::encode(Sha256::digest(body))).into_bytes()
}

pub fn random_code(groups: usize) -> String {
    let mut rng = rand::rngs::OsRng;
    (0..groups)
        .map(|_| (0..4).map(|_| ALPHABET[rng.gen_range(0..ALPHABET.len())] as char).collect::<String>())
        .collect::<Vec<_>>()
        .join("-")
}

/// "pass 7k3m-…" → "7K3M…": uppercase, alphanumerics only, PASS prefix dropped.
pub fn normalize_code(s: &str) -> String {
    let up: String = s.chars().filter(|c| c.is_ascii_alphanumeric()).map(|c| c.to_ascii_uppercase()).collect();
    up.strip_prefix("PASS").map(str::to_string).unwrap_or(up)
}

// ---- config -------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HubConfig {
    pub listen: SocketAddr,
    pub data_dir: PathBuf,
    /// TLS certificate + key (PEM). Leave out when a reverse proxy such as
    /// Caddy terminates HTTPS in front of the hub.
    #[serde(default)]
    pub tls_cert: Option<PathBuf>,
    #[serde(default)]
    pub tls_key: Option<PathBuf>,
    /// "Add to Google/Apple Wallet" (optional; off when left out).
    #[serde(default)]
    pub wallet: Option<crate::wallet::WalletConfig>,
    /// The signed update list to check; left out means the usual one, ""
    /// turns online checks off.
    #[serde(default)]
    pub update_url: Option<String>,
}

impl HubConfig {
    pub fn load(path: &Path) -> Result<HubConfig> {
        let raw = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Ok(toml::from_str(&raw).context("parsing hub config")?)
    }
    pub fn dev(data_dir: PathBuf) -> HubConfig {
        HubConfig { listen: "127.0.0.1:8700".parse().unwrap(), data_dir, tls_cert: None, tls_key: None, wallet: None, update_url: None }
    }
}

// ---- storage ------------------------------------------------------------------

const HUB_SCHEMA: &str = "
CREATE TABLE vendos (
    id         TEXT PRIMARY KEY,
    name       TEXT NOT NULL,
    pubkey     BLOB NOT NULL,
    created_at INTEGER NOT NULL,
    last_seen  INTEGER
);
CREATE TABLE enroll_codes (
    code_hash  BLOB PRIMARY KEY,
    name       TEXT NOT NULL,
    expires_at INTEGER NOT NULL
);
CREATE TABLE passes (
    id                TEXT PRIMARY KEY,
    code_hash         BLOB NOT NULL UNIQUE,
    balance_secs      INTEGER NOT NULL DEFAULT 0 CHECK (balance_secs >= 0),
    created_at        INTEGER NOT NULL,
    last_used         INTEGER NOT NULL,
    lease_vendo       TEXT REFERENCES vendos (id),
    lease_secs        INTEGER NOT NULL DEFAULT 0 CHECK (lease_secs >= 0),
    lease_running     INTEGER NOT NULL DEFAULT 0,
    lease_reported_at INTEGER
);
CREATE TABLE credits (
    vendo_id  TEXT NOT NULL,
    credit_id INTEGER NOT NULL,
    pass_id   TEXT NOT NULL,
    pesos     INTEGER NOT NULL,
    secs      INTEGER NOT NULL,
    ts        INTEGER NOT NULL,
    PRIMARY KEY (vendo_id, credit_id)
);
CREATE TABLE events (
    id     INTEGER PRIMARY KEY,
    ts     INTEGER NOT NULL,
    vendo  TEXT,
    pass   TEXT,
    action TEXT NOT NULL,
    secs   INTEGER NOT NULL DEFAULT 0
);
";

pub struct HubDb {
    conn: Mutex<Connection>,
    /// HMAC key for pass codes; lives inside the encrypted DB.
    code_key: Zeroizing<Vec<u8>>,
    chain: String,
}

#[derive(Serialize)]
pub struct VendoRow {
    pub id: String,
    pub name: String,
    pub last_seen: Option<i64>,
    pub leases: u64,
}

fn now() -> i64 {
    crate::db::now()
}

/// Time a lease still holds, given its last report.
fn lease_value(secs: i64, running: bool, reported_at: Option<i64>, now: i64) -> i64 {
    if running {
        (secs - (now - reported_at.unwrap_or(now)).max(0)).max(0)
    } else {
        secs
    }
}

impl HubDb {
    pub fn open(data_dir: &Path) -> Result<HubDb> {
        std::fs::create_dir_all(data_dir)?;
        let raw = read_or_create_secret(&data_dir.join("hub-db.key"))?;
        let mut k = Zeroizing::new([0u8; 32]);
        k.copy_from_slice(&raw);
        let conn = Connection::open(data_dir.join("hub.db"))?;
        apply_key(&conn, &DbKey(k))?;
        let v: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if v < 1 {
            conn.execute_batch(HUB_SCHEMA)?;
            conn.pragma_update(None, "user_version", 1)?;
        }
        conn.execute_batch("CREATE TABLE IF NOT EXISTS secrets (name TEXT PRIMARY KEY, value BLOB NOT NULL);")?;
        let code_key: Option<Vec<u8>> =
            conn.query_row("SELECT value FROM secrets WHERE name = 'code_key'", [], |r| r.get(0)).optional()?;
        let code_key = match code_key {
            Some(k) => k,
            None => {
                let k: [u8; 32] = rand::random();
                conn.execute("INSERT INTO secrets (name, value) VALUES ('code_key', ?1)", [k.to_vec()])?;
                k.to_vec()
            }
        };
        let chain: Option<Vec<u8>> =
            conn.query_row("SELECT value FROM secrets WHERE name = 'chain_id'", [], |r| r.get(0)).optional()?;
        let chain = match chain {
            Some(c) => c,
            None => {
                let c: [u8; 8] = rand::random();
                conn.execute("INSERT INTO secrets (name, value) VALUES ('chain_id', ?1)", [c.to_vec()])?;
                c.to_vec()
            }
        };
        Ok(HubDb { conn: Mutex::new(conn), code_key: Zeroizing::new(code_key), chain: hex::encode(chain) })
    }

    /// Identifies this hub's passes (not secret).
    #[cfg(test)]
    pub fn chain(&self) -> &str {
        &self.chain
    }

    fn code_hash(&self, code: &str) -> Vec<u8> {
        let mut m = Hmac::<Sha256>::new_from_slice(&self.code_key).expect("hmac key");
        m.update(normalize_code(code).as_bytes());
        m.finalize().into_bytes().to_vec()
    }

    fn event(c: &Connection, vendo: Option<&str>, pass: Option<&str>, action: &str, secs: i64) {
        let _ = c.execute(
            "INSERT INTO events (ts, vendo, pass, action, secs) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![now(), vendo, pass, action, secs],
        );
    }

    // -- vendos

    pub fn add_enroll_code(&self, name: &str) -> Result<String> {
        let code = random_code(3);
        let c = self.conn.lock().unwrap();
        c.execute(
            "INSERT INTO enroll_codes (code_hash, name, expires_at) VALUES (?1, ?2, ?3)",
            params![self.code_hash(&code), name, now() + 24 * 3600],
        )?;
        Ok(code)
    }

    pub fn enroll(&self, code: &str, pubkey: &[u8; 32]) -> Result<Option<EnrollResp>> {
        let mut c = self.conn.lock().unwrap();
        let tx = c.transaction()?;
        // Single-use: the code row is deleted in the same transaction.
        let name: Option<String> = tx
            .query_row(
                "DELETE FROM enroll_codes WHERE code_hash = ?1 AND expires_at > ?2 RETURNING name",
                params![self.code_hash(code), now()],
                |r| r.get(0),
            )
            .optional()?;
        let Some(name) = name else { return Ok(None) };
        let id = format!("v_{}", hex::encode(rand::random::<[u8; 8]>()));
        tx.execute(
            "INSERT INTO vendos (id, name, pubkey, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![id, name, pubkey.to_vec(), now()],
        )?;
        Self::event(&tx, Some(&id), None, "enroll", 0);
        tx.commit()?;
        Ok(Some(EnrollResp { vendo_id: id, name, chain: self.chain.clone() }))
    }

    pub fn vendo_key(&self, id: &str) -> Result<Option<[u8; 32]>> {
        let c = self.conn.lock().unwrap();
        let k: Option<Vec<u8>> = c.query_row("SELECT pubkey FROM vendos WHERE id = ?1", [id], |r| r.get(0)).optional()?;
        Ok(k.and_then(|k| k.try_into().ok()))
    }

    pub fn vendos(&self) -> Result<Vec<VendoRow>> {
        let c = self.conn.lock().unwrap();
        let mut st = c.prepare(
            "SELECT v.id, v.name, v.last_seen, (SELECT COUNT(*) FROM passes p WHERE p.lease_vendo = v.id)
             FROM vendos v ORDER BY v.created_at",
        )?;
        let rows = st.query_map([], |r| {
            Ok(VendoRow { id: r.get(0)?, name: r.get(1)?, last_seen: r.get(2)?, leases: r.get::<_, i64>(3)? as u64 })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn remove_vendo(&self, id: &str) -> Result<bool> {
        let mut c = self.conn.lock().unwrap();
        let tx = c.transaction()?;
        // Hand its leases back to the passes (as of the last report).
        let t = now();
        tx.execute(
            "UPDATE passes SET balance_secs = balance_secs + CASE WHEN lease_running = 1
                 THEN MAX(0, lease_secs - MAX(0, ?2 - COALESCE(lease_reported_at, ?2))) ELSE lease_secs END,
               lease_vendo = NULL, lease_secs = 0, lease_running = 0, lease_reported_at = NULL
             WHERE lease_vendo = ?1",
            params![id, t],
        )?;
        let n = tx.execute("DELETE FROM vendos WHERE id = ?1", [id])?;
        tx.commit()?;
        Ok(n > 0)
    }

    // -- passes

    /// A new pass, leased (empty) to the vendo where the customer just paid.
    pub fn create_pass(&self, vendo: &str) -> Result<PassResp> {
        let c = self.conn.lock().unwrap();
        loop {
            let code = random_code(4);
            let id = format!("p_{}", hex::encode(rand::random::<[u8; 8]>()));
            let t = now();
            let n = c.execute(
                "INSERT OR IGNORE INTO passes (id, code_hash, created_at, last_used, lease_vendo, lease_running, lease_reported_at)
                 VALUES (?1, ?2, ?3, ?3, ?4, 1, ?3)",
                params![id, self.code_hash(&code), t, vendo],
            )?;
            if n == 1 {
                Self::event(&c, Some(vendo), Some(&id), "create", 0);
                return Ok(PassResp { pass_id: id, code: format!("PASS-{code}") });
            }
        }
    }

    /// Moves all of a pass's time to `vendo`, taking it back from wherever
    /// it was leased. Returns None for an unknown code.
    pub fn claim(&self, vendo: &str, code: &str) -> Result<Option<ClaimResp>> {
        let mut c = self.conn.lock().unwrap();
        let tx = c.transaction()?;
        let t = now();
        let row: Option<(String, i64, Option<String>, i64, bool, Option<i64>)> = tx
            .query_row(
                "SELECT id, balance_secs, lease_vendo, lease_secs, lease_running, lease_reported_at FROM passes WHERE code_hash = ?1",
                [self.code_hash(code)],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
            )
            .optional()?;
        let Some((id, balance, lease_vendo, lease_secs, running, reported)) = row else { return Ok(None) };
        let refund = if lease_vendo.is_some() { lease_value(lease_secs, running, reported, t) } else { 0 };
        let total = balance + refund;
        // Time that was counting down keeps counting; paused time (and the
        // balance) stays paused until the customer resumes it.
        let running = lease_vendo.is_some() && running && refund > 0;
        tx.execute(
            "UPDATE passes SET balance_secs = 0, lease_vendo = ?2, lease_secs = ?3, lease_running = ?5,
               lease_reported_at = ?4, last_used = ?4 WHERE id = ?1",
            params![id, vendo, total, t, running],
        )?;
        if let Some(from) = lease_vendo.as_deref().filter(|f| *f != vendo) {
            Self::event(&tx, Some(from), Some(&id), "moved_away", refund);
        }
        Self::event(&tx, Some(vendo), Some(&id), "claim", total);
        tx.commit()?;
        Ok(Some(ClaimResp { pass_id: id, secs: total as u64, running }))
    }

    /// The pass a code belongs to (for a wallet pass), noted in the event log.
    pub fn wallet_pass(&self, vendo: &str, code: &str, kind: WalletKind) -> Result<Option<String>> {
        let c = self.conn.lock().unwrap();
        let id: Option<String> =
            c.query_row("SELECT id FROM passes WHERE code_hash = ?1", [self.code_hash(code)], |r| r.get(0)).optional()?;
        if let Some(id) = &id {
            let action = match kind {
                WalletKind::Google => "wallet_google",
                WalletKind::Apple => "wallet_apple",
            };
            Self::event(&c, Some(vendo), Some(id), action, 0);
        }
        Ok(id)
    }

    pub fn sync(&self, vendo: &str, req: &SyncReq) -> Result<SyncResp> {
        if req.leases.len() > MAX_SYNC_LEASES || req.credits.len() > MAX_SYNC_CREDITS {
            bail!("sync too large");
        }
        let mut c = self.conn.lock().unwrap();
        let tx = c.transaction()?;
        let t = now();
        let mut out = SyncResp { chain: self.chain.clone(), ..Default::default() };
        // Credits first: time sold here for a pass is what lets its lease grow.
        for cr in &req.credits {
            let secs = cr.secs.min(crate::engine::MAX_GRANT_SECS) as i64;
            let new = tx.execute(
                "INSERT OR IGNORE INTO credits (vendo_id, credit_id, pass_id, pesos, secs, ts) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![vendo, cr.id, cr.pass_id, cr.pesos, secs, cr.ts],
            )?;
            if new == 1 {
                tx.execute(
                    "UPDATE passes SET lease_secs = MIN(?3, lease_secs + ?4) WHERE id = ?1 AND lease_vendo = ?2",
                    params![cr.pass_id, vendo, crate::engine::MAX_GRANT_SECS as i64, secs],
                )?;
            }
            out.acked.push(cr.id);
        }
        let mut reported = std::collections::HashSet::new();
        for l in &req.leases {
            let held: Option<i64> = tx
                .query_row("SELECT lease_secs FROM passes WHERE id = ?1 AND lease_vendo = ?2", params![l.pass_id, vendo], |r| r.get(0))
                .optional()?;
            let Some(held) = held else {
                out.revoked.push(l.pass_id.clone());
                continue;
            };
            // Never more than the lease held at its last report (or grant)
            // plus credits since: an honest vendo's time only grows by sales.
            let remaining = l.remaining_secs.min(i64::MAX as u64) as i64;
            if remaining > held {
                tracing::warn!(vendo, pass = %l.pass_id, reported = remaining, held, "lease report above what was granted; capped");
                Self::event(&tx, Some(vendo), Some(&l.pass_id), "capped", remaining - held);
            }
            tx.execute(
                "UPDATE passes SET lease_secs = ?3, lease_running = ?4, lease_reported_at = ?5, last_used = ?5
                 WHERE id = ?1 AND lease_vendo = ?2",
                params![l.pass_id, vendo, remaining.min(held), l.running, t],
            )?;
            reported.insert(l.pass_id.clone());
        }
        // Leases the vendo didn't report: it dropped them (their time ran out
        // there, or the grant never reached it), so whatever they still hold
        // goes back to the pass. Fresh grants may postdate this report.
        let mut st = tx.prepare("SELECT id, lease_secs, lease_running, lease_reported_at FROM passes WHERE lease_vendo = ?1")?;
        let mine: Vec<(String, i64, bool, Option<i64>)> =
            st.query_map([vendo], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<rusqlite::Result<_>>()?;
        drop(st);
        for (id, secs, running, at) in mine.iter().filter(|(id, ..)| !reported.contains(id)) {
            if at.is_some_and(|at| t - at < LEASE_GRACE_SECS) {
                continue;
            }
            let back = lease_value(*secs, *running, *at, t);
            tx.execute(
                "UPDATE passes SET balance_secs = balance_secs + ?2, lease_vendo = NULL, lease_secs = 0, lease_running = 0,
                   lease_reported_at = NULL WHERE id = ?1",
                params![id, back],
            )?;
            if back > 0 {
                Self::event(&tx, Some(vendo), Some(id), "returned", back);
            }
        }
        tx.execute("UPDATE vendos SET last_seen = ?2 WHERE id = ?1", params![vendo, t])?;
        tx.commit()?;
        Ok(out)
    }

    #[cfg(test)]
    pub fn lease_for_tests(&self, pass_id: &str) -> (i64, Option<String>, i64) {
        self.conn
            .lock()
            .unwrap()
            .query_row("SELECT balance_secs, lease_vendo, lease_secs FROM passes WHERE id = ?1", [pass_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
    }

    pub fn summary(&self) -> Result<(u64, u64, u64)> {
        let c = self.conn.lock().unwrap();
        let passes: i64 = c.query_row("SELECT COUNT(*) FROM passes", [], |r| r.get(0))?;
        let held: i64 = c.query_row("SELECT COALESCE(SUM(balance_secs + lease_secs), 0) FROM passes", [], |r| r.get(0))?;
        let since = now() - 86400;
        let pesos: i64 = c.query_row("SELECT COALESCE(SUM(pesos), 0) FROM credits WHERE ts >= ?1", [since], |r| r.get(0))?;
        Ok((passes as u64, held as u64, pesos as u64))
    }
}

// ---- HTTP server --------------------------------------------------------------

pub struct HubState {
    pub db: HubDb,
    pub wallet: Option<Wallet>,
    nonces: Mutex<HashMap<String, i64>>,
    enroll_fails: Mutex<HashMap<IpAddr, (u32, i64)>>,
}

impl HubState {
    pub fn new(db: HubDb, wallet: Option<Wallet>) -> HubState {
        HubState { db, wallet, nonces: Mutex::new(HashMap::new()), enroll_fails: Mutex::new(HashMap::new()) }
    }
    fn wallet_support(&self) -> WalletSupport {
        self.wallet.as_ref().map(Wallet::support).unwrap_or_default()
    }
}

type Shared = Arc<HubState>;

pub fn router(st: Shared) -> Router {
    Router::new()
        .route("/v1/health", get(|| async { "ok" }))
        .route("/v1/enroll", post(enroll))
        .route("/v1/passes", post(create_pass))
        .route("/v1/claim", post(claim))
        .route("/v1/sync", post(sync))
        .route("/v1/wallet", post(wallet_issue))
        .route("/v1/wallet/apple/{file}", get(wallet_apple).head(wallet_apple_head))
        .route("/v1/wallet/google/{token}", get(wallet_google).head(wallet_google_head))
        .route("/v1/wallet/logo.png", get(wallet_logo))
        .layer(axum::middleware::from_fn(crate::server::security_headers))
        .layer(DefaultBodyLimit::max(SMALL_BODY))
        .with_state(st)
}

fn hub_err(code: StatusCode, msg: &str) -> Response {
    ApiError::new(code, msg).into_response()
}

fn unknown_pass() -> Response {
    ApiError::new(StatusCode::NOT_FOUND, "unknown pass").with_code(UNKNOWN_PASS).into_response()
}

/// Reads a request body, refusing anything over `limit` bytes.
async fn read_body(body: Body, limit: usize) -> Result<Bytes, Response> {
    axum::body::to_bytes(body, limit).await.map_err(|_| hub_err(StatusCode::PAYLOAD_TOO_LARGE, "request too large"))
}

/// Checks a vendo's request: headers first (known vendo, fresh timestamp),
/// so a stranger's body is never read, then the signature over the body
/// (at most `limit` bytes). Returns the vendo's id and the body.
async fn authenticate(st: &HubState, path: &str, headers: &HeaderMap, body: Body, limit: usize) -> Result<(String, Bytes), Response> {
    let h = |k: &str| headers.get(k).and_then(|v| v.to_str().ok()).map(str::to_string);
    let (Some(vendo), Some(ts), Some(nonce), Some(sig)) = (h("x-vendo-id"), h("x-ts"), h("x-nonce"), h("x-sig")) else {
        return Err(hub_err(StatusCode::UNAUTHORIZED, "unsigned request"));
    };
    let ts: i64 = ts.parse().map_err(|_| hub_err(StatusCode::UNAUTHORIZED, "bad timestamp"))?;
    if (now() - ts).abs() > MAX_SKEW_SECS {
        return Err(hub_err(StatusCode::UNAUTHORIZED, "clock skew too large; check NTP on the vendo"));
    }
    if nonce.len() < 16 || nonce.len() > 64 {
        return Err(hub_err(StatusCode::UNAUTHORIZED, "bad nonce"));
    }
    if vendo.len() > 64 {
        return Err(hub_err(StatusCode::UNAUTHORIZED, "unknown vendo"));
    }
    let key = st.db.vendo_key(&vendo).ok().flatten().ok_or_else(|| hub_err(StatusCode::UNAUTHORIZED, "unknown vendo"))?;
    let sig: [u8; 64] = hex::decode(&sig).ok().and_then(|s| s.try_into().ok()).ok_or_else(|| hub_err(StatusCode::UNAUTHORIZED, "bad signature"))?;
    let vk = VerifyingKey::from_bytes(&key).map_err(|_| hub_err(StatusCode::UNAUTHORIZED, "bad key"))?;
    let body = read_body(body, limit).await?;
    vk.verify(&signing_message("POST", path, &vendo, ts, &nonce, &body), &Signature::from_bytes(&sig))
        .map_err(|_| hub_err(StatusCode::UNAUTHORIZED, "bad signature"))?;
    // Replay protection: each nonce once within the skew window.
    let mut seen = st.nonces.lock().unwrap();
    let t = now();
    seen.retain(|_, at| t - *at < 2 * MAX_SKEW_SECS);
    if seen.insert(format!("{vendo}:{nonce}"), t).is_some() {
        return Err(hub_err(StatusCode::UNAUTHORIZED, "replayed request"));
    }
    Ok((vendo, body))
}

/// Where an enrolment attempt came from. Behind a reverse proxy on this
/// machine (Caddy) every connection is from 127.0.0.1, so the address the
/// proxy put last in X-Forwarded-For is used; anyone else could forge that
/// header, so it is ignored for direct connections.
fn client_ip(peer: SocketAddr, headers: &HeaderMap) -> IpAddr {
    if !peer.ip().is_loopback() {
        return peer.ip();
    }
    headers
        .get_all("x-forwarded-for")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .last()
        .and_then(|ip| ip.trim().parse().ok())
        .unwrap_or(peer.ip())
}

/// Addresses tracked for enrolment lockouts, at most (memory guard).
const MAX_ENROLL_TRACKED: usize = 10_000;

fn parse<T: for<'a> Deserialize<'a>>(body: &[u8]) -> Result<T, Response> {
    serde_json::from_slice(body).map_err(|_| hub_err(StatusCode::BAD_REQUEST, "bad request body"))
}

fn internal(e: anyhow::Error) -> Response {
    tracing::error!("hub: {e:#}");
    hub_err(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
}

async fn enroll(State(st): State<Shared>, ConnectInfo(addr): ConnectInfo<SocketAddr>, headers: HeaderMap, body: Body) -> Response {
    let ip = client_ip(addr, &headers);
    {
        let mut f = st.enroll_fails.lock().unwrap();
        let t = now();
        f.retain(|_, (_, since)| t - *since <= 3600);
        match f.get(&ip) {
            Some((n, _)) if *n >= 10 => return hub_err(StatusCode::TOO_MANY_REQUESTS, "too many attempts"),
            None if f.len() >= MAX_ENROLL_TRACKED => return hub_err(StatusCode::TOO_MANY_REQUESTS, "too many attempts"),
            _ => {}
        }
    }
    let body = match read_body(body, SMALL_BODY).await {
        Ok(b) => b,
        Err(e) => return e,
    };
    let req: EnrollReq = match parse(&body) {
        Ok(r) => r,
        Err(e) => return e,
    };
    let Some(pk) = hex::decode(&req.pubkey).ok().and_then(|k| <[u8; 32]>::try_from(k).ok()) else {
        return hub_err(StatusCode::BAD_REQUEST, "bad public key");
    };
    if VerifyingKey::from_bytes(&pk).is_err() {
        return hub_err(StatusCode::BAD_REQUEST, "bad public key");
    }
    match st.db.enroll(&req.code, &pk) {
        Ok(Some(resp)) => {
            tracing::info!(vendo = %resp.vendo_id, name = %resp.name, "vendo enrolled");
            Json(resp).into_response()
        }
        Ok(None) => {
            st.enroll_fails.lock().unwrap().entry(ip).or_insert((0, now())).0 += 1;
            hub_err(StatusCode::FORBIDDEN, "enrollment code is wrong, used or expired")
        }
        Err(e) => internal(e),
    }
}

async fn create_pass(State(st): State<Shared>, headers: HeaderMap, body: Body) -> Response {
    let vendo = match authenticate(&st, "/v1/passes", &headers, body, SMALL_BODY).await {
        Ok((v, _)) => v,
        Err(e) => return e,
    };
    match st.db.create_pass(&vendo) {
        Ok(p) => Json(p).into_response(),
        Err(e) => internal(e),
    }
}

async fn claim(State(st): State<Shared>, headers: HeaderMap, body: Body) -> Response {
    let (vendo, body) = match authenticate(&st, "/v1/claim", &headers, body, SMALL_BODY).await {
        Ok(v) => v,
        Err(e) => return e,
    };
    let req: ClaimReq = match parse(&body) {
        Ok(r) => r,
        Err(e) => return e,
    };
    if normalize_code(&req.code).len() != 16 {
        return unknown_pass();
    }
    match st.db.claim(&vendo, &req.code) {
        Ok(Some(r)) => Json(r).into_response(),
        Ok(None) => unknown_pass(),
        Err(e) => internal(e),
    }
}

async fn sync(State(st): State<Shared>, headers: HeaderMap, body: Body) -> Response {
    let (vendo, body) = match authenticate(&st, "/v1/sync", &headers, body, SYNC_BODY).await {
        Ok(v) => v,
        Err(e) => return e,
    };
    let req: SyncReq = match parse(&body) {
        Ok(r) => r,
        Err(e) => return e,
    };
    if req.leases.len() > MAX_SYNC_LEASES || req.credits.len() > MAX_SYNC_CREDITS {
        tracing::warn!(vendo, leases = req.leases.len(), credits = req.credits.len(), "sync too large; refused");
        return ApiError::new(StatusCode::PAYLOAD_TOO_LARGE, "too many leases or credits in one sync")
            .with_code("sync_too_large")
            .into_response();
    }
    match st.db.sync(&vendo, &req) {
        Ok(mut r) => {
            r.wallet = st.wallet_support();
            Json(r).into_response()
        }
        Err(e) => internal(e),
    }
}

// ---- wallet passes ------------------------------------------------------------

/// A vendo asks for a wallet pass on behalf of a customer: returns a
/// single-use download path the customer's phone opens directly.
async fn wallet_issue(State(st): State<Shared>, headers: HeaderMap, body: Body) -> Response {
    let (vendo, body) = match authenticate(&st, "/v1/wallet", &headers, body, SMALL_BODY).await {
        Ok(v) => v,
        Err(e) => return e,
    };
    let req: WalletReq = match parse(&body) {
        Ok(r) => r,
        Err(e) => return e,
    };
    let Some(wallet) = st.wallet.as_ref().filter(|w| w.support().has(req.kind)) else {
        return hub_err(StatusCode::NOT_IMPLEMENTED, "this wallet isn't set up on the hub");
    };
    if normalize_code(&req.code).len() != 16 {
        return unknown_pass();
    }
    let pass_id = match st.db.wallet_pass(&vendo, &req.code, req.kind) {
        Ok(Some(id)) => id,
        Ok(None) => return unknown_pass(),
        Err(e) => return internal(e),
    };
    match wallet.issue(req.kind, &pass_id, &req.code, now()) {
        Ok(token) => {
            let path = match req.kind {
                WalletKind::Apple => format!("/v1/wallet/apple/{token}.pkpass"),
                WalletKind::Google => format!("/v1/wallet/google/{token}"),
            };
            Json(WalletResp { path, expires_in: crate::wallet::TOKEN_TTL_SECS }).into_response()
        }
        Err(e) => internal(e),
    }
}

fn wallet_gone() -> Response {
    (
        StatusCode::GONE,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        "This link has expired or was already used. Go back to the WiFi page and tap Add to Wallet again.",
    )
        .into_response()
}

async fn wallet_apple(State(st): State<Shared>, AxPath(file): AxPath<String>) -> Response {
    let pass = file.strip_suffix(".pkpass").and_then(|t| st.wallet.as_ref()?.take(WalletKind::Apple, t, now()));
    match pass {
        Some(bytes) => (
            [
                (header::CONTENT_TYPE, "application/vnd.apple.pkpass"),
                (header::CONTENT_DISPOSITION, "attachment; filename=\"piso-pass.pkpass\""),
                (header::CACHE_CONTROL, "no-store"),
            ],
            bytes.to_vec(),
        )
            .into_response(),
        None => wallet_gone(),
    }
}

async fn wallet_apple_head(State(st): State<Shared>, AxPath(file): AxPath<String>) -> Response {
    match file.strip_suffix(".pkpass").filter(|t| st.wallet.as_ref().is_some_and(|w| w.is_waiting(WalletKind::Apple, t, now()))) {
        Some(_) => ([(header::CONTENT_TYPE, "application/vnd.apple.pkpass"), (header::CACHE_CONTROL, "no-store")]).into_response(),
        None => wallet_gone(),
    }
}

async fn wallet_google(State(st): State<Shared>, AxPath(token): AxPath<String>) -> Response {
    let url = st.wallet.as_ref().and_then(|w| w.take(WalletKind::Google, &token, now()));
    match url.and_then(|u| String::from_utf8(u.to_vec()).ok()) {
        Some(url) => Redirect::to(&url).into_response(),
        None => wallet_gone(),
    }
}

async fn wallet_google_head(State(st): State<Shared>, AxPath(token): AxPath<String>) -> Response {
    if st.wallet.as_ref().is_some_and(|w| w.is_waiting(WalletKind::Google, &token, now())) {
        StatusCode::OK.into_response()
    } else {
        wallet_gone()
    }
}

async fn wallet_logo() -> Response {
    ([(header::CONTENT_TYPE, "image/png"), (header::CACHE_CONTROL, "public, max-age=86400")], crate::wallet::GOOGLE_LOGO).into_response()
}

pub async fn run(cfg: HubConfig, dev: bool) -> Result<()> {
    let db = HubDb::open(&cfg.data_dir)?;
    let (passes, held, today) = db.summary()?;
    tracing::info!("hub: {passes} passes, {} h of time held, ₱{today} in the last 24 h", held / 3600);
    let wallet = match cfg.wallet {
        Some(w) => Some(Wallet::load(w, now())?),
        None => None,
    };
    if let Some(s) = wallet.as_ref().map(Wallet::support) {
        let on = |b: bool| if b { "on" } else { "off" };
        tracing::info!("wallet passes: Google Wallet {}, Apple Wallet {}", on(s.google), on(s.apple));
    }
    let state = Arc::new(HubState::new(db, wallet));
    let tls = match (&cfg.tls_cert, &cfg.tls_key) {
        (Some(c), Some(k)) => Some(crate::tls::from_files(c, k)?),
        (None, None) => None,
        _ => bail!("set both tls_cert and tls_key, or neither"),
    };
    let listener = tokio::net::TcpListener::bind(cfg.listen).await.with_context(|| format!("binding {}", cfg.listen))?;
    tracing::info!("hub listening on {}://{}", if tls.is_some() { "https" } else { "http" }, cfg.listen);
    let (_stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    // Behind Caddy every connection comes from 127.0.0.1, so no per-address cap.
    let limits = crate::server::Limits { max_conns: 1024, per_ip: 1024, request_timeout: std::time::Duration::from_secs(30) };
    let srv = tokio::spawn(crate::server::serve(listener, router(state), tls, limits, None, stop_rx));
    // An update is only kept once the new version gets this far.
    if let Err(e) = crate::update::report_healthy(&cfg.data_dir, false) {
        tracing::warn!("writing health report: {e:#}");
    }
    match crate::update::hub_url(cfg.update_url.as_deref(), dev) {
        Ok(url) => crate::update::spawn_hub_checks(crate::update::Profile::hub(&cfg.data_dir, cfg.listen, dev), url, dev),
        Err(e) => tracing::warn!("update_url in hub.toml: {e}"),
    }
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {},
        _ = srv => {},
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use axum::body::to_bytes;
    use tower::ServiceExt;

    fn hub() -> (PathBuf, HubDb) {
        let dir = std::env::temp_dir().join(format!("pisowifi-hub-{}", rand::random::<u64>()));
        let db = HubDb::open(&dir).unwrap();
        (dir, db)
    }

    /// A real hub on 127.0.0.1 for vendo-side tests. Stops when dropped.
    pub(crate) struct TestHub {
        pub url: String,
        pub st: Arc<HubState>,
        dir: PathBuf,
        _stop: tokio::sync::watch::Sender<bool>,
    }

    impl Drop for TestHub {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    pub(crate) async fn serve_test_hub() -> TestHub {
        let (dir, db) = hub();
        let st = Arc::new(HubState::new(db, None));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (tx, rx) = tokio::sync::watch::channel(false);
        let limits = crate::server::Limits { max_conns: 64, per_ip: 64, request_timeout: std::time::Duration::from_secs(30) };
        tokio::spawn(crate::server::serve(listener, router(st.clone()), None, limits, None, rx));
        TestHub { url, st, dir, _stop: tx }
    }

    /// Moves a pass's last report (or grant) into the past.
    fn age_lease(db: &HubDb, pass_id: &str, secs: i64) {
        db.conn
            .lock()
            .unwrap()
            .execute("UPDATE passes SET lease_reported_at = lease_reported_at - ?2 WHERE id = ?1", params![pass_id, secs])
            .unwrap();
    }

    fn lease_of(db: &HubDb, pass_id: &str) -> (i64, Option<String>, i64) {
        db.lease_for_tests(pass_id)
    }

    fn credit(id: i64, pass_id: &str, pesos: u32, secs: u64) -> CreditReport {
        CreditReport { id, pass_id: pass_id.into(), pesos, secs, ts: now() }
    }

    fn signed_request(key: &ed25519_dalek::SigningKey, vendo: &str, path: &str, body: Body, signed_body: &[u8]) -> http::Request<Body> {
        use ed25519_dalek::Signer;
        let (ts, nonce) = (now(), hex::encode(rand::random::<[u8; 16]>()));
        let sig = key.sign(&signing_message("POST", path, vendo, ts, &nonce, signed_body));
        http::Request::post(path)
            .header("x-vendo-id", vendo)
            .header("x-ts", ts.to_string())
            .header("x-nonce", nonce)
            .header("x-sig", hex::encode(sig.to_bytes()))
            .body(body)
            .unwrap()
    }

    fn signed(key: &ed25519_dalek::SigningKey, vendo: &str, path: &str, body: &serde_json::Value) -> http::Request<Body> {
        let bytes = serde_json::to_vec(body).unwrap();
        signed_request(key, vendo, path, Body::from(bytes.clone()), &bytes)
    }

    fn vendo(db: &HubDb, name: &str) -> String {
        let code = db.add_enroll_code(name).unwrap();
        let key = ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng);
        let r = db.enroll(&code, key.verifying_key().as_bytes()).unwrap().unwrap();
        assert!(db.enroll(&code, key.verifying_key().as_bytes()).unwrap().is_none(), "codes are single-use");
        r.vendo_id
    }

    #[test]
    fn time_follows_the_customer_between_locations() {
        let (dir, db) = hub();
        let a = vendo(&db, "Branch A");
        let b = vendo(&db, "Branch B");
        // Customer pays at A: pass created, A reports the sale and 2 h paused.
        let p = db.create_pass(&a).unwrap();
        let lease = |remaining, running| LeaseReport { pass_id: p.pass_id.clone(), remaining_secs: remaining, running };
        db.sync(&a, &SyncReq { leases: vec![lease(7200, false)], credits: vec![credit(1, &p.pass_id, 20, 7200)] }).unwrap();
        // They show up at B with their pass.
        let got = db.claim(&b, &p.code.to_lowercase()).unwrap().unwrap();
        assert_eq!(got.secs, 7200);
        // A learns the pass moved away on its next report.
        let r = db.sync(&a, &SyncReq { leases: vec![lease(7200, false)], credits: vec![] }).unwrap();
        assert_eq!(r.revoked, vec![p.pass_id.clone()]);
        // B reports 1 h left; claiming again at A returns that hour.
        db.sync(&b, &SyncReq { leases: vec![lease(3600, false)], credits: vec![] }).unwrap();
        assert_eq!(db.claim(&a, &p.code).unwrap().unwrap().secs, 3600);
        assert!(db.claim(&a, "PASS-AAAA-BBBB-CCCC-DDDD").unwrap().is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_sync_never_takes_back_a_lease_granted_after_its_snapshot() {
        let (dir, db) = hub();
        let a = vendo(&db, "Branch A");
        let b = vendo(&db, "Branch B");
        let p = db.create_pass(&b).unwrap();
        db.sync(&b, &SyncReq { leases: vec![], credits: vec![credit(1, &p.pass_id, 20, 7200)] }).unwrap();
        db.sync(&b, &SyncReq { leases: vec![LeaseReport { pass_id: p.pass_id.clone(), remaining_secs: 7200, running: false }], credits: vec![] }).unwrap();
        // A takes its snapshot (no passes yet); the customer claims at A; then
        // A's sync arrives with the old snapshot.
        let before_claim = SyncReq::default();
        assert_eq!(db.claim(&a, &p.code).unwrap().unwrap().secs, 7200);
        let r = db.sync(&a, &before_claim).unwrap();
        assert!(r.revoked.is_empty());
        assert_eq!(lease_of(&db, &p.pass_id), (0, Some(a.clone()), 7200), "the fresh lease is untouched");
        // A lease A really dropped (or whose grant never reached it) goes back
        // to the pass, not to zero.
        age_lease(&db, &p.pass_id, LEASE_GRACE_SECS + 1);
        db.sync(&a, &SyncReq::default()).unwrap();
        assert_eq!(lease_of(&db, &p.pass_id), (7200, None, 0));
        assert_eq!(db.claim(&b, &p.code).unwrap().unwrap().secs, 7200, "nothing lost");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn reports_cant_raise_a_lease_beyond_what_was_sold() {
        let (dir, db) = hub();
        let a = vendo(&db, "Branch A");
        let p = db.create_pass(&a).unwrap();
        let report = |secs| SyncReq { leases: vec![LeaseReport { pass_id: p.pass_id.clone(), remaining_secs: secs, running: true }], credits: vec![] };
        // A stolen vendo key reporting time nobody paid for gets nothing.
        db.sync(&a, &report(86_400)).unwrap();
        assert_eq!(lease_of(&db, &p.pass_id).2, 0);
        // A ₱5 sale (30 min) lets the lease grow by exactly that.
        db.sync(&a, &SyncReq { leases: report(1800).leases, credits: vec![credit(7, &p.pass_id, 5, 1800)] }).unwrap();
        assert_eq!(lease_of(&db, &p.pass_id).2, 1800);
        db.sync(&a, &report(1700)).unwrap();
        assert_eq!(lease_of(&db, &p.pass_id).2, 1700, "time used is gone");
        db.sync(&a, &report(1800)).unwrap();
        assert_eq!(lease_of(&db, &p.pass_id).2, 1700, "and can't be reported back");
        // Re-sending an already stored credit doesn't count twice.
        db.sync(&a, &SyncReq { leases: report(3500).leases, credits: vec![credit(7, &p.pass_id, 5, 1800)] }).unwrap();
        assert_eq!(lease_of(&db, &p.pass_id).2, 1700);
        let capped: i64 = db.conn.lock().unwrap().query_row("SELECT COUNT(*) FROM events WHERE action = 'capped'", [], |r| r.get(0)).unwrap();
        assert_eq!(capped, 3, "each attempt is in the event log");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn claims_say_whether_the_time_was_running() {
        let (dir, db) = hub();
        let a = vendo(&db, "Branch A");
        let b = vendo(&db, "Branch B");
        let p = db.create_pass(&a).unwrap();
        let sync = |v: &str, running, credits| {
            let l = LeaseReport { pass_id: p.pass_id.clone(), remaining_secs: 3600, running };
            db.sync(v, &SyncReq { leases: vec![l], credits }).unwrap();
        };
        // Paused at A: claimed at B, it stays paused.
        sync(&a, false, vec![credit(1, &p.pass_id, 10, 3600)]);
        assert!(!db.claim(&b, &p.code).unwrap().unwrap().running);
        // Running at B: it keeps running at A.
        sync(&b, true, vec![]);
        assert!(db.claim(&a, &p.code).unwrap().unwrap().running);
        // Time sitting in the balance isn't running anywhere.
        age_lease(&db, &p.pass_id, LEASE_GRACE_SECS + 1);
        db.sync(&a, &SyncReq::default()).unwrap();
        let c = db.claim(&b, &p.code).unwrap().unwrap();
        assert!(c.secs > 0 && !c.running);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_chain_id_is_random_per_hub_and_stays() {
        let (dir, db) = hub();
        let chain = db.chain().to_string();
        assert_eq!(chain.len(), 16);
        let code = db.add_enroll_code("A").unwrap();
        let key = ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng);
        let r = db.enroll(&code, key.verifying_key().as_bytes()).unwrap().unwrap();
        assert_eq!(r.chain, chain);
        assert_eq!(db.sync(&r.vendo_id, &SyncReq::default()).unwrap().chain, chain);
        drop(db);
        assert_eq!(HubDb::open(&dir).unwrap().chain(), chain, "survives a restart");
        let (dir2, other) = hub();
        assert_ne!(other.chain(), chain);
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(dir2);
    }

    #[tokio::test]
    async fn requests_are_checked_before_their_bodies_are_read() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let (dir, db) = hub();
        let key = ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng);
        let vendo = db.enroll(&db.add_enroll_code("A").unwrap(), key.verifying_key().as_bytes()).unwrap().unwrap().vendo_id;
        let pass = db.create_pass(&vendo).unwrap();
        let app = router(Arc::new(HubState::new(db, None)));

        // A body that notices being read.
        let read = Arc::new(AtomicBool::new(false));
        let watched = |read: Arc<AtomicBool>| Body::from_stream(Watched(read, false));
        let stranger = ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng);
        let req = signed_request(&stranger, "v_nobody", "/v1/sync", watched(read.clone()), b"");
        assert_eq!(app.clone().oneshot(req).await.unwrap().status(), StatusCode::UNAUTHORIZED);
        assert!(!read.load(Ordering::SeqCst), "an unknown vendo's body is never read");
        let mut stale = signed_request(&key, &vendo, "/v1/claim", watched(read.clone()), b"");
        stale.headers_mut().insert("x-ts", (now() - 3600).to_string().parse().unwrap());
        assert_eq!(app.clone().oneshot(stale).await.unwrap().status(), StatusCode::UNAUTHORIZED);
        assert!(!read.load(Ordering::SeqCst), "nor a stale request's");

        // A known vendo's body is read, up to 4 KB on small routes.
        let req = signed_request(&key, &vendo, "/v1/claim", watched(read.clone()), b"");
        assert_eq!(app.clone().oneshot(req).await.unwrap().status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert!(read.load(Ordering::SeqCst));
        let big = serde_json::json!({ "code": "x".repeat(5000) });
        assert_eq!(app.clone().oneshot(signed(&key, &vendo, "/v1/claim", &big)).await.unwrap().status(), StatusCode::PAYLOAD_TOO_LARGE);

        // Unknown codes say so in a way a program can check.
        let r = app.clone().oneshot(signed(&key, &vendo, "/v1/claim", &serde_json::json!({ "code": "PASS-AAAA-BBBB-CCCC-DDDD" }))).await.unwrap();
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
        let body: serde_json::Value = serde_json::from_slice(&to_bytes(r.into_body(), 4096).await.unwrap()).unwrap();
        assert_eq!(body["code"], UNKNOWN_PASS);

        // A sync over the cap is refused whole, not cut short.
        let lease = LeaseReport { pass_id: pass.pass_id.clone(), remaining_secs: 0, running: false };
        let leases = vec![lease; MAX_SYNC_LEASES + 1];
        let r = app.clone().oneshot(signed(&key, &vendo, "/v1/sync", &serde_json::json!({ "leases": leases, "credits": [] }))).await.unwrap();
        assert_eq!(r.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let leases = vec![LeaseReport { pass_id: pass.pass_id.clone(), remaining_secs: 0, running: false }; MAX_SYNC_LEASES];
        let r = app.clone().oneshot(signed(&key, &vendo, "/v1/sync", &serde_json::json!({ "leases": leases, "credits": [] }))).await.unwrap();
        assert_eq!(r.status(), StatusCode::OK, "a full-size sync fits the body limit");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 1 MB of junk that flags when it's read.
    struct Watched(Arc<std::sync::atomic::AtomicBool>, bool);

    impl futures_core::Stream for Watched {
        type Item = Result<Bytes, std::io::Error>;
        fn poll_next(mut self: std::pin::Pin<&mut Self>, _: &mut std::task::Context<'_>) -> std::task::Poll<Option<Self::Item>> {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            let done = std::mem::replace(&mut self.1, true);
            std::task::Poll::Ready((!done).then(|| Ok(Bytes::from(vec![b'x'; 1 << 20]))))
        }
    }

    #[test]
    fn enrol_lockouts_use_the_proxys_client_address() {
        let peer = |ip: &str| SocketAddr::new(ip.parse().unwrap(), 50000);
        let mut h = HeaderMap::new();
        assert_eq!(client_ip(peer("127.0.0.1"), &h), "127.0.0.1".parse::<IpAddr>().unwrap());
        // Caddy adds the client's address last; anything before it is the client's own claim.
        h.append("x-forwarded-for", "6.6.6.6, 203.0.113.7".parse().unwrap());
        assert_eq!(client_ip(peer("127.0.0.1"), &h), "203.0.113.7".parse::<IpAddr>().unwrap());
        h.append("x-forwarded-for", "198.51.100.2".parse().unwrap());
        assert_eq!(client_ip(peer("::1"), &h), "198.51.100.2".parse::<IpAddr>().unwrap());
        // Straight from the internet: the header is the sender's to forge.
        assert_eq!(client_ip(peer("203.0.113.9"), &h), "203.0.113.9".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn running_leases_lose_elapsed_time_on_transfer() {
        assert_eq!(lease_value(600, true, Some(1000), 1100), 500);
        assert_eq!(lease_value(600, false, Some(1000), 1100), 600);
        assert_eq!(lease_value(60, true, Some(1000), 5000), 0);
    }

    #[tokio::test]
    async fn wallet_links_are_signed_for_and_work_once() {
        use axum::body::{to_bytes, Body};
        use ed25519_dalek::Signer;
        use tower::ServiceExt;

        let (dir, db) = hub();
        let key = ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng);
        let enroll = db.add_enroll_code("A").unwrap();
        let vendo = db.enroll(&enroll, key.verifying_key().as_bytes()).unwrap().unwrap().vendo_id;
        let pass = db.create_pass(&vendo).unwrap();
        let app = router(Arc::new(HubState::new(db, Some(crate::wallet::tests::test_wallet()))));

        let signed = |path: &str, body: serde_json::Value| {
            let body = serde_json::to_vec(&body).unwrap();
            let (ts, nonce) = (now(), hex::encode(rand::random::<[u8; 16]>()));
            let sig = key.sign(&signing_message("POST", path, &vendo, ts, &nonce, &body));
            http::Request::post(path)
                .header("x-vendo-id", &vendo)
                .header("x-ts", ts.to_string())
                .header("x-nonce", nonce)
                .header("x-sig", hex::encode(sig.to_bytes()))
                .body(Body::from(body))
                .unwrap()
        };
        let get = |method: &str, path: &str| http::Request::builder().method(method).uri(path).body(Body::empty()).unwrap();

        // Sync tells vendos which buttons to show.
        let r = app.clone().oneshot(signed("/v1/sync", serde_json::json!({ "leases": [], "credits": [] }))).await.unwrap();
        let sync: SyncResp = serde_json::from_slice(&to_bytes(r.into_body(), 1 << 20).await.unwrap()).unwrap();
        assert_eq!(sync.wallet, WalletSupport { google: true, apple: true });

        // Unsigned or unknown codes get nothing.
        let mut unsigned = signed("/v1/wallet", serde_json::json!({ "code": pass.code, "kind": "apple" }));
        unsigned.headers_mut().remove("x-sig");
        assert_eq!(app.clone().oneshot(unsigned).await.unwrap().status(), StatusCode::UNAUTHORIZED);
        let r = app.clone().oneshot(signed("/v1/wallet", serde_json::json!({ "code": "PASS-AAAA-BBBB-CCCC-DDDD", "kind": "apple" })));
        assert_eq!(r.await.unwrap().status(), StatusCode::NOT_FOUND);

        // Apple: a .pkpass, once. HEAD (link previews) doesn't use the link up.
        let r = app.clone().oneshot(signed("/v1/wallet", serde_json::json!({ "code": pass.code, "kind": "apple" }))).await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        let w: WalletResp = serde_json::from_slice(&to_bytes(r.into_body(), 1 << 20).await.unwrap()).unwrap();
        assert!(w.path.starts_with("/v1/wallet/apple/") && w.path.ends_with(".pkpass"));
        assert!(!w.path.contains(&normalize_code(&pass.code)[..4]), "no code in the link");
        assert_eq!(w.expires_in, 600);
        assert_eq!(app.clone().oneshot(get("HEAD", &w.path)).await.unwrap().status(), StatusCode::OK);
        let r = app.clone().oneshot(get("GET", &w.path)).await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(r.headers()["content-type"], "application/vnd.apple.pkpass");
        assert!(to_bytes(r.into_body(), 1 << 20).await.unwrap().starts_with(b"PK"), "zip file");
        assert_eq!(app.clone().oneshot(get("GET", &w.path)).await.unwrap().status(), StatusCode::GONE);

        // Google: a redirect to the save link, once.
        let r = app.clone().oneshot(signed("/v1/wallet", serde_json::json!({ "code": pass.code, "kind": "google" }))).await.unwrap();
        let w: WalletResp = serde_json::from_slice(&to_bytes(r.into_body(), 1 << 20).await.unwrap()).unwrap();
        let r = app.clone().oneshot(get("GET", &w.path)).await.unwrap();
        assert_eq!(r.status(), StatusCode::SEE_OTHER);
        assert!(r.headers()["location"].to_str().unwrap().starts_with("https://pay.google.com/gp/v/save/ey"));
        assert_eq!(app.clone().oneshot(get("GET", &w.path)).await.unwrap().status(), StatusCode::GONE);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn example_config_parses_with_and_without_wallet() {
        let raw = include_str!("../deploy/hub.example.toml");
        assert!(toml::from_str::<HubConfig>(raw).unwrap().wallet.is_none(), "wallet is off by default");
        // Uncommenting the wallet section must give a valid config.
        let (head, wallet) = raw.split_at(raw.find("# ---- Wallet passes").unwrap());
        let on: Vec<&str> = wallet
            .lines()
            .map(|l| l.strip_prefix("# ").filter(|r| r.starts_with('[') || r.contains(" = ")).unwrap_or(l))
            .collect();
        let cfg: HubConfig = toml::from_str(&format!("{head}\n{}", on.join("\n"))).unwrap();
        let w = cfg.wallet.unwrap();
        assert!(w.google_issuer_id.is_some() && w.apple_wwdr_pem.is_some());
        assert_eq!(w.locations.len(), 1);
    }

    #[test]
    fn signatures_cover_the_body() {
        let key = ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng);
        use ed25519_dalek::Signer;
        let msg = signing_message("POST", "/v1/claim", "v_1", 100, "nonce-0123456789", b"{\"code\":\"x\"}");
        let sig = key.sign(&msg);
        let tampered = signing_message("POST", "/v1/claim", "v_1", 100, "nonce-0123456789", b"{\"code\":\"y\"}");
        assert!(key.verifying_key().verify(&msg, &sig).is_ok());
        assert!(key.verifying_key().verify(&tampered, &sig).is_err());
    }
}
