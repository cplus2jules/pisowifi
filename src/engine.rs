//! The vending logic: who owns the coin slot, how much time each device has,
//! pause/resume, vouchers. All state changes go through one lock, and the
//! firewall is told about every change.

use crate::db::{ClientRow, Db, Rate};
use crate::hub::proto::LeaseReport;
use crate::firewall::Firewall;
use crate::gpio::Enable;
use crate::net::Mac;
use crate::rates;
use serde::Serialize;
use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const VOUCHER_MAX_FAILS: u32 = 5;
const VOUCHER_LOCKOUT: Duration = Duration::from_secs(10 * 60);
/// A coin finishing within this long after its slot closed still counts
/// for that customer (they tapped "Start internet" as it dropped).
const LATE_COIN: Duration = Duration::from_secs(8);
/// After a reservation expires with no coins, that phone waits this long
/// before reserving again, so one phone can't hog the slot.
const EMPTY_SLOT_COOLDOWN: Duration = Duration::from_secs(20);
/// Longest grant accepted from anywhere (owner, voucher, hub): guards the clock maths.
pub const MAX_GRANT_SECS: u64 = 400 * 86400;

pub struct Engine {
    state: Mutex<State>,
    db: Arc<Db>,
    fw: Firewall,
    enable: Option<(Enable, u32)>,
    pesos_per_pulse: u32,
    window: Duration,
    started: Instant,
    /// The GPIO line is being read. While it isn't, coins are refused.
    reader_ok: Arc<AtomicBool>,
}

struct State {
    clients: HashMap<Mac, Client>,
    slot: Option<Slot>,
    rates: Vec<Rate>,
    voucher_fails: HashMap<Mac, (u32, Instant)>,
    pulses_total: u64,
    /// Piso Pass held by each phone at this vendo (time leased from the hub).
    links: HashMap<Mac, String>,
    /// Pulses have arrived and the coin isn't complete yet.
    counting: bool,
    /// The slot that closed most recently, for a coin still dropping then.
    last_slot: Option<(Slot, Instant)>,
    /// Phones whose reservation just ran out with no coins.
    cooldown: HashMap<Mac, Instant>,
}

struct Client {
    ip: Ipv4Addr,
    /// Set while the clock is running.
    until: Option<Instant>,
    /// Time banked while paused.
    banked: Duration,
}

impl Client {
    fn remaining(&self, now: Instant) -> Duration {
        match self.until {
            Some(u) => u.saturating_duration_since(now),
            None => self.banked,
        }
    }
}

#[derive(Clone)]
struct Slot {
    mac: Mac,
    ip: Ipv4Addr,
    pesos: u32,
    /// Each coin's value, in the order it was inserted.
    coins: Vec<u32>,
    deadline: Instant,
}

#[derive(Serialize)]
pub struct SlotView {
    /// "idle", "mine" (this device is inserting) or "busy" (someone else is).
    pub state: &'static str,
    pub pesos: u32,
    pub credit_secs: u64,
    pub secs_left: u64,
    pub window_secs: u64,
    pub coins: Vec<u32>,
}

#[derive(Serialize)]
pub struct MeView {
    pub device: String,
    pub ip: String,
    pub remaining_secs: u64,
    pub running: bool,
    pub slot: SlotView,
    pub rates: Vec<Rate>,
}

#[derive(Serialize)]
pub struct ClientView {
    pub mac: String,
    pub ip: String,
    pub remaining_secs: u64,
    pub running: bool,
}

#[derive(Serialize)]
pub struct SlotAdminView {
    pub state: &'static str,
    pub mac: Option<String>,
    pub pesos: u32,
    pub secs_left: u64,
    pub pulses_total: u64,
    pub gated: bool,
    pub window_secs: u64,
    /// The coin line is being read (false: coins are refused, check the log).
    pub reader_ok: bool,
}

