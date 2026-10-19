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
use crate::hub::proto::{CreditReport, LeaseReport};
use crate::net::Mac;
use crate::rates;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
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
/// Paused Piso Pass time only restarts if the hub was heard from this
/// recently: while out of touch, this vendo wouldn't know the pass had been
/// claimed at another location. Shorter than any trip between two vendos.
pub const PASS_TRUST: Duration = Duration::from_secs(120);
/// What a blocked phone is told when it tries to buy or use time.
pub const BLOCKED: &str = "This device can't buy time here. Please talk to the owner.";
/// Setting: minutes a phone can be off the WiFi before its running time
/// pauses by itself (0: never).
pub const AUTO_PAUSE_KEY: &str = "auto_pause_mins";
pub const AUTO_PAUSE_DEFAULT_MINS: u32 = 5;
/// What the dashboard offers. Shorter would pause phones that are only
/// asleep and slow to answer.
pub const AUTO_PAUSE_CHOICES: &[u32] = &[0, 2, 3, 5, 10, 15, 30];
/// A phone whose time resumed when it came back is welcomed if it opens
/// the page within this long.
const WELCOME_BACK_FOR: Duration = Duration::from_secs(30 * 60);

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
    /// Last successful hub sync (Piso Pass).
    last_sync: Option<Instant>,
    /// Devices the owner blocked: they get no time here, from anywhere.
    blocked: HashSet<Mac>,
    /// Auto-pause after this many minutes off the WiFi (0: off).
    auto_pause_mins: u32,
    /// When each phone with time was last known to be on the WiFi.
    seen: HashMap<Mac, Instant>,
    /// Phones whose time resumed when they came back, not yet welcomed.
    welcomes: HashMap<Mac, Instant>,
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
    /// Paused by auto-pause because the phone left the WiFi: it resumes by
    /// itself when the phone is back (time the customer paused doesn't).
    away: bool,
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
    /// The owner blocked this phone.
    pub blocked: bool,
    /// Minutes off the WiFi before time pauses by itself (0: off).
    pub auto_pause_mins: u32,
    /// This phone just came back and its paused time started again (shown once).
    pub welcome_back: bool,
}

#[derive(Serialize)]
pub struct ClientView {
    pub mac: String,
    pub ip: String,
    pub remaining_secs: u64,
    pub running: bool,
    /// Paused because it left the WiFi; starts again when it's back.
    pub away: bool,
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
    /// The owner blocked this phone.
    Blocked,
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
    /// The owner blocked this phone.
    Blocked,
}

/// Owner adding time from the dashboard.
#[derive(Debug, PartialEq)]
pub enum AdminAdd {
    Running,
    /// Added to the time it saved; it stays paused until the customer resumes.
    Paused,
    /// Not on the network as far as we can tell.
    Unknown,
    /// The owner blocked this device: unblock it first.
    Blocked,
}

/// Owner blocking a device.
#[derive(Debug, PartialEq)]
pub enum Block {
    Done,
    /// It's inserting coins right now; blocking would swallow them.
    Inserting,
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
        let blocked: HashSet<Mac> = db.load_blocked()?.into_iter().collect();
        let auto_pause_mins = auto_pause_mins_of(db.get_setting(AUTO_PAUSE_KEY)?.as_deref());
        let mut clients = HashMap::new();
        for row in db.load_clients()? {
            if blocked.contains(&row.mac) {
                continue;
            }
            let left = Duration::from_secs(row.remaining_secs.min(MAX_GRANT_SECS));
            // Downtime isn't charged: the clock resumes where it stopped.
            let c = if row.paused {
                Client { ip: row.ip, until: None, banked: left, away: row.away }
            } else {
                Client { ip: row.ip, until: Some(now + left), banked: Duration::ZERO, away: false }
            };
            clients.insert(row.mac, c);
        }
        tracing::info!("restored {} customer sessions", clients.len());
        // Nobody was watched while the service was down: everyone starts as here.
        let seen = clients.keys().map(|m| (*m, now)).collect();
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
                last_sync: None,
                blocked,
                auto_pause_mins,
                seen,
                welcomes: HashMap::new(),
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
        // The page asking is the phone itself (unless it's a copy of its MAC elsewhere).
        let real = self.rebind(&mut st, mac, ip, now);
        let resumed = real && self.saw_locked(&mut st, mac, now);
        let welcome_back = real && st.welcomes.remove(&mac).is_some();
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
        let view = MeView {
            device: mac.masked(),
            ip: ip.to_string(),
            remaining_secs: remaining,
            running,
            slot,
            rates: st.rates.clone(),
            blocked: st.blocked.contains(&mac),
            auto_pause_mins: st.auto_pause_mins,
            welcome_back,
        };
        drop(st);
        if resumed {
            self.flush(false);
        }
        view
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
        if st.blocked.contains(&mac) {
            return CoinStart::Blocked;
        }
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
            self.grant_locked(&mut st, prev.mac, prev.ip, pesos, secs);
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
                c.away = false;
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

