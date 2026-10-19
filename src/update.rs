//! Installing updates without risking the shop.
//!
//! Who does what:
//! * The daemon (sandboxed) checks the signed update list (channels/
//!   stable.json in the public releases repository) a few minutes after it
//!   starts and about once a day. When the owner taps Install (or at 3 AM,
//!   if they turned that on) it downloads this board's bundle, checks its
//!   size, SHA-256 and signature, stages it in <data_dir>/update/staged and
//!   writes <data_dir>/update/request. A vendo without internet gets the
//!   same bundle uploaded in the dashboard instead.
//! * pisowifi-update.path sees the request and starts pisowifi-update.service,
//!   which runs `pisowifi update --run-pending` as root. It never trusts the
//!   daemon's checks: it verifies the signature and checksum again from the
//!   raw files, then
//!   1. unpacks into /usr/local/lib/pisowifi/releases/<version> and checks
//!      the new binary runs on this board;
//!   2. stops the service. Sessions are saved on the way out, and the
//!      firewall table stays loaded, so customers who are online stay online;
//!   3. copies the data folder (the SQLCipher database is encrypted on
//!      disk, so the copy is too) and the unit files;
//!   4. points `current` at the new release (`previous` keeps the old one)
//!      with an atomic symlink swap, and starts it;
//!   5. waits up to 90 s for it to report healthy (firewall loaded, portal
//!      and dashboard listening) and stay up. If it doesn't, the old binary,
//!      units and data are put back and the old version is started.
//! * A power cut halfway is finished at the next boot: the same unit runs
//!   then and completes or rolls back whatever was left.
//!
//! Rollback records live under /usr/local/lib/pisowifi, which the daemon
//! can't write, so a compromised daemon can't steer the root-side installer.

use crate::config::Config;
use crate::db::Db;
use crate::engine::Engine;
use crate::release::{self, Channel, ChannelFile, Expect, Kind, Manifest, Version};
use anyhow::{bail, Context, Result};
use ed25519_dalek::VerifyingKey;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::ffi::OsString;
use std::fs;
use std::io::Read;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Dashboard uploads (a bundle is a few MB).
pub const MAX_UPLOAD_BYTES: usize = 16 * 1024 * 1024;
const HEALTH: &str = "health.json";
const INCOMING: &str = ".incoming";

// ---- where things live -------------------------------------------------------------

pub struct Layout {
    /// releases/<version>-<time>/, current, previous, rollback/, state.json
    pub root: PathBuf,
    /// /usr/local/bin/pisowifi -> <root>/current/pisowifi
    pub bin_link: PathBuf,
    pub units_dir: PathBuf,
}

impl Layout {
    pub fn system() -> Layout {
        Layout {
            root: "/usr/local/lib/pisowifi".into(),
            bin_link: "/usr/local/bin/pisowifi".into(),
            units_dir: "/etc/systemd/system".into(),
        }
    }
    fn releases(&self) -> PathBuf {
        self.root.join("releases")
    }
    fn current(&self) -> PathBuf {
        self.root.join("current")
    }
    fn previous(&self) -> PathBuf {
        self.root.join("previous")
    }
    fn rollback(&self) -> PathBuf {
        self.root.join("rollback")
    }
    pub fn state_path(&self) -> PathBuf {
        self.root.join("state.json")
    }
}

/// What is being updated: a vendo or the hub.
pub struct Profile {
    pub kind: Kind,
    pub service: String,
    pub update_service: String,
    pub data_dir: PathBuf,
    /// Where bundles are staged (the vendo daemon writes here).
    pub update_dir: PathBuf,
    /// Must be answering once the new version is up.
    pub listen: Vec<SocketAddr>,
    /// Check that the nftables table is loaded.
    pub firewall: bool,
    /// When the newest update list seen was signed (refuses replayed old lists).
    pub seen: PathBuf,
}

impl Profile {
    pub fn vendo(cfg: &Config, dev: bool) -> Profile {
        Profile {
            kind: Kind::Vendo,
            service: "pisowifi.service".into(),
            update_service: "pisowifi-update.service".into(),
            data_dir: cfg.storage.data_dir.clone(),
            update_dir: cfg.storage.data_dir.join("update"),
            listen: vec![cfg.portal.listen, cfg.admin.listen],
            firewall: !dev,
            seen: cfg.storage.data_dir.join("update").join("list-seen"),
        }
    }

    pub fn hub(data_dir: &Path, listen: SocketAddr, dev: bool) -> Profile {
        let update_dir: PathBuf = if dev { data_dir.join("update") } else { "/var/lib/pisowifi-update".into() };
        Profile {
            kind: Kind::Hub,
            service: "pisowifi-hub.service".into(),
            update_service: "pisowifi-hub-update.service".into(),
            data_dir: data_dir.to_path_buf(),
            seen: update_dir.join("list-seen"),
            update_dir,
            listen: vec![listen],
            firewall: false,
        }
    }

    fn units(&self) -> impl Iterator<Item = &'static str> {
        self.kind.files().iter().copied().filter(|f| [".service", ".path", ".timer"].iter().any(|ext| f.ends_with(ext)))
    }

    fn staged(&self) -> PathBuf {
        self.update_dir.join("staged")
    }
}

/// The updater's own units get enabled when a release brings them; the
/// main service is install.sh's business, and the hub's 3 AM timer the owner's.
fn wants_enable(unit: &str) -> bool {
    unit.contains("-update.")
}

#[derive(Clone, Copy)]
pub struct Timing {
    /// How long a new version has to report healthy.
    pub trial: Duration,
    /// ...and then stay up without restarting.
    pub stable: Duration,
    pub poll: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Timing { trial: Duration::from_secs(90), stable: Duration::from_secs(10), poll: Duration::from_secs(1) }
    }
}

// ---- health, written by the running daemon ------------------------------------------------

#[derive(Serialize, Deserialize)]
pub struct Health {
    pub version: String,
    pub pid: u32,
    pub started: i64,
    /// The nftables ruleset is loaded.
    pub firewall: bool,
}

/// Called once everything is up: listeners bound and serving, firewall loaded.
pub fn report_healthy(data_dir: &Path, firewall: bool) -> Result<()> {
    let h = Health { version: env!("CARGO_PKG_VERSION").into(), pid: std::process::id(), started: crate::db::now(), firewall };
    write_atomic(&data_dir.join(HEALTH), &serde_json::to_vec(&h)?, 0o644)
}

pub fn clear_health(data_dir: &Path) {
    let _ = fs::remove_file(data_dir.join(HEALTH));
}

// ---- progress and results ---------------------------------------------------------------

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    #[default]
    Idle,
    Installing,
    Trial,
    RollingBack,
    Done,
    RolledBack,
    Failed,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct State {
    pub phase: Phase,
    pub from: String,
    pub to: String,
    pub message: String,
    /// The last version that had to be rolled back; automatic installs skip it.
    pub failed_version: Option<String>,
    pub at: i64,
}

pub fn read_state(path: &Path) -> Option<State> {
    read_json(path)
}

/// Written before anything is touched, so an interrupted update can be
/// finished or undone.
#[derive(Clone, Serialize, Deserialize)]
struct Meta {
    from: String,
    to: String,
    /// Relative to the layout root, like the `current` link.
    new_dir: PathBuf,
    old_current: Option<PathBuf>,
    old_previous: Option<PathBuf>,
    units_saved: bool,
    /// Units that existed before (and were saved).
    units: Vec<String>,
    data_saved: bool,
}

// ---- service control ------------------------------------------------------------------

pub trait Service {
    fn stop(&mut self) -> Result<()>;
    fn start(&mut self) -> Result<()>;
    fn main_pid(&mut self) -> Option<u32>;
    fn reload(&mut self) -> Result<()>;
    fn enable(&mut self, unit: &str, start: bool) -> Result<()>;
    fn disable(&mut self, unit: &str);
}

pub struct Systemd {
    unit: String,
}

impl Systemd {
    pub fn new(unit: &str) -> Systemd {
        Systemd { unit: unit.into() }
    }
}

