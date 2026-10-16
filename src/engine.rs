//! The vending logic: who owns the coin slot, how much time each device has,
//! pause/resume, vouchers. All state changes go through one lock, and the
//! firewall is told about every change.
//!
//! The database is written after the lock is released, and only with what
//! changed (running clocks are checkpointed every 30 s): the SD card lasts
//! longer and customers' requests don't wait on it.

use crate::db::{Batch, ClientRow, Db, Ledger, OpenSlot, Rate};
use crate::firewall::{Firewall, Grant};
use crate::gpio::Enable;
use crate::hub::proto::LeaseReport;
use crate::net::Mac;
use crate::rates;
use serde::Serialize;
use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::path::PathBuf;
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
/// A refused move (possible MAC clone) is logged once per device and address this often.
const CLONE_ALERT_EVERY: Duration = Duration::from_secs(10 * 60);
/// Longest grant accepted from anywhere (owner, voucher, hub): guards the clock maths.
pub const MAX_GRANT_SECS: u64 = 400 * 86400;

pub struct Engine {
    state: Mutex<State>,
    /// Held while saving, so saves land in the order they were taken.
    /// Always taken before `state`, never while holding it.
    writer: Mutex<()>,
    db: Arc<Db>,
    fw: Firewall,
    enable: Option<(Enable, u32)>,
    pesos_per_pulse: u32,
    window: Duration,
    started: Instant,
    /// The GPIO line is being read. While it isn't, coins are refused.
    reader_ok: Arc<AtomicBool>,
    /// dnsmasq's lease file: a grant only follows a device to an address
    /// dnsmasq gave it. None (dev mode): no check.
    leases: Option<PathBuf>,
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
    /// What the database holds, so only changes are written.
    saved: Saved,
    /// Sales, pass credits and log lines waiting for the next save.
    ledger: Vec<Ledger>,
    /// Refused moves already logged: (device, address it asked from) → when.
    clone_alerts: HashMap<(Mac, Ipv4Addr), Instant>,
}

#[derive(Default)]
struct Saved {
    clients: HashMap<Mac, Client>,
    links: HashMap<Mac, String>,
    slot: Option<OpenSlot>,
}

#[derive(Clone, PartialEq)]
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
    /// Rates when the customer tapped Insert coin: a change made while
    /// they're inserting doesn't re-price coins already in.
    rates: Vec<Rate>,
}

impl Slot {
    fn record(&self) -> OpenSlot {
        OpenSlot { mac: self.mac, ip: self.ip, coins: self.coins.clone(), rates: self.rates.clone() }
    }
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

/// Owner adding time from the dashboard.
#[derive(Debug, PartialEq)]
pub enum AdminAdd {
    Running,
    /// Added to the time it saved; it stays paused until the customer resumes.
    Paused,
    /// Not on the network as far as we can tell.
    Unknown,
}

impl Engine {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        db: Arc<Db>,
        fw: Firewall,
        enable: Option<(Enable, u32)>,
        pesos_per_pulse: u32,
        window_secs: u64,
        reader_ok: Arc<AtomicBool>,
        leases: Option<PathBuf>,
    ) -> anyhow::Result<Arc<Engine>> {
        let now = Instant::now();
        let mut clients = HashMap::new();
        for row in db.load_clients()? {
            let left = Duration::from_secs(row.remaining_secs.min(MAX_GRANT_SECS));
            // Downtime isn't charged: the clock resumes where it stopped.
            let c = if row.paused {
                Client { ip: row.ip, until: None, banked: left }
            } else {
                Client { ip: row.ip, until: Some(now + left), banked: Duration::ZERO }
            };
            clients.insert(row.mac, c);
        }
        tracing::info!("restored {} customer sessions", clients.len());
        let rates = db.rates()?;
        let links: HashMap<Mac, String> = db.load_links()?.into_iter().collect();
        let open = db.open_slot().unwrap_or_else(|e| {
            // Can't be credited; tell the owner once instead of failing every start.
            tracing::error!("{e:#}");
            db.audit("coin", "slot_unreadable", "Coins inserted before a restart couldn't be read back. Check with the customer.");
            if let Err(e) = db.delete_setting(crate::db::OPEN_SLOT_KEY) {
                tracing::error!("{e:#}");
            }
            None
        });
        let engine = Arc::new(Engine {
            state: Mutex::new(State {
                saved: Saved { clients: clients.clone(), links: links.clone(), slot: open.clone() },
                clients,
                slot: None,
                rates,
                voucher_fails: HashMap::new(),
                pulses_total: 0,
                links,
                counting: false,
                last_slot: None,
                cooldown: HashMap::new(),
                ledger: vec![],
                clone_alerts: HashMap::new(),
            }),
            writer: Mutex::new(()),
            db,
            fw,
            enable,
            pesos_per_pulse,
            window: Duration::from_secs(window_secs),
            started: now,
            reader_ok,
            leases,
        });
        if let Some(open) = open {
            // The power went (or the service died) while someone was inserting.
            let mut st = engine.state.lock().unwrap();
            let pesos: u32 = open.coins.iter().sum();
            let slot = Slot { mac: open.mac, ip: open.ip, pesos, coins: open.coins, deadline: now, rates: open.rates };
            if let Some((pesos, secs)) = engine.finish_slot(&mut st, slot) {
                tracing::warn!(mac = %open.mac, pesos, secs, "coins inserted before the restart credited");
                st.ledger.push(Ledger::Audit {
                    actor: "coin",
                    action: "slot_restored",
                    detail: format!("₱{pesos} inserted by {} before a restart, credited", open.mac),
                });
            }
            st.last_slot = None;
        }
        engine.flush(false);
        Ok(engine)
    }

