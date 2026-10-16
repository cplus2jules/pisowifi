//! Refuses dashboard passwords that an attacker on the customer WiFi could
//! guess: the 10k most common passwords (and those with digits tacked on),
//! repeated or sequential characters, keyboard runs, and passwords built
//! from the WiFi, store or product name.

use std::collections::HashSet;
use std::sync::OnceLock;

pub const MIN_LEN: usize = 12;

pub const GUIDANCE: &str = "Use 4 or more random words (e.g. \"mango tricycle lamp ocean\") or 14+ random \
characters. Don't reuse a password from anywhere else, and never the old LPB password: \
LPB kept it in plain text on the SD card.";

const COMMON_LIST: &str = include_str!("../data/common-passwords.txt");

const KEYBOARD_RUNS: &[&str] = &[
    "1234567890",
    "qwertyuiop",
    "asdfghjkl",
    "zxcvbnm",
    "qwertyuiopasdfghjklzxcvbnm",
    "1qaz2wsx3edc4rfv5tgb6yhn7ujm8ik9ol0p",
    "qazwsxedcrfvtgbyhnujmikolp",
    "abcdefghijklmnopqrstuvwxyz",
];

const LOCAL_WORDS: &[&str] = &["pisowifi", "piso", "wifi", "admin", "password", "lpb", "vendo"];

fn common() -> &'static HashSet<&'static str> {
    static SET: OnceLock<HashSet<&'static str>> = OnceLock::new();
    SET.get_or_init(|| COMMON_LIST.lines().map(str::trim).filter(|l| !l.is_empty()).collect())
}

/// `extra_words`: names specific to this box (portal name, SSID).
pub fn check(pw: &str, extra_words: &[&str]) -> Result<(), String> {
    if pw.chars().count() < MIN_LEN {
        return Err(format!("Use at least {MIN_LEN} characters."));
    }
    let lower = pw.to_lowercase();
    let compact: String = lower.chars().filter(|c| !c.is_whitespace()).collect();
    let chars: Vec<char> = compact.chars().collect();

    if chars.windows(2).all(|w| w[0] == w[1]) {
        return Err("That's one character repeated. Pick something harder to guess.".into());
    }
    if is_sequence(&chars) {
        return Err("Sequences like 1234567890 are among the first things guessed.".into());
    }
    if mostly_run(&chars) {
        return Err("Keyboard runs like qwertyuiop or 1234567890 are among the first things guessed.".into());
    }
    if is_repeated_chunk(&chars) {
        return Err("Repeating the same part over and over is easy to guess.".into());
    }
    // "Password2024!" → "password": common words with digits or symbols bolted on.
    let core = compact
        .trim_end_matches(|c: char| c.is_ascii_digit() || !c.is_alphanumeric())
        .trim_start_matches(|c: char| c.is_ascii_digit() || !c.is_alphanumeric());
    if common().contains(compact.as_str()) || (core.len() >= 4 && common().contains(core)) {
        return Err("That password is on the list of the most common passwords.".into());
    }
    let mut words: Vec<String> = LOCAL_WORDS.iter().map(|w| w.to_string()).collect();
    for w in extra_words {
        let w: String = w.to_lowercase().chars().filter(|c| c.is_alphanumeric()).collect();
        if w.len() >= 4 {
            words.push(w);
        }
    }
    let alnum: String = compact.chars().filter(|c| c.is_alphanumeric()).collect();
    for w in &words {
        if alnum.contains(w.as_str()) && alnum.len().saturating_sub(w.len()) < 8 {
            return Err("Don't build the password from the WiFi, store or product name.".into());
        }
    }
    Ok(())
}

/// Every step is +1 or every step is -1 (wrapping 9→0), e.g. 3456789012.
fn is_sequence(chars: &[char]) -> bool {
    let step = |a: char, b: char| -> Option<i32> {
        match (a.to_digit(10), b.to_digit(10)) {
            (Some(x), Some(y)) => Some(((y as i32 - x as i32) + 10) % 10),
            _ if a.is_ascii_lowercase() && b.is_ascii_lowercase() => Some(b as i32 - a as i32),
            _ => None,
        }
    };
    let steps: Option<Vec<i32>> = chars.windows(2).map(|w| step(w[0], w[1])).collect();
    match steps {
        Some(s) if !s.is_empty() => s.iter().all(|&d| d == 1) || s.iter().all(|&d| d == 9 || d == -1),
        _ => false,
    }
}

/// True when a keyboard run of 6+ characters makes up all but < 6 of it.
fn mostly_run(chars: &[char]) -> bool {
    let runs: Vec<Vec<char>> = KEYBOARD_RUNS
        .iter()
        .flat_map(|r| [r.chars().collect::<Vec<_>>(), r.chars().rev().collect()])
        .collect();
    let n = chars.len();
    for len in (6..=n).rev() {
        if n - len >= 6 {
            break;
        }
        for start in 0..=n - len {
            let w = &chars[start..start + len];
            if runs.iter().any(|r| r.windows(len).any(|rw| rw == w)) {
                return true;
            }
        }
    }
    false
}

fn is_repeated_chunk(chars: &[char]) -> bool {
    let n = chars.len();
    (1..=n / 2).any(|k| n % k == 0 && chars.chunks(k).all(|c| c == &chars[..k]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_guessable_passwords() {
        for bad in [
            "password123",
            "Password1234!",
            "1234567890ab",
            "123456789012",
            "0987654321098",
            "aaaaaaaaaaaaaa",
            "qwertyuiopasdf",
            "abcabcabcabc",
            "iloveyou2025!",
            "PisoWiFi2024!",
            "Aling Nena 2024",
            "short",
        ] {
            assert!(check(bad, &["Aling Nena", "PisoWiFi-Nena"]).is_err(), "{bad} should be rejected");
        }
    }

    #[test]
    fn accepts_strong_passwords() {
        for good in ["mango tricycle lamp ocean", "q7#Vt9!pLm2@Xz", "correct horse battery staple"] {
            assert!(check(good, &["Aling Nena"]).is_ok(), "{good} should be accepted");
        }
    }

    #[test]
    fn list_is_loaded() {
        assert!(common().len() > 9000);
        assert!(common().contains("iloveyou"));
    }
}