fn systemctl(args: &[&str]) -> Result<String> {
    let out = Command::new("systemctl").args(args).stdin(Stdio::null()).output().context("running systemctl")?;
    if !out.status.success() {
        bail!("systemctl {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

impl Service for Systemd {
    fn stop(&mut self) -> Result<()> {
        systemctl(&["stop", &self.unit]).map(|_| ())
    }
    fn start(&mut self) -> Result<()> {
        systemctl(&["start", &self.unit]).map(|_| ())
    }
    fn main_pid(&mut self) -> Option<u32> {
        systemctl(&["show", "-p", "MainPID", "--value", &self.unit]).ok()?.parse().ok().filter(|p| *p != 0)
    }
    fn reload(&mut self) -> Result<()> {
        systemctl(&["daemon-reload"]).map(|_| ())
    }
    fn enable(&mut self, unit: &str, start: bool) -> Result<()> {
        systemctl(&["enable", "--quiet", unit])?;
        if start {
            // --no-block: never wait on a job that might be waiting on us.
            systemctl(&["start", "--no-block", unit])?;
        }
        Ok(())
    }
    fn disable(&mut self, unit: &str) {
        let _ = systemctl(&["disable", "--quiet", "--now", "--no-block", unit]);
    }
}

// ---- the installer (root) ------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Nothing,
    Updated(String),
    RolledBack(String),
}

pub struct Applier<'a> {
    pub p: &'a Profile,
    pub l: &'a Layout,
    pub svc: &'a mut dyn Service,
    pub t: Timing,
    pub key: Option<VerifyingKey>,
    pub target: &'a str,
}

impl Applier<'_> {
    /// pisowifi-update.service: finish an interrupted update, or install
    /// what the dashboard (or `pisowifi update`) staged.
    pub fn run_pending(&mut self) -> Result<Outcome> {
        ensure_dir(&self.l.root, 0o755)?;
        ensure_dir(&self.l.releases(), 0o755)?;
        let _lock = lock(&self.l.root)?;
        let _ = fs::remove_file(self.p.update_dir.join("request"));
        if let Some(meta) = read_json::<Meta>(&self.l.rollback().join("meta.json")) {
            return self.recover(meta);
        }
        if self.p.staged().is_dir() {
            return self.apply(None);
        }
        Ok(Outcome::Nothing)
    }

    /// install.sh's upgrade mode: an unpacked bundle folder, put in place by
    /// root. Same swap and rollback; no signature, since whoever runs it is
    /// root already. `allow_same` reinstalls the installed version (repair).
    pub fn install_dir(&mut self, dir: &Path, allow_same: bool) -> Result<Outcome> {
        ensure_dir(&self.l.root, 0o755)?;
        ensure_dir(&self.l.releases(), 0o755)?;
        let _lock = lock(&self.l.root)?;
        if let Some(meta) = read_json::<Meta>(&self.l.rollback().join("meta.json")) {
            self.recover(meta)?;
        }
        self.apply(Some((dir, allow_same)))
    }

    fn apply(&mut self, dir: Option<(&Path, bool)>) -> Result<Outcome> {
        let prepared = installed_version(&self.l.bin_link).and_then(|from| {
            self.adopt_legacy(&from.to_string())?;
            Ok((from, self.prepare(dir, from)?))
        });
        let (from, (new_dir, to)) = match prepared {
            Ok(x) => x,
            Err(e) => {
                let _ = fs::remove_dir_all(self.l.releases().join(INCOMING));
                let _ = fs::remove_dir_all(self.p.staged());
                self.set_state(Phase::Failed, "", "", &format!("{e:#}"));
                return Err(e);
            }
        };
        let (from, to) = (from.to_string(), to.to_string());
        tracing::info!("installing version {to} (now {from})");
        self.set_state(Phase::Installing, &from, &to, "Stopping the old version");
        if let Err(e) = self.svc.stop() {
            let _ = fs::remove_dir_all(self.l.root.join(&new_dir));
            let _ = fs::remove_dir_all(self.p.staged());
            let e = e.context("stopping the service; nothing was changed");
            self.set_state(Phase::Failed, &from, &to, &format!("{e:#}"));
            return Err(e);
        }
        let mut meta = Meta {
            from,
            to,
            new_dir,
            old_current: fs::read_link(self.l.current()).ok(),
            old_previous: fs::read_link(self.l.previous()).ok(),
            units_saved: false,
            units: vec![],
            data_saved: false,
        };
        if let Err(e) = self.switch(&mut meta) {
            return Ok(self.rollback(&meta, &format!("{e:#}")));
        }
        self.set_state(Phase::Trial, &meta.from, &meta.to, &format!("Starting version {}", meta.to));
        if let Err(e) = self.svc.start() {
            tracing::warn!("starting version {}: {e:#}", meta.to);
        }
        Ok(match self.wait_healthy(&meta.to) {
            Ok(()) => self.commit(&meta),
            Err(reason) => self.rollback(&meta, &reason),
        })
    }

    /// Unpacks and checks the new release. Nothing running is touched.
    fn prepare(&self, dir: Option<(&Path, bool)>, from: Version) -> Result<(PathBuf, Version)> {
        let incoming = self.l.releases().join(INCOMING);
        let _ = fs::remove_dir_all(&incoming);
        ensure_dir(&incoming, 0o755)?;
        let v = match dir {
            None => {
                let bundle = read_limited(&self.p.staged().join("bundle.tar.gz"), release::MAX_BUNDLE_BYTES)?;
                let expect = Expect { kind: self.p.kind, target: self.target, current: Some(from), allow_same: false };
                let verified = release::verify(&bundle, self.key.as_ref(), &expect)?;
                verified.unpack(self.p.kind, &incoming)?;
                Version::parse(&verified.manifest.version)?
            }
            Some((src, allow_same)) => {
                release::copy_dir(src, self.p.kind, &incoming)?;
                let v = run_version(&incoming.join("pisowifi"))?;
                if v < from || (v == from && !allow_same) {
                    bail!("Version {v} is not newer than the installed {from}. Nothing was changed.");
                }
                v
            }
        };
        // Right architecture, not truncated, and the version it claims to be.
        let ran = run_version(&incoming.join("pisowifi")).context("The new version doesn't run on this board. Nothing was changed.")?;
        if ran != v {
            bail!("The new binary says it is version {ran}, but the update says {v}. Nothing was changed.");
        }
        let stamp = crate::db::now();
        let mut rel = PathBuf::from("releases").join(format!("{v}-{stamp}"));
        for n in 1.. {
            if !self.l.root.join(&rel).exists() {
                break;
            }
            rel = PathBuf::from("releases").join(format!("{v}-{stamp}-{n}"));
        }
        fs::rename(&incoming, self.l.root.join(&rel))?;
        Ok((rel, v))
    }

    /// Installs from before this layout have the binary itself at
    /// /usr/local/bin/pisowifi: move it into releases/ so it can be rolled back to.
    fn adopt_legacy(&self, from: &str) -> Result<()> {
        let target = self.l.current().join("pisowifi");
        if fs::read_link(&self.l.bin_link).ok().as_deref() == Some(target.as_path()) && target.exists() {
            return Ok(());
        }
        let rel = PathBuf::from("releases").join(format!("{from}-installed"));
        let dir = self.l.root.join(&rel);
        if !dir.join("pisowifi").exists() {
            ensure_dir(&dir, 0o755)?;
            let data = fs::read(&self.l.bin_link).with_context(|| format!("reading {}", self.l.bin_link.display()))?;
            release::write_file(&dir.join("pisowifi"), &data, 0o755)?;
        }
        set_link(&self.l.current(), &rel)?;
        set_link(&self.l.bin_link, &target)
    }

    /// Saves what's needed to go back, then switches. The service is stopped.
    fn switch(&mut self, meta: &mut Meta) -> Result<()> {
        let rb = self.l.rollback();
        let _ = fs::remove_dir_all(&rb);
        ensure_dir(&rb, 0o700)?;
        ensure_dir(&rb.join("units"), 0o700)?;
        ensure_dir(&rb.join("data"), 0o700)?;
        self.save_meta(meta)?;
        for u in self.p.units() {
            let f = self.l.units_dir.join(u);
            if f.exists() {
                fs::copy(&f, rb.join("units").join(u))?;
                meta.units.push(u.into());
            }
        }
        meta.units_saved = true;
        self.save_meta(meta)?;
        copy_files(&self.p.data_dir, &rb.join("data")).context("copying the data folder")?;
        meta.data_saved = true;
        self.save_meta(meta)?;
        if let Some(c) = &meta.old_current {
            set_link(&self.l.previous(), c)?;
        }
        set_link(&self.l.current(), &meta.new_dir)?;
        self.install_units(&self.l.root.join(&meta.new_dir))?;
        clear_health(&self.p.data_dir);
        Ok(())
    }

    fn install_units(&mut self, release_dir: &Path) -> Result<()> {
        let mut changed = false;
        for u in self.p.units() {
            let Ok(new) = fs::read(release_dir.join(u)) else { continue };
            let dst = self.l.units_dir.join(u);
            if fs::read(&dst).ok().as_deref() != Some(&new[..]) {
                write_atomic(&dst, &new, 0o644)?;
                changed = true;
            }
        }
        if changed {
            self.svc.reload()?;
        }
        for u in self.p.units().filter(|u| wants_enable(u) && release_dir.join(u).exists()) {
            if let Err(e) = self.svc.enable(u, u.ends_with(".path")) {
                tracing::warn!("enabling {u}: {e:#}");
            }
        }
        Ok(())
    }

    fn commit(&mut self, meta: &Meta) -> Outcome {
        let failed = read_state(&self.l.state_path()).and_then(|s| s.failed_version).filter(|f| *f != meta.to);
        let msg = format!("Updated to version {}.", meta.to);
        self.write_state(State {
            phase: Phase::Done,
            from: meta.from.clone(),
            to: meta.to.clone(),
            message: msg.clone(),
            failed_version: failed,
            at: crate::db::now(),
        });
        let _ = fs::remove_dir_all(self.l.rollback());
        let _ = fs::remove_dir_all(self.p.staged());
        self.prune();
        tracing::info!("{msg}");
        Outcome::Updated(meta.to.clone())
    }

    fn rollback(&mut self, meta: &Meta, reason: &str) -> Outcome {
        tracing::error!("version {} failed ({reason}); going back to {}", meta.to, meta.from);
        self.set_state(Phase::RollingBack, &meta.from, &meta.to, &format!("Putting version {} back", meta.from));
        let mut problems = Vec::new();
        if let Err(e) = self.svc.stop() {
            problems.push(format!("stopping: {e:#}"));
        }
        if let Err(e) = self.restore_links(meta) {
            problems.push(format!("links: {e:#}"));
        }
        if meta.units_saved {
            if let Err(e) = self.restore_units(meta) {
                problems.push(format!("units: {e:#}"));
            }
        }
        if meta.data_saved {
            if let Err(e) = restore_files(&self.l.rollback().join("data"), &self.p.data_dir) {
                problems.push(format!("data: {e:#}"));
            }
        }
        if fs::read_link(self.l.current()).ok().as_deref() != Some(meta.new_dir.as_path()) {
            let _ = fs::remove_dir_all(self.l.root.join(&meta.new_dir));
        }
        clear_health(&self.p.data_dir);
        if let Err(e) = self.svc.start() {
            problems.push(format!("starting: {e:#}"));
        }
        let back = self.wait_healthy(&meta.from);
        let mut msg = format!(
            "Version {} couldn't be started ({reason}), so version {} was put back. \
             Settings, sales and customers' time are as they were when the update started.",
            meta.to, meta.from
        );
        if let Err(r) = &back {
            msg += &format!(" Version {} isn't reporting healthy either ({r}): check journalctl -u {}.", meta.from, self.p.service);
        }
        if !problems.is_empty() {
            msg += &format!(" Problems while going back: {}.", problems.join("; "));
        } else {
            let _ = fs::remove_dir_all(self.l.rollback());
        }
        self.write_state(State {
            phase: Phase::RolledBack,
            from: meta.from.clone(),
            to: meta.to.clone(),
            message: msg.clone(),
            failed_version: Some(meta.to.clone()),
            at: crate::db::now(),
        });
        let _ = fs::remove_dir_all(self.p.staged());
        tracing::warn!("{msg}");
        Outcome::RolledBack(msg)
    }

    fn restore_links(&self, meta: &Meta) -> Result<()> {
        if let Some(c) = &meta.old_current {
            set_link(&self.l.current(), c)?;
        }
        match &meta.old_previous {
            Some(p) => set_link(&self.l.previous(), p),
            None => {
                let _ = fs::remove_file(self.l.previous());
                Ok(())
            }
        }
    }

    fn restore_units(&mut self, meta: &Meta) -> Result<()> {
        let saved = self.l.rollback().join("units");
        for u in self.p.units() {
            let dst = self.l.units_dir.join(u);
            if meta.units.iter().any(|m| m == u) {
                let old = fs::read(saved.join(u))?;
                if fs::read(&dst).ok().as_deref() != Some(&old[..]) {
                    write_atomic(&dst, &old, 0o644)?;
                }
            } else if dst.exists() {
                // Brought by the failed release.
                self.svc.disable(u);
                fs::remove_file(&dst)?;
            }
        }
        self.svc.reload()
    }

    /// Something stopped an update halfway (power cut, killed updater).
    fn recover(&mut self, meta: Meta) -> Result<Outcome> {
        tracing::warn!("an update from {} to {} was interrupted; finishing it", meta.from, meta.to);
        if fs::read_link(self.l.current()).ok().as_deref() == Some(meta.new_dir.as_path()) {
            if self.svc.main_pid().is_none() {
                let _ = self.svc.start();
            }
            return Ok(match self.wait_healthy(&meta.to) {
                Ok(()) => self.commit(&meta),
                Err(r) => self.rollback(&meta, &r),
            });
        }
        Ok(self.rollback(&meta, "the update was interrupted before it started"))
    }

    /// Healthy = running, reporting this version from this process, firewall
    /// loaded, ports answering, and still the same process a little later.
    fn wait_healthy(&mut self, version: &str) -> Result<(), String> {
        let deadline = Instant::now() + self.t.trial;
        loop {
            let problem = match self.health_problem(version) {
                None => {
                    let pid = self.svc.main_pid();
                    std::thread::sleep(self.t.stable);
                    match self.health_problem(version) {
                        None if self.svc.main_pid() == pid => return Ok(()),
                        None => "it restarted right after starting".to_string(),
                        Some(p) => p,
                    }
                }
                Some(p) => p,
            };
            if Instant::now() >= deadline {
                return Err(problem);
            }
            std::thread::sleep(self.t.poll);
        }
    }

    fn health_problem(&mut self, version: &str) -> Option<String> {
        let Some(pid) = self.svc.main_pid() else { return Some("it stopped or kept restarting".into()) };
        let fresh = read_json::<Health>(&self.p.data_dir.join(HEALTH)).filter(|h| h.version == version && h.pid == pid);
        let Some(h) = fresh else { return Some("it didn't report healthy".into()) };
        if self.p.firewall && (!h.firewall || !nft_table_loaded()) {
            return Some("the firewall wasn't loaded".into());
        }
        self.p.listen.iter().find(|a| refused(**a)).map(|a| format!("nothing answers on {a}"))
    }

    fn prune(&self) {
        let keep: Vec<OsString> = [self.l.current(), self.l.previous()]
            .iter()
            .filter_map(|l| fs::read_link(l).ok())
            .filter_map(|t| t.file_name().map(|n| n.to_owned()))
            .collect();
        for e in fs::read_dir(self.l.releases()).into_iter().flatten().flatten() {
            if !keep.contains(&e.file_name()) {
                let _ = fs::remove_dir_all(e.path());
            }
        }
    }

    fn save_meta(&self, meta: &Meta) -> Result<()> {
        write_atomic(&self.l.rollback().join("meta.json"), &serde_json::to_vec_pretty(meta)?, 0o600)
    }

    fn set_state(&self, phase: Phase, from: &str, to: &str, message: &str) {
        let failed = read_state(&self.l.state_path()).and_then(|s| s.failed_version);
        let at = crate::db::now();
        self.write_state(State { phase, from: from.into(), to: to.into(), message: message.into(), failed_version: failed, at });
    }

    fn write_state(&self, s: State) {
        // Readable by the daemon, which shows it on the dashboard.
        let res = serde_json::to_vec_pretty(&s).map_err(anyhow::Error::from).and_then(|b| write_atomic(&self.l.state_path(), &b, 0o644));
        if let Err(e) = res {
            tracing::error!("writing update state: {e:#}");
        }
    }
}