    pub fn uptime_secs(&self) -> u64 {
        self.started.elapsed().as_secs()
    }

    /// Every running customer, for the firewall.
    pub fn grants(&self) -> Vec<Grant> {
        let now = Instant::now();
        let st = self.state.lock().unwrap();
        grants_of(&st, now)
    }

    /// Has the firewall compare the kernel with the running customers. Sent
    /// under the lock, so it's queued after every change made so far.
    pub fn reconcile_firewall(&self) {
        let st = self.state.lock().unwrap();
        self.fw.reconcile(grants_of(&st, Instant::now()));
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
                credit_secs: if s.mac == mac { rates::secs_for(s.pesos, &s.rates) } else { 0 },
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

    /// DHCP may hand a device a new IP; move its grant along with it. Only
    /// to an address dnsmasq actually leased to that MAC, though: otherwise
    /// a second device copying a paying phone's MAC could take its time.
    /// Returns false when the move was refused.
    fn rebind(&self, st: &mut State, mac: Mac, ip: Ipv4Addr, now: Instant) -> bool {
        let Some(old) = st.clients.get(&mac).map(|c| c.ip) else { return true };
        if old == ip {
            return true;
        }
        if let Some(path) = &self.leases {
            let leased = match crate::net::read_leases(path) {
                Ok(leases) => leases.iter().any(|l| l.mac == mac && l.ip == ip),
                Err(e) => {
                    tracing::error!("{e:#}");
                    false
                }
            };
            if !leased {
                let due = st.clone_alerts.get(&(mac, ip)).is_none_or(|t| now - *t >= CLONE_ALERT_EVERY);
                if due {
                    tracing::warn!(%mac, %ip, %old, "possible MAC clone: time not moved");
                    st.clone_alerts.insert((mac, ip), now);
                    st.ledger.push(Ledger::Audit {
                        actor: "system",
                        action: "mac_clone",
                        detail: format!(
                            "Possible MAC clone: {mac} has time on {old}, but asked from {ip}, which dnsmasq didn't give it. Time not moved."
                        ),
                    });
                }
                return false;
            }
        }
        let c = st.clients.get_mut(&mac).unwrap();
        if let Some(u) = c.until {
            self.fw.revoke(mac, c.ip);
            self.fw.allow(mac, ip, u.saturating_duration_since(now).as_secs());
        }
        c.ip = ip;
        true
    }

    /// The address this device's time lives on after a request from `ip`.
    fn ip_for(&self, st: &mut State, mac: Mac, ip: Ipv4Addr, now: Instant) -> Ipv4Addr {
        if self.rebind(st, mac, ip, now) {
            ip
        } else {
            st.clients[&mac].ip
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
        let rates = st.rates.clone();
        st.slot = Some(Slot { mac, ip, pesos: 0, coins: vec![], deadline: now + self.window, rates });
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
        if !st.slot.as_ref().is_some_and(|s| s.mac == mac) {
            return None;
        }
        let slot = st.slot.take().unwrap();
        let credited = self.finish_slot(&mut st, slot);
        drop(st);
        self.flush(false);
        credited
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
            st.ledger.push(Ledger::Audit {
                actor: "coin",
                action: "unusual_burst",
                detail: format!("{pulses} pulses = ₱{pesos}"),
            });
        }
        if let Some(s) = &mut st.slot {
            s.pesos += pesos;
            s.coins.push(pesos);
            s.deadline = now + self.window;
        } else if let Some((prev, closed)) = st.last_slot.take().filter(|(_, t)| now - *t < LATE_COIN) {
            // The customer tapped "Start internet" (or time ran out) as this
            // coin dropped: it's still theirs, priced as part of what they paid.
            let before = rates::secs_for(prev.pesos, &prev.rates);
            let total = prev.pesos + pesos;
            let secs = rates::secs_for(total, &prev.rates).saturating_sub(before);
            tracing::info!(mac = %prev.mac, pesos, secs, "late coin credited");
            st.ledger.push(Ledger::Sale { mac: Some(prev.mac), pesos, secs, source: "coin" });
            if let Some(pass) = st.links.get(&prev.mac).cloned() {
                st.ledger.push(Ledger::PassCredit { pass_id: pass, pesos, secs });
            }
            self.add_time_locked(&mut st, prev.mac, prev.ip, secs);
            st.last_slot = Some((Slot { pesos: total, ..prev }, closed));
        } else {
            // Acceptor isn't gated and nobody tapped "Insert coin".
            tracing::warn!(pesos, "coin received with no customer waiting");
            st.ledger.push(Ledger::Sale { mac: None, pesos, secs: 0, source: "unclaimed" });
        }
        drop(st);
        // Saved now (with the open slot), so a power cut can't lose it.
        self.flush(false);
    }

    /// On shutdown: credit coins already inserted instead of dropping them.
    pub fn finish_open_slot(&self) {
        let mut st = self.state.lock().unwrap();
        if let Some(slot) = st.slot.take() {
            self.finish_slot(&mut st, slot);
        }
        drop(st);
        self.flush(false);
    }

    pub fn pause(&self, mac: Mac, ip: Ipv4Addr) -> bool {
        let now = Instant::now();
        let mut st = self.state.lock().unwrap();
        // A copy of this MAC on another address can't pause the real phone.
        if !self.rebind(&mut st, mac, ip, now) {
            return false;
        }
        let paused = match st.clients.get_mut(&mac) {
            Some(c) if c.until.is_some() => {
                c.banked = c.remaining(now);
                c.until = None;
                self.fw.revoke(mac, c.ip);
                true
            }
            _ => false,
        };
        drop(st);
        // Saved now: a power cut must not bring it back as running.
        self.flush(false);
        paused
    }

    pub fn resume(&self, mac: Mac, ip: Ipv4Addr) -> bool {
        let now = Instant::now();
        let mut st = self.state.lock().unwrap();
        if !self.rebind(&mut st, mac, ip, now) {
            return false;
        }
        let resumed = match st.clients.get_mut(&mac) {
            Some(c) if c.until.is_none() && !c.banked.is_zero() => {
                c.until = Some(now + c.banked);
                self.fw.allow(mac, c.ip, c.banked.as_secs());
                c.banked = Duration::ZERO;
                true
            }
            _ => false,
        };
        drop(st);
        self.flush(false);
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
                let mut st = self.state.lock().unwrap();
                st.voucher_fails.remove(&mac);
                st.ledger.push(Ledger::Sale { mac: Some(mac), pesos: 0, secs, source: "voucher" });
                self.add_time_locked(&mut st, mac, ip, secs);
                drop(st);
                self.flush(false);
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

    /// Adds time for the owner. A paused customer stays paused (the time
    /// waits with the rest of theirs); a device without time needs `seen_at`,
    /// the address it was seen at on the LAN.
    pub fn admin_add_time(&self, mac: Mac, secs: u64, seen_at: Option<Ipv4Addr>) -> AdminAdd {
        let mut st = self.state.lock().unwrap();
        let res = match st.clients.get_mut(&mac) {
            Some(c) if c.until.is_none() => {
                c.banked = (c.banked + Duration::from_secs(secs)).min(Duration::from_secs(MAX_GRANT_SECS));
                AdminAdd::Paused
            }
            Some(c) => {
                let ip = c.ip;
                self.add_time_locked(&mut st, mac, ip, secs);
                AdminAdd::Running
            }
            None => match seen_at {
                Some(ip) => {
                    self.add_time_locked(&mut st, mac, ip, secs);
                    AdminAdd::Running
                }
                None => AdminAdd::Unknown,
            },
        };
        drop(st);
        self.flush(false);
        res
    }

    pub fn kick(&self, mac: Mac) -> bool {
        let mut st = self.state.lock().unwrap();
        let kicked = match st.clients.remove(&mac) {
            Some(c) => {
                if c.until.is_some() {
                    self.fw.revoke(mac, c.ip);
                }
                true
            }
            None => false,
        };
        drop(st);
        self.flush(false);
        kicked
    }

    pub fn test_coin(&self, pesos: u32) {
        self.on_coin(pesos, (pesos / self.pesos_per_pulse).max(1));
    }

    /// New rates apply from the next Insert coin; a slot already open keeps
    /// the rates its customer was shown.
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

    /// Called every second: expires sessions and abandoned coin slots, and
    /// saves whatever changed.
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
        st.clone_alerts.retain(|_, t| now - *t < CLONE_ALERT_EVERY);
        drop(st);
        self.flush(false);
    }

    /// Checkpoints running clocks (every 30 s and on shutdown), so a power
    /// cut costs customers at most that much.
    pub fn persist(&self) {
        self.flush(true);
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
        self.state.lock().unwrap().links.insert(mac, pass_id);
        self.flush(false);
    }

    /// Puts a pass's time (just leased from the hub) on this phone. The hub's
    /// figure already includes any time this vendo held for the pass, so
    /// that is replaced, never added twice. Unlinked local time is merged in.
    pub fn claim_into(&self, mac: Mac, ip: Ipv4Addr, pass_id: String, secs: u64) -> u64 {
        let now = Instant::now();
        let mut st = self.state.lock().unwrap();
        // A copy of this MAC elsewhere gets the time put on the real phone.
        let ip = self.ip_for(&mut st, mac, ip, now);
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
        }
        drop(st);
        self.flush(false);
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
        drop(st);
        self.flush(false);
    }