pub enum CoinStart {
    Ok,
    Busy(u64),
    /// The coin reader isn't working: taking coins now would lose them.
    Offline,
    /// This phone's last reservation expired empty; try again in N s.
    Wait(u64),
}

impl Engine {
    pub fn new(
        db: Arc<Db>,
        fw: Firewall,
        enable: Option<(Enable, u32)>,
        pesos_per_pulse: u32,
        window_secs: u64,
        reader_ok: Arc<AtomicBool>,
    ) -> anyhow::Result<Arc<Engine>> {
        let now = Instant::now();
        let mut clients = HashMap::new();
        for row in db.load_clients()? {
            let left = Duration::from_secs(row.remaining_secs);
            let c = if row.paused {
                Client { ip: row.ip, until: None, banked: left }
            } else {
                // Downtime isn't charged: the clock resumes where it stopped.
                fw.allow(row.mac, row.ip, row.remaining_secs);
                Client { ip: row.ip, until: Some(now + left), banked: Duration::ZERO }
            };
            clients.insert(row.mac, c);
        }
        tracing::info!("restored {} customer sessions", clients.len());
        let rates = db.rates()?;
        let links = db.load_links()?.into_iter().collect();
        Ok(Arc::new(Engine {
            state: Mutex::new(State {
                clients,
                slot: None,
                rates,
                voucher_fails: HashMap::new(),
                pulses_total: 0,
                links,
                counting: false,
                last_slot: None,
                cooldown: HashMap::new(),
            }),
            db,
            fw,
            enable,
            pesos_per_pulse,
            window: Duration::from_secs(window_secs),
            started: now,
            reader_ok,
        }))
    }

    pub fn uptime_secs(&self) -> u64 {
        self.started.elapsed().as_secs()
    }

    // ---- customer actions ----------------------------------------------

    pub fn me(&self, mac: Mac, ip: Ipv4Addr) -> MeView {
        let now = Instant::now();
        let mut st = self.state.lock().unwrap();
        self.rebind(&mut st, mac, ip, now);
        let (remaining, running) = st
            .clients
            .get(&mac)
            .map(|c| (c.remaining(now).as_secs(), c.until.is_some()))
            .unwrap_or((0, false));
        let slot = match &st.slot {
            None => SlotView {
                state: "idle",
                pesos: 0,
                credit_secs: 0,
                secs_left: 0,
                window_secs: self.window.as_secs(),
                coins: vec![],
            },
            Some(s) => SlotView {
                state: if s.mac == mac { "mine" } else { "busy" },
                pesos: if s.mac == mac { s.pesos } else { 0 },
                credit_secs: if s.mac == mac { rates::secs_for(s.pesos, &st.rates) } else { 0 },
                secs_left: s.deadline.saturating_duration_since(now).as_secs(),
                window_secs: self.window.as_secs(),
                coins: if s.mac == mac { s.coins.clone() } else { vec![] },
            },
        };
        MeView {
            device: mac.masked(),
            ip: ip.to_string(),
            remaining_secs: remaining,
            running,
            slot,
            rates: st.rates.clone(),
        }
    }

    /// DHCP may hand a device a new IP; move its grant along with it.
    fn rebind(&self, st: &mut State, mac: Mac, ip: Ipv4Addr, now: Instant) {
        if let Some(c) = st.clients.get_mut(&mac) {
            if c.ip != ip {
                if let Some(u) = c.until {
                    self.fw.revoke(mac, c.ip);
                    self.fw.allow(mac, ip, u.saturating_duration_since(now).as_secs());
                }
                c.ip = ip;
            }
        }
    }