fn installed_version(bin: &Path) -> Result<Version> {
    run_version(bin).with_context(|| format!("no working pisowifi at {}; run install.sh first", bin.display()))
}

/// `<bin> --version`, with a time limit.
fn run_version(bin: &Path) -> Result<Version> {
    let mut child = Command::new(bin)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("running {}", bin.display()))?;
    let deadline = Instant::now() + Duration::from_secs(10);
    while child.try_wait()?.is_none() {
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("{} --version didn't finish", bin.display());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let mut out = String::new();
    child.stdout.take().context("no output")?.take(256).read_to_string(&mut out)?;
    let v = out.trim().strip_prefix("pisowifi ").with_context(|| format!("{} isn't a pisowifi binary", bin.display()))?;
    Version::parse(v)
}

fn nft_table_loaded() -> bool {
    Command::new("nft")
        .args(["list", "table", "inet", "pisowifi"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// "Connection refused" means the address is ours and nothing listens. Any
/// other failure (LAN cable out, address not up yet) proves nothing either way.
fn refused(addr: SocketAddr) -> bool {
    let addr = match addr.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => SocketAddr::new(Ipv4Addr::LOCALHOST.into(), addr.port()),
        IpAddr::V6(ip) if ip.is_unspecified() => SocketAddr::new(Ipv6Addr::LOCALHOST.into(), addr.port()),
        _ => addr,
    };
    matches!(TcpStream::connect_timeout(&addr, Duration::from_secs(2)), Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused)
}

// ---- files -------------------------------------------------------------------------

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    let bytes = read_limited(path, 1 << 20).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn read_limited(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let len = fs::metadata(path).with_context(|| format!("{} not found", path.display()))?.len();
    if len > limit {
        bail!("{} is too large", path.display());
    }
    let mut data = Vec::with_capacity(len as usize);
    fs::File::open(path)?.take(limit).read_to_end(&mut data)?;
    Ok(data)
}

fn tmp_name(path: &Path) -> PathBuf {
    path.with_file_name(format!(".{}.tmp", path.file_name().unwrap_or_default().to_string_lossy()))
}

fn sync_parent(path: &Path) {
    if let Some(f) = path.parent().and_then(|d| fs::File::open(d).ok()) {
        let _ = f.sync_all();
    }
}

/// Write to a temporary name, then rename over: readers see old or new, never half.
fn write_atomic(path: &Path, data: &[u8], mode: u32) -> Result<()> {
    let tmp = tmp_name(path);
    let _ = fs::remove_file(&tmp);
    release::write_file(&tmp, data, mode)?;
    fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
    sync_parent(path);
    Ok(())
}

fn ensure_dir(path: &Path, mode: u32) -> Result<()> {
    fs::create_dir_all(path).with_context(|| format!("creating {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    let _ = mode;
    Ok(())
}

/// Atomically points `link` at `target` (an existing link is replaced in one step).
fn set_link(link: &Path, target: &Path) -> Result<()> {
    let tmp = tmp_name(link);
    let _ = fs::remove_file(&tmp);
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, &tmp).with_context(|| format!("linking {}", link.display()))?;
    #[cfg(not(unix))]
    bail!("symlinks need a Unix system ({})", target.display());
    fs::rename(&tmp, link).with_context(|| format!("replacing {}", link.display()))?;
    sync_parent(link);
    Ok(())
}

/// Copies a file with its mode and owner (the hub's data belongs to its
/// systemd DynamicUser), via a temporary name.
fn copy_preserving(from: &Path, to: &Path) -> Result<()> {
    let meta = fs::metadata(from)?;
    let tmp = tmp_name(to);
    let _ = fs::remove_file(&tmp);
    fs::copy(from, &tmp).with_context(|| format!("copying {}", from.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if let Err(e) = std::os::unix::fs::chown(&tmp, Some(meta.uid()), Some(meta.gid())) {
            // Only root can give files away; in tests they're ours already.
            if e.kind() != std::io::ErrorKind::PermissionDenied {
                return Err(e.into());
            }
        }
    }
    #[cfg(not(unix))]
    let _ = meta;
    fs::File::open(&tmp)?.sync_all()?;
    fs::rename(&tmp, to)?;
    Ok(())
}

fn data_files(dir: &Path) -> Result<Vec<OsString>> {
    let mut names = Vec::new();
    for e in fs::read_dir(dir)? {
        let e = e?;
        let name = e.file_name();
        if e.file_type()?.is_file() && name != HEALTH && !name.to_string_lossy().starts_with('.') {
            names.push(name);
        }
    }
    Ok(names)
}

/// The data folder's files (database, its WAL, salt, keys, certificate).
/// Sub-folders such as update/ are left out.
fn copy_files(src: &Path, dst: &Path) -> Result<()> {
    for name in data_files(src)? {
        copy_preserving(&src.join(&name), &dst.join(&name))?;
    }
    sync_parent(&dst.join("x"));
    Ok(())
}

/// Puts the data folder back exactly as saved: files the failed version
/// added (a new -wal, say) go, saved files are restored.
fn restore_files(saved: &Path, data_dir: &Path) -> Result<()> {
    let keep: HashSet<OsString> = data_files(saved)?.into_iter().collect();
    for name in data_files(data_dir)? {
        if !keep.contains(&name) {
            fs::remove_file(data_dir.join(&name))?;
        }
    }
    for name in &keep {
        copy_preserving(&saved.join(name), &data_dir.join(name))?;
    }
    sync_parent(&data_dir.join("x"));
    Ok(())
}

pub struct Lock(#[allow(dead_code)] fs::File);

/// One installer at a time.
fn lock(dir: &Path) -> Result<Lock> {
    let f = fs::OpenOptions::new().create(true).truncate(false).write(true).open(dir.join("lock"))?;
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        // SAFETY: flock on a file descriptor we own.
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            bail!("Another update is being installed right now.");
        }
    }
    Ok(Lock(f))
}

// ---- staging (daemon or CLI) ------------------------------------------------------------

/// Puts a verified bundle in <update_dir>/<sub>/bundle.tar.gz: "staged" is
/// what the installer picks up; "uploaded" waits for the owner to confirm.
fn stage_into(update_dir: &Path, sub: &str, bundle: &[u8]) -> Result<()> {
    ensure_dir(update_dir, 0o700)?;
    let tmp = update_dir.join(format!("{sub}.tmp"));
    let _ = fs::remove_dir_all(&tmp);
    ensure_dir(&tmp, 0o700)?;
    release::write_file(&tmp.join("bundle.tar.gz"), bundle, 0o600)?;
    let dst = update_dir.join(sub);
    let _ = fs::remove_dir_all(&dst);
    fs::rename(&tmp, &dst)?;
    sync_parent(&dst);
    Ok(())
}

pub fn stage(update_dir: &Path, bundle: &[u8]) -> Result<()> {
    stage_into(update_dir, "staged", bundle)
}

/// Wakes pisowifi-update.path.
fn request(update_dir: &Path) -> Result<()> {
    write_atomic(&update_dir.join("request"), b"", 0o600)
}

// ---- checking online ----------------------------------------------------------------------

/// The signed update list CI publishes in the public releases repository.
pub const CHANNEL_URL: &str = "https://raw.githubusercontent.com/cplus2jules/pisowifi-releases/main/channels/stable.json";

/// A custom update list address; empty means the default one.
pub fn clean_url(url: &str, dev: bool) -> Result<Option<String>> {
    let u = url.trim();
    if u.is_empty() {
        return Ok(None);
    }
    if u.len() > 300 || u.chars().any(|c| c.is_whitespace() || c.is_control()) {
        bail!("That doesn't look like a web address.");
    }
    if !(u.starts_with("https://") || (dev && u.starts_with("http://"))) {
        bail!("The update address must start with https://");
    }
    if !u.ends_with(".json") {
        bail!("The update address is the update list itself, ending in .json (for example {CHANNEL_URL}).");
    }
    Ok(Some(u.into()))
}

/// HTTPS only (plain http just for dev mode's local test server), redirects
/// included, with a size cap and timeouts so a slow or hostile server can't
/// hold the vendo up or fill its disk.
fn get(url: &str, limit: u64, total: Duration, dev: bool) -> Result<Vec<u8>> {
    let agent = ureq::AgentBuilder::new()
        .https_only(!dev)
        .timeout_connect(Duration::from_secs(15))
        .timeout_read(Duration::from_secs(60))
        .timeout(total)
        .redirects(5)
        .user_agent(concat!("pisowifi/", env!("CARGO_PKG_VERSION")))
        .build();
    match agent.get(url).call() {
        Ok(r) => {
            let mut data = Vec::new();
            r.into_reader().take(limit + 1).read_to_end(&mut data).context("The download was interrupted.")?;
            if data.len() as u64 > limit {
                bail!("The update server sent more than expected.");
            }
            Ok(data)
        }
        Err(ureq::Error::Status(404, _)) => bail!("Nothing has been published at the update address yet."),
        Err(ureq::Error::Status(code, _)) => bail!("The update server answered {code}."),
        Err(_) => bail!("Couldn't reach the update server (is this box online?)."),
    }
}

/// The update list, signature checked.
pub fn fetch_channel(url: &str, key: Option<&VerifyingKey>, dev: bool) -> Result<Channel> {
    let key = key.ok_or_else(release::no_key_error)?;
    let list = get(url, release::MAX_CHANNEL_BYTES, Duration::from_secs(30), dev)?;
    let sig = get(&format!("{url}.sig"), release::MAX_CHANNEL_SIG_BYTES, Duration::from_secs(30), dev)?;
    release::open_channel(&list, &sig, key)
}

/// A newer version for this box, found on the update list.
#[derive(Clone, Debug)]
pub struct Offer {
    pub version: Version,
    pub notes: String,
    pub issued_at: i64,
    pub file: ChannelFile,
    pub url: String,
}

/// The newest list this box has seen (its issued_at), kept in a file.
fn seen(path: &Path) -> i64 {
    fs::read_to_string(path).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0)
}

/// Asks the update list what's newest. None: up to date, or the release has
/// nothing for this board.
pub fn check_online(p: &Profile, url: &str, key: Option<&VerifyingKey>, current: Version, dev: bool) -> Result<Option<Offer>> {
    let c = fetch_channel(url, key, dev)?;
    if c.issued_at < seen(&p.seen) {
        bail!("The update server sent an older update list than before, so it was ignored. Try again later.");
    }
    let note = || -> Result<()> {
        match p.seen.parent() {
            Some(dir) if !dir.exists() => ensure_dir(dir, 0o700)?,
            _ => {}
        }
        write_atomic(&p.seen, format!("{}\n", c.issued_at).as_bytes(), 0o644)
    };
    // The hub's unprivileged daemon can't write here; its root-side installs do.
    if let Err(e) = note() {
        tracing::debug!("can't note the update list's date: {e:#}");
    }
    let Some(file) = c.file(p.kind, release::TARGET).cloned() else { return Ok(None) };
    let version = Version::parse(&c.version)?;
    if version <= current {
        return Ok(None);
    }
    Ok(Some(Offer { version, notes: c.notes, issued_at: c.issued_at, url: release::channel_file_url(url, &file.path)?, file }))
}

/// Downloads an offered bundle and checks everything: size and SHA-256
/// against the list, then the bundle's own signature, board and version.
pub fn download(p: &Profile, o: &Offer, key: Option<&VerifyingKey>, current: Version, dev: bool) -> Result<Vec<u8>> {
    let data = get(&o.url, o.file.size, Duration::from_secs(20 * 60), dev)?;
    if data.len() as u64 != o.file.size || !hex::encode(Sha256::digest(&data)).eq_ignore_ascii_case(&o.file.sha256) {
        bail!("The download doesn't match the update list (damaged or tampered with). Nothing was changed.");
    }
    let v = release::verify(&data, key, &Expect { kind: p.kind, target: release::TARGET, current: Some(current), allow_same: false })?;
    if Version::parse(&v.manifest.version)? != o.version {
        bail!("The download is a different version than the update list says. Nothing was changed.");
    }
    Ok(data)
}

// ---- `pisowifi update` -------------------------------------------------------------------

pub struct Args {
    pub check: bool,
    pub install: bool,
    /// (the hub's 3 AM timer) Like install, but quiet when up to date, and
    /// a version that had to be rolled back isn't tried again.
    pub auto: bool,
    pub file: Option<PathBuf>,
    /// Another update list address for --check / --install; on its own it installs.
    pub url: Option<String>,
    pub run_pending: bool,
    pub install_dir: Option<PathBuf>,
    pub allow_same: bool,
}

fn update_cmd(kind: Kind) -> &'static str {
    match kind {
        Kind::Vendo => "pisowifi update",
        Kind::Hub => "pisowifi hub update",
    }
}

pub fn cli(p: &Profile, a: Args, dev: bool, configured_url: &dyn Fn() -> Result<Option<String>>) -> Result<()> {
    let l = Layout::system();
    let key = release::trusted_key();
    let cmd = update_cmd(p.kind);
    if a.run_pending || a.install_dir.is_some() {
        if dev {
            bail!("Installing happens on the vendo or hub itself, not in dev mode.");
        }
        need_root()?;
        let mut svc = Systemd::new(&p.service);
        let mut ap = Applier { p, l: &l, svc: &mut svc, t: Timing::default(), key, target: release::TARGET };
        let out = match &a.install_dir {
            Some(d) => ap.install_dir(d, a.allow_same)?,
            None => ap.run_pending()?,
        };
        return match out {
            Outcome::Nothing => Ok(()),
            Outcome::Updated(v) => {
                println!("Updated to version {v}.");
                Ok(())
            }
            Outcome::RolledBack(msg) => bail!("{msg}"),
        };
    }
    // Compared with what's installed, not with this binary (it may be a
    // newer one, run straight from a bundle).
    let current = installed_version(&l.bin_link).unwrap_or_else(|_| Version::current());
    let expect = Expect { kind: p.kind, target: release::TARGET, current: Some(current), allow_same: false };
    let list_url = || -> Result<Option<String>> {
        match a.url.as_deref().filter(|u| !u.is_empty()) {
            Some(u) => clean_url(u, dev),
            None => configured_url(),
        }
    };
    let online = a.check || a.install || a.auto || a.url.is_some();
    if a.file.is_none() && !online {
        print_status(&l, current)?;
        match list_url() {
            Ok(Some(url)) => match check_online(p, &url, key.as_ref(), current, dev) {
                Ok(Some(o)) => println!("Version {} is available. Install it with: sudo {cmd} --install", o.version),
                Ok(None) => println!("Up to date with the update list."),
                Err(e) => println!("Couldn't check online: {e}"),
            },
            Ok(None) => println!("Online checks are off."),
            Err(e) => println!("Couldn't check online: {e}"),
        }
        return Ok(());
    }
    let bundle = if let Some(file) = &a.file {
        read_limited(file, release::MAX_BUNDLE_BYTES)?
    } else {
        let url = list_url()?.ok_or_else(|| match p.kind {
            Kind::Vendo => anyhow::anyhow!("Online checks are off. Turn them on in the dashboard (Settings → Update), or install a file: sudo {cmd} --file <bundle>"),
            Kind::Hub => anyhow::anyhow!("Online checks are off (update_url = \"\" in hub.toml). Install a file instead: sudo {cmd} --file <bundle>"),
        })?;
        let Some(o) = check_online(p, &url, key.as_ref(), current, dev)? else {
            if !a.auto {
                println!("Up to date: version {current}.");
            }
            return Ok(());
        };
        println!("Version {} is available (installed: {current}).", o.version);
        if !o.notes.is_empty() {
            println!("\n{}\n", o.notes);
        }
        if a.check {
            println!("Install it with: sudo {cmd} --install");
            return Ok(());
        }
        let failed = read_state(&l.state_path()).and_then(|s| s.failed_version);
        if a.auto && failed.as_deref() == Some(o.version.to_string().as_str()) {
            println!("Version {} couldn't start last time, so it isn't installed automatically. Install it by hand: sudo {cmd} --install", o.version);
            return Ok(());
        }
        println!("Downloading version {} ({:.1} MB)…", o.version, o.file.size as f64 / 1e6);
        download(p, &o, key.as_ref(), current, dev)?
    };
    let v = release::verify(&bundle, key.as_ref(), &expect)?;
    println!("Checked: version {} for {}, signed with your release key.", v.manifest.version, v.manifest.target);
    if a.file.is_some() && !v.manifest.notes.is_empty() {
        println!("\n{}\n", v.manifest.notes);
    }
    if dev {
        println!("Dev mode: nothing is installed on this computer.");
        return Ok(());
    }
    need_root()?;
    if systemctl(&["cat", &p.update_service]).is_err() {
        bail!("The updater isn't set up on this machine yet. Run the installer from the unpacked bundle once (it keeps everything).");
    }
    stage(&p.update_dir, &bundle)?;
    let since = crate::db::now();
    systemctl(&["start", "--no-block", &p.update_service])?;
    println!("Installing. Customers stay online; this takes about a minute.");
    follow(&l, since)
}

fn need_root() -> Result<()> {
    #[cfg(unix)]
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        bail!("Run this as root (sudo).");
    }
    Ok(())
}

