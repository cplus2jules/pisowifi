//! Coin acceptor I/O through the Linux GPIO character device (/dev/gpiochipN).
//! Each pulse from the acceptor arrives as one `()` on the channel.
//!
//! On the H3 only the PA and PG banks can raise interrupts. A coin wire on
//! any other header pin (PC, PD) is polled every millisecond instead.

use crate::config::Coin;
use anyhow::Result;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use tokio::sync::mpsc;

/// Orange Pi One / PC 40-pin header: (physical pin, H3 pin name, line offset
/// on gpiochip0). Only the pins usable as GPIO are listed.
pub const ORANGE_PI_ONE_HEADER: &[(u8, &str, u32)] = &[
    (3, "PA12", 12), (5, "PA11", 11), (7, "PA6", 6), (8, "PA13", 13),
    (10, "PA14", 14), (11, "PA1", 1), (12, "PD14", 110), (13, "PA0", 0),
    (15, "PA3", 3), (16, "PC4", 68), (18, "PC7", 71), (19, "PC0", 64),
    (21, "PC1", 65), (22, "PA2", 2), (23, "PC2", 66), (24, "PC3", 67),
    (26, "PA21", 21), (27, "PA19", 19), (28, "PA18", 18), (29, "PA7", 7),
    (31, "PA8", 8), (32, "PG8", 200), (33, "PA9", 9), (35, "PA10", 10),
    (36, "PG9", 201), (37, "PA20", 20), (38, "PG6", 198), (40, "PG7", 199),
];