    pub fn resume(&self, mac: Mac, ip: Ipv4Addr) -> Resume {
        let now = Instant::now();
        let mut st = self.state.lock().unwrap();
        if st.blocked.contains(&mac) {
            return Resume::Blocked;
        }
        if !self.rebind(&mut st, mac, ip, now) {
            return Resume::Nothing;
        }
        let untrusted = st.links.contains_key(&mac) && !Self::pass_trusted(&st);
        let res = match st.clients.get_mut(&mac) {
            Some(c) if c.until.is_none() && !c.banked.is_zero() => {
                if untrusted {
                    return Resume::NeedsHub;
                }
                c.until = Some(now + c.banked);
                self.fw.allow(mac, c.ip, c.banked.as_secs());
                c.banked = Duration::ZERO;
                c.away = false;
                st.seen.insert(mac, now);
                Resume::Resumed
            }
            _ => Resume::Nothing,
        };
        drop(st);
        self.flush(false);
        res
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
        if self.is_blocked(mac) {
            return Err(BLOCKED);
        }
        self.code_attempt_allowed(mac)?;
        match self.db.redeem_voucher(code, mac) {
            Ok(Some(secs)) => {
                let mut st = self.state.lock().unwrap();
                st.voucher_fails.remove(&mac);
                st.ledger.push(Ledger::Sale { mac: Some(mac), pesos: 0, secs, source: "voucher" });
                self.grant_locked(&mut st, mac, ip, 0, secs);
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
        if st.blocked.contains(&mac) {
            return AdminAdd::Blocked;
        }
        let known = st.clients.get(&mac).map(|c| (c.until.is_none(), c.ip));
        let res = match known {
            Some((true, _)) => {
                let c = st.clients.get_mut(&mac).unwrap();
                c.banked = (c.banked + Duration::from_secs(secs)).min(Duration::from_secs(MAX_GRANT_SECS));
                if let Some(pass) = st.links.get(&mac).cloned() {
                    Self::queue_credit(&mut st, &pass, 0, secs);
                }
                AdminAdd::Paused
            }
            Some((false, ip)) => {
                self.grant_locked(&mut st, mac, ip, 0, secs);
                AdminAdd::Running
            }
            None => match seen_at {
                Some(ip) => {
                    self.grant_locked(&mut st, mac, ip, 0, secs);
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
        let kicked = self.remove_client_locked(&mut st, mac);
        drop(st);
        self.flush(false);
        kicked
    }

    /// Blocks a device: its time here ends (like `kick`) and it can't buy,
    /// redeem, claim or resume time until it's unblocked. Saved at once.
    pub fn block(&self, mac: Mac) -> anyhow::Result<Block> {
        let mut st = self.state.lock().unwrap();
        if st.slot.as_ref().is_some_and(|s| s.mac == mac) {
            return Ok(Block::Inserting);
        }
        self.db.set_blocked(mac, true)?;
        st.blocked.insert(mac);
        st.cooldown.remove(&mac);
        self.remove_client_locked(&mut st, mac);
        drop(st);
        self.flush(false);
        Ok(Block::Done)
    }

    /// Returns whether it was blocked.
    pub fn unblock(&self, mac: Mac) -> anyhow::Result<bool> {
        let mut st = self.state.lock().unwrap();
        self.db.set_blocked(mac, false)?;
        Ok(st.blocked.remove(&mac))
    }

    pub fn is_blocked(&self, mac: Mac) -> bool {
        self.state.lock().unwrap().blocked.contains(&mac)
    }

    pub fn blocked(&self) -> Vec<Mac> {
        self.state.lock().unwrap().blocked.iter().copied().collect()
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
                away: c.away,
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
        let State { seen, clients, welcomes, .. } = &mut *st;
        seen.retain(|m, _| clients.contains_key(m));
        welcomes.retain(|_, t| now - *t < WELCOME_BACK_FOR);
        drop(st);
        self.flush(false);
    }

    // ---- auto-pause -----------------------------------------------------

    pub fn auto_pause_mins(&self) -> u32 {
        self.state.lock().unwrap().auto_pause_mins
    }

    /// `mins` must be one of [`AUTO_PAUSE_CHOICES`].
    pub fn set_auto_pause(&self, mins: u32) -> anyhow::Result<()> {
        self.db.set_setting(AUTO_PAUSE_KEY, &mins.to_string())?;
        let now = Instant::now();
        let mut st = self.state.lock().unwrap();
        if st.auto_pause_mins == 0 && mins > 0 {
            // Nobody was watched while it was off: everyone starts as here.
            let State { seen, clients, .. } = &mut *st;
            seen.extend(clients.keys().map(|m| (*m, now)));
        }
        st.auto_pause_mins = mins;
        Ok(())
    }

    /// Phones for the presence watcher to look for: those with running time
    /// (while auto-pause is on) and those it paused, so they resume when back.
    pub fn watched(&self) -> Vec<(Mac, Ipv4Addr)> {
        let st = self.state.lock().unwrap();
        let on = st.auto_pause_mins > 0;
        st.clients.iter().filter(|(_, c)| c.away || (on && c.until.is_some())).map(|(m, c)| (*m, c.ip)).collect()
    }

    /// What the presence watcher found on the WiFi just now (MAC → address).
    /// Running time of a phone gone longer than the auto-pause setting
    /// pauses; time paused that way starts again once its phone is back.
    pub fn presence(&self, here: &HashMap<Mac, Ipv4Addr>) {
        self.presence_at(here, Instant::now());
    }

    fn presence_at(&self, here: &HashMap<Mac, Ipv4Addr>, now: Instant) {
        let mut st = self.state.lock().unwrap();
        let mut changed = false;
        for (&mac, &ip) in here {
            // Its time follows it to a new address only if dnsmasq gave it that one.
            if st.clients.contains_key(&mac) && self.rebind(&mut st, mac, ip, now) {
                changed |= self.saw_locked(&mut st, mac, now);
            }
        }
        if st.auto_pause_mins > 0 {
            let after = Duration::from_secs(u64::from(st.auto_pause_mins) * 60);
            let gone: Vec<Mac> = st
                .clients
                .iter()
                .filter(|(m, c)| c.until.is_some() && st.seen.get(m).is_none_or(|t| now.saturating_duration_since(*t) >= after))
                .map(|(m, _)| *m)
                .collect();
            for mac in gone {
                let c = st.clients.get_mut(&mac).unwrap();
                c.banked = c.remaining(now);
                c.until = None;
                c.away = true;
                self.fw.revoke(mac, c.ip);
                tracing::info!(%mac, "left the WiFi: time paused until it's back");
                changed = true;
            }
        }
        drop(st);
        if changed {
            self.flush(false);
        }
    }

    /// `mac` is on the WiFi now. Time auto-pause paused starts again, unless
    /// it's Piso Pass time and the hub is out of touch (the pass may have
    /// been claimed elsewhere meanwhile). Returns whether it started.
    fn saw_locked(&self, st: &mut State, mac: Mac, now: Instant) -> bool {
        if !st.clients.contains_key(&mac) {
            return false;
        }
        st.seen.insert(mac, now);
        let trusted = !st.links.contains_key(&mac) || Self::pass_trusted(st);
        let c = st.clients.get_mut(&mac).unwrap();
        if !c.away || c.until.is_some() || c.banked.is_zero() || !trusted {
            return false;
        }
        c.until = Some(now + c.banked);
        self.fw.allow(mac, c.ip, c.banked.as_secs());
        c.banked = Duration::ZERO;
        c.away = false;
        st.welcomes.insert(mac, now);
        tracing::info!(%mac, "back on the WiFi: time started again");
        true
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

    /// Puts this phone's time here on a pass just created on the hub, and
    /// reports that time as sold for it (`pesos`: coins paid just now).
    pub fn link_pass(&self, mac: Mac, pass_id: String, pesos: u32) {
        let mut st = self.state.lock().unwrap();
        let secs = st.clients.get(&mac).map(|c| c.remaining(Instant::now()).as_secs()).unwrap_or(0);
        Self::queue_credit(&mut st, &pass_id, pesos, secs);
        st.links.insert(mac, pass_id);
        drop(st);
        self.flush(false);
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
        // The portal refuses blocked phones before asking the hub; this is the backstop.
        if st.blocked.contains(&mac) {
            return (0, false);
        }
        // A copy of this MAC elsewhere gets the time put on the real phone.
        let ip = self.ip_for(&mut st, mac, ip, now);
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
        Self::queue_credit(&mut st, &pass_id, pesos, extra);
        st.links.insert(mac, pass_id);
        let total = extra.saturating_add(secs).min(MAX_GRANT_SECS);
        let run = total > 0 && (run || local_running);
        if total > 0 {
            self.set_time_locked(&mut st, mac, ip, total, run);
        }
        drop(st);
        self.flush(false);
        (total, run)
    }

    /// What to send the hub: each pass held here with its time, and sales
    /// not yet reported. The hub caps a lease at what was sold for it, so the
    /// credits sent must cover every second the reports include: the
    /// snapshot is taken with no credit waiting to be saved and no save
    /// under way.
    pub fn sync_snapshot(&self, max_credits: u32) -> (Vec<LeaseReport>, Vec<CreditReport>) {
        let mut tries = 0;
        loop {
            self.flush(false);
            let _order = self.writer.lock().unwrap();
            let st = self.state.lock().unwrap();
            tries += 1;
            if tries < 5 && st.ledger.iter().any(|l| matches!(l, Ledger::PassCredit { .. })) {
                // A sale came in after the flush.
                continue;
            }
            let now = Instant::now();
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
            return (leases, credits);
        }
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
        let gone: Vec<(Mac, bool)> = st
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
        for (m, moved) in gone {
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
            drop(st);
            self.flush(false);
        }
    }

    // ---- internals ------------------------------------------------------

    /// Ends a phone's time here. Returns whether it had any.
    fn remove_client_locked(&self, st: &mut State, mac: Mac) -> bool {
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
        self.grant_locked(st, slot.mac, slot.ip, slot.pesos, secs);
        Some((slot.pesos, secs))
    }

    fn pass_trusted(st: &State) -> bool {
        st.last_sync.is_some_and(|t| t.elapsed() < PASS_TRUST)
    }

    /// Reports time as sold for a pass, in the same save as the time itself.
    fn queue_credit(st: &mut State, pass_id: &str, pesos: u32, secs: u64) {
        if pesos == 0 && secs == 0 {
            return;
        }
        st.ledger.push(Ledger::PassCredit { pass_id: pass_id.to_string(), pesos, secs });
    }

    /// New time for a phone (coins, voucher, owner), which starts its clock.
    /// Time added to a pass is reported to the hub as sold for it.
    fn grant_locked(&self, st: &mut State, mac: Mac, ip: Ipv4Addr, pesos: u32, secs: u64) {
        // A copy of this MAC elsewhere gets the time put on the real phone.
        let ip = self.ip_for(st, mac, ip, Instant::now());
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
        if let Some(pass) = st.links.get(&mac).cloned() {
            Self::queue_credit(st, &pass, pesos, secs);
        }
        self.add_time_locked(st, mac, ip, secs);
    }

    /// Sets a phone's time outright, running or paused.
    fn set_time_locked(&self, st: &mut State, mac: Mac, ip: Ipv4Addr, secs: u64, run: bool) {
        if st.blocked.contains(&mac) {
            return;
        }
        let now = Instant::now();
        let left = Duration::from_secs(secs.min(MAX_GRANT_SECS));
        st.seen.insert(mac, now);
        let c = st.clients.entry(mac).or_insert(Client { ip, until: None, banked: Duration::ZERO, away: false });
        c.away = false;
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
    }

    /// Paying (coins, vouchers, passes) always starts the clock.
    fn add_time_locked(&self, st: &mut State, mac: Mac, ip: Ipv4Addr, secs: u64) {
        // Blocked: no time from anywhere (even a coin that was still dropping).
        if st.blocked.contains(&mac) {
            return;
        }
        let now = Instant::now();
        let ip = self.ip_for(st, mac, ip, now);
        let add = Duration::from_secs(secs.min(MAX_GRANT_SECS));
        st.seen.insert(mac, now);
        let c = st.clients.entry(mac).or_insert(Client { ip, until: None, banked: Duration::ZERO, away: false });
        c.away = false;
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
                        away: c.away,
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

/// The saved auto-pause setting; unset or unexpected means the default.
fn auto_pause_mins_of(setting: Option<&str>) -> u32 {
    setting.and_then(|v| v.trim().parse().ok()).filter(|m| AUTO_PAUSE_CHOICES.contains(m)).unwrap_or(AUTO_PAUSE_DEFAULT_MINS)
}

fn grants_of(st: &State, now: Instant) -> Vec<Grant> {
    st.clients
        .iter()
        .filter(|(m, _)| !st.blocked.contains(m))
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
    fn a_blocked_device_gets_no_time_from_anywhere_even_after_a_restart() {
        let (dir, e) = engine(true);
        let (mac, other) = (Mac::parse(PHONE).unwrap(), Mac::parse("02:00:00:00:00:02").unwrap());
        buy(&e, mac, IP, 5);
        e.pause(mac, IP);
        buy(&e, other, Ipv4Addr::new(10, 0, 0, 10), 1);
        assert_eq!(e.block(mac).unwrap(), Block::Done);
        assert_eq!(remaining(&e, mac), 0, "its time ends, like a kick");
        assert!(e.me(mac, IP).blocked && !e.me(other, IP).blocked);
        assert!(matches!(e.coin_start(mac, IP), CoinStart::Blocked));
        assert!(matches!(e.resume(mac, IP), Resume::Blocked));
        assert_eq!(e.admin_add_time(mac, 600, Some(IP)), AdminAdd::Blocked);
        let codes = e.db.create_vouchers(1, 600, "").unwrap();
        assert_eq!(e.redeem_voucher(mac, IP, &codes[0]), Err(BLOCKED));
        assert_eq!(e.redeem_voucher(other, IP, &codes[0]).unwrap(), 600, "the code wasn't used up");
        assert_eq!(e.claim_into(mac, IP, "pass-1".into(), 900, true, 0), (0, false));
        assert!(e.grants().iter().all(|g| g.mac != mac));
        assert!(e.grants().iter().any(|g| g.mac == other), "other phones are untouched");

        // Still blocked after a restart, until the owner unblocks it.
        drop(e);
        let e = engine_at(&dir, true, None);
        assert!(e.is_blocked(mac));
        assert!(matches!(e.coin_start(mac, IP), CoinStart::Blocked));
        assert!(e.unblock(mac).unwrap());
        assert!(!e.unblock(mac).unwrap(), "already unblocked");
        buy(&e, mac, IP, 1);
        assert!(remaining(&e, mac) > 0);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn blocking_waits_while_the_device_is_inserting_coins() {
        let (dir, e) = engine(true);
        let mac = Mac::parse(PHONE).unwrap();
        assert!(matches!(e.coin_start(mac, IP), CoinStart::Ok));
        e.on_coin(5, 5);
        assert_eq!(e.block(mac).unwrap(), Block::Inserting, "blocking now would swallow its coins");
        assert!(!e.is_blocked(mac));
        e.coin_done(mac).unwrap();
        assert_eq!(e.block(mac).unwrap(), Block::Done);
        // A coin still dropping as it was blocked is recorded but gives no time.
        e.on_coin(1, 1);
        assert_eq!(remaining(&e, mac), 0);
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

    fn paid(e: &Engine, mac: Mac, pesos: u32) -> u64 {
        assert!(matches!(e.coin_start(mac, IP), CoinStart::Ok));
        e.test_coin(pesos);
        e.coin_done(mac).unwrap().1
    }

    fn report(e: &Engine, pass: &str) -> Option<LeaseReport> {
        e.sync_snapshot(100).0.into_iter().find(|l| l.pass_id == pass)
    }

    #[test]
    fn spent_pass_links_are_forgotten_once_the_hub_heard_zero() {
        let (dir, e) = engine(true);
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

    #[test]
    fn time_added_to_a_pass_is_reported_as_sold() {
        let (dir, e) = engine(true);
        let mac = Mac::parse(PHONE).unwrap();
        let first = paid(&e, mac, 5);
        e.admin_add_time(mac, 600, None); // before the pass: plain local time
        e.link_pass(mac, "p_1".into(), 5);
        e.after_sync(&[], &[]);
        let more = paid(&e, mac, 10);
        e.admin_add_time(mac, 300, None);
        let (leases, credits) = e.sync_snapshot(100);
        let sold: Vec<(u32, u64)> = credits.iter().map(|c| (c.pesos, c.secs)).collect();
        // Linking covers all the time already on the phone; later grants each add theirs.
        assert!(sold[0].0 == 5 && first + 600 - sold[0].1 <= 1, "{sold:?}");
        assert_eq!(&sold[1..], [(10, more), (0, 300)]);
        let sum: u64 = sold.iter().map(|s| s.1).sum();
        assert!(leases[0].remaining_secs <= sum, "a report never exceeds what was reported sold");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn claimed_pass_time_stays_paused_unless_asked() {
        let (dir, e) = engine(true);
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

    #[test]
    fn paused_pass_time_waits_for_the_hub() {
        let (dir, e) = engine(true);
        let (a, b) = (Mac::parse(PHONE).unwrap(), Mac::parse("02:00:00:00:00:02").unwrap());
        e.claim_into(a, IP, "p_a".into(), 3600, false, 0);
        // Never synced (or not lately): the pass may have been claimed elsewhere.
        assert!(matches!(e.resume(a, IP), Resume::NeedsHub));
        e.after_sync(&[], &[]);
        assert!(matches!(e.resume(a, IP), Resume::Resumed));
        // Plain local time doesn't need the hub.
        paid(&e, b, 5);
        assert!(e.pause(b, IP));
        e.state.lock().unwrap().last_sync = Instant::now().checked_sub(PASS_TRUST + Duration::from_secs(1));
        assert!(matches!(e.resume(b, IP), Resume::Resumed));
        // Coins would restart paused pass time too: while out of touch, the
        // phone lets go of the pass (the hub gives the time back to it) and
        // only the coins run.
        assert!(e.pause(a, IP));
        let coins = paid(&e, a, 5);
        assert!(e.pass_of(a).is_none());
        assert!(e.me(a, IP).running && coins - remaining(&e, a) <= 1);
        assert!(e.sync_snapshot(100).1.is_empty(), "nothing reported sold for the pass");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// `mins` minutes from now. (Not "ago": a test machine that booted a
    /// minute ago has no Instant an hour back.)
    fn later(mins: u64) -> Instant {
        Instant::now() + Duration::from_secs(mins * 60)
    }

    fn view(e: &Engine, mac: Mac) -> ClientView {
        e.clients().into_iter().find(|c| c.mac == mac.to_string()).unwrap()
    }

    #[test]
    fn a_phone_that_leaves_is_paused_and_starts_again_when_back() {
        let (dir, e) = engine(true);
        let mac = Mac::parse(PHONE).unwrap();
        buy(&e, mac, IP, 5);
        assert_eq!(e.auto_pause_mins(), AUTO_PAUSE_DEFAULT_MINS);
        let here = HashMap::from([(mac, IP)]);
        e.presence(&here);
        e.presence_at(&HashMap::new(), later(4));
        assert!(view(&e, mac).running, "gone 4 minutes: not yet");

        e.presence_at(&HashMap::new(), later(5));
        let v = view(&e, mac);
        assert!(!v.running && v.away && v.remaining_secs > 0);
        assert!(e.grants().is_empty(), "no internet while paused");
        let row = e.db.load_clients().unwrap().into_iter().find(|r| r.mac == mac).unwrap();
        assert!(row.paused && row.away, "saved at once");

        // Still away after a restart, and back on the WiFi later: it runs again.
        drop(e);
        let e = engine_at(&dir, true, None);
        assert!(view(&e, mac).away);
        e.presence(&here);
        assert!(view(&e, mac).running && !view(&e, mac).away);
        assert_eq!(e.grants().len(), 1);
        let me = e.me(mac, IP);
        assert!(me.running && me.welcome_back);
        assert!(!e.me(mac, IP).welcome_back, "welcomed once");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn opening_the_page_counts_as_being_back() {
        let (dir, e) = engine(true);
        let mac = Mac::parse(PHONE).unwrap();
        buy(&e, mac, IP, 5);
        e.presence_at(&HashMap::new(), later(30));
        assert!(view(&e, mac).away);
        let me = e.me(mac, IP);
        assert!(me.running && me.welcome_back);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn time_the_customer_paused_stays_paused_when_they_come_back() {
        let (dir, e) = engine(true);
        let mac = Mac::parse(PHONE).unwrap();
        buy(&e, mac, IP, 5);
        assert!(e.pause(mac, IP));
        e.presence_at(&HashMap::new(), later(30));
        e.presence(&HashMap::from([(mac, IP)]));
        let me = e.me(mac, IP);
        assert!(!me.running && !me.welcome_back);
        assert!(!view(&e, mac).away);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn auto_pause_can_be_turned_off_and_on() {
        let (dir, e) = engine(true);
        let mac = Mac::parse(PHONE).unwrap();
        buy(&e, mac, IP, 5);
        e.set_auto_pause(0).unwrap();
        assert!(e.watched().is_empty());
        e.presence_at(&HashMap::new(), later(60));
        assert!(view(&e, mac).running, "off: time keeps running");
        // Turned on again: nobody was watched meanwhile, so everyone starts as here.
        e.set_auto_pause(10).unwrap();
        e.presence(&HashMap::new());
        assert!(view(&e, mac).running);
        assert_eq!(e.watched(), vec![(mac, IP)]);
        drop(e);
        assert_eq!(engine_at(&dir, true, None).auto_pause_mins(), 10, "saved");
        assert_eq!(auto_pause_mins_of(None), AUTO_PAUSE_DEFAULT_MINS);
        assert_eq!(auto_pause_mins_of(Some("0")), 0);
        assert_eq!(auto_pause_mins_of(Some("7")), AUTO_PAUSE_DEFAULT_MINS, "not a choice");
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
