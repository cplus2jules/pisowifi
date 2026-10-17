//! Encrypted storage (SQLCipher: AES-256-CBC per page + HMAC-SHA512 page
//! authentication). The WAL and any temp files are encrypted too.
//!
//! All queries use bound parameters; no SQL is ever built from user input.

use crate::keys::DbKey;
use crate::net::Mac;
use anyhow::{bail, Context, Result};
use chrono::{Datelike, Local, NaiveDate, TimeZone, Timelike};
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

/// What a device has told us about itself, and the owner's name for it.
#[derive(Debug, Clone)]
pub struct DeviceRow {
    pub mac: Mac,
    pub nickname: String,
    pub hostname: String,
    pub user_agent: String,
    pub first_seen: i64,
    pub last_seen: i64,
}

/// All-time purchases by one device.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct Spend {
    pub pesos: u64,
    pub count: u64,
    pub secs: u64,
    pub last: i64,
}

#[derive(Debug, Serialize)]
pub struct SourceTotal {
    pub source: String,
    pub pesos: u64,
    pub count: u64,
    pub secs: u64,
}

/// Sales over a period, for the Sales page.
#[derive(Debug, Serialize)]
pub struct Report {
    pub days: u32,
    /// "day" or "month" (a year is shown month by month).
    pub group: &'static str,
    pub buckets: Vec<DaySales>,
    pub sources: Vec<SourceTotal>,
    /// Pesos by weekday (0 = Monday) and hour.
    pub heat: Vec<Vec<u64>>,
    pub pesos: u64,
    pub count: u64,
    pub secs: u64,
    pub customers: u64,
    pub prev_pesos: u64,
    pub best: Option<DaySales>,
    pub best_ever: Option<DaySales>,
}

/// Cash in the coin box since it was last emptied.
#[derive(Debug, Serialize)]
pub struct CoinBox {
    pub since: Option<i64>,
    pub pesos: u64,
    pub count: u64,
    pub history: Vec<Collection>,
}

#[derive(Debug, Serialize)]
pub struct Collection {
    pub ts: i64,
    /// What the sales say should have been in the box.
    pub expected: u64,
    /// What the owner counted.
    pub counted: u64,
    pub note: String,
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

/// v4: secrets that must stay inside the encrypted database (the vendo's
/// hub signing key).
const SCHEMA_V4: &str = "
CREATE TABLE secrets (name TEXT PRIMARY KEY, value BLOB NOT NULL);
";

/// v5: devices the owner can tell apart (model, own name, nickname), and
/// each time the coin box was emptied.
const SCHEMA_V5: &str = "
CREATE TABLE devices (
    mac        TEXT PRIMARY KEY,
    nickname   TEXT NOT NULL,
    hostname   TEXT NOT NULL,
    user_agent TEXT NOT NULL,
    first_seen INTEGER NOT NULL,
    last_seen  INTEGER NOT NULL
);
CREATE INDEX sales_mac ON sales (mac);
CREATE TABLE collections (
    id       INTEGER PRIMARY KEY,
    ts       INTEGER NOT NULL,
    expected INTEGER NOT NULL,
    counted  INTEGER NOT NULL,
    note     TEXT NOT NULL
);
";

/// v6: devices the owner blocked from buying or using time here.
const SCHEMA_V6: &str = "
CREATE TABLE blocked (mac TEXT PRIMARY KEY, ts INTEGER NOT NULL);
";

/// Voucher alphabet without look-alikes (no 0/O, 1/I/L).
const VOUCHER_ALPHABET: &[u8] = b"23456789ABCDEFGHJKMNPQRSTUVWXYZ";

pub fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Local midnight at the start of `d`.
fn start_of(d: NaiveDate) -> i64 {
    Local.from_local_datetime(&d.and_hms_opt(0, 0, 0).unwrap()).earliest().map(|t| t.timestamp()).unwrap_or(0)
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
        if v < 4 {
            let tx = c.transaction()?;
            tx.execute_batch(SCHEMA_V4)?;
            tx.pragma_update(None, "user_version", 4)?;
            tx.commit()?;
        }
        if v < 5 {
            let tx = c.transaction()?;
            tx.execute_batch(SCHEMA_V5)?;
            tx.pragma_update(None, "user_version", 5)?;
            tx.commit()?;
        }
        if v < 6 {
            let tx = c.transaction()?;
            tx.execute_batch(SCHEMA_V6)?;
            tx.pragma_update(None, "user_version", 6)?;
            tx.commit()?;
        }
        Ok(())
    }

