//! The vending logic: who owns the coin slot, how much time each device has,
//! pause/resume, vouchers. All state changes go through one lock, and the
//! firewall is told about every change.

use crate::db::{ClientRow, Db, Rate};
use crate::hub::proto::{CreditReport, LeaseReport};
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
/// Paused Piso Pass time only restarts if the hub was heard from this
/// recently: while out of touch, this vendo wouldn't know the pass had been
/// claimed at another location. Shorter than any trip between two vendos.
pub const PASS_TRUST: Duration = Duration::from_secs(120);

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
    /// Last successful hub sync (Piso Pass).
    last_sync: Option<Instant>,
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

pub enum Resume {
    Resumed,
    Nothing,
    /// Paused Piso Pass time, and the hub hasn't been heard from lately.
    NeedsHub,
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
                last_sync: None,
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
            self.grant_locked(&mut st, prev.mac, prev.ip, pesos, secs);
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

    pub fn resume(&self, mac: Mac, ip: Ipv4Addr) -> Resume {
        let now = Instant::now();
        let mut st = self.state.lock().unwrap();
        self.rebind(&mut st, mac, ip, now);
        let untrusted = st.links.contains_key(&mac) && !Self::pass_trusted(&st);
        match st.clients.get_mut(&mac) {
            Some(c) if c.until.is_none() && !c.banked.is_zero() => {
                if untrusted {
                    return Resume::NeedsHub;
                }
                c.until = Some(now + c.banked);
                self.fw.allow(mac, c.ip, c.banked.as_secs());
                c.banked = Duration::ZERO;
                self.persist_locked(&st, now);
                Resume::Resumed
            }
            _ => Resume::Nothing,
        }
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
                self.grant_locked(&mut st, mac, ip, 0, secs);
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
        self.grant_locked(&mut st, mac, ip, 0, secs);
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

    /// Puts this phone's time here on a pass just created on the hub, and
    /// reports that time as sold for it (`pesos`: coins paid just now).
    pub fn link_pass(&self, mac: Mac, pass_id: String, pesos: u32) {
        let mut st = self.state.lock().unwrap();
        let secs = st.clients.get(&mac).map(|c| c.remaining(Instant::now()).as_secs()).unwrap_or(0);
        self.queue_credit(&pass_id, pesos, secs);
        st.links.insert(mac, pass_id);
        self.persist_locked(&st, Instant::now());
    }

    /// Puts a pass's time (just leased from the hub) on this phone. The hub's
    /// figure already includes any time this vendo held for the pass, so
    /// that is replaced, never added twice. Unlinked local time is merged in
    /// and reported as sold for the pass (`pesos`: coins paid just now).
    ///
    /// The clock runs if `run` (the customer asked, or the time was running
    /// where it came from) or if this phone's own time here was running;
    /// otherwise it stays paused, so a phone that joins the WiFi in a pocket
    /// doesn't burn paused time. Returns (seconds, running).
    pub fn claim_into(&self, mac: Mac, ip: Ipv4Addr, pass_id: String, secs: u64, run: bool, pesos: u32) -> (u64, bool) {
        let now = Instant::now();
        let mut st = self.state.lock().unwrap();
        let (extra, local_running) = match st.links.get(&mac) {
            None => st.clients.get(&mac).map(|c| (c.remaining(now).as_secs(), c.until.is_some())).unwrap_or((0, false)),
            Some(_) => (0, false),
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
        self.queue_credit(&pass_id, pesos, extra);
        st.links.insert(mac, pass_id);
        let total = extra.saturating_add(secs).min(MAX_GRANT_SECS);
        let run = total > 0 && (run || local_running);
        if total > 0 {
            self.set_time_locked(&mut st, mac, ip, total, run);
        } else {
            self.persist_locked(&st, now);
        }
        (total, run)
    }

    /// What to send the hub: each pass held here with its time, and sales
    /// not yet reported. Taken together under the lock, so the credits sent
    /// cover every second the reports include.
    pub fn sync_snapshot(&self, max_credits: u32) -> (Vec<LeaseReport>, Vec<CreditReport>) {
        let now = Instant::now();
        let st = self.state.lock().unwrap();
        let leases = st
            .links
            .iter()
            .map(|(mac, pass)| {
                let c = st.clients.get(mac);
                LeaseReport {
                    pass_id: pass.clone(),
                    remaining_secs: c.map(|c| c.remaining(now).as_secs()).unwrap_or(0),
                    running: c.is_some_and(|c| c.until.is_some()),
                }
            })
            .collect();
        let credits = self.db.outbox_list(max_credits).unwrap_or_else(|e| {
            tracing::error!("reading pass credits: {e:#}");
            vec![]
        });
        (leases, credits)
    }

    /// After the hub accepted `sent`: cut off phones whose pass was claimed
    /// at another location, and forget passes whose time here had run out
    /// (reported as zero) so links don't pile up. A customer who comes back
    /// is linked again by claiming their pass.
    pub fn after_sync(&self, sent: &[LeaseReport], revoked: &[String]) {
        let now = Instant::now();
        let mut st = self.state.lock().unwrap();
        st.last_sync = Some(now);
        let revoked: std::collections::HashSet<&String> = revoked.iter().collect();
        let spent: std::collections::HashSet<&String> = sent.iter().filter(|r| r.remaining_secs == 0).map(|r| &r.pass_id).collect();
        let mut changed = false;
        let drop: Vec<(Mac, bool)> = st
            .links
            .iter()
            .filter_map(|(m, p)| {
                let has_time = st.clients.get(m).is_some_and(|c| !c.remaining(now).is_zero());
                if revoked.contains(p) {
                    Some((*m, true))
                } else if spent.contains(p) && !has_time {
                    // Still nothing here (no coins since the report).
                    Some((*m, false))
                } else {
                    None
                }
            })
            .collect();
        for (m, moved) in drop {
            st.links.remove(&m);
            if let Some(c) = st.clients.remove(&m) {
                if moved {
                    tracing::info!(mac = %m, "pass moved to another location");
                }
                if c.until.is_some() {
                    self.fw.revoke(m, c.ip);
                }
            }
            changed = true;
        }
        if changed {
            self.persist_locked(&st, now);
        }
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
        self.grant_locked(st, slot.mac, slot.ip, slot.pesos, secs);
        Some((slot.pesos, secs))
    }

    fn pass_trusted(st: &State) -> bool {
        st.last_sync.is_some_and(|t| t.elapsed() < PASS_TRUST)
    }

    fn queue_credit(&self, pass_id: &str, pesos: u32, secs: u64) {
        if pesos == 0 && secs == 0 {
            return;
        }
        if let Err(e) = self.db.outbox_add(pass_id, pesos, secs) {
            tracing::error!("queueing pass credit: {e:#}");
        }
    }

    /// New time for a phone (coins, voucher, owner), which starts its clock.
    /// Time added to a pass is reported to the hub as sold for it.
    fn grant_locked(&self, st: &mut State, mac: Mac, ip: Ipv4Addr, pesos: u32, secs: u64) {
        // Starting the clock would also restart paused pass time. If the hub
        // has been out of touch, that pass may have been claimed elsewhere
        // meanwhile, so the phone lets go of it first: the hub gives the
        // paused time back to the pass on our next sync, and the portal
        // claims it again.
        let paused = st.clients.get(&mac).is_some_and(|c| c.until.is_none() && !c.banked.is_zero());
        if paused && st.links.contains_key(&mac) && !Self::pass_trusted(st) {
            tracing::info!(%mac, "hub out of touch: paused pass time set aside");
            st.links.remove(&mac);
            st.clients.remove(&mac);
        }
        if let Some(pass) = st.links.get(&mac) {
            self.queue_credit(pass, pesos, secs);
        }
        self.add_time_locked(st, mac, ip, secs);
    }

    /// Sets a phone's time outright, running or paused.
    fn set_time_locked(&self, st: &mut State, mac: Mac, ip: Ipv4Addr, secs: u64, run: bool) {
        let now = Instant::now();
        let left = Duration::from_secs(secs.min(MAX_GRANT_SECS));
        let c = st.clients.entry(mac).or_insert(Client { ip, until: None, banked: Duration::ZERO });
        if run {
            c.until = Some(now + left);
            c.banked = Duration::ZERO;
            self.fw.allow(mac, ip, left.as_secs());
        } else {
            if c.until.is_some() {
                self.fw.revoke(mac, c.ip);
            }
            c.until = None;
            c.banked = left;
        }
        c.ip = ip;
        self.persist_locked(st, now);
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

    fn paid(e: &Engine, mac: Mac, pesos: u32) -> u64 {
        assert!(matches!(e.coin_start(mac, IP), CoinStart::Ok));
        e.test_coin(pesos);
        e.coin_done(mac).unwrap().1
    }

    fn report(e: &Engine, pass: &str) -> Option<LeaseReport> {
        e.sync_snapshot(100).0.into_iter().find(|l| l.pass_id == pass)
    }

    #[tokio::test]
    async fn spent_pass_links_are_forgotten_once_the_hub_heard_zero() {
        let (dir, e) = engine(true).await;
        let (a, b) = (Mac::parse(PHONE).unwrap(), Mac::parse("02:00:00:00:00:02").unwrap());
        paid(&e, a, 5);
        e.link_pass(a, "p_a".into(), 5);
        paid(&e, b, 5);
        e.link_pass(b, "p_b".into(), 5);
        // A's time runs out; B still has time.
        e.kick(a);
        let (sent, _) = e.sync_snapshot(100);
        assert_eq!(report(&e, "p_a").unwrap().remaining_secs, 0);
        e.after_sync(&sent, &[]);
        assert!(e.pass_of(a).is_none(), "spent link dropped");
        assert_eq!(e.pass_of(b).as_deref(), Some("p_b"));
        assert_eq!(e.db.load_links().unwrap().len(), 1, "and not saved");
        // Zero was reported, but coins arrived before the hub answered: kept.
        e.kick(b);
        let (sent, _) = e.sync_snapshot(100);
        paid(&e, b, 1);
        e.after_sync(&sent, &[]);
        assert_eq!(e.pass_of(b).as_deref(), Some("p_b"));
        // A pass claimed at another location: cut off.
        e.after_sync(&[], &["p_b".into()]);
        assert!(e.pass_of(b).is_none() && !e.has_time(b));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn time_added_to_a_pass_is_reported_as_sold() {
        let (dir, e) = engine(true).await;
        let mac = Mac::parse(PHONE).unwrap();
        let first = paid(&e, mac, 5);
        e.admin_add_time(mac, 600); // before the pass: plain local time
        e.link_pass(mac, "p_1".into(), 5);
        e.after_sync(&[], &[]);
        let more = paid(&e, mac, 10);
        e.admin_add_time(mac, 300);
        let (leases, credits) = e.sync_snapshot(100);
        let sold: Vec<(u32, u64)> = credits.iter().map(|c| (c.pesos, c.secs)).collect();
        // Linking covers all the time already on the phone; later grants each add theirs.
        assert!(sold[0].0 == 5 && first + 600 - sold[0].1 <= 1, "{sold:?}");
        assert_eq!(&sold[1..], [(10, more), (0, 300)]);
        let sum: u64 = sold.iter().map(|s| s.1).sum();
        assert!(leases[0].remaining_secs <= sum, "a report never exceeds what was reported sold");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn claimed_pass_time_stays_paused_unless_asked() {
        let (dir, e) = engine(true).await;
        let (a, b) = (Mac::parse(PHONE).unwrap(), Mac::parse("02:00:00:00:00:02").unwrap());
        // Phone joins the WiFi in a pocket: paused time stays paused.
        assert_eq!(e.claim_into(a, IP, "p_a".into(), 3600, false, 0), (3600, false));
        assert!(!e.me(a, IP).running && remaining(&e, a) == 3600);
        // Running where it came from, or the customer typed the code: runs.
        assert_eq!(e.claim_into(b, IP, "p_b".into(), 3600, true, 0), (3600, true));
        // Paused pass time joining time already running here runs with it,
        // and the local time is reported as sold for the pass.
        let d = Mac::parse("02:00:00:00:00:04").unwrap();
        let bought = paid(&e, d, 5);
        let (total, running) = e.claim_into(d, IP, "p_d".into(), 3600, false, 5);
        assert!(running && total.abs_diff(3600 + bought) <= 1);
        let credits = e.sync_snapshot(100).1;
        assert!(credits.iter().any(|c| c.pass_id == "p_d" && c.pesos == 5 && bought - c.secs <= 1));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn paused_pass_time_waits_for_the_hub() {
        let (dir, e) = engine(true).await;
        let (a, b) = (Mac::parse(PHONE).unwrap(), Mac::parse("02:00:00:00:00:02").unwrap());
        e.claim_into(a, IP, "p_a".into(), 3600, false, 0);
        // Never synced (or not lately): the pass may have been claimed elsewhere.
        assert!(matches!(e.resume(a, IP), Resume::NeedsHub));
        e.after_sync(&[], &[]);
        assert!(matches!(e.resume(a, IP), Resume::Resumed));
        // Plain local time doesn't need the hub.
        paid(&e, b, 5);
        assert!(e.pause(b));
        e.state.lock().unwrap().last_sync = Instant::now().checked_sub(PASS_TRUST + Duration::from_secs(1));
        assert!(matches!(e.resume(b, IP), Resume::Resumed));
        // Coins would restart paused pass time too: while out of touch, the
        // phone lets go of the pass (the hub gives the time back to it) and
        // only the coins run.
        assert!(e.pause(a));
        let coins = paid(&e, a, 5);
        assert!(e.pass_of(a).is_none());
        assert!(e.me(a, IP).running && coins - remaining(&e, a) <= 1);
        assert!(e.sync_snapshot(100).1.is_empty(), "nothing reported sold for the pass");
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