    pub fn coin_start(&self, mac: Mac, ip: Ipv4Addr) -> CoinStart {
        let now = Instant::now();
        let mut st = self.state.lock().unwrap();
        if let Some(s) = &st.slot {
            if s.mac != mac {
                return CoinStart::Busy(s.deadline.saturating_duration_since(now).as_secs());
            }
            // Already yours. Only coins extend the window, so a phone can't
            // hold the slot forever by tapping again.
            return CoinStart::Ok;
        }
        if !self.reader_ok.load(Ordering::Relaxed) {
            return CoinStart::Offline;
        }
        if let Some(until) = st.cooldown.get(&mac).map(|t| *t + EMPTY_SLOT_COOLDOWN).filter(|t| *t > now) {
            return CoinStart::Wait((until - now).as_secs() + 1);
        }
        st.slot = Some(Slot { mac, ip, pesos: 0, coins: vec![], deadline: now + self.window });
        self.set_acceptor(true);
        CoinStart::Ok
    }

    /// A coin is still being counted (pulses arrived, burst not over yet).
    pub fn counting(&self) -> bool {
        self.state.lock().unwrap().counting
    }

    /// Ends this phone's coin session; returns (pesos, seconds) credited.
    /// Callers should wait while `counting()` so a coin mid-count isn't cut off.
    pub fn coin_done(&self, mac: Mac) -> Option<(u32, u64)> {
        let mut st = self.state.lock().unwrap();
        if st.slot.as_ref().is_some_and(|s| s.mac == mac) {
            let slot = st.slot.take().unwrap();
            return self.finish_slot(&mut st, slot);
        }
        None
    }

    /// A raw pulse from the acceptor: a coin is being counted.
    pub fn pulse(&self) {
        self.state.lock().unwrap().counting = true;
    }

    /// One coin, decoded from a burst of `pulses` pulses.
    pub fn on_coin(&self, pesos: u32, pulses: u32) {
        let now = Instant::now();
        let mut st = self.state.lock().unwrap();
        st.counting = false;
        st.pulses_total += pulses as u64;
        if ![1, 5, 10, 20].contains(&pesos) {
            // Not a real coin's worth: two coins merged, a misread, or tampering.
            tracing::warn!(pesos, pulses, "unusual pulse burst");
            self.db.audit("coin", "unusual_burst", &format!("{pulses} pulses = ₱{pesos}"));
        }
        if let Some(s) = &mut st.slot {
            s.pesos += pesos;
            s.coins.push(pesos);
            s.deadline = now + self.window;
            return;
        }
        // The customer tapped "Start internet" (or time ran out) as this coin
        // dropped: it's still theirs, priced as part of what they paid.
        if let Some((prev, closed)) = st.last_slot.take().filter(|(_, t)| now - *t < LATE_COIN) {
            let before = rates::secs_for(prev.pesos, &st.rates);
            let total = prev.pesos + pesos;
            let secs = rates::secs_for(total, &st.rates).saturating_sub(before);
            tracing::info!(mac = %prev.mac, pesos, secs, "late coin credited");
            if let Err(e) = self.db.add_sale(Some(prev.mac), pesos, secs, "coin") {
                tracing::error!("recording sale: {e:#}");
            }
            if let Some(pass) = st.links.get(&prev.mac) {
                if let Err(e) = self.db.outbox_add(pass, pesos, secs) {
                    tracing::error!("queueing pass credit: {e:#}");
                }
            }
            self.add_time_locked(&mut st, prev.mac, prev.ip, secs);
            st.last_slot = Some((Slot { pesos: total, ..prev }, closed));
            return;
        }
        // Acceptor isn't gated and nobody tapped "Insert coin".
        tracing::warn!(pesos, "coin received with no customer waiting");
        let _ = self.db.add_sale(None, pesos, 0, "unclaimed");
    }

    /// On shutdown: credit coins already inserted instead of dropping them.
    pub fn finish_open_slot(&self) {
        let mut st = self.state.lock().unwrap();
        if let Some(slot) = st.slot.take() {
            self.finish_slot(&mut st, slot);
        }
    }