fn print_status(l: &Layout, current: Version) -> Result<()> {
    println!("Installed: version {current} ({})", release::TARGET);
    if release::trusted_key().is_none() {
        println!("{}", release::no_key_error());
    }
    match read_state(&l.state_path()) {
        Some(s) if s.phase != Phase::Idle => println!("Last update: {}", s.message),
        _ => println!("No updates installed yet."),
    }
    Ok(())
}

/// Prints the installer's progress until it's done.
fn follow(l: &Layout, since: i64) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(10 * 60);
    let mut last = String::new();
    loop {
        if let Some(s) = read_state(&l.state_path()).filter(|s| s.at >= since) {
            if s.message != last {
                println!("  {}", s.message);
                last = s.message.clone();
            }
            match s.phase {
                Phase::Done => return Ok(()),
                Phase::RolledBack | Phase::Failed => bail!("The update was not installed."),
                _ => {}
            }
        }
        if Instant::now() > deadline {
            bail!("Still installing after 10 minutes; see journalctl -u pisowifi-update");
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

// ---- the dashboard's side (vendo daemon) ------------------------------------------------------

/// "0" turns online checks off; they're on unless the owner says otherwise.
pub const ONLINE_KEY: &str = "update_online";
/// Another update list address; empty means the default one.
pub const URL_KEY: &str = "update_url";
pub const AUTO_KEY: &str = "update_auto";

/// The vendo's update list address from its settings; None while online checks are off.
pub fn vendo_url(db: &Db) -> Result<Option<String>> {
    if db.get_setting(ONLINE_KEY)?.as_deref() == Some("0") {
        return Ok(None);
    }
    Ok(Some(db.get_setting(URL_KEY)?.filter(|u| !u.is_empty()).unwrap_or_else(|| CHANNEL_URL.into())))
}

/// The hub's, from update_url in hub.toml: left out means the default, "" means off.
pub fn hub_url(setting: Option<&str>, dev: bool) -> Result<Option<String>> {
    match setting {
        None => Ok(Some(CHANNEL_URL.into())),
        Some(u) if u.trim().is_empty() => Ok(None),
        Some(u) => clean_url(u, dev),
    }
}

/// When to look again: about a day after a good check, sooner after a
/// failed one, spread out so a fleet of vendos doesn't ask all at once.
fn next_check(ok: bool) -> i64 {
    use rand::Rng;
    let mut r = rand::thread_rng();
    crate::db::now() + if ok { 22 * 3600 + r.gen_range(0..4 * 3600) } else { 3600 + r.gen_range(0..3600) }
}

#[derive(Default)]
struct Status {
    last_check: Option<i64>,
    next_check: i64,
    check_error: Option<String>,
    /// A newer version on the update list.
    found: Option<Offer>,
    /// An uploaded file that checked out, waiting for Install now.
    ready: Option<Manifest>,
    /// "checking", "downloading" or "waiting" (staged; the installer takes over).
    busy: Option<&'static str>,
    error: Option<String>,
    auto_day: Option<chrono::NaiveDate>,
}

pub struct Updater {
    p: Profile,
    dev: bool,
    db: Arc<Db>,
    key: Option<VerifyingKey>,
    state_path: PathBuf,
    st: Mutex<Status>,
}

fn summary(m: &Manifest) -> Value {
    json!({ "version": m.version, "notes": m.notes, "created": m.created })
}

impl Updater {
    pub fn new(p: Profile, dev: bool, db: Arc<Db>) -> Arc<Updater> {
        // An upload nobody confirmed before a restart is dropped.
        let _ = fs::remove_dir_all(p.update_dir.join("uploaded"));
        let state_path = if dev { p.update_dir.join("state.json") } else { Layout::system().state_path() };
        Arc::new(Updater { p, dev, db, key: release::trusted_key(), state_path, st: Mutex::new(Status::default()) })
    }

    /// The update list address; None while online checks are off.
    pub fn url(&self) -> Option<String> {
        vendo_url(&self.db).ok().flatten()
    }

    pub fn auto(&self) -> bool {
        self.db.get_setting(AUTO_KEY).ok().flatten().as_deref() == Some("1")
    }

    pub fn set_settings(&self, online: bool, auto: bool, url: &str) -> Result<(), String> {
        let url = clean_url(url, self.dev).map_err(|e| e.to_string())?;
        let custom = self.db.get_setting(URL_KEY).ok().flatten().unwrap_or_default();
        self.db.set_setting(ONLINE_KEY, if online { "1" } else { "0" }).map_err(|e| e.to_string())?;
        self.db.set_setting(URL_KEY, url.as_deref().unwrap_or("")).map_err(|e| e.to_string())?;
        self.db.set_setting(AUTO_KEY, if auto && online { "1" } else { "0" }).map_err(|e| e.to_string())?;
        let mut s = self.st.lock().unwrap();
        if !online || custom != url.unwrap_or_default() {
            // What was found came from elsewhere; look again soon.
            (s.found, s.check_error, s.last_check, s.next_check) = (None, None, None, 0);
        }
        Ok(())
    }

    pub fn view(&self) -> Value {
        let current = Version::current();
        let s = self.st.lock().unwrap();
        let available = s.found.as_ref().filter(|o| o.version > current).map(|o| {
            json!({ "version": o.version.to_string(), "notes": o.notes, "issued_at": o.issued_at, "size": o.file.size })
        });
        json!({
            "current": current.to_string(),
            "signed": self.key.is_some(),
            "ready": s.ready.as_ref().map(summary),
            "available": available,
            "online": self.url().is_some(),
            "url": self.db.get_setting(URL_KEY).ok().flatten().unwrap_or_default(),
            "default_url": CHANNEL_URL,
            "last_check": s.last_check,
            "check_error": s.check_error,
            "busy": s.busy,
            "error": s.error,
            "auto": self.auto(),
            "state": read_state(&self.state_path).filter(|s| s.phase != Phase::Idle),
        })
    }

    fn set_busy(&self, b: Option<&'static str>) {
        self.st.lock().unwrap().busy = b;
    }

    /// The owner uploaded a bundle (a vendo with no internet): check it now,
    /// keep it until they tap Install now. Nothing is installed yet.
    pub fn upload(&self, bundle: &[u8]) -> Result<Manifest, String> {
        if self.st.lock().unwrap().busy.is_some() {
            return Err("An update is already on its way in.".into());
        }
        let expect = Expect { kind: self.p.kind, target: release::TARGET, current: Some(Version::current()), allow_same: false };
        let v = release::verify(bundle, self.key.as_ref(), &expect).map_err(|e| e.to_string())?;
        stage_into(&self.p.update_dir, "uploaded", bundle).map_err(|e| format!("{e:#}"))?;
        let mut s = self.st.lock().unwrap();
        s.ready = Some(v.manifest.clone());
        s.error = None;
        Ok(v.manifest)
    }

    /// The owner tapped Check now: an old download message is out of date.
    pub fn clear_error(&self) {
        self.st.lock().unwrap().error = None;
    }

    pub fn cancel_upload(&self) {
        let _ = fs::remove_dir_all(self.p.update_dir.join("uploaded"));
        self.st.lock().unwrap().ready = None;
    }

    /// Asks the update list what's newest. Failures are quiet: shown in the
    /// card, logged at info level.
    pub async fn check(self: &Arc<Self>) {
        let Some(url) = self.url() else { return };
        {
            let mut s = self.st.lock().unwrap();
            if s.busy.is_some() {
                return;
            }
            s.busy = Some("checking");
        }
        let me = self.clone();
        let res = tokio::task::spawn_blocking(move || check_online(&me.p, &url, me.key.as_ref(), Version::current(), me.dev)).await;
        let mut s = self.st.lock().unwrap();
        s.busy = None;
        s.last_check = Some(crate::db::now());
        let res = res.map_err(anyhow::Error::from).and_then(|r| r);
        s.next_check = next_check(res.is_ok());
        match res {
            Ok(found) => {
                if let Some(o) = &found {
                    if s.found.as_ref().is_none_or(|f| f.version != o.version) {
                        tracing::info!("update: version {} is available", o.version);
                    }
                }
                s.found = found;
                s.check_error = None;
            }
            Err(e) => {
                tracing::info!("update check: {e}");
                s.check_error = Some(e.to_string());
            }
        }
    }

    /// Install now: the uploaded file if there is one, otherwise the newest
    /// version on the update list. The installer takes it from there.
    pub fn start_install(self: &Arc<Self>, actor: &str, auto: bool) -> Result<(), String> {
        if self.key.is_none() {
            return Err(release::no_key_error().to_string());
        }
        let mut s = self.st.lock().unwrap();
        if s.busy.is_some() {
            return Err("Already working on an update.".into());
        }
        if !auto {
            if let Some(m) = s.ready.clone() {
                if self.dev {
                    return Err(format!("Dev mode: version {} checks out. Installing happens only on the vendo.", m.version));
                }
                let dir = &self.p.update_dir;
                let _ = fs::remove_dir_all(dir.join("staged"));
                fs::rename(dir.join("uploaded"), dir.join("staged")).map_err(|e| format!("{e}"))?;
                request(dir).map_err(|e| format!("{e:#}"))?;
                s.ready = None;
                s.busy = Some("waiting");
                s.error = None;
                drop(s);
                self.db.audit(actor, "update_install", &format!("version {} (uploaded file)", m.version));
                self.watch_installer();
                return Ok(());
            }
        }
        if self.url().is_none() {
            return Err("Online checks are off. Choose the update file instead.".into());
        }
        s.busy = Some("checking");
        s.error = None;
        drop(s);
        let (me, actor) = (self.clone(), actor.to_string());
        tokio::spawn(async move {
            let me2 = me.clone();
            let res = tokio::task::spawn_blocking(move || me2.download_and_stage(auto)).await.map_err(anyhow::Error::from).and_then(|r| r);
            match res {
                Ok(v) => {
                    me.db.audit(&actor, "update_install", &format!("version {v}"));
                    me.set_busy(Some("waiting"));
                    me.watch_installer();
                }
                Err(e) => {
                    let msg = format!("{e:#}");
                    tracing::warn!("update: {msg}");
                    me.db.audit(&actor, "update_failed", &msg);
                    let mut s = me.st.lock().unwrap();
                    s.busy = None;
                    s.error = Some(msg);
                }
            }
        });
        Ok(())
    }

    /// The installer stops this process within seconds. Still here later?
    fn watch_installer(self: &Arc<Self>) {
        let me = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(180)).await;
            let mut s = me.st.lock().unwrap();
            if s.busy == Some("waiting") {
                s.busy = None;
                s.error = Some("The installer didn't start. On the vendo run: sudo pisowifi update --status".into());
            }
        });
    }

    fn download_and_stage(&self, auto: bool) -> Result<String> {
        let url = self.url().context("online checks are off")?;
        // Asked again, so Install always gets the newest.
        let current = Version::current();
        let Some(o) = check_online(&self.p, &url, self.key.as_ref(), current, self.dev)? else {
            self.st.lock().unwrap().found = None;
            bail!("Already up to date (version {current}).");
        };
        let failed = read_state(&self.state_path).and_then(|s| s.failed_version);
        if auto && failed.as_deref() == Some(o.version.to_string().as_str()) {
            bail!("Version {} couldn't start last time, so it isn't installed automatically. Tap Install now to try again.", o.version);
        }
        self.set_busy(Some("downloading"));
        let bundle = download(&self.p, &o, self.key.as_ref(), current, self.dev)?;
        let v = o.version;
        self.st.lock().unwrap().found = Some(o);
        if self.dev {
            bail!("Dev mode: version {v} downloaded and checks out. Installing happens only on the vendo.");
        }
        stage(&self.p.update_dir, &bundle)?;
        request(&self.p.update_dir)?;
        Ok(v.to_string())
    }
}