    // ---- internals ------------------------------------------------------

    fn finish_slot(&self, st: &mut State, slot: Slot) -> Option<(u32, u64)> {
        self.set_acceptor(false);
        if slot.pesos == 0 {
            return None;
        }
        st.last_slot = Some((Slot { coins: vec![], ..slot.clone() }, Instant::now()));
        let secs = rates::secs_for(slot.pesos, &slot.rates);
        tracing::info!(mac = %slot.mac, pesos = slot.pesos, secs, "coins credited");
        // Goes into the same transaction as the time and the cleared slot.
        st.ledger.push(Ledger::Sale { mac: Some(slot.mac), pesos: slot.pesos, secs, source: "coin" });
        if let Some(pass) = st.links.get(&slot.mac).cloned() {
            st.ledger.push(Ledger::PassCredit { pass_id: pass, pesos: slot.pesos, secs });
        }
        self.add_time_locked(st, slot.mac, slot.ip, secs);
        Some((slot.pesos, secs))
    }

    /// Paying (coins, vouchers, passes) always starts the clock.
    fn add_time_locked(&self, st: &mut State, mac: Mac, ip: Ipv4Addr, secs: u64) {
        let now = Instant::now();
        let ip = self.ip_for(st, mac, ip, now);
        let add = Duration::from_secs(secs.min(MAX_GRANT_SECS));
        let c = st.clients.entry(mac).or_insert(Client { ip, until: None, banked: Duration::ZERO });
        let total = (c.remaining(now) + add).min(Duration::from_secs(MAX_GRANT_SECS));
        c.until = Some(now + total);
        c.banked = Duration::ZERO;
        self.fw.allow(mac, ip, total.as_secs());
    }

