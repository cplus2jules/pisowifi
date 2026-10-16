//! Coin acceptor I/O through the Linux GPIO character device (/dev/gpiochipN).
//! Each pulse from the acceptor arrives as one `()` on the channel.
//!
//! On the H3 only the PA and PG banks can raise interrupts. A coin wire on
//! any other header pin (PC, PD) is polled every millisecond instead.
//!
//! With `pulse_width_ms` set, a pulse only counts once it has ended and
//! lasted that long: electrical noise and a wire held low don't add coins.

use crate::config::Coin;
use anyhow::Result;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// Checks each pulse's width (time from start to end).
pub struct PulseFilter {
    min: Duration,
    max: Duration,
    started: Option<Duration>,
    rejected: u32,
    last_warn: Option<Instant>,
}

pub enum Edge {
    /// A pulse ended within the allowed widths: one count.
    Pulse,
    /// A pulse ended outside them, with its width.
    Rejected(Duration),
    None,
}

impl PulseFilter {
    pub fn new([min_ms, max_ms]: [u64; 2]) -> Self {
        PulseFilter {
            min: Duration::from_millis(min_ms),
            max: Duration::from_millis(max_ms),
            started: None,
            rejected: 0,
            last_warn: None,
        }
    }

    /// The line changed to `active` at `t` (any monotonic clock).
    pub fn edge(&mut self, active: bool, t: Duration) -> Edge {
        if active {
            self.started = Some(t);
            return Edge::None;
        }
        let Some(start) = self.started.take() else { return Edge::None };
        let width = t.saturating_sub(start);
        if (self.min..=self.max).contains(&width) {
            Edge::Pulse
        } else {
            Edge::Rejected(width)
        }
    }

    /// Logs rejected pulses without flooding the log when a line is noisy.
    pub fn note_rejected(&mut self, width: Duration) {
        self.rejected += 1;
        if self.last_warn.is_none_or(|t| t.elapsed() >= Duration::from_secs(10)) {
            tracing::warn!(
                "coin slot: ignored {} pulse(s) outside {}–{} ms (latest {} ms)",
                self.rejected,
                self.min.as_millis(),
                self.max.as_millis(),
                width.as_millis()
            );
            self.rejected = 0;
            self.last_warn = Some(Instant::now());
        }
    }
}

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
    use gpiocdev::line::{Bias, EdgeDetection, EdgeKind, Value};
    use gpiocdev::Request;
    use std::sync::atomic::Ordering;

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
            // Checking widths needs the end of each pulse too.
            let detect = if cfg.pulse_width_ms.is_some() { EdgeDetection::BothEdges } else { EdgeDetection::RisingEdge };
            b.with_debounce_period(Duration::from_millis(cfg.debounce_ms))
                .with_edge_detection(detect)
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
        let mut filter = cfg.pulse_width_ms.map(PulseFilter::new);
        loop {
            let ev = req.read_edge_event()?;
            let count = match &mut filter {
                None => true,
                Some(f) => {
                    // Kernel timestamps: exact even if this thread was busy.
                    let t = Duration::from_nanos(ev.timestamp_ns);
                    match f.edge(ev.kind == EdgeKind::Rising, t) {
                        Edge::Pulse => true,
                        Edge::Rejected(w) => {
                            f.note_rejected(w);
                            false
                        }
                        Edge::None => false,
                    }
                }
            };
            if count && tx.send(()).is_err() {
                return Ok(());
            }
        }
    }

    /// Software debounce: a level counts once it has held for `debounce_ms`.
    fn poll_loop(cfg: &Coin, req: &Request, tx: &mpsc::UnboundedSender<()>, ok: &AtomicBool) -> Result<()> {
        let debounce = Duration::from_millis(cfg.debounce_ms);
        let mut stable = req.value(cfg.line)?;
        let start = Instant::now();
        let (mut seen, mut since) = (stable, start);
        let mut filter = cfg.pulse_width_ms.map(PulseFilter::new);
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
                let active = stable == Value::Active;
                let count = match &mut filter {
                    None => active,
                    // The change happened when the level first moved (`since`).
                    Some(f) => match f.edge(active, since - start) {
                        Edge::Pulse => true,
                        Edge::Rejected(w) => {
                            f.note_rejected(w);
                            false
                        }
                        Edge::None => false,
                    },
                };
                if count && tx.send(()).is_err() {
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
    pub fn probe(chip: &str, limit: Option<Duration>) -> Result<()> {
        let deadline = limit.map(|d| Instant::now() + d);
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
            if deadline.is_some_and(|d| now >= d) {
                println!("Stopped after {} s.", limit.unwrap_or_default().as_secs());
                return Ok(());
            }
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

    pub fn probe(_chip: &str, _limit: Option<std::time::Duration>) -> Result<()> {
        anyhow::bail!("coin-probe only works on the Orange Pi (Linux)")
    }
}

pub use imp::{probe, spawn_reader, Enable};

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn only_pulses_of_the_right_width_count() {
        let mut f = PulseFilter::new([15, 150]);
        // A normal 50 ms pulse counts when it ends.
        assert!(matches!(f.edge(true, ms(1000)), Edge::None));
        assert!(matches!(f.edge(false, ms(1050)), Edge::Pulse));
        // A 3 ms glitch and a wire held low for a second don't.
        f.edge(true, ms(2000));
        assert!(matches!(f.edge(false, ms(2003)), Edge::Rejected(w) if w == ms(3)));
        f.edge(true, ms(3000));
        assert!(matches!(f.edge(false, ms(4000)), Edge::Rejected(_)));
        // An end with no start seen (e.g. the reader started mid-pulse) is ignored.
        assert!(matches!(f.edge(false, ms(5000)), Edge::None));
        // Both limits are inclusive.
        f.edge(true, ms(6000));
        assert!(matches!(f.edge(false, ms(6150)), Edge::Pulse));
    }
}