pub fn describe_line(offset: u32) -> String {
    match ORANGE_PI_ONE_HEADER.iter().find(|(_, _, o)| *o == offset) {
        Some((pin, name, _)) => format!("line {offset} = {name}, header pin {pin}"),
        None => format!("line {offset} (not on the 40-pin header)"),
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use super::*;
    use gpiocdev::line::{Bias, EdgeDetection, Value};
    use gpiocdev::Request;
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    /// Reads pulses forever. `ok` says whether the line is being read, so the
    /// portal can refuse coins (instead of losing them) while it isn't.
    pub fn spawn_reader(cfg: &Coin, tx: mpsc::UnboundedSender<()>, ok: Arc<AtomicBool>) {
        let cfg = cfg.clone();
        std::thread::Builder::new()
            .name("coin-reader".into())
            .spawn(move || loop {
                match read_loop(&cfg, &tx, &ok) {
                    Ok(()) => return,
                    Err(e) => {
                        ok.store(false, Ordering::Relaxed);
                        tracing::error!("coin slot: {e:#}; retrying in 5s (the portal refuses coins until then)");
                        std::thread::sleep(Duration::from_secs(5));
                    }
                }
            })
            .expect("spawn coin reader");
    }

    fn input(cfg: &Coin, edges: bool) -> gpiocdev::Result<Request> {
        let mut b = Request::builder();
        b.on_chip(&cfg.chip).with_consumer("pisowifi-coin").with_line(cfg.line).as_input().with_bias(Bias::PullUp);
        if edges {
            // With active-low, "rising" means the acceptor started a pulse.
            b.with_debounce_period(Duration::from_millis(cfg.debounce_ms))
                .with_edge_detection(EdgeDetection::RisingEdge)
                .with_kernel_event_buffer_size(256);
        }
        if cfg.active_low {
            b.as_active_low();
        }
        b.request()
    }

    fn read_loop(cfg: &Coin, tx: &mpsc::UnboundedSender<()>, ok: &AtomicBool) -> Result<()> {
        let req = match input(cfg, true) {
            Ok(r) => r,
            Err(e) => {
                // No interrupts on this pin (or no debounce support in an old
                // kernel): poll it. A busy or missing line fails here too.
                let r = input(cfg, false)?;
                tracing::warn!("coin slot: {} can't signal edges ({e}); polling it every 1 ms", describe_line(cfg.line));
                return poll_loop(cfg, &r, tx, ok);
            }
        };
        tracing::info!("coin slot listening on {} {}", cfg.chip, describe_line(cfg.line));
        ok.store(true, Ordering::Relaxed);
        loop {
            req.read_edge_event()?;
            if tx.send(()).is_err() {
                return Ok(());
            }
        }
    }

    /// Software debounce: a level counts once it has held for `debounce_ms`.
    fn poll_loop(cfg: &Coin, req: &Request, tx: &mpsc::UnboundedSender<()>, ok: &AtomicBool) -> Result<()> {
        let debounce = Duration::from_millis(cfg.debounce_ms);
        let mut stable = req.value(cfg.line)?;
        let (mut seen, mut since) = (stable, Instant::now());
        tracing::info!("coin slot polling {} {}", cfg.chip, describe_line(cfg.line));
        ok.store(true, Ordering::Relaxed);
        loop {
            std::thread::sleep(Duration::from_millis(1));
            let v = req.value(cfg.line)?;
            let now = Instant::now();
            if v != seen {
                (seen, since) = (v, now);
            } else if v != stable && now - since >= debounce {
                stable = v;
                if stable == Value::Active && tx.send(()).is_err() {
                    return Ok(());
                }
            }
        }
    }

    /// Output that turns the acceptor on only while a customer is inserting.
    pub struct Enable(Request);

    impl Enable {
        pub fn open(cfg: &Coin) -> Result<Option<Enable>> {
            let Some(line) = cfg.enable_line else { return Ok(None) };
            let mut b = Request::builder();
            b.on_chip(&cfg.chip).with_consumer("pisowifi-coin-enable").with_line(line).as_output(Value::Inactive);
            if cfg.enable_active_low {
                b.as_active_low();
            }
            Ok(Some(Enable(b.request()?)))
        }

        pub fn set(&self, on: bool, line: u32) {
            let v = if on { Value::Active } else { Value::Inactive };
            if let Err(e) = self.0.set_value(line, v) {
                tracing::error!("coin enable line: {e}");
            }
        }
    }

    /// One header pin being watched by `probe`.
    struct Watch {
        pin: u8,
        name: &'static str,
        line: u32,
        req: Request,
        edges: bool,
        last: Value,
        /// Current burst: pulse count, first/last edge, shortest/longest low time.
        pulses: u32,
        low_since: Option<Instant>,
        last_edge: Option<Instant>,
        widths: (u128, u128),
    }

    /// Watches every header pin (each on its own, so a busy pin doesn't hide
    /// the rest) and prints each burst of pulses: which pin, how many pulses,
    /// and how wide they are.
    pub fn probe(chip: &str) -> Result<()> {
        let mut watches = vec![];
        for &(pin, name, line) in ORANGE_PI_ONE_HEADER {
            let edges = Request::builder()
                .on_chip(chip)
                .with_consumer("pisowifi-probe")
                .with_line(line)
                .as_input()
                .with_edge_detection(EdgeDetection::BothEdges)
                .request()
                .is_ok();
            let req = Request::builder().on_chip(chip).with_consumer("pisowifi-probe").with_line(line).as_input().with_bias(Bias::PullUp).request();
            match req {
                Ok(req) => {
                    let last = req.value(line)?;
                    watches.push(Watch { pin, name, line, req, edges, last, pulses: 0, low_since: None, last_edge: None, widths: (u128::MAX, 0) });
                }
                Err(e) => println!("  header pin {pin:>2} ({name}): can't watch it ({e}); in use by something else?"),
            }
        }
        println!("Watching {} header pins on {chip}. Drop a few coins (Ctrl-C to stop)…", watches.len());
        println!("(If nothing shows up, check the acceptor has power and its COIN wire goes to a header pin.)\n");
        loop {
            std::thread::sleep(Duration::from_millis(1));
            let now = Instant::now();
            for w in &mut watches {
                let v = w.req.value(w.line)?;
                if v != w.last {
                    w.last = v;
                    w.last_edge = Some(now);
                    // The acceptor pulls the wire low for each pulse.
                    match v {
                        Value::Inactive => {
                            w.pulses += 1;
                            w.low_since = Some(now);
                        }
                        Value::Active => {
                            if let Some(t) = w.low_since.take() {
                                let ms = (now - t).as_millis();
                                w.widths = (w.widths.0.min(ms), w.widths.1.max(ms));
                            }
                        }
                    }
                }
                if w.pulses > 0 && w.last_edge.is_some_and(|t| now - t > Duration::from_millis(400)) {
                    let (lo, hi) = w.widths;
                    let width = if hi == 0 { String::new() } else { format!(", each {lo}–{hi} ms long") };
                    let polled = if w.edges { "" } else { " (no interrupts on this pin: pisowifi will poll it)" };
                    println!(
                        "header pin {} ({}): {} pulse{}{width} → set [coin] line = {}{polled}",
                        w.pin,
                        w.name,
                        w.pulses,
                        if w.pulses == 1 { "" } else { "s" },
                        w.line
                    );
                    w.pulses = 0;
                    w.widths = (u128::MAX, 0);
                }
            }
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    use super::*;

    pub fn spawn_reader(_cfg: &Coin, _tx: mpsc::UnboundedSender<()>, _ok: Arc<AtomicBool>) {
        tracing::warn!("GPIO is only available on Linux; use the dashboard's test-coin button");
    }

    pub struct Enable;

    impl Enable {
        pub fn open(_cfg: &Coin) -> Result<Option<Enable>> {
            Ok(None)
        }
        pub fn set(&self, _on: bool, _line: u32) {}
    }

    pub fn probe(_chip: &str) -> Result<()> {
        anyhow::bail!("coin-probe only works on the Orange Pi (Linux)")
    }
}

pub use imp::{probe, spawn_reader, Enable};