    /// Writes what changed since the last save. `checkpoint` also writes
    /// every running clock's time left. Never call with `state` held.
    fn flush(&self, checkpoint: bool) {
        let _order = self.writer.lock().unwrap();
        let now = Instant::now();
        let mut batch = Batch::default();
        let mut client_marks: Vec<(Mac, Option<Client>)> = vec![];
        let mut link_marks: Vec<(Mac, Option<String>)> = vec![];
        let mut slot_mark = None;
        {
            let mut st = self.state.lock().unwrap();
            for (m, c) in &st.clients {
                if st.saved.clients.get(m) != Some(c) || (checkpoint && c.until.is_some()) {
                    batch.clients.push(ClientRow {
                        mac: *m,
                        ip: c.ip,
                        remaining_secs: c.remaining(now).as_secs(),
                        paused: c.until.is_none(),
                    });
                    client_marks.push((*m, Some(c.clone())));
                }
            }
            for m in st.saved.clients.keys().filter(|m| !st.clients.contains_key(m)) {
                batch.clients_gone.push(*m);
                client_marks.push((*m, None));
            }
            for (m, p) in &st.links {
                if st.saved.links.get(m) != Some(p) {
                    batch.links.push((*m, p.clone()));
                    link_marks.push((*m, Some(p.clone())));
                }
            }
            for m in st.saved.links.keys().filter(|m| !st.links.contains_key(m)) {
                batch.links_gone.push(*m);
                link_marks.push((*m, None));
            }
            let slot = st.slot.as_ref().filter(|s| !s.coins.is_empty()).map(Slot::record);
            if slot != st.saved.slot {
                batch.slot = Some(slot.clone());
                slot_mark = Some(slot);
            }
            batch.ledger = std::mem::take(&mut st.ledger);
        }
        if batch.is_empty() {
            return;
        }
        let res = self.db.save(&batch);
        let mut st = self.state.lock().unwrap();
        match res {
            Ok(()) => {
                for (m, c) in client_marks {
                    match c {
                        Some(c) => st.saved.clients.insert(m, c),
                        None => st.saved.clients.remove(&m),
                    };
                }
                for (m, p) in link_marks {
                    match p {
                        Some(p) => st.saved.links.insert(m, p),
                        None => st.saved.links.remove(&m),
                    };
                }
                if let Some(slot) = slot_mark {
                    st.saved.slot = slot;
                }
            }
            Err(e) => {
                // Kept for the next save; nothing is marked as written.
                tracing::error!("saving: {e:#}");
                let mut ledger = batch.ledger;
                ledger.append(&mut st.ledger);
                st.ledger = ledger;
            }
        }
    }