/// Checks the update list a couple of minutes after start and about once a
/// day after that, and does the optional 3 AM install (only while no one
/// is inserting coins).
pub fn spawn_daily(u: Arc<Updater>, engine: Arc<Engine>) {
    use chrono::Timelike;
    use rand::Rng;
    tokio::spawn(async move {
        let first = if u.dev { 10 } else { rand::thread_rng().gen_range(60..240) };
        tokio::time::sleep(Duration::from_secs(first)).await;
        let mut tick = tokio::time::interval(Duration::from_secs(10 * 60));
        loop {
            tick.tick().await;
            if u.url().is_none() {
                continue;
            }
            let due = u.st.lock().unwrap().next_check <= crate::db::now();
            if due {
                u.check().await;
            }
            let now = chrono::Local::now();
            let today = now.date_naive();
            let ready = {
                let s = u.st.lock().unwrap();
                let newer = s.found.as_ref().is_some_and(|o| o.version > Version::current());
                newer && s.busy.is_none() && s.auto_day != Some(today)
            };
            if ready && now.hour() == 3 && u.auto() && engine.slot_admin().state == "idle" && !engine.counting() {
                u.st.lock().unwrap().auto_day = Some(today);
                if let Err(e) = u.start_install("system", true) {
                    tracing::info!("automatic update: {e}");
                }
            }
        }
    });
}