    // ---- secrets --------------------------------------------------------

    pub fn get_secret(&self, name: &str) -> Result<Option<Zeroizing<Vec<u8>>>> {
        let c = self.conn.lock().unwrap();
        let v: Option<Vec<u8>> = c.query_row("SELECT value FROM secrets WHERE name = ?1", [name], |r| r.get(0)).optional()?;
        Ok(v.map(Zeroizing::new))
    }

    /// Stores a secret unless one by that name exists; returns the stored one.
    pub fn put_secret_once(&self, name: &str, value: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
        {
            let c = self.conn.lock().unwrap();
            c.execute("INSERT OR IGNORE INTO secrets (name, value) VALUES (?1, ?2)", params![name, value])?;
        }
        self.get_secret(name)?.context("secret not stored")
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

    /// Forgets which phone holds which pass and the sales not yet reported:
    /// both belong to one hub enrolment. Phones keep their time here.
    pub fn clear_pass_state(&self) -> Result<()> {
        let c = self.conn.lock().unwrap();
        c.execute_batch("DELETE FROM pass_links; DELETE FROM hub_outbox;")?;
        Ok(())
    }

    /// At startup: pass links and unsent credits belong to the hub enrolment
    /// they were made under. After `join-hub` (a new vendo id) or `leave-hub`
    /// they are dropped, since a running service writes them back on its way
    /// down. Returns true if anything was cleared.
    pub fn reset_pass_state_if_hub_changed(&self, vendo_id: Option<&str>) -> Result<bool> {
        let owner = self.get_setting("pass_links_hub")?;
        if owner.as_deref() == vendo_id {
            return Ok(false);
        }
        // Links from before this was recorded belong to the current enrolment.
        let adopt = owner.is_none() && vendo_id.is_some();
        if !adopt {
            self.clear_pass_state()?;
        }
        match vendo_id {
            Some(id) => self.set_setting("pass_links_hub", id)?,
            None => self.delete_setting("pass_links_hub")?,
        }
        Ok(!adopt)
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

    /// Sales in the period ending today: per day (per month for a year),
    /// by source and by hour of the week.
    pub fn report(&self, days: u32) -> Result<Report> {
        let c = self.conn.lock().unwrap();
        let today = Local::now().date_naive();
        let monthly = days > 92;
        let first = if monthly {
            (0..11).fold(today.with_day(1).unwrap(), |d, _| (d - chrono::Days::new(1)).with_day(1).unwrap())
        } else {
            today - chrono::Days::new(days.max(1) as u64 - 1)
        };
        let since = start_of(first);
        let length = (today - first).num_days() as u64 + 1;
        let prev_pesos: i64 = c.query_row(
            "SELECT COALESCE(SUM(pesos), 0) FROM sales WHERE ts >= ?1 AND ts < ?2 AND source != 'unclaimed'",
            [start_of(first - chrono::Days::new(length)), since],
            |r| r.get(0),
        )?;

        let key = |d: NaiveDate| if monthly { d.format("%Y-%m").to_string() } else { d.format("%Y-%m-%d").to_string() };
        let mut buckets: Vec<DaySales> = vec![];
        let mut d = first;
        while d <= today {
            let date = if monthly { d.format("%Y-%m-01").to_string() } else { d.format("%Y-%m-%d").to_string() };
            if buckets.last().is_none_or(|b| b.date != date) {
                buckets.push(DaySales { date, pesos: 0, count: 0 });
            }
            d = d + chrono::Days::new(1);
        }
        let index: std::collections::HashMap<String, usize> =
            buckets.iter().enumerate().map(|(i, b)| (b.date[..if monthly { 7 } else { 10 }].to_string(), i)).collect();

        let mut heat = vec![vec![0u64; 24]; 7];
        let mut sources: Vec<SourceTotal> = vec![];
        let mut customers = std::collections::HashSet::new();
        let (mut pesos, mut count, mut secs) = (0u64, 0u64, 0u64);
        let mut st = c.prepare("SELECT ts, mac, pesos, secs, source FROM sales WHERE ts >= ?1")?;
        let mut rows = st.query([since])?;
        while let Some(r) = rows.next()? {
            let (ts, mac, p, s, source): (i64, Option<String>, i64, i64, String) = (r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?);
            let (p, s) = (p.max(0) as u64, s.max(0) as u64);
            match sources.iter_mut().find(|t| t.source == source) {
                Some(t) => {
                    t.pesos += p;
                    t.count += 1;
                    t.secs += s;
                }
                None => sources.push(SourceTotal { source: source.clone(), pesos: p, count: 1, secs: s }),
            }
            if source == "unclaimed" {
                continue;
            }
            let Some(at) = Local.timestamp_opt(ts, 0).single() else { continue };
            if let Some(&i) = index.get(&key(at.date_naive())) {
                buckets[i].pesos += p;
                buckets[i].count += 1;
            }
            heat[at.weekday().num_days_from_monday() as usize][at.hour() as usize] += p;
            pesos += p;
            count += 1;
            secs += s;
            if let Some(m) = mac {
                customers.insert(m);
            }
        }
        sources.sort_by(|a, b| b.pesos.cmp(&a.pesos).then(b.count.cmp(&a.count)));
        let best = buckets.iter().filter(|b| b.pesos > 0).max_by_key(|b| b.pesos).map(|b| DaySales { date: b.date.clone(), ..*b });
        let best_ever = c
            .query_row(
                "SELECT date(ts, 'unixepoch', 'localtime') AS d, SUM(pesos), COUNT(*) FROM sales
                 WHERE source != 'unclaimed' GROUP BY d ORDER BY 2 DESC LIMIT 1",
                [],
                |r| Ok(DaySales { date: r.get(0)?, pesos: r.get::<_, i64>(1)? as u64, count: r.get::<_, i64>(2)? as u64 }),
            )
            .optional()?
            .filter(|d| d.pesos > 0);
        Ok(Report {
            days,
            group: if monthly { "month" } else { "day" },
            buckets,
            sources,
            heat,
            pesos,
            count,
            secs,
            customers: customers.len() as u64,
            prev_pesos: prev_pesos as u64,
            best,
            best_ever,
        })
    }

    /// Every sale since `since`, oldest first (for the spreadsheet download).
    pub fn sales_since(&self, since: i64) -> Result<Vec<Sale>> {
        let c = self.conn.lock().unwrap();
        let mut st = c.prepare("SELECT ts, mac, pesos, secs, source FROM sales WHERE ts >= ?1 ORDER BY id LIMIT 200000")?;
        let rows = st.query_map([since], |r| {
            Ok(Sale { ts: r.get(0)?, mac: r.get(1)?, pesos: r.get(2)?, secs: r.get::<_, i64>(3)? as u64, source: r.get(4)? })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    // ---- devices --------------------------------------------------------

    pub fn load_devices(&self) -> Result<Vec<DeviceRow>> {
        let c = self.conn.lock().unwrap();
        let mut st = c.prepare("SELECT mac, nickname, hostname, user_agent, first_seen, last_seen FROM devices")?;
        let rows = st.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))
        })?;
        let mut out = vec![];
        for row in rows {
            let (mac, nickname, hostname, user_agent, first_seen, last_seen) = row?;
            if let Ok(mac) = Mac::parse(&mac) {
                out.push(DeviceRow { mac, nickname, hostname, user_agent, first_seen, last_seen });
            }
        }
        Ok(out)
    }

    pub fn save_device(&self, d: &DeviceRow) -> Result<()> {
        let c = self.conn.lock().unwrap();
        c.execute(
            "INSERT INTO devices (mac, nickname, hostname, user_agent, first_seen, last_seen) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT (mac) DO UPDATE SET nickname = excluded.nickname, hostname = excluded.hostname,
                 user_agent = excluded.user_agent, last_seen = excluded.last_seen",
            params![d.mac.to_string(), d.nickname, d.hostname, d.user_agent, d.first_seen, d.last_seen],
        )?;
        Ok(())
    }

    /// Drops devices the owner never named that haven't been back since `before`.
    pub fn forget_devices(&self, before: i64) -> Result<()> {
        let c = self.conn.lock().unwrap();
        c.execute("DELETE FROM devices WHERE last_seen < ?1 AND nickname = ''", [before])?;
        Ok(())
    }

    /// All-time purchases per device.
    pub fn device_spend(&self) -> Result<std::collections::HashMap<String, Spend>> {
        let c = self.conn.lock().unwrap();
        let mut st = c.prepare(
            "SELECT mac, SUM(pesos), COUNT(*), SUM(secs), MAX(ts) FROM sales
             WHERE mac IS NOT NULL AND source != 'unclaimed' GROUP BY mac",
        )?;
        let rows = st.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                Spend {
                    pesos: r.get::<_, i64>(1)? as u64,
                    count: r.get::<_, i64>(2)? as u64,
                    secs: r.get::<_, i64>(3)? as u64,
                    last: r.get(4)?,
                },
            ))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn device_sales(&self, mac: Mac, limit: u32) -> Result<Vec<Sale>> {
        let c = self.conn.lock().unwrap();
        let mut st = c.prepare("SELECT ts, mac, pesos, secs, source FROM sales WHERE mac = ?1 ORDER BY id DESC LIMIT ?2")?;
        let rows = st.query_map(params![mac.to_string(), limit], |r| {
            Ok(Sale { ts: r.get(0)?, mac: r.get(1)?, pesos: r.get(2)?, secs: r.get::<_, i64>(3)? as u64, source: r.get(4)? })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    // ---- blocked devices ------------------------------------------------

    pub fn load_blocked(&self) -> Result<Vec<Mac>> {
        let c = self.conn.lock().unwrap();
        let mut st = c.prepare("SELECT mac FROM blocked")?;
        let rows = st.query_map([], |r| r.get::<_, String>(0))?;
        let mut out = vec![];
        for m in rows {
            if let Ok(mac) = Mac::parse(&m?) {
                out.push(mac);
            }
        }
        Ok(out)
    }

    pub fn set_blocked(&self, mac: Mac, blocked: bool) -> Result<()> {
        let c = self.conn.lock().unwrap();
        if blocked {
            c.execute("INSERT OR IGNORE INTO blocked (mac, ts) VALUES (?1, ?2)", params![mac.to_string(), now()])?;
        } else {
            c.execute("DELETE FROM blocked WHERE mac = ?1", [mac.to_string()])?;
        }
        Ok(())
    }

    // ---- coin box -------------------------------------------------------
    // Coins (claimed or not) are cash in the box until the owner empties it.

    pub fn coinbox(&self) -> Result<CoinBox> {
        let c = self.conn.lock().unwrap();
        let since: Option<i64> = c.query_row("SELECT MAX(ts) FROM collections", [], |r| r.get(0))?;
        let (pesos, count) = c.query_row(
            "SELECT COALESCE(SUM(pesos), 0), COUNT(*) FROM sales WHERE source IN ('coin', 'unclaimed') AND ts > ?1",
            [since.unwrap_or(i64::MIN)],
            |r| Ok((r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)? as u64)),
        )?;
        let mut st = c.prepare("SELECT ts, expected, counted, note FROM collections ORDER BY id DESC LIMIT 12")?;
        let history = st
            .query_map([], |r| {
                Ok(Collection { ts: r.get(0)?, expected: r.get::<_, i64>(1)? as u64, counted: r.get::<_, i64>(2)? as u64, note: r.get(3)? })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(CoinBox { since, pesos, count, history })
    }

    /// The owner emptied the coin box: what the sales say was in it, and
    /// what they counted (the same when they didn't count).
    pub fn collect(&self, counted: Option<u64>, note: &str) -> Result<Collection> {
        let mut c = self.conn.lock().unwrap();
        let tx = c.transaction()?;
        let ts = now();
        let since: Option<i64> = tx.query_row("SELECT MAX(ts) FROM collections", [], |r| r.get(0))?;
        let expected: i64 = tx.query_row(
            "SELECT COALESCE(SUM(pesos), 0) FROM sales WHERE source IN ('coin', 'unclaimed') AND ts > ?1 AND ts <= ?2",
            [since.unwrap_or(i64::MIN), ts],
            |r| r.get(0),
        )?;
        let expected = expected as u64;
        let counted = counted.unwrap_or(expected);
        tx.execute(
            "INSERT INTO collections (ts, expected, counted, note) VALUES (?1, ?2, ?3, ?4)",
            params![ts, expected as i64, counted as i64, note],
        )?;
        tx.commit()?;
        Ok(Collection { ts, expected, counted, note: note.to_string() })
    }

    /// Sample data for a dev vendo (`pisowifi --dev demo-data`).
    pub fn demo_sale(&self, ts: i64, mac: Option<Mac>, pesos: u32, secs: u64, source: &str) -> Result<()> {
        let c = self.conn.lock().unwrap();
        c.execute(
            "INSERT INTO sales (ts, mac, pesos, secs, source) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![ts, mac.map(|m| m.to_string()), pesos, secs as i64, source],
        )?;
        Ok(())
    }

    pub fn demo_collection(&self, ts: i64, expected: u64, counted: u64) -> Result<()> {
        let c = self.conn.lock().unwrap();
        c.execute(
            "INSERT INTO collections (ts, expected, counted, note) VALUES (?1, ?2, ?3, '')",
            params![ts, expected as i64, counted as i64],
        )?;
        Ok(())
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
    fn coin_box_counts_cash_since_it_was_emptied() {
        let (_dir, db) = test_db();
        let mac = Mac::parse("da:a1:19:00:11:22").unwrap();
        let t = now();
        db.demo_sale(t - 100, Some(mac), 5, 7200, "coin").unwrap();
        db.demo_sale(t - 90, None, 1, 0, "unclaimed").unwrap();
        db.demo_sale(t - 80, Some(mac), 0, 7200, "voucher").unwrap();
        let b = db.coinbox().unwrap();
        assert_eq!((b.since, b.pesos, b.count), (None, 6, 2), "coins and unclaimed coins are cash; vouchers aren't");
        let c = db.collect(Some(4), "two pesos short").unwrap();
        assert_eq!((c.expected, c.counted), (6, 4));
        let b = db.coinbox().unwrap();
        assert_eq!((b.pesos, b.history.len(), b.history[0].note.as_str()), (0, 1, "two pesos short"));
        assert!(b.since.is_some());
        db.demo_sale(t - 70, Some(mac), 10, 18000, "coin").unwrap();
        assert_eq!(db.coinbox().unwrap().pesos, 0, "a sale from before it was emptied isn't in the box");
        let c = db.collect(None, "").unwrap();
        assert_eq!((c.expected, c.counted), (0, 0), "not counted: the expected amount");
    }

    #[test]
    fn report_groups_by_day_source_and_hour() {
        let (_dir, db) = test_db();
        let mac = Mac::parse("da:a1:19:00:11:22").unwrap();
        let other = Mac::parse("da:a1:19:00:11:33").unwrap();
        let t = now();
        db.demo_sale(t, Some(mac), 5, 7200, "coin").unwrap();
        db.demo_sale(t, Some(other), 20, 43200, "coin").unwrap();
        db.demo_sale(t, Some(mac), 0, 3600, "voucher").unwrap();
        db.demo_sale(t, None, 3, 0, "unclaimed").unwrap();
        db.demo_sale(t - 40 * 86400, Some(mac), 10, 18000, "coin").unwrap();
        let r = db.report(30).unwrap();
        assert_eq!((r.group, r.buckets.len()), ("day", 30));
        assert_eq!((r.pesos, r.count, r.customers, r.prev_pesos), (25, 3, 2, 10));
        assert_eq!(r.buckets.last().unwrap().pesos, 25);
        assert_eq!(r.heat.iter().flatten().sum::<u64>(), 25);
        let coins = r.sources.iter().find(|s| s.source == "coin").unwrap();
        assert_eq!((coins.pesos, coins.count), (25, 2));
        assert!(r.sources.iter().any(|s| s.source == "unclaimed" && s.pesos == 3));
        assert_eq!(r.best_ever.unwrap().pesos, 25);
        let y = db.report(365).unwrap();
        assert_eq!((y.group, y.buckets.len(), y.pesos), ("month", 12, 35));
        let spend = db.device_spend().unwrap();
        assert_eq!(spend[&mac.to_string()].pesos, 15);
        assert_eq!(spend[&mac.to_string()].count, 3);
    }

    #[test]
    fn devices_are_saved_and_unnamed_old_ones_forgotten() {
        let (_dir, db) = test_db();
        let row = |mac: &str, nickname: &str, last_seen: i64| DeviceRow {
            mac: Mac::parse(mac).unwrap(),
            nickname: nickname.into(),
            hostname: String::new(),
            user_agent: "UA".into(),
            first_seen: 1,
            last_seen,
        };
        db.save_device(&row("da:a1:19:00:00:01", "", 10)).unwrap();
        db.save_device(&row("da:a1:19:00:00:02", "Kuya Ben", 10)).unwrap();
        db.save_device(&row("da:a1:19:00:00:03", "", 1000)).unwrap();
        let mut updated = row("da:a1:19:00:00:03", "Ate", 2000);
        updated.first_seen = 999;
        db.save_device(&updated).unwrap();
        db.forget_devices(100).unwrap();
        let mut left = db.load_devices().unwrap();
        left.sort_by_key(|d| d.mac);
        assert_eq!(left.len(), 2);
        assert_eq!(left[0].nickname, "Kuya Ben");
        assert_eq!((left[1].nickname.as_str(), left[1].first_seen, left[1].last_seen), ("Ate", 1, 2000));
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
    fn pass_links_are_dropped_when_the_hub_enrolment_changes() {
        let (_dir, db) = test_db();
        let mac = Mac::parse("02:11:22:33:44:55").unwrap();
        let fill = |db: &Db| {
            db.save(&Batch {
                links: vec![(mac, "p_1".into())],
                ledger: vec![Ledger::PassCredit { pass_id: "p_1".into(), pesos: 5, secs: 600 }],
                ..Default::default()
            })
            .unwrap();
        };
        let held = |db: &Db| (db.load_links().unwrap().len(), db.outbox_list(10).unwrap().len());
        // Links made before the owner was recorded belong to the current hub.
        fill(&db);
        assert!(!db.reset_pass_state_if_hub_changed(Some("v_a")).unwrap());
        assert_eq!(held(&db), (1, 1));
        assert!(!db.reset_pass_state_if_hub_changed(Some("v_a")).unwrap(), "same hub: kept");
        assert_eq!(held(&db), (1, 1));
        // join-hub as a new vendo: the old service saved its links on the way down.
        assert!(db.reset_pass_state_if_hub_changed(Some("v_b")).unwrap());
        assert_eq!(held(&db), (0, 0));
        // leave-hub.
        fill(&db);
        assert!(db.reset_pass_state_if_hub_changed(None).unwrap());
        assert_eq!(held(&db), (0, 0));
        assert!(!db.reset_pass_state_if_hub_changed(None).unwrap(), "no hub, nothing to do");
    }

    #[test]
    fn secrets_are_written_once() {
        let (_dir, db) = test_db();
        assert!(db.get_secret("k").unwrap().is_none());
        assert_eq!(*db.put_secret_once("k", b"first").unwrap(), b"first");
        assert_eq!(*db.put_secret_once("k", b"second").unwrap(), b"first", "never overwritten");
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
        db.put_secret_once("hub_signing_key", &[3u8; 32]).unwrap();
        let bk = dir.0.join("backup.db");
        db.export_backup(&bk, "correct horse").unwrap();
        let key2 = DbKey(Zeroizing::new([9u8; 32]));
        assert!(import_backup(&bk, "wrong", &dir.0.join("x.db"), &key2).is_err());
        import_backup(&bk, "correct horse", &dir.0.join("new.db"), &key2).unwrap();
        let db2 = Db::open(&dir.0.join("new.db"), &key2).unwrap();
        assert_eq!(db2.rates().unwrap(), vec![Rate { pesos: 5, minutes: 99 }]);
        // A replacement board keeps its hub identity.
        assert_eq!(*db2.get_secret("hub_signing_key").unwrap().unwrap(), [3u8; 32]);
    }
}
