//! Coin acceptor I/O through the Linux GPIO character device (/dev/gpiochipN).
//! Each pulse from the acceptor arrives as one `()` on the channel.

use crate::config::Coin;
use anyhow::Result;
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
    use std::time::Duration;

    pub fn spawn_reader(cfg: &Coin, tx: mpsc::UnboundedSender<()>) {
        let cfg = cfg.clone();
        std::thread::Builder::new()
            .name("coin-reader".into())
            .spawn(move || loop {
                match read_loop(&cfg, &tx) {
                    Ok(()) => return,
                    Err(e) => {
                        tracing::error!("coin slot: {e:#}; retrying in 5s");
                        std::thread::sleep(Duration::from_secs(5));
                    }
                }
            })
            .expect("spawn coin reader");
    }

    fn read_loop(cfg: &Coin, tx: &mpsc::UnboundedSender<()>) -> Result<()> {
        let mut b = Request::builder();
        b.on_chip(&cfg.chip)
            .with_consumer("pisowifi-coin")
            .with_line(cfg.line)
            .as_input()
            .with_bias(Bias::PullUp)
            .with_debounce_period(Duration::from_millis(cfg.debounce_ms))
            // With active-low, "rising" means the acceptor started a pulse.
            .with_edge_detection(EdgeDetection::RisingEdge);
        if cfg.active_low {
            b.as_active_low();
        }
        let req = b.request()?;
        tracing::info!("coin slot listening on {} {}", cfg.chip, describe_line(cfg.line));
        loop {
            req.read_edge_event()?;
            if tx.send(()).is_err() {
                return Ok(());
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

    /// Watches every free line on the chip and prints which ones toggle.
    pub fn probe(chip: &str) -> Result<()> {
        let c = gpiocdev::Chip::from_path(chip)?;
        let n = c.info()?.num_lines;
        let free: Vec<u32> = (0..n).filter(|&o| c.line_info(o).map(|i| !i.used).unwrap_or(false)).collect();
        println!("Watching {} free lines on {chip}. Drop a coin (Ctrl-C to stop)…", free.len());
        let mut handles = vec![];
        for chunk in free.chunks(64) {
            let mut b = Request::builder();
            b.on_chip(chip)
                .with_consumer("pisowifi-probe")
                .with_lines(chunk)
                .as_input()
                .with_bias(Bias::PullUp)
                .with_edge_detection(EdgeDetection::BothEdges);
            let req = match b.request() {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("skipping lines {chunk:?}: {e}");
                    continue;
                }
            };
            handles.push(std::thread::spawn(move || loop {
                match req.read_edge_event() {
                    Ok(ev) => println!("pulse edge on {}", describe_line(ev.offset)),
                    Err(e) => {
                        eprintln!("{e}");
                        return;
                    }
                }
            }));
        }
        for h in handles {
            let _ = h.join();
        }
        Ok(())
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    use super::*;

    pub fn spawn_reader(_cfg: &Coin, _tx: mpsc::UnboundedSender<()>) {
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
