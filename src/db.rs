//! Encrypted storage (SQLCipher: AES-256-CBC per page + HMAC-SHA512 page
//! authentication). The WAL and any temp files are encrypted too.
//!
//! All queries use bound parameters; no SQL is ever built from user input.

use crate::keys::DbKey;
use crate::net::Mac;
use anyhow::{bail, Context, Result};
use chrono::{Local, NaiveDate, TimeZone};
use rand::Rng;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::path::Path;
use std::sync::Mutex;
use zeroize::Zeroizing;

pub struct Db {
    conn: Mutex<Connection>,
}

#[derive(Debug, Clone, Copy, Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct Rate {
    pub pesos: u32,
    pub minutes: u32,
}

#[derive(Debug, Clone)]
pub struct ClientRow {
    pub mac: Mac,
    pub ip: std::net::Ipv4Addr,
    pub remaining_secs: u64,
    pub paused: bool,
}

/// Coins in the slot that haven't been credited yet. Saved on every coin,
/// so a power cut mid-insert doesn't lose them: they're credited on boot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct OpenSlot {
    pub mac: Mac,
    pub ip: std::net::Ipv4Addr,
    pub coins: Vec<u32>,
    /// The rates when the customer tapped Insert coin (what they were shown).
    pub rates: Vec<Rate>,
}

/// Money and log entries that go in with the time they paid for.
#[derive(Debug, Clone)]
pub enum Ledger {
    Sale { mac: Option<Mac>, pesos: u32, secs: u64, source: &'static str },
    PassCredit { pass_id: String, pesos: u32, secs: u64 },
    Audit { actor: &'static str, action: &'static str, detail: String },
}

/// Everything that changed since the last save, written in one transaction.
#[derive(Debug, Default)]
pub struct Batch {
    pub clients: Vec<ClientRow>,
    pub clients_gone: Vec<Mac>,
    pub links: Vec<(Mac, String)>,
    pub links_gone: Vec<Mac>,
    /// Some(Some): coins are in the slot; Some(None): they've been credited.
    pub slot: Option<Option<OpenSlot>>,
    pub ledger: Vec<Ledger>,
}

impl Batch {
    pub fn is_empty(&self) -> bool {
        self.clients.is_empty()
            && self.clients_gone.is_empty()
            && self.links.is_empty()
            && self.links_gone.is_empty()
            && self.slot.is_none()
            && self.ledger.is_empty()
    }
}

pub const OPEN_SLOT_KEY: &str = "open_slot";

#[derive(Debug, Serialize)]
pub struct Sale {
    pub ts: i64,
    pub mac: Option<String>,
    pub pesos: u32,
    pub secs: u64,
    pub source: String,
}

#[derive(Debug, Serialize)]
pub struct DaySales {
    pub date: String,
    pub pesos: u64,
    pub count: u64,
}

#[derive(Debug, Serialize)]
pub struct TopCustomer {
    pub mac: String,
    pub pesos: u64,
    pub count: u64,
}

#[derive(Debug, Serialize)]
pub struct SalesSummary {
    pub today_pesos: u64,
    pub today_count: u64,
    pub week_pesos: u64,
    pub month_pesos: u64,
    pub prev_month_pesos: u64,
    pub month_count: u64,
    pub vouchers_used_30d: u64,
    pub all_time_pesos: u64,
    pub unclaimed_pesos: u64,
    pub top_customers: Vec<TopCustomer>,
    pub days: Vec<DaySales>,
    pub recent: Vec<Sale>,
}

#[derive(Debug, Serialize)]
pub struct VoucherRow {
    pub id: String,
    pub secs: u64,
    pub label: String,
    pub created_at: i64,
    pub used_at: Option<i64>,
    pub used_by: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct AuditRow {
    pub ts: i64,
    pub actor: String,
    pub action: String,
    pub detail: String,
}

const SCHEMA_V1: &str = "
CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE rates (
    pesos   INTEGER PRIMARY KEY CHECK (pesos > 0),
    minutes INTEGER NOT NULL CHECK (minutes > 0)
);
CREATE TABLE clients (
    mac            TEXT PRIMARY KEY,
    ip             TEXT NOT NULL,
    remaining_secs INTEGER NOT NULL CHECK (remaining_secs >= 0),
    paused         INTEGER NOT NULL,
    updated_at     INTEGER NOT NULL
);
CREATE TABLE sales (
    id     INTEGER PRIMARY KEY,
    ts     INTEGER NOT NULL,
    mac    TEXT,
    pesos  INTEGER NOT NULL,
    secs   INTEGER NOT NULL,
    source TEXT NOT NULL
);
CREATE INDEX sales_ts ON sales (ts);
CREATE TABLE vouchers (
    code_hash  BLOB PRIMARY KEY,
    secs       INTEGER NOT NULL CHECK (secs > 0),
    label      TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    used_at    INTEGER,
    used_by    TEXT
);
CREATE TABLE audit (
    id     INTEGER PRIMARY KEY,
    ts     INTEGER NOT NULL,
    actor  TEXT NOT NULL,
    action TEXT NOT NULL,
    detail TEXT NOT NULL
);
INSERT INTO rates (pesos, minutes) VALUES (1, 20), (5, 120), (10, 300), (20, 720);
INSERT INTO settings (key, value) VALUES ('portal_name', 'Piso WiFi');
";

/// v2: owner-uploaded images (logo, advert poster) for the customer page.
const SCHEMA_V2: &str = "
CREATE TABLE assets (
    name       TEXT PRIMARY KEY,
    mime       TEXT NOT NULL,
    data       BLOB NOT NULL,
    updated_at INTEGER NOT NULL
);
";

/// v3: Piso Pass links (which phone holds which pass here) and sales
/// waiting to be reported to the hub.
const SCHEMA_V3: &str = "
CREATE TABLE pass_links (mac TEXT PRIMARY KEY, pass_id TEXT NOT NULL);
CREATE TABLE hub_outbox (
    id      INTEGER PRIMARY KEY AUTOINCREMENT,
    pass_id TEXT NOT NULL,
    pesos   INTEGER NOT NULL,
    secs    INTEGER NOT NULL,
    ts      INTEGER NOT NULL
);
";

/// Voucher alphabet without look-alikes (no 0/O, 1/I/L).
const VOUCHER_ALPHABET: &[u8] = b"23456789ABCDEFGHJKMNPQRSTUVWXYZ";

pub fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

impl Db {
    pub fn open(path: &Path, key: &DbKey) -> Result<Db> {
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        apply_key(&conn, key)?;
        let db = Db { conn: Mutex::new(conn) };
        db.migrate()?;
        Ok(db)
    }