    fn set_acceptor(&self, on: bool) {
        if let Some((en, line)) = &self.enable {
            en.set(on, *line);
        }
    }
}

fn grants_of(st: &State, now: Instant) -> Vec<Grant> {
    st.clients
        .iter()
        .filter_map(|(m, c)| c.until.map(|u| Grant { mac: *m, ip: c.ip, secs: u.saturating_duration_since(now).as_secs() }))
        .filter(|g| g.secs > 0)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::DbKey;
    use std::path::Path;
    use zeroize::Zeroizing;

    fn tmp() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pisowifi-engine-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// An engine on the database in `dir` (a second call is a restart).
    fn engine_at(dir: &Path, reader_ok: bool, leases: Option<PathBuf>) -> Arc<Engine> {
        let db = Arc::new(Db::open(&dir.join("t.db"), &DbKey(Zeroizing::new([7u8; 32]))).unwrap());
        let (fw, _) = Firewall::new(&crate::config::Config::dev(dir.to_path_buf()), true);
        Engine::new(db, fw, None, 1, 60, Arc::new(AtomicBool::new(reader_ok)), leases).unwrap()
    }

    fn engine(reader_ok: bool) -> (PathBuf, Arc<Engine>) {
        let dir = tmp();
        let e = engine_at(&dir, reader_ok, None);
        (dir, e)
    }

    const PHONE: &str = "02:11:22:33:44:55";
    const IP: Ipv4Addr = Ipv4Addr::new(10, 0, 0, 9);

    fn remaining(e: &Engine, mac: Mac) -> u64 {
        e.me(mac, IP).remaining_secs
    }

    fn buy(e: &Engine, mac: Mac, ip: Ipv4Addr, pesos: u32) {
        assert!(matches!(e.coin_start(mac, ip), CoinStart::Ok));
        e.on_coin(pesos, pesos);
        e.coin_done(mac).unwrap();
    }

    #[test]
    fn a_coin_still_dropping_after_start_internet_is_credited() {
        let (dir, e) = engine(true);
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

    #[test]
    fn the_slot_owner_cant_extend_it_by_tapping_again() {
        let (dir, e) = engine(true);
        let (a, b) = (Mac::parse(PHONE).unwrap(), Mac::parse("02:00:00:00:00:02").unwrap());
        assert!(matches!(e.coin_start(a, IP), CoinStart::Ok));
        let deadline = e.state.lock().unwrap().slot.as_ref().unwrap().deadline;
        std::thread::sleep(Duration::from_millis(20));
        assert!(matches!(e.coin_start(a, IP), CoinStart::Ok));
        assert_eq!(e.state.lock().unwrap().slot.as_ref().unwrap().deadline, deadline);
        assert!(matches!(e.coin_start(b, IP), CoinStart::Busy(_)));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn coins_are_refused_while_the_reader_is_down() {
        let (dir, e) = engine(false);
        assert!(matches!(e.coin_start(Mac::parse(PHONE).unwrap(), IP), CoinStart::Offline));
        assert!(!e.slot_admin().reader_ok);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn pausing_is_saved_straight_away() {
        let (dir, e) = engine(true);
        let mac = Mac::parse(PHONE).unwrap();
        buy(&e, mac, IP, 5);
        assert!(e.pause(mac, IP));
        let row = e.db.load_clients().unwrap().into_iter().find(|r| r.mac == mac).unwrap();
        assert!(row.paused, "a power cut now must not bring it back as running");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn coins_in_the_slot_survive_a_power_cut() {
        let dir = tmp();
        let e = engine_at(&dir, true, None);
        let mac = Mac::parse(PHONE).unwrap();
        let rates = e.state.lock().unwrap().rates.clone();
        assert!(matches!(e.coin_start(mac, IP), CoinStart::Ok));
        e.on_coin(5, 5);
        e.on_coin(1, 1);
        // The power goes: no "Start internet", no orderly shutdown.
        drop(e);
        let e = engine_at(&dir, true, None);
        assert!(rates::secs_for(6, &rates) - remaining(&e, mac) <= 1);
        assert!(e.me(mac, IP).running);
        assert_eq!(e.db.sales_summary(1).unwrap().today_pesos, 6);
        assert!(e.db.open_slot().unwrap().is_none());
        assert!(e.db.list_audit(5).unwrap().iter().any(|a| a.action == "slot_restored"));
        drop(e);
        // Credited once, not again on the next start.
        let e = engine_at(&dir, true, None);
        assert_eq!(e.db.sales_summary(1).unwrap().today_pesos, 6);
        assert!(rates::secs_for(6, &rates) - remaining(&e, mac) <= 2);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_rate_change_mid_insert_doesnt_reprice_coins_already_in() {
        let (dir, e) = engine(true);
        let (a, b) = (Mac::parse(PHONE).unwrap(), Mac::parse("02:00:00:00:00:02").unwrap());
        let old = e.state.lock().unwrap().rates.clone();
        assert!(matches!(e.coin_start(a, IP), CoinStart::Ok));
        e.on_coin(5, 5);
        e.set_rates(vec![Rate { pesos: 1, minutes: 1 }]);
        e.on_coin(1, 1);
        assert_eq!(e.me(a, IP).slot.credit_secs, rates::secs_for(6, &old));
        assert_eq!(e.coin_done(a), Some((6, rates::secs_for(6, &old))));
        // The next customer pays the new price.
        assert!(matches!(e.coin_start(b, IP), CoinStart::Ok));
        e.on_coin(1, 1);
        assert_eq!(e.coin_done(b), Some((1, 60)));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_copied_mac_on_another_address_cant_take_the_time() {
        let dir = tmp();
        let leases = dir.join("dnsmasq.leases");
        std::fs::write(&leases, format!("0 {PHONE} 10.0.0.9 phone *\n")).unwrap();
        let e = engine_at(&dir, true, Some(leases.clone()));
        let mac = Mac::parse(PHONE).unwrap();
        let clone_ip = Ipv4Addr::new(10, 0, 0, 66);
        buy(&e, mac, IP, 5);
        e.me(mac, clone_ip);
        assert_eq!(e.clients()[0].ip, "10.0.0.9");
        assert!(!e.pause(mac, clone_ip), "the copy can't pause the real phone");
        // Coins from the copy are credited to the real phone's address.
        buy(&e, mac, clone_ip, 1);
        assert_eq!(e.clients()[0].ip, "10.0.0.9");
        assert_eq!(e.grants().iter().map(|g| g.ip).collect::<Vec<_>>(), vec![IP]);
        e.tick();
        let alerts = e.db.list_audit(10).unwrap().into_iter().filter(|a| a.action == "mac_clone").count();
        assert_eq!(alerts, 1, "logged once, not on every page load");
        // dnsmasq really did give the phone a new address: its time follows.
        std::fs::write(&leases, format!("0 {PHONE} 10.0.0.66 phone *\n")).unwrap();
        e.me(mac, clone_ip);
        assert_eq!(e.clients()[0].ip, "10.0.0.66");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn owner_time_keeps_a_paused_customer_paused() {
        let (dir, e) = engine(true);
        let mac = Mac::parse(PHONE).unwrap();
        buy(&e, mac, IP, 5);
        assert!(e.pause(mac, IP));
        let before = remaining(&e, mac);
        assert_eq!(e.admin_add_time(mac, 600, None), AdminAdd::Paused);
        let me = e.me(mac, IP);
        assert!(!me.running);
        assert_eq!(me.remaining_secs, before + 600);
        assert!(e.grants().is_empty(), "still offline until they resume");
        // A device on the WiFi without any time yet.
        let other = Mac::parse("02:00:00:00:00:02").unwrap();
        assert_eq!(e.admin_add_time(other, 600, None), AdminAdd::Unknown);
        let at = Ipv4Addr::new(10, 0, 0, 20);
        assert_eq!(e.admin_add_time(other, 600, Some(at)), AdminAdd::Running);
        assert_eq!(e.grants().iter().map(|g| (g.mac, g.ip)).collect::<Vec<_>>(), vec![(other, at)]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn only_rows_that_changed_are_written() {
        let (dir, e) = engine(true);
        let (a, b) = (Mac::parse(PHONE).unwrap(), Mac::parse("02:00:00:00:00:02").unwrap());
        buy(&e, a, IP, 5);
        assert!(e.pause(a, IP));
        buy(&e, b, Ipv4Addr::new(10, 0, 0, 10), 5);
        e.persist();
        // Mark a's row so a rewrite would show.
        e.db.exec_for_test(&format!("UPDATE clients SET remaining_secs = 4242 WHERE mac = '{a}'"));
        let row = |m: Mac| e.db.load_clients().unwrap().into_iter().find(|r| r.mac == m).map(|r| r.remaining_secs);
        buy(&e, b, Ipv4Addr::new(10, 0, 0, 10), 1);
        e.persist();
        assert_eq!(row(a), Some(4242), "paused and unchanged: not rewritten");
        assert!(row(b).unwrap() > 4242, "running clock checkpointed");
        assert!(e.kick(a));
        assert_eq!(row(a), None);
        let _ = std::fs::remove_dir_all(dir);
    }
}
