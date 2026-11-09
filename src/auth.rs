//! Owner authentication.
//!
//! * Password hashed with Argon2id (OWASP parameters), stored in the
//!   encrypted DB. It can only be set from the device's shell, never from
//!   the web, so a customer can't "claim" an unconfigured box.
//! * Login attempts are rate limited per IP, and box-wide: past 20 failures
//!   in 15 minutes (from any addresses) only one attempt per 30 s is allowed,
//!   so rotating IPs/MACs doesn't help. Only one hash runs at a time.
//! * Guessable passwords (top-10k list, sequences, the WiFi name) are refused.
//! * Session tokens are 256-bit random; only their SHA-256 is kept (in RAM).
//! * Cookies are HttpOnly + Secure + SameSite=Strict, and every write also
//!   needs a per-session CSRF token header.

use anyhow::{anyhow, Result};
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::{Algorithm, Argon2, Params, Version};
use rand::RngCore;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use subtle_eq::ct_eq;
use tokio::sync::Semaphore;

pub const PASSWORD_KEY: &str = "admin_password";
const MAX_FAILS: u32 = 5;
const LOCKOUT: Duration = Duration::from_secs(15 * 60);
const IDLE_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const GLOBAL_MAX_FAILS: u32 = 20;
const GLOBAL_WINDOW: Duration = Duration::from_secs(15 * 60);
const GLOBAL_SLOW: Duration = Duration::from_secs(30);
const MAX_TRACKED_IPS: usize = 4096;
const ARGON_M_KIB: u32 = 64 * 1024;
const ARGON_T: u32 = 3;

fn argon2() -> Argon2<'static> {
    // 64 MiB, 3 passes (~1-2 s on an H3; hashes run one at a time, so RAM is fine).
    Argon2::new(Algorithm::Argon2id, Version::V0x13, Params::new(ARGON_M_KIB, ARGON_T, 1, None).unwrap())
}

/// Checks the password policy, then hashes. `local_words`: the portal name
/// and SSID, which must not be the basis of the password.
pub fn hash_password(pw: &str, local_words: &[&str]) -> Result<String> {
    crate::pwpolicy::check(pw, local_words).map_err(|e| anyhow!("{e}"))?;
    hash_unchecked(pw)
}

/// Hash without the policy check: used to upgrade an existing hash on login.
fn hash_unchecked(pw: &str) -> Result<String> {
    let salt = SaltString::generate(&mut rand::rngs::OsRng);
    Ok(argon2().hash_password(pw.as_bytes(), &salt).map_err(|e| anyhow!("{e}"))?.to_string())
}

/// Verifies, and if the stored hash uses older (weaker) Argon2 settings,
/// returns a fresh hash to store in its place.
pub fn verify_and_upgrade(pw: &str, phc: &str) -> (bool, Option<String>) {
    let Ok(h) = PasswordHash::new(phc) else { return (false, None) };
    if Argon2::default().verify_password(pw.as_bytes(), &h).is_err() {
        return (false, None);
    }
    let weaker = Params::try_from(&h).map(|p| p.m_cost() < ARGON_M_KIB || p.t_cost() < ARGON_T).unwrap_or(true);
    (true, if weaker { hash_unchecked(pw).ok() } else { None })
}

pub fn verify_password(pw: &str, phc: &str) -> bool {
    verify_and_upgrade(pw, phc).0
}

struct Session {
    ip: IpAddr,
    csrf: String,
    created: Instant,
    last_seen: Instant,
}

pub struct Auth {
    sessions: Mutex<HashMap<[u8; 32], Session>>,
    fails: Mutex<HashMap<IpAddr, (u32, Instant)>>,
    global: Mutex<GlobalFails>,
    pub hashing: Semaphore,
    max_age: Duration,
}

pub struct SessionInfo {
    pub csrf: String,
}

struct GlobalFails {
    window_start: Instant,
    count: u32,
    last_try: Option<Instant>,
}

pub enum LoginCheck {
    Allowed,
    LockedOut(u64),
    /// Box-wide slow mode is on.
    Throttled(u64),
}

impl Auth {
    pub fn new(session_hours: u64) -> Auth {
        Auth {
            sessions: Mutex::new(HashMap::new()),
            fails: Mutex::new(HashMap::new()),
            global: Mutex::new(GlobalFails { window_start: Instant::now(), count: 0, last_try: None }),
            hashing: Semaphore::new(1),
            max_age: Duration::from_secs(session_hours * 3600),
        }
    }

    pub fn max_age_secs(&self) -> u64 {
        self.max_age.as_secs()
    }

    /// Call before every login attempt.
    pub fn check_rate(&self, ip: IpAddr) -> LoginCheck {
        {
            let mut f = self.fails.lock().unwrap();
            if let Some((n, last)) = f.get(&ip).copied() {
                let age = last.elapsed();
                if age > LOCKOUT {
                    f.remove(&ip);
                } else if n >= MAX_FAILS {
                    return LoginCheck::LockedOut((LOCKOUT - age).as_secs());
                }
            }
        }
        let mut g = self.global.lock().unwrap();
        if g.window_start.elapsed() > GLOBAL_WINDOW {
            *g = GlobalFails { window_start: Instant::now(), count: 0, last_try: None };
        }
        if g.count >= GLOBAL_MAX_FAILS {
            if let Some(t) = g.last_try.filter(|t| t.elapsed() < GLOBAL_SLOW) {
                return LoginCheck::Throttled((GLOBAL_SLOW - t.elapsed()).as_secs().max(1));
            }
            g.last_try = Some(Instant::now());
        }
        LoginCheck::Allowed
    }