    fn migrate(&self) -> Result<()> {
        let mut c = self.conn.lock().unwrap();
        let v: i64 = c.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if v < 1 {
            let tx = c.transaction()?;
            tx.execute_batch(SCHEMA_V1)?;
            tx.pragma_update(None, "user_version", 1)?;
            tx.commit()?;
        }
        if v < 2 {
            let tx = c.transaction()?;
            tx.execute_batch(SCHEMA_V2)?;
            tx.pragma_update(None, "user_version", 2)?;
            tx.commit()?;
        }
        if v < 3 {
            let tx = c.transaction()?;
            tx.execute_batch(SCHEMA_V3)?;
            tx.pragma_update(None, "user_version", 3)?;
            tx.commit()?;
        }
        Ok(())
    }

    // ---- settings -------------------------------------------------------

    pub fn get_setting(&self, key: &str) -> Result<Option<String>> {
        let c = self.conn.lock().unwrap();
        Ok(c.query_row("SELECT value FROM settings WHERE key = ?1", [key], |r| r.get(0))
            .optional()?)
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        let c = self.conn.lock().unwrap();
        c.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    // ---- images -----------------------------------------------------------

    /// Stores an image and returns its new version (milliseconds), used to
    /// bust browser caches.
    pub fn put_asset(&self, name: &str, mime: &str, data: &[u8]) -> Result<i64> {
        let c = self.conn.lock().unwrap();
        let version = chrono::Utc::now().timestamp_millis();
        c.execute(
            "INSERT INTO assets (name, mime, data, updated_at) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(name) DO UPDATE SET mime = excluded.mime, data = excluded.data, updated_at = excluded.updated_at",
            params![name, mime, data, version],
        )?;
        Ok(version)
    }

    pub fn get_asset(&self, name: &str) -> Result<Option<(String, Vec<u8>, i64)>> {
        let c = self.conn.lock().unwrap();
        Ok(c.query_row("SELECT mime, data, updated_at FROM assets WHERE name = ?1", [name], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .optional()?)
    }

    pub fn delete_asset(&self, name: &str) -> Result<bool> {
        let c = self.conn.lock().unwrap();
        Ok(c.execute("DELETE FROM assets WHERE name = ?1", [name])? > 0)
    }

    pub fn delete_setting(&self, key: &str) -> Result<()> {
        let c = self.conn.lock().unwrap();
        c.execute("DELETE FROM settings WHERE key = ?1", [key])?;
        Ok(())
    }

    // ---- Piso Pass --------------------------------------------------------

    pub fn load_links(&self) -> Result<Vec<(Mac, String)>> {
        let c = self.conn.lock().unwrap();
        let mut st = c.prepare("SELECT mac, pass_id FROM pass_links")?;
        let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        let mut out = vec![];
        for row in rows {
            let (mac, pass) = row?;
            if let Ok(mac) = Mac::parse(&mac) {
                out.push((mac, pass));
            }
        }
        Ok(out)
    }

    pub fn outbox_add(&self, pass_id: &str, pesos: u32, secs: u64) -> Result<()> {
        let c = self.conn.lock().unwrap();
        c.execute(
            "INSERT INTO hub_outbox (pass_id, pesos, secs, ts) VALUES (?1, ?2, ?3, ?4)",
            params![pass_id, pesos, secs as i64, now()],
        )?;
        Ok(())
    }

    pub fn outbox_list(&self, limit: u32) -> Result<Vec<crate::hub::proto::CreditReport>> {
        let c = self.conn.lock().unwrap();
        let mut st = c.prepare("SELECT id, pass_id, pesos, secs, ts FROM hub_outbox ORDER BY id LIMIT ?1")?;
        let rows = st.query_map([limit], |r| {
            Ok(crate::hub::proto::CreditReport {
                id: r.get(0)?,
                pass_id: r.get(1)?,
                pesos: r.get(2)?,
                secs: r.get::<_, i64>(3)? as u64,
                ts: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn outbox_delete(&self, ids: &[i64]) -> Result<()> {
        let mut c = self.conn.lock().unwrap();
        let tx = c.transaction()?;
        for id in ids {
            tx.execute("DELETE FROM hub_outbox WHERE id = ?1", [id])?;
        }
        tx.commit()?;
        Ok(())
    }

    // ---- rates ----------------------------------------------------------

    pub fn rates(&self) -> Result<Vec<Rate>> {
        let c = self.conn.lock().unwrap();
        let mut st = c.prepare("SELECT pesos, minutes FROM rates ORDER BY pesos")?;
        let rows = st.query_map([], |r| Ok(Rate { pesos: r.get(0)?, minutes: r.get(1)? }))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn set_rates(&self, rates: &[Rate]) -> Result<()> {
        let mut c = self.conn.lock().unwrap();
        let tx = c.transaction()?;
        tx.execute("DELETE FROM rates", [])?;
        for r in rates {
            tx.execute("INSERT INTO rates (pesos, minutes) VALUES (?1, ?2)", params![r.pesos, r.minutes])?;
        }
        tx.commit()?;
        Ok(())
    }

    // ---- clients (persisted session time) -------------------------------

    pub fn load_clients(&self) -> Result<Vec<ClientRow>> {
        let c = self.conn.lock().unwrap();
        let mut st = c.prepare("SELECT mac, ip, remaining_secs, paused FROM clients WHERE remaining_secs > 0")?;
        let rows = st.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?, r.get::<_, bool>(3)?))
        })?;
        let mut out = vec![];
        for row in rows {
            let (mac, ip, secs, paused) = row?;
            if let (Ok(mac), Ok(ip)) = (Mac::parse(&mac), ip.parse()) {
                out.push(ClientRow { mac, ip, remaining_secs: secs.max(0) as u64, paused });
            }
        }
        Ok(out)
    }

    pub fn open_slot(&self) -> Result<Option<OpenSlot>> {
        let Some(raw) = self.get_setting(OPEN_SLOT_KEY)? else { return Ok(None) };
        serde_json::from_str(&raw).map(Some).context("reading the saved coin slot")
    }

    /// Writes one batch of changes atomically: a coin's sale, the time it
    /// bought and the cleared slot either all land or none do.
    pub fn save(&self, b: &Batch) -> Result<()> {
        if b.is_empty() {
            return Ok(());
        }
        let mut c = self.conn.lock().unwrap();
        let tx = c.transaction()?;
        let ts = now();
        for r in &b.clients {
            tx.execute(
                "INSERT INTO clients (mac, ip, remaining_secs, paused, updated_at) VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(mac) DO UPDATE SET ip = excluded.ip, remaining_secs = excluded.remaining_secs,
                 paused = excluded.paused, updated_at = excluded.updated_at",
                params![r.mac.to_string(), r.ip.to_string(), r.remaining_secs.min(i64::MAX as u64) as i64, r.paused, ts],
            )?;
        }
        for m in &b.clients_gone {
            tx.execute("DELETE FROM clients WHERE mac = ?1", [m.to_string()])?;
        }
        for (m, pass) in &b.links {
            tx.execute(
                "INSERT INTO pass_links (mac, pass_id) VALUES (?1, ?2)
                 ON CONFLICT(mac) DO UPDATE SET pass_id = excluded.pass_id",
                params![m.to_string(), pass],
            )?;
        }
        for m in &b.links_gone {
            tx.execute("DELETE FROM pass_links WHERE mac = ?1", [m.to_string()])?;
        }
        match &b.slot {
            None => {}
            Some(Some(slot)) => {
                tx.execute(
                    "INSERT INTO settings (key, value) VALUES (?1, ?2)
                     ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                    params![OPEN_SLOT_KEY, serde_json::to_string(slot)?],
                )?;
            }
            Some(None) => {
                tx.execute("DELETE FROM settings WHERE key = ?1", [OPEN_SLOT_KEY])?;
            }
        }
        for e in &b.ledger {
            match e {
                Ledger::Sale { mac, pesos, secs, source } => tx.execute(
                    "INSERT INTO sales (ts, mac, pesos, secs, source) VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![ts, mac.map(|m| m.to_string()), pesos, *secs as i64, source],
                )?,
                Ledger::PassCredit { pass_id, pesos, secs } => tx.execute(
                    "INSERT INTO hub_outbox (pass_id, pesos, secs, ts) VALUES (?1, ?2, ?3, ?4)",
                    params![pass_id, pesos, *secs as i64, ts],
                )?,
                Ledger::Audit { actor, action, detail } => tx.execute(
                    "INSERT INTO audit (ts, actor, action, detail) VALUES (?1, ?2, ?3, ?4)",
                    params![ts, actor, action, detail],
                )?,
            };
        }
        tx.commit()?;
        Ok(())
    }

    // ---- sales ----------------------------------------------------------

    pub fn add_sale(&self, mac: Option<Mac>, pesos: u32, secs: u64, source: &str) -> Result<()> {
        let c = self.conn.lock().unwrap();
        c.execute(
            "INSERT INTO sales (ts, mac, pesos, secs, source) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![now(), mac.map(|m| m.to_string()), pesos, secs as i64, source],
        )?;
        Ok(())
    }

    pub fn sales_summary(&self, days: u32) -> Result<SalesSummary> {
        let c = self.conn.lock().unwrap();
        let today = Local::now().date_naive();
        let start_of = |d: NaiveDate| -> i64 {
            Local
                .from_local_datetime(&d.and_hms_opt(0, 0, 0).unwrap())
                .earliest()
                .map(|t| t.timestamp())
                .unwrap_or(0)
        };
        let sum_since = |ts: i64| -> rusqlite::Result<(u64, u64)> {
            c.query_row(
                "SELECT COALESCE(SUM(pesos), 0), COUNT(*) FROM sales WHERE ts >= ?1 AND source != 'unclaimed'",
                [ts],
                |r| Ok((r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)? as u64)),
            )
        };
        let (today_pesos, today_count) = sum_since(start_of(today))?;
        let (week_pesos, _) = sum_since(start_of(today - chrono::Days::new(6)))?;
        let month_start = start_of(today - chrono::Days::new(29));
        let (month_pesos, month_count) = sum_since(month_start)?;
        let (all_time_pesos, _) = sum_since(i64::MIN)?;
        let prev_month_pesos: i64 = c.query_row(
            "SELECT COALESCE(SUM(pesos), 0) FROM sales WHERE ts >= ?1 AND ts < ?2 AND source != 'unclaimed'",
            [start_of(today - chrono::Days::new(59)), month_start],
            |r| r.get(0),
        )?;
        let vouchers_used_30d: i64 =
            c.query_row("SELECT COUNT(*) FROM vouchers WHERE used_at >= ?1", [month_start], |r| r.get(0))?;
        let mut st = c.prepare(
            "SELECT mac, SUM(pesos), COUNT(*) FROM sales
             WHERE ts >= ?1 AND source = 'coin' AND mac IS NOT NULL
             GROUP BY mac ORDER BY 2 DESC LIMIT 4",
        )?;
        let top_customers = st
            .query_map([month_start], |r| {
                Ok(TopCustomer { mac: r.get(0)?, pesos: r.get::<_, i64>(1)? as u64, count: r.get::<_, i64>(2)? as u64 })
            })?
            .collect::<rusqlite::Result<_>>()?;
        let unclaimed_pesos: i64 = c.query_row(
            "SELECT COALESCE(SUM(pesos), 0) FROM sales WHERE source = 'unclaimed'",
            [],
            |r| r.get(0),
        )?;

        let mut out_days = vec![];
        for i in (0..days).rev() {
            let d = today - chrono::Days::new(i as u64);
            let (a, b) = (start_of(d), start_of(d + chrono::Days::new(1)));
            let (pesos, count) = c.query_row(
                "SELECT COALESCE(SUM(pesos), 0), COUNT(*) FROM sales
                 WHERE ts >= ?1 AND ts < ?2 AND source != 'unclaimed'",
                [a, b],
                |r| Ok((r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)? as u64)),
            )?;
            out_days.push(DaySales { date: d.format("%Y-%m-%d").to_string(), pesos, count });
        }

        let mut st = c.prepare("SELECT ts, mac, pesos, secs, source FROM sales ORDER BY id DESC LIMIT 25")?;
        let recent = st
            .query_map([], |r| {
                Ok(Sale {
                    ts: r.get(0)?,
                    mac: r.get(1)?,
                    pesos: r.get(2)?,
                    secs: r.get::<_, i64>(3)? as u64,
                    source: r.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;

        Ok(SalesSummary {
            today_pesos,
            today_count,
            week_pesos,
            month_pesos,
            prev_month_pesos: prev_month_pesos as u64,
            month_count,
            vouchers_used_30d: vouchers_used_30d as u64,
            all_time_pesos,
            unclaimed_pesos: unclaimed_pesos as u64,
            top_customers,
            days: out_days,
            recent,
        })
    }

    // ---- vouchers -------------------------------------------------------
    // Only a hash of each code is stored, so even a decrypted database
    // doesn't reveal unused codes. Codes are shown once, at creation.

    pub fn create_vouchers(&self, count: u32, secs: u64, label: &str) -> Result<Vec<String>> {
        let mut c = self.conn.lock().unwrap();
        let tx = c.transaction()?;
        let mut codes = vec![];
        let mut rng = rand::rngs::OsRng;
        while codes.len() < count as usize {
            let raw: String = (0..12)
                .map(|_| VOUCHER_ALPHABET[rng.gen_range(0..VOUCHER_ALPHABET.len())] as char)
                .collect();
            let n = tx.execute(
                "INSERT OR IGNORE INTO vouchers (code_hash, secs, label, created_at) VALUES (?1, ?2, ?3, ?4)",
                params![voucher_hash(&raw).to_vec(), secs as i64, label, now()],
            )?;
            if n == 1 {
                codes.push(format!("{}-{}-{}", &raw[0..4], &raw[4..8], &raw[8..12]));
            }
        }
        tx.commit()?;
        Ok(codes)
    }

    /// Marks a voucher used and returns its duration, or None if invalid/used.
    pub fn redeem_voucher(&self, code: &str, mac: Mac) -> Result<Option<u64>> {
        let normalized: String = code
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .map(|c| c.to_ascii_uppercase())
            .collect();
        if normalized.len() != 12 {
            return Ok(None);
        }
        let c = self.conn.lock().unwrap();
        let hash = voucher_hash(&normalized).to_vec();
        // Single atomic UPDATE so a code can never be redeemed twice.
        let secs: Option<i64> = c
            .query_row(
                "UPDATE vouchers SET used_at = ?1, used_by = ?2
                 WHERE code_hash = ?3 AND used_at IS NULL RETURNING secs",
                params![now(), mac.to_string(), hash],
                |r| r.get(0),
            )
            .optional()?;
        Ok(secs.map(|s| s as u64))
    }

    pub fn list_vouchers(&self) -> Result<Vec<VoucherRow>> {
        let c = self.conn.lock().unwrap();
        let mut st = c.prepare(
            "SELECT code_hash, secs, label, created_at, used_at, used_by FROM vouchers
             ORDER BY created_at DESC LIMIT 300",
        )?;
        let rows = st.query_map([], |r| {
            let h: Vec<u8> = r.get(0)?;
            Ok(VoucherRow {
                id: hex::encode(&h[..4]),
                secs: r.get::<_, i64>(1)? as u64,
                label: r.get(2)?,
                created_at: r.get(3)?,
                used_at: r.get(4)?,
                used_by: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn delete_unused_voucher(&self, id: &str) -> Result<bool> {
        if id.len() != 8 || !id.chars().all(|c| c.is_ascii_hexdigit()) {
            return Ok(false);
        }
        let c = self.conn.lock().unwrap();
        let prefix = hex::decode(id)?;
        let n = c.execute(
            "DELETE FROM vouchers WHERE substr(code_hash, 1, 4) = ?1 AND used_at IS NULL",
            [prefix],
        )?;
        Ok(n > 0)
    }

    // ---- audit log ------------------------------------------------------

    pub fn audit(&self, actor: &str, action: &str, detail: &str) {
        let c = self.conn.lock().unwrap();
        if let Err(e) = c.execute(
            "INSERT INTO audit (ts, actor, action, detail) VALUES (?1, ?2, ?3, ?4)",
            params![now(), actor, action, detail],
        ) {
            tracing::error!("audit write failed: {e}");
        }
    }

    pub fn list_audit(&self, limit: u32) -> Result<Vec<AuditRow>> {
        let c = self.conn.lock().unwrap();
        let mut st = c.prepare("SELECT ts, actor, action, detail FROM audit ORDER BY id DESC LIMIT ?1")?;
        let rows = st.query_map([limit], |r| {
            Ok(AuditRow { ts: r.get(0)?, actor: r.get(1)?, action: r.get(2)?, detail: r.get(3)? })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    #[cfg(test)]
    pub fn exec_for_test(&self, sql: &str) {
        self.conn.lock().unwrap().execute_batch(sql).unwrap();
    }

    // ---- backup ---------------------------------------------------------

    /// Writes a copy encrypted with an owner passphrase (SQLCipher's
    /// PBKDF2-HMAC-SHA512, 256k iterations) so data survives a dead board.
    pub fn export_backup(&self, out: &Path, passphrase: &str) -> Result<()> {
        if out.exists() {
            bail!("{} already exists", out.display());
        }
        let c = self.conn.lock().unwrap();
        c.execute("ATTACH DATABASE ?1 AS backup KEY ?2", params![out.to_string_lossy(), passphrase])?;
        let r = c.query_row("SELECT sqlcipher_export('backup')", [], |_| Ok(())).and_then(|_| {
            // sqlcipher_export doesn't carry the schema version over.
            let v: i64 = c.query_row("PRAGMA main.user_version", [], |r| r.get(0))?;
            c.execute_batch(&format!("PRAGMA backup.user_version = {v};"))
        });
        c.execute("DETACH DATABASE backup", [])?;
        r?;
        Ok(())
    }
}

/// Opens the database with `key`; a database still encrypted with the
/// pre-Argon2 `legacy` key is re-encrypted in place first.
pub fn open_migrating(path: &Path, key: &DbKey, legacy: &DbKey) -> Result<Db> {
    if !path.exists() {
        return Db::open(path, key);
    }
    match Db::open(path, key) {
        Ok(db) => Ok(db),
        Err(e) => {
            let conn = Connection::open(path)?;
            conn.pragma_update(None, "key", legacy.pragma_value().as_str())?;
            if conn.query_row("SELECT count(*) FROM sqlite_master", [], |_| Ok(())).is_err() {
                return Err(e);
            }
            // Fold the WAL back in, then change the key.
            let _: String = conn.query_row("PRAGMA journal_mode = DELETE", [], |r| r.get(0))?;
            conn.pragma_update(None, "rekey", key.pragma_value().as_str())?;
            drop(conn);
            tracing::info!("database re-encrypted with the v2 (Argon2id) key");
            Db::open(path, key)
        }
    }
}

/// Restores a passphrase backup into a fresh database keyed for this board.
pub fn import_backup(backup: &Path, passphrase: &str, dest: &Path, key: &DbKey) -> Result<()> {
    if dest.exists() {
        bail!("{} already exists; move it away first", dest.display());
    }
    let c = Connection::open(backup)?;
    c.pragma_update(None, "key", passphrase)?;
    c.query_row("SELECT count(*) FROM sqlite_master", [], |_| Ok(()))
        .context("wrong passphrase or not a pisowifi backup")?;
    let k = key.pragma_value();
    // Raw-key literal is hex we generated ourselves, never user input.
    c.execute_batch(&Zeroizing::new(format!(
        "ATTACH DATABASE '{}' AS fresh KEY \"{}\";",
        dest.to_string_lossy().replace('\'', "''"),
        k.as_str()
    )))?;
    c.query_row("SELECT sqlcipher_export('fresh')", [], |_| Ok(()))?;
    let v: i64 = c.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    c.execute_batch(&format!("PRAGMA fresh.user_version = {v}; DETACH DATABASE fresh;"))?;
    Ok(())
}

pub(crate) fn apply_key(conn: &Connection, key: &DbKey) -> Result<()> {
    conn.pragma_update(None, "key", key.pragma_value().as_str())?;
    let version: String = conn
        .query_row("PRAGMA cipher_version", [], |r| r.get(0))
        .context("SQLCipher is not compiled in")?;
    if version.is_empty() {
        bail!("SQLCipher is not active; refusing to store data unencrypted");
    }
    conn.query_row("SELECT count(*) FROM sqlite_master", [], |_| Ok(()))
        .context("cannot decrypt database (wrong board, or the salt file changed)")?;
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = FULL;
         PRAGMA secure_delete = ON;
         PRAGMA foreign_keys = ON;
         PRAGMA temp_store = MEMORY;",
    )?;
    Ok(())
}

fn voucher_hash(code: &str) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(b"pisowifi voucher v1:");
    h.update(code.as_bytes());
    h.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_db() -> (tempdir::Dir, Db) {
        let dir = tempdir::Dir::new();
        let key = DbKey(Zeroizing::new([7u8; 32]));
        let db = Db::open(&dir.0.join("t.db"), &key).unwrap();
        (dir, db)
    }

    mod tempdir {
        pub struct Dir(pub std::path::PathBuf);
        impl Dir {
            pub fn new() -> Self {
                let p = std::env::temp_dir().join(format!("pisowifi-test-{}", rand::random::<u64>()));
                std::fs::create_dir_all(&p).unwrap();
                Dir(p)
            }
        }
        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    #[test]
    fn file_is_encrypted_and_wrong_key_fails() {
        let (dir, db) = test_db();
        db.set_setting("portal_name", "SECRET-MARKER").unwrap();
        drop(db);
        let raw = std::fs::read(dir.0.join("t.db")).unwrap();
        assert!(!raw.windows(13).any(|w| w == b"SECRET-MARKER"));
        assert!(!raw.starts_with(b"SQLite format 3"));
        let wrong = DbKey(Zeroizing::new([8u8; 32]));
        assert!(Db::open(&dir.0.join("t.db"), &wrong).is_err());
    }

    #[test]
    fn voucher_redeems_once() {
        let (_d, db) = test_db();
        let codes = db.create_vouchers(3, 3600, "test").unwrap();
        let mac = Mac::parse("a4:5e:60:11:22:33").unwrap();
        assert_eq!(db.redeem_voucher(&codes[0].to_lowercase(), mac).unwrap(), Some(3600));
        assert_eq!(db.redeem_voucher(&codes[0], mac).unwrap(), None);
        assert_eq!(db.redeem_voucher("AAAA-BBBB-CCCC", mac).unwrap(), None);
    }

    #[test]
    fn upgrades_v1_key_to_v2_in_place() {
        let dir = tempdir::Dir::new();
        let path = dir.0.join("m.db");
        let v1 = DbKey(Zeroizing::new([1u8; 32]));
        let v2 = DbKey(Zeroizing::new([2u8; 32]));
        let db = Db::open(&path, &v1).unwrap();
        db.set_setting("portal_name", "Old Box").unwrap();
        drop(db);
        let db = open_migrating(&path, &v2, &v1).unwrap();
        assert_eq!(db.get_setting("portal_name").unwrap().as_deref(), Some("Old Box"));
        drop(db);
        assert!(Db::open(&path, &v1).is_err(), "old key no longer works");
        assert!(Db::open(&path, &v2).is_ok());
        // Already migrated: opens directly.
        assert!(open_migrating(&path, &v2, &v1).is_ok());
    }

    #[test]
    fn batches_upsert_and_delete_single_rows() {
        let (_d, db) = test_db();
        let (a, b) = (Mac::parse("02:00:00:00:00:0a").unwrap(), Mac::parse("02:00:00:00:00:0b").unwrap());
        let row = |mac, secs, paused| ClientRow { mac, ip: "10.0.0.9".parse().unwrap(), remaining_secs: secs, paused };
        db.save(&Batch { clients: vec![row(a, 100, false), row(b, 200, true)], ..Default::default() }).unwrap();
        db.save(&Batch { clients: vec![row(a, 70, false)], ..Default::default() }).unwrap();
        let mut rows = db.load_clients().unwrap();
        rows.sort_by_key(|r| r.mac);
        assert_eq!(rows.iter().map(|r| r.remaining_secs).collect::<Vec<_>>(), vec![70, 200]);
        db.save(&Batch { clients_gone: vec![a], links: vec![(b, "P1".into())], ..Default::default() }).unwrap();
        assert_eq!(db.load_clients().unwrap().len(), 1);
        assert_eq!(db.load_links().unwrap(), vec![(b, "P1".to_string())]);
        db.save(&Batch { links_gone: vec![b], ..Default::default() }).unwrap();
        assert!(db.load_links().unwrap().is_empty());
    }

    #[test]
    fn open_slot_is_saved_and_cleared_with_its_sale() {
        let (_d, db) = test_db();
        let slot = OpenSlot {
            mac: Mac::parse("02:00:00:00:00:0a").unwrap(),
            ip: "10.0.0.9".parse().unwrap(),
            coins: vec![5, 1],
            rates: vec![Rate { pesos: 1, minutes: 20 }],
        };
        db.save(&Batch { slot: Some(Some(slot.clone())), ..Default::default() }).unwrap();
        assert_eq!(db.open_slot().unwrap(), Some(slot.clone()));
        let sale = Ledger::Sale { mac: Some(slot.mac), pesos: 6, secs: 7200, source: "coin" };
        db.save(&Batch { slot: Some(None), ledger: vec![sale], ..Default::default() }).unwrap();
        assert_eq!(db.open_slot().unwrap(), None);
        assert_eq!(db.sales_summary(1).unwrap().today_pesos, 6);
    }

    #[test]
    fn backup_roundtrip() {
        let (dir, db) = test_db();
        db.set_rates(&[Rate { pesos: 5, minutes: 99 }]).unwrap();
        let bk = dir.0.join("backup.db");
        db.export_backup(&bk, "correct horse").unwrap();
        let key2 = DbKey(Zeroizing::new([9u8; 32]));
        assert!(import_backup(&bk, "wrong", &dir.0.join("x.db"), &key2).is_err());
        import_backup(&bk, "correct horse", &dir.0.join("new.db"), &key2).unwrap();
        let db2 = Db::open(&dir.0.join("new.db"), &key2).unwrap();
        assert_eq!(db2.rates().unwrap(), vec![Rate { pesos: 5, minutes: 99 }]);
    }
}