/// The hub has no dashboard: it checks the same way and says so in its log.
/// Installing is `sudo pisowifi hub update --install`, or the optional
/// pisowifi-hub-autoupdate.timer.
pub fn spawn_hub_checks(p: Profile, url: Option<String>, dev: bool) {
    use rand::Rng;
    let Some(url) = url else { return };
    tokio::spawn(async move {
        let p = Arc::new(p);
        let mut wait = if dev { 10 } else { rand::thread_rng().gen_range(60..240) };
        loop {
            tokio::time::sleep(Duration::from_secs(wait)).await;
            let (p2, url2) = (p.clone(), url.clone());
            let res = tokio::task::spawn_blocking(move || check_online(&p2, &url2, release::trusted_key().as_ref(), Version::current(), dev))
                .await
                .map_err(anyhow::Error::from)
                .and_then(|r| r);
            match &res {
                Ok(Some(o)) => tracing::info!("update: version {} is available. Install it with: sudo pisowifi hub update --install", o.version),
                Ok(None) => tracing::debug!("update: up to date"),
                Err(e) => tracing::info!("update check: {e}"),
            }
            wait = (next_check(res.is_ok()) - crate::db::now()).max(60) as u64;
        }
    });
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::db::{Batch, ClientRow};
    use crate::keys::DbKey;
    use crate::net::Mac;
    use crate::release::tests::{signed_bundle, test_key, TARGET_ARM};
    use std::process::Child;
    use zeroize::Zeroizing;

    /// Stands in for systemd: runs whatever /usr/local/bin/pisowifi points
    /// at, as a child process.
    struct FakeSystemd {
        bin: PathBuf,
        data: PathBuf,
        child: Option<Child>,
        calls: Vec<String>,
    }

    impl Service for FakeSystemd {
        fn stop(&mut self) -> Result<()> {
            if let Some(mut c) = self.child.take() {
                let _ = c.kill();
                let _ = c.wait();
            }
            self.calls.push("stop".into());
            Ok(())
        }
        fn start(&mut self) -> Result<()> {
            self.stop()?;
            self.calls.pop();
            let child = Command::new(&self.bin)
                .arg("run")
                .env("HEALTH", self.data.join(HEALTH))
                .env("DATA", &self.data)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?;
            self.child = Some(child);
            self.calls.push("start".into());
            Ok(())
        }
        fn main_pid(&mut self) -> Option<u32> {
            let c = self.child.as_mut()?;
            c.try_wait().ok().flatten().is_none().then(|| c.id())
        }
        fn reload(&mut self) -> Result<()> {
            self.calls.push("reload".into());
            Ok(())
        }
        fn enable(&mut self, unit: &str, start: bool) -> Result<()> {
            self.calls.push(format!("enable {unit}{}", if start { " --now" } else { "" }));
            Ok(())
        }
        fn disable(&mut self, unit: &str) {
            self.calls.push(format!("disable {unit}"));
        }
    }

    impl Drop for FakeSystemd {
        fn drop(&mut self) {
            let _ = self.stop();
        }
    }

    /// A "binary" that reports healthy and keeps running.
    fn good(version: &str) -> Vec<u8> {
        format!(
            "#!/bin/sh\n[ \"$1\" = --version ] && {{ echo \"pisowifi {version}\"; exit 0; }}\n\
             printf '{{\"version\":\"{version}\",\"pid\":%s,\"started\":0,\"firewall\":false}}' $$ > \"$HEALTH.tmp\"\n\
             mv \"$HEALTH.tmp\" \"$HEALTH\"\nexec sleep 60\n"
        )
        .into_bytes()
    }

    /// One that damages the database on its way down and never comes up.
    fn bad(version: &str) -> Vec<u8> {
        format!(
            "#!/bin/sh\n[ \"$1\" = --version ] && {{ echo \"pisowifi {version}\"; exit 0; }}\n\
             echo garbage >> \"$DATA/pisowifi.db\"\necho junk > \"$DATA/pisowifi.db-wal\"\nexit 1\n"
        )
        .into_bytes()
    }

    const KEY: [u8; 32] = [7u8; 32];
    const PHONE: &str = "02:11:22:33:44:55";

    struct Box_ {
        dir: PathBuf,
        layout: Layout,
        profile: Profile,
        svc: FakeSystemd,
    }

    impl Drop for Box_ {
        fn drop(&mut self) {
            let _ = self.svc.stop();
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    /// A vendo as install.sh (before this change) left it: the 0.1.0 binary
    /// at bin/pisowifi, one unit, a database with a sale and a customer online.
    fn installed_box() -> Box_ {
        let dir = std::env::temp_dir().join(format!("pisowifi-update-{}", rand::random::<u64>()));
        let layout = Layout { root: dir.join("lib"), bin_link: dir.join("bin/pisowifi"), units_dir: dir.join("units") };
        let data = dir.join("data");
        for d in [&dir.join("bin"), &layout.units_dir, &data] {
            fs::create_dir_all(d).unwrap();
        }
        release::write_file(&layout.bin_link, &good("0.1.0"), 0o755).unwrap();
        fs::write(layout.units_dir.join("pisowifi.service"), "old unit").unwrap();
        let db = Db::open(&data.join("pisowifi.db"), &DbKey(Zeroizing::new(KEY))).unwrap();
        db.add_sale(Some(Mac::parse(PHONE).unwrap()), 20, 7200, "coin").unwrap();
        let client = ClientRow { mac: Mac::parse(PHONE).unwrap(), ip: "10.0.0.9".parse().unwrap(), remaining_secs: 5000, paused: false, away: false };
        db.save(&Batch { clients: vec![client], ..Default::default() }).unwrap();
        drop(db);
        fs::write(data.join("db.salt"), [1u8; 32]).unwrap();
        let profile = Profile {
            kind: Kind::Vendo,
            service: "pisowifi.service".into(),
            update_service: "pisowifi-update.service".into(),
            data_dir: data.clone(),
            update_dir: data.join("update"),
            listen: vec![],
            firewall: false,
            seen: data.join("update/list-seen"),
        };
        let mut svc = FakeSystemd { bin: layout.bin_link.clone(), data, child: None, calls: vec![] };
        svc.start().unwrap();
        Box_ { dir, layout, profile, svc }
    }

    fn fast() -> Timing {
        Timing { trial: Duration::from_secs(3), stable: Duration::from_millis(300), poll: Duration::from_millis(50) }
    }

    /// Stages a bundle signed the way CI signs releases, as the dashboard would.
    fn stage_release(b: &Box_, binary: &[u8], version: &str) {
        let files: [(&str, &[u8]); 4] =
            [("pisowifi", binary), ("pisowifi.service", b"new unit"), ("pisowifi-update.path", b"path unit"), ("install.sh", b"#!/bin/sh")];
        stage(&b.profile.update_dir, &signed_bundle(&test_key(1), Kind::Vendo, TARGET_ARM, version, "0.1.0", &files)).unwrap();
        request(&b.profile.update_dir).unwrap();
    }

    fn run_pending(b: &mut Box_) -> Result<Outcome> {
        let Box_ { layout, profile, svc, .. } = b;
        Applier { p: profile, l: layout, svc, t: fast(), key: Some(test_key(1).verifying_key()), target: TARGET_ARM }.run_pending()
    }

    fn link(b: &Box_, name: &str) -> String {
        fs::read_link(b.layout.root.join(name)).unwrap().to_string_lossy().into_owned()
    }

    fn running_version(b: &mut Box_) -> String {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let pid = b.svc.main_pid().expect("the service is running");
            match read_json::<Health>(&b.profile.data_dir.join(HEALTH)) {
                Some(h) if h.pid == pid => return h.version,
                _ if Instant::now() > deadline => panic!("no health report"),
                _ => std::thread::sleep(Duration::from_millis(20)),
            }
        }
    }

    fn releases(b: &Box_) -> Vec<String> {
        let mut v: Vec<_> = fs::read_dir(b.layout.releases()).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
        v.sort();
        v
    }

    fn assert_data_intact(b: &Box_) {
        assert!(!b.profile.data_dir.join("pisowifi.db-wal").exists(), "the failed version's WAL is gone");
        let db = Db::open(&b.profile.data_dir.join("pisowifi.db"), &DbKey(Zeroizing::new(KEY))).expect("database opens with the board's key");
        let clients = db.load_clients().unwrap();
        assert_eq!(clients.len(), 1, "the customer's session survived");
        assert_eq!(clients[0].remaining_secs, 5000);
        assert_eq!(db.sales_summary(14).unwrap().all_time_pesos, 20, "the sale survived");
    }

    #[test]
    fn a_good_update_is_installed_and_the_old_one_kept() {
        let mut b = installed_box();
        assert_eq!(running_version(&mut b), "0.1.0");
        stage_release(&b, &good("0.2.0"), "0.2.0");
        assert_eq!(run_pending(&mut b).unwrap(), Outcome::Updated("0.2.0".into()));

        assert_eq!(running_version(&mut b), "0.2.0");
        assert!(link(&b, "current").starts_with("releases/0.2.0-"));
        assert_eq!(link(&b, "previous"), "releases/0.1.0-installed");
        assert_eq!(fs::read_link(&b.layout.bin_link).unwrap(), b.layout.root.join("current/pisowifi"));
        assert_eq!(run_version(&b.layout.bin_link).unwrap(), Version(0, 2, 0));
        assert_eq!(fs::read(b.layout.units_dir.join("pisowifi.service")).unwrap(), b"new unit");
        assert!(b.svc.calls.contains(&"enable pisowifi-update.path --now".to_string()));
        assert!(!b.layout.root.join("rollback").exists() && !b.profile.staged().exists());
        assert!(!b.profile.update_dir.join("request").exists());
        assert_eq!(releases(&b).len(), 2);
        let st = read_state(&b.layout.state_path()).unwrap();
        assert_eq!((st.phase, st.failed_version), (Phase::Done, None));
        assert_data_intact(&b);
        // Nothing staged: nothing to do.
        assert_eq!(run_pending(&mut b).unwrap(), Outcome::Nothing);
    }

    /// The dev-mode rollback drill: a release that crashes and damages the
    /// database is undone, and the shop is back as it was.
    #[test]
    fn a_broken_update_is_rolled_back_with_its_data() {
        let mut b = installed_box();
        stage_release(&b, &bad("0.2.0"), "0.2.0");
        let out = run_pending(&mut b).unwrap();
        let Outcome::RolledBack(msg) = out else { panic!("{out:?}") };
        assert!(msg.contains("version 0.1.0 was put back"), "{msg}");

        assert_eq!(running_version(&mut b), "0.1.0");
        assert_eq!(link(&b, "current"), "releases/0.1.0-installed");
        assert!(!b.layout.root.join("previous").exists());
        assert_eq!(run_version(&b.layout.bin_link).unwrap(), Version(0, 1, 0));
        assert_eq!(releases(&b), ["0.1.0-installed"], "the failed release is removed");
        assert_eq!(fs::read(b.layout.units_dir.join("pisowifi.service")).unwrap(), b"old unit");
        assert!(!b.layout.units_dir.join("pisowifi-update.path").exists());
        assert!(b.svc.calls.contains(&"disable pisowifi-update.path".to_string()));
        assert_data_intact(&b);
        let st = read_state(&b.layout.state_path()).unwrap();
        assert_eq!(st.phase, Phase::RolledBack);
        assert_eq!(st.failed_version.as_deref(), Some("0.2.0"));
        assert!(!b.layout.root.join("rollback").exists() && !b.profile.staged().exists());

        // A fixed 0.2.1 goes in normally afterwards.
        stage_release(&b, &good("0.2.1"), "0.2.1");
        assert_eq!(run_pending(&mut b).unwrap(), Outcome::Updated("0.2.1".into()));
        assert_eq!(read_state(&b.layout.state_path()).unwrap().failed_version.as_deref(), Some("0.2.0"));
    }

    #[test]
    fn a_bad_signature_or_old_version_touches_nothing() {
        let mut b = installed_box();
        let pid = b.svc.main_pid();
        // Signed by someone else.
        let foreign = signed_bundle(&test_key(2), Kind::Vendo, TARGET_ARM, "9.9.9", "0.0.0", &[("pisowifi", &good("9.9.9")[..])]);
        stage(&b.profile.update_dir, &foreign).unwrap();
        assert!(run_pending(&mut b).unwrap_err().to_string().contains("isn't signed"));
        // Same version as installed.
        stage_release(&b, &good("0.1.0"), "0.1.0");
        assert!(run_pending(&mut b).unwrap_err().to_string().contains("not newer"));
        // Claims 0.2.0 but the binary inside says otherwise.
        stage_release(&b, &good("0.1.5"), "0.2.0");
        assert!(run_pending(&mut b).unwrap_err().to_string().contains("says it is version 0.1.5"));

        assert_eq!(b.svc.main_pid(), pid, "the running version was never stopped");
        assert_eq!(read_state(&b.layout.state_path()).unwrap().phase, Phase::Failed);
        assert_eq!(releases(&b), ["0.1.0-installed"]);
        assert!(!b.profile.staged().exists());
        assert_data_intact(&b);
    }

    #[test]
    fn an_update_cut_off_by_a_power_failure_is_undone_at_boot() {
        let mut b = installed_box();
        stage_release(&b, &bad("0.2.0"), "0.2.0");
        // Run the first half by hand: switched to 0.2.0, then the power went.
        {
            let Box_ { layout, profile, svc, .. } = &mut b;
            let ap = Applier { p: profile, l: layout, svc, t: fast(), key: Some(test_key(1).verifying_key()), target: TARGET_ARM };
            ap.adopt_legacy("0.1.0").unwrap();
            let (new_dir, _) = ap.prepare(None, Version(0, 1, 0)).unwrap();
            let mut meta = Meta {
                from: "0.1.0".into(),
                to: "0.2.0".into(),
                new_dir,
                old_current: fs::read_link(layout.current()).ok(),
                old_previous: None,
                units_saved: false,
                units: vec![],
                data_saved: false,
            };
            let mut ap = Applier { p: profile, l: layout, svc, t: fast(), key: None, target: TARGET_ARM };
            ap.svc.stop().unwrap();
            ap.switch(&mut meta).unwrap();
        }
        // "Boot": systemd starts the (broken) new version, then the updater runs.
        b.svc.start().unwrap();
        std::thread::sleep(Duration::from_millis(200));
        let out = run_pending(&mut b).unwrap();
        assert!(matches!(out, Outcome::RolledBack(_)), "{out:?}");
        assert_eq!(running_version(&mut b), "0.1.0");
        assert_eq!(link(&b, "current"), "releases/0.1.0-installed");
        assert_data_intact(&b);
    }

    #[test]
    fn install_sh_upgrade_mode_uses_the_same_swap() {
        let mut b = installed_box();
        let src = b.dir.join("bundle");
        fs::create_dir_all(&src).unwrap();
        release::write_file(&src.join("pisowifi"), &good("0.3.0"), 0o755).unwrap();
        fs::write(src.join("pisowifi.service"), "new unit").unwrap();
        let Box_ { layout, profile, svc, .. } = &mut b;
        let mut ap = Applier { p: profile, l: layout, svc, t: fast(), key: None, target: TARGET_ARM };
        assert_eq!(ap.install_dir(&src, false).unwrap(), Outcome::Updated("0.3.0".into()));
        // The same version again only with allow_same (repair); older never.
        assert!(ap.install_dir(&src, false).unwrap_err().to_string().contains("not newer"));
        assert_eq!(ap.install_dir(&src, true).unwrap(), Outcome::Updated("0.3.0".into()));
        release::write_file(&src.join("old"), &good("0.2.0"), 0o755).unwrap();
        fs::rename(src.join("old"), src.join("pisowifi")).unwrap();
        assert!(ap.install_dir(&src, true).is_err());
        assert_eq!(running_version(&mut b), "0.3.0");
    }

    #[test]
    fn only_a_refused_connection_counts_as_not_listening() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        assert!(!refused(addr));
        drop(l);
        assert!(refused(addr));
        assert!(!refused("192.0.2.1:9".parse().unwrap()), "unreachable proves nothing");
    }

    #[test]
    fn update_addresses_must_be_https() {
        assert_eq!(clean_url(" ", false).unwrap(), None, "empty means the usual list");
        assert_eq!(clean_url(" https://example.com/rel/stable.json ", false).unwrap().as_deref(), Some("https://example.com/rel/stable.json"));
        assert!(clean_url("https://example.com/rel/", false).is_err(), "the list itself, not a folder");
        assert!(clean_url("http://example.com/stable.json", false).is_err());
        assert!(clean_url("http://127.0.0.1:9000/stable.json", true).is_ok());
        assert!(clean_url("https://a b/stable.json", false).is_err());
        assert!(clean_url("file:///etc/stable.json", false).is_err());
        assert_eq!(hub_url(None, false).unwrap().as_deref(), Some(CHANNEL_URL));
        assert_eq!(hub_url(Some(""), false).unwrap(), None);
    }

    /// A tiny web server for the update list tests. Serves files under
    /// `root`, and remembers what was asked for.
    fn serve(root: PathBuf) -> (String, Arc<Mutex<Vec<String>>>) {
        use std::io::Write;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let asked = Arc::new(Mutex::new(Vec::new()));
        let log = asked.clone();
        std::thread::spawn(move || {
            for mut conn in listener.incoming().flatten() {
                let mut req = Vec::new();
                let mut buf = [0u8; 1024];
                while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                    match conn.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => req.extend_from_slice(&buf[..n]),
                    }
                }
                let line = String::from_utf8_lossy(&req).lines().next().unwrap_or_default().to_string();
                let path = line.split(' ').nth(1).unwrap_or("/").to_string();
                log.lock().unwrap().push(path.clone());
                let _ = match fs::read(root.join(path.trim_start_matches('/'))) {
                    Ok(body) => conn
                        .write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).as_bytes())
                        .and_then(|_| conn.write_all(&body)),
                    Err(_) => conn.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"),
                };
            }
        });
        (base, asked)
    }

    /// The releases repository as CI publishes it, served locally.
    struct Published {
        dir: PathBuf,
        url: String,
        asked: Arc<Mutex<Vec<String>>>,
        profile: Profile,
        bundle_path: PathBuf,
    }

    impl Drop for Published {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    fn publish(version: &str) -> Published {
        let (dir, _, _) = release::tests::signed_release(&test_key(1), version, release::TARGET);
        let ver_dir = dir.join(format!("v{version}"));
        fs::rename(dir.join("dist"), &ver_dir).unwrap();
        let (base, asked) = serve(dir.clone());
        let data = dir.join("data");
        let profile = Profile {
            kind: Kind::Vendo,
            service: "pisowifi.service".into(),
            update_service: "pisowifi-update.service".into(),
            data_dir: data.clone(),
            update_dir: data.join("update"),
            listen: vec![],
            firewall: false,
            seen: data.join("update/list-seen"),
        };
        let bundle_path = ver_dir.join(release::asset_name(Kind::Vendo, release::TARGET));
        Published { url: format!("{base}/channels/stable.json"), dir, asked, profile, bundle_path }
    }

    #[test]
    fn a_vendo_finds_a_new_version_and_downloads_only_its_own_bundle() {
        let pb = publish("0.2.0");
        let key = test_key(1).verifying_key();
        let o = check_online(&pb.profile, &pb.url, Some(&key), Version(0, 1, 0), true).unwrap().expect("0.2.0 is newer");
        assert_eq!((o.version, o.notes.as_str()), (Version(0, 2, 0), "Faster coin counting."));
        let bundle = download(&pb.profile, &o, Some(&key), Version(0, 1, 0), true).unwrap();
        assert_eq!(bundle, fs::read(&pb.bundle_path).unwrap());
        let asset = release::asset_name(Kind::Vendo, release::TARGET);
        assert_eq!(*pb.asked.lock().unwrap(), ["/channels/stable.json", "/channels/stable.json.sig", &format!("/v0.2.0/{asset}")]);
        // Already on it: nothing to offer, nothing downloaded.
        assert!(check_online(&pb.profile, &pb.url, Some(&key), Version(0, 2, 0), true).unwrap().is_none());
        assert!(check_online(&pb.profile, &pb.url, Some(&key), Version(0, 3, 0), true).unwrap().is_none());
        // Another key's list is refused.
        let err = check_online(&pb.profile, &pb.url, Some(&test_key(2).verifying_key()), Version(0, 1, 0), true).unwrap_err();
        assert!(err.to_string().contains("isn't signed"), "{err}");
    }

    #[test]
    fn an_old_list_a_changed_download_or_plain_http_is_refused() {
        let pb = publish("0.2.0");
        let key = test_key(1).verifying_key();
        let o = check_online(&pb.profile, &pb.url, Some(&key), Version(0, 1, 0), true).unwrap().unwrap();
        // Replaying an older (validly signed) list could hide an update.
        fs::write(&pb.profile.seen, format!("{}\n", o.issued_at + 1)).unwrap();
        let err = check_online(&pb.profile, &pb.url, Some(&key), Version(0, 1, 0), true).unwrap_err();
        assert!(err.to_string().contains("older update list"), "{err}");
        // A changed file of the same size, or a longer one, is caught before its signature is even read.
        let good = fs::read(&pb.bundle_path).unwrap();
        let mut changed = good.clone();
        *changed.last_mut().unwrap() ^= 1;
        fs::write(&pb.bundle_path, &changed).unwrap();
        assert!(download(&pb.profile, &o, Some(&key), Version(0, 1, 0), true).unwrap_err().to_string().contains("doesn't match"));
        fs::write(&pb.bundle_path, [good.as_slice(), b"extra"].concat()).unwrap();
        assert!(download(&pb.profile, &o, Some(&key), Version(0, 1, 0), true).unwrap_err().to_string().contains("more than expected"));
        // Outside dev mode only HTTPS is used.
        fs::remove_file(&pb.profile.seen).unwrap();
        assert!(check_online(&pb.profile, &pb.url, Some(&key), Version(0, 1, 0), false).is_err());
        // An older installed version is still refused by the bundle check itself.
        fs::write(&pb.bundle_path, &good).unwrap();
        assert!(download(&pb.profile, &o, Some(&key), Version(0, 2, 0), true).unwrap_err().to_string().contains("not newer"));
    }
}