    /// Returns true the moment box-wide slow mode switches on (for the audit log).
    pub fn record_fail(&self, ip: IpAddr) -> bool {
        {
            let mut f = self.fails.lock().unwrap();
            if f.len() >= MAX_TRACKED_IPS && !f.contains_key(&ip) {
                f.retain(|_, (_, last)| last.elapsed() < LOCKOUT);
                if f.len() >= MAX_TRACKED_IPS {
                    // Still full of recent failures: forget the oldest half.
                    let mut ages: Vec<(IpAddr, Instant)> = f.iter().map(|(k, (_, t))| (*k, *t)).collect();
                    ages.sort_by_key(|(_, t)| *t);
                    for (k, _) in ages.iter().take(ages.len() / 2) {
                        f.remove(k);
                    }
                }
            }
            let e = f.entry(ip).or_insert((0, Instant::now()));
            e.0 += 1;
            e.1 = Instant::now(); // lockout runs from the latest failure
        }
        let mut g = self.global.lock().unwrap();
        if g.window_start.elapsed() > GLOBAL_WINDOW {
            *g = GlobalFails { window_start: Instant::now(), count: 0, last_try: None };
        }
        g.count += 1;
        g.count == GLOBAL_MAX_FAILS
    }

    /// Creates a session and returns (cookie token, csrf token).
    pub fn create(&self, ip: IpAddr) -> (String, String) {
        self.fails.lock().unwrap().remove(&ip);
        let token = random_hex(32);
        let csrf = random_hex(32);
        let now = Instant::now();
        let mut s = self.sessions.lock().unwrap();
        s.retain(|_, v| v.created.elapsed() < self.max_age && v.last_seen.elapsed() < IDLE_TIMEOUT);
        s.insert(digest(&token), Session { ip, csrf: csrf.clone(), created: now, last_seen: now });
        (token, csrf)
    }

    pub fn check(&self, token: &str, ip: IpAddr) -> Option<SessionInfo> {
        let mut s = self.sessions.lock().unwrap();
        let key = digest(token);
        let sess = s.get_mut(&key)?;
        if sess.created.elapsed() >= self.max_age || sess.last_seen.elapsed() >= IDLE_TIMEOUT || sess.ip != ip {
            s.remove(&key);
            return None;
        }
        sess.last_seen = Instant::now();
        Some(SessionInfo { csrf: sess.csrf.clone() })
    }

    pub fn csrf_ok(info: &SessionInfo, header: Option<&str>) -> bool {
        header.is_some_and(|h| ct_eq(h.as_bytes(), info.csrf.as_bytes()))
    }

    pub fn destroy(&self, token: &str) {
        self.sessions.lock().unwrap().remove(&digest(token));
    }

    /// Logs everyone out (after a password change).
    pub fn destroy_all(&self) {
        self.sessions.lock().unwrap().clear();
    }
}

fn digest(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

fn random_hex(n: usize) -> String {
    let mut b = vec![0u8; n];
    rand::rngs::OsRng.fill_bytes(&mut b);
    hex::encode(b)
}

mod subtle_eq {
    /// Constant-time comparison for secrets of equal length.
    pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
        if a.len() != b.len() {
            return false;
        }
        a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_roundtrip() {
        let h = hash_password("mango tricycle lamp ocean", &[]).unwrap();
        assert!(h.starts_with("$argon2id$v=19$m=65536,t=3"));
        assert!(verify_password("mango tricycle lamp ocean", &h));
        assert!(!verify_password("wrong", &h));
        assert!(hash_password("short", &[]).is_err());
        assert!(hash_password("password1234", &[]).is_err());
    }

    #[test]
    fn old_hashes_still_verify_and_get_upgraded() {
        let old = Argon2::new(Algorithm::Argon2id, Version::V0x13, Params::new(19 * 1024, 2, 1, None).unwrap())
            .hash_password(b"old owner password", &SaltString::generate(&mut rand::rngs::OsRng))
            .unwrap()
            .to_string();
        let (ok, new) = verify_and_upgrade("old owner password", &old);
        assert!(ok);
        let new = new.expect("weaker hash should be upgraded");
        assert!(verify_and_upgrade("old owner password", &new) == (true, None));
    }

    #[test]
    fn box_wide_slow_mode_beats_ip_rotation() {
        let a = Auth::new(1);
        let mut engaged = false;
        for i in 0..GLOBAL_MAX_FAILS {
            let ip: IpAddr = format!("10.0.{}.{}", i / 200, i % 200 + 2).parse().unwrap();
            assert!(matches!(a.check_rate(ip), LoginCheck::Allowed));
            engaged |= a.record_fail(ip);
        }
        assert!(engaged);
        let fresh: IpAddr = "10.0.9.9".parse().unwrap();
        assert!(matches!(a.check_rate(fresh), LoginCheck::Allowed), "one try per 30 s");
        assert!(matches!(a.check_rate(fresh), LoginCheck::Throttled(_)));
    }

    #[test]
    fn sessions_bind_to_ip_and_lock_out() {
        let a = Auth::new(1);
        let ip: IpAddr = "10.0.0.5".parse().unwrap();
        let (tok, csrf) = a.create(ip);
        let info = a.check(&tok, ip).unwrap();
        assert!(Auth::csrf_ok(&info, Some(&csrf)));
        assert!(!Auth::csrf_ok(&info, Some("nope")));
        assert!(a.check(&tok, "10.0.0.6".parse().unwrap()).is_none());
        for _ in 0..MAX_FAILS {
            a.record_fail(ip);
        }
        assert!(matches!(a.check_rate(ip), LoginCheck::LockedOut(_)));
    }
}