    pub fn pause(&self, mac: Mac) -> bool {
        let now = Instant::now();
        let mut st = self.state.lock().unwrap();
        let paused = match st.clients.get_mut(&mac) {
            Some(c) if c.until.is_some() => {
                c.banked = c.remaining(now);
                c.until = None;
                self.fw.revoke(mac, c.ip);
                true
            }
            _ => false,
        };
        if paused {
            // Saved now: a power cut must not bring it back as running.
            self.persist_locked(&st, now);
        }
        paused
    }

    pub fn resume(&self, mac: Mac, ip: Ipv4Addr) -> bool {
        let now = Instant::now();
        let mut st = self.state.lock().unwrap();
        self.rebind(&mut st, mac, ip, now);
        let resumed = match st.clients.get_mut(&mac) {
            Some(c) if c.until.is_none() && !c.banked.is_zero() => {
                c.until = Some(now + c.banked);
                self.fw.allow(mac, c.ip, c.banked.as_secs());
                c.banked = Duration::ZERO;
                true
            }
            _ => false,
        };
        if resumed {
            self.persist_locked(&st, now);
        }
        resumed
    }

    /// Wrong voucher or pass codes lock a phone out for a while.
    pub fn code_attempt_allowed(&self, mac: Mac) -> Result<(), &'static str> {
        let mut st = self.state.lock().unwrap();
        if let Some((n, since)) = st.voucher_fails.get(&mac).copied() {
            if since.elapsed() > VOUCHER_LOCKOUT {
                st.voucher_fails.remove(&mac);
            } else if n >= VOUCHER_MAX_FAILS {
                return Err("Too many wrong codes. Try again in 10 minutes.");
            }
        }
        Ok(())
    }

    pub fn record_code_fail(&self, mac: Mac) {
        let mut st = self.state.lock().unwrap();
        st.voucher_fails.entry(mac).or_insert((0, Instant::now())).0 += 1;
    }

    pub fn redeem_voucher(&self, mac: Mac, ip: Ipv4Addr, code: &str) -> Result<u64, &'static str> {
        self.code_attempt_allowed(mac)?;
        match self.db.redeem_voucher(code, mac) {
            Ok(Some(secs)) => {
                let _ = self.db.add_sale(Some(mac), 0, secs, "voucher");
                let mut st = self.state.lock().unwrap();
                st.voucher_fails.remove(&mac);
                self.add_time_locked(&mut st, mac, ip, secs);
                Ok(secs)
            }
            Ok(None) => {
                self.record_code_fail(mac);
                Err("That code is invalid or already used.")
            }
            Err(e) => {
                tracing::error!("voucher: {e:#}");
                Err("Something went wrong. Please try again.")
            }
        }
    }

    // ---- owner actions --------------------------------------------------

    pub fn admin_add_time(&self, mac: Mac, secs: u64) -> bool {
        let mut st = self.state.lock().unwrap();
        let Some(ip) = st.clients.get(&mac).map(|c| c.ip) else { return false };
        self.add_time_locked(&mut st, mac, ip, secs);
        true
    }

    pub fn kick(&self, mac: Mac) -> bool {
        let mut st = self.state.lock().unwrap();
        match st.clients.remove(&mac) {
            Some(c) => {
                if c.until.is_some() {
                    self.fw.revoke(mac, c.ip);
                }
                true
            }
            None => false,
        }
    }

    pub fn test_coin(&self, pesos: u32) {
        self.on_coin(pesos, (pesos / self.pesos_per_pulse).max(1));
    }

    pub fn set_rates(&self, rates: Vec<Rate>) {
        self.state.lock().unwrap().rates = rates;
    }

    pub fn clients(&self) -> Vec<ClientView> {
        let now = Instant::now();
        let st = self.state.lock().unwrap();
        let mut v: Vec<_> = st
            .clients
            .iter()
            .map(|(m, c)| ClientView {
                mac: m.to_string(),
                ip: c.ip.to_string(),
                remaining_secs: c.remaining(now).as_secs(),
                running: c.until.is_some(),
            })
            .collect();
        v.sort_by(|a, b| b.running.cmp(&a.running).then(b.remaining_secs.cmp(&a.remaining_secs)));
        v
    }

    pub fn slot_admin(&self) -> SlotAdminView {
        let now = Instant::now();
        let st = self.state.lock().unwrap();
        SlotAdminView {
            state: if st.slot.is_some() { "inserting" } else { "idle" },
            mac: st.slot.as_ref().map(|s| s.mac.to_string()),
            pesos: st.slot.as_ref().map(|s| s.pesos).unwrap_or(0),
            secs_left: st.slot.as_ref().map(|s| s.deadline.saturating_duration_since(now).as_secs()).unwrap_or(0),
            pulses_total: st.pulses_total,
            gated: self.enable.is_some(),
            window_secs: self.window.as_secs(),
            reader_ok: self.reader_ok.load(Ordering::Relaxed),
        }
    }

    // ---- housekeeping ---------------------------------------------------

    /// Called every second: expires sessions and abandoned coin slots.
    pub fn tick(&self) {
        let now = Instant::now();
        let mut st = self.state.lock().unwrap();
        let expired: Vec<Mac> = st
            .clients
            .iter()
            .filter(|(_, c)| c.until.is_some_and(|u| u <= now))
            .map(|(m, _)| *m)
            .collect();
        for mac in expired {
            if let Some(c) = st.clients.remove(&mac) {
                tracing::info!(%mac, "time is up");
                self.fw.revoke(mac, c.ip);
            }
        }
        // Never close the slot in the middle of counting a coin.
        if !st.counting && st.slot.as_ref().is_some_and(|s| s.deadline <= now) {
            let slot = st.slot.take().unwrap();
            if slot.pesos == 0 {
                st.cooldown.insert(slot.mac, now);
            }
            self.finish_slot(&mut st, slot);
        }
        st.cooldown.retain(|_, t| now - *t < EMPTY_SLOT_COOLDOWN);
        st.voucher_fails.retain(|_, (_, t)| t.elapsed() < VOUCHER_LOCKOUT);
    }

    pub fn snapshot(&self) -> Vec<ClientRow> {
        let now = Instant::now();
        let st = self.state.lock().unwrap();
        st.clients
            .iter()
            .map(|(m, c)| ClientRow {
                mac: *m,
                ip: c.ip,
                remaining_secs: c.remaining(now).as_secs(),
                paused: c.until.is_none(),
            })
            .collect()
    }

    pub fn persist(&self) {
        let st = self.state.lock().unwrap();
        self.persist_locked(&st, Instant::now());
    }

    // ---- Piso Pass ------------------------------------------------------

    pub fn pass_of(&self, mac: Mac) -> Option<String> {
        self.state.lock().unwrap().links.get(&mac).cloned()
    }

    pub fn has_time(&self, mac: Mac) -> bool {
        self.state.lock().unwrap().clients.get(&mac).is_some_and(|c| !c.remaining(Instant::now()).is_zero())
    }

    pub fn holds_pass_time(&self, mac: Mac) -> bool {
        let st = self.state.lock().unwrap();
        st.links.contains_key(&mac) && st.clients.get(&mac).is_some_and(|c| !c.remaining(Instant::now()).is_zero())
    }

    pub fn link_pass(&self, mac: Mac, pass_id: String) {
        let mut st = self.state.lock().unwrap();
        st.links.insert(mac, pass_id);
        self.persist_locked(&st, Instant::now());
    }

    /// Puts a pass's time (just leased from the hub) on this phone. The hub's
    /// figure already includes any time this vendo held for the pass, so
    /// that is replaced, never added twice. Unlinked local time is merged in.
    pub fn claim_into(&self, mac: Mac, ip: Ipv4Addr, pass_id: String, secs: u64) -> u64 {
        let now = Instant::now();
        let mut st = self.state.lock().unwrap();
        let extra = match st.links.get(&mac) {
            None => st.clients.get(&mac).map(|c| c.remaining(now).as_secs()).unwrap_or(0),
            Some(_) => 0,
        };
        // Drop every local session tied to this pass (incl. an older MAC of the same phone).
        let holders: Vec<Mac> = st.links.iter().filter(|(_, p)| **p == pass_id).map(|(m, _)| *m).chain([mac]).collect();
        for m in holders {
            st.links.remove(&m);
            if let Some(c) = st.clients.remove(&m) {
                if c.until.is_some() {
                    self.fw.revoke(m, c.ip);
                }
            }
        }
        st.links.insert(mac, pass_id);
        let total = extra + secs;
        if total > 0 {
            self.add_time_locked(&mut st, mac, ip, total);
        } else {
            self.persist_locked(&st, now);
        }
        total
    }

    /// Heartbeat payload: what each pass held here has left.
    pub fn lease_reports(&self) -> Vec<LeaseReport> {
        let now = Instant::now();
        let st = self.state.lock().unwrap();
        st.links
            .iter()
            .map(|(mac, pass)| {
                let c = st.clients.get(mac);
                LeaseReport {
                    pass_id: pass.clone(),
                    remaining_secs: c.map(|c| c.remaining(now).as_secs()).unwrap_or(0),
                    running: c.is_some_and(|c| c.until.is_some()),
                }
            })
            .collect()
    }

    /// The customer took their pass to another location: cut this phone off.
    pub fn revoke_passes(&self, ids: &[String]) {
        if ids.is_empty() {
            return;
        }
        let mut st = self.state.lock().unwrap();
        let macs: Vec<Mac> = st.links.iter().filter(|(_, p)| ids.contains(p)).map(|(m, _)| *m).collect();
        for m in macs {
            st.links.remove(&m);
            if let Some(c) = st.clients.remove(&m) {
                tracing::info!(mac = %m, "pass moved to another location");
                if c.until.is_some() {
                    self.fw.revoke(m, c.ip);
                }
            }
        }
        self.persist_locked(&st, Instant::now());
    }

    // ---- internals ------------------------------------------------------

    fn finish_slot(&self, st: &mut State, slot: Slot) -> Option<(u32, u64)> {
        self.set_acceptor(false);
        if slot.pesos == 0 {
            return None;
        }
        st.last_slot = Some((Slot { coins: vec![], ..slot.clone() }, Instant::now()));
        let secs = rates::secs_for(slot.pesos, &st.rates);
        tracing::info!(mac = %slot.mac, pesos = slot.pesos, secs, "coins credited");
        if let Err(e) = self.db.add_sale(Some(slot.mac), slot.pesos, secs, "coin") {
            tracing::error!("recording sale: {e:#}");
        }
        if let Some(pass) = st.links.get(&slot.mac) {
            if let Err(e) = self.db.outbox_add(pass, slot.pesos, secs) {
                tracing::error!("queueing pass credit: {e:#}");
            }
        }
        self.add_time_locked(st, slot.mac, slot.ip, secs);
        Some((slot.pesos, secs))
    }

    fn add_time_locked(&self, st: &mut State, mac: Mac, ip: Ipv4Addr, secs: u64) {
        let now = Instant::now();
        let add = Duration::from_secs(secs.min(MAX_GRANT_SECS));
        let c = st.clients.entry(mac).or_insert(Client { ip, until: None, banked: Duration::ZERO });
        // Paying (or a voucher) always starts the clock.
        let total = (c.remaining(now) + add).min(Duration::from_secs(MAX_GRANT_SECS));
        c.until = Some(now + total);
        c.banked = Duration::ZERO;
        c.ip = ip;
        self.fw.allow(mac, ip, total.as_secs());
        self.persist_locked(st, now);
    }

    fn persist_locked(&self, st: &State, now: Instant) {
        let rows: Vec<ClientRow> = st
            .clients
            .iter()
            .map(|(m, c)| ClientRow {
                mac: *m,
                ip: c.ip,
                remaining_secs: c.remaining(now).as_secs(),
                paused: c.until.is_none(),
            })
            .collect();
        if let Err(e) = self.db.save_clients(&rows) {
            tracing::error!("saving sessions: {e:#}");
        }
        let links: Vec<(Mac, String)> = st.links.iter().map(|(m, p)| (*m, p.clone())).collect();
        if let Err(e) = self.db.save_links(&links) {
            tracing::error!("saving pass links: {e:#}");
        }
    }

    fn set_acceptor(&self, on: bool) {
        if let Some((en, line)) = &self.enable {
            en.set(on, *line);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::DbKey;
    use zeroize::Zeroizing;

    async fn engine(reader_ok: bool) -> (std::path::PathBuf, Arc<Engine>) {
        let dir = std::env::temp_dir().join(format!("pisowifi-engine-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = Arc::new(Db::open(&dir.join("t.db"), &DbKey(Zeroizing::new([7u8; 32]))).unwrap());
        let fw = Firewall::start(&crate::config::Config::dev(dir.clone()), true).await.unwrap();
        let e = Engine::new(db, fw, None, 1, 60, Arc::new(AtomicBool::new(reader_ok))).unwrap();
        (dir, e)
    }

    const PHONE: &str = "02:11:22:33:44:55";
    const IP: Ipv4Addr = Ipv4Addr::new(10, 0, 0, 9);

    fn remaining(e: &Engine, mac: Mac) -> u64 {
        e.me(mac, IP).remaining_secs
    }

    #[tokio::test]
    async fn a_coin_still_dropping_after_start_internet_is_credited() {
        let (dir, e) = engine(true).await;
        let mac = Mac::parse(PHONE).unwrap();
        let rates = e.state.lock().unwrap().rates.clone();
        assert!(matches!(e.coin_start(mac, IP), CoinStart::Ok));
        e.pulse();
        e.on_coin(5, 5);
        assert_eq!(e.coin_done(mac), Some((5, rates::secs_for(5, &rates))));
        // The ₱20 finishes counting just after "Start internet": still theirs,
        // priced as ₱25 in total.
        e.pulse();
        assert!(e.counting());
        e.on_coin(20, 20);
        assert!(!e.counting());
        // (The clock is running, so allow a second.)
        assert!(rates::secs_for(25, &rates) - remaining(&e, mac) <= 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn the_slot_owner_cant_extend_it_by_tapping_again() {
        let (dir, e) = engine(true).await;
        let (a, b) = (Mac::parse(PHONE).unwrap(), Mac::parse("02:00:00:00:00:02").unwrap());
        assert!(matches!(e.coin_start(a, IP), CoinStart::Ok));
        let deadline = e.state.lock().unwrap().slot.as_ref().unwrap().deadline;
        std::thread::sleep(Duration::from_millis(20));
        assert!(matches!(e.coin_start(a, IP), CoinStart::Ok));
        assert_eq!(e.state.lock().unwrap().slot.as_ref().unwrap().deadline, deadline);
        assert!(matches!(e.coin_start(b, IP), CoinStart::Busy(_)));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn coins_are_refused_while_the_reader_is_down() {
        let (dir, e) = engine(false).await;
        assert!(matches!(e.coin_start(Mac::parse(PHONE).unwrap(), IP), CoinStart::Offline));
        assert!(!e.slot_admin().reader_ok);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn pausing_is_saved_straight_away() {
        let (dir, e) = engine(true).await;
        let mac = Mac::parse(PHONE).unwrap();
        e.coin_start(mac, IP);
        e.on_coin(5, 5);
        e.coin_done(mac);
        assert!(e.pause(mac));
        let row = e.db.load_clients().unwrap().into_iter().find(|r| r.mac == mac).unwrap();
        assert!(row.paused, "a power cut now must not bring it back as running");
        let _ = std::fs::remove_dir_all(dir);
    }
}
