//! `pisowifi --dev demo-data`: fills a dev vendo with four months of
//! made-up sales and a dozen phones, so the dashboard has something to
//! show. Refused outside dev mode and on a vendo that already has sales.

use crate::db::{now, Db, DeviceRow};
use crate::net::Mac;
use anyhow::{bail, Result};
use rand::Rng;

const PHONES: &[(&str, &str, &str)] = &[
    // (hostname, nickname, User-Agent)
    ("Galaxy-A53-5G", "", "Mozilla/5.0 (Linux; Android 14; SM-A536E Build/UP1A.231005.007; wv) AppleWebKit/537.36 (KHTML, like Gecko) Version/4.0 Chrome/128.0.6613.146 Mobile Safari/537.36"),
    ("Juans-iPhone", "", "Mozilla/5.0 (iPhone; CPU iPhone OS 17_5_1 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.5 Mobile/15E148 Safari/604.1"),
    ("", "Aling Nena", "Mozilla/5.0 (Linux; Android 13; SM-A146P) AppleWebKit/537.36 (KHTML, like Gecko) SamsungBrowser/23.0 Chrome/115.0.0.0 Mobile Safari/537.36"),
    ("", "", "Mozilla/5.0 (Linux; Android 13; RMX3710 Build/TP1A.220905.001; wv) AppleWebKit/537.36 (KHTML, like Gecko) Version/4.0 Chrome/127.0.6533.103 Mobile Safari/537.36"),
    ("Redmi-Note-13", "", "Mozilla/5.0 (Linux; Android 14; 23129RAA4G Build/UKQ1.231003.002; wv) AppleWebKit/537.36 (KHTML, like Gecko) Version/4.0 Chrome/126.0.6478.134 Mobile Safari/537.36"),
    ("", "", "Mozilla/5.0 (Linux; Android 12; Infinix X6831 Build/SP1A.210812.016; wv) AppleWebKit/537.36 (KHTML, like Gecko) Version/4.0 Chrome/125.0.6422.165 Mobile Safari/537.36"),
    ("", "Kuya Ben", "Mozilla/5.0 (Linux; Android 10; K) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/129.0.0.0 Mobile Safari/537.36"),
    ("", "", "Mozilla/5.0 (Linux; Android 13; CPH2591 Build/TP1A.220905.001; wv) AppleWebKit/537.36 (KHTML, like Gecko) Version/4.0 Chrome/128.0.6613.127 Mobile Safari/537.36 [FB_IAB/FB4A;FBAV/478.0.0.42.110;]"),
    ("Marias-iPhone", "", "Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Mobile/15E148"),
    ("", "", "Mozilla/5.0 (Linux; Android 14; V2250 Build/UP1A.231005.007; wv) AppleWebKit/537.36 (KHTML, like Gecko) Version/4.0 Chrome/127.0.6533.64 Mobile Safari/537.36"),
    ("DESKTOP-K3J9Q2", "", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/129.0.0.0 Safari/537.36"),
    ("", "", "Mozilla/5.0 (Linux; Android 14; SM-S921B) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/128.0.0.0 Mobile Safari/537.36"),
    ("", "", "Mozilla/5.0 (Linux; Android 13; TECNO KJ5 Build/TP1A.220624.014; wv) AppleWebKit/537.36 (KHTML, like Gecko) Version/4.0 Chrome/124.0.6367.179 Mobile Safari/537.36"),
    ("", "", "Mozilla/5.0 (Linux; Android 12; SM-A055F Build/SP1A.210812.016; wv) AppleWebKit/537.36 (KHTML, like Gecko) Version/4.0 Chrome/126.0.6478.71 Mobile Safari/537.36"),
];

pub fn fill(db: &Db) -> Result<()> {
    if !db.sales_since(i64::MIN)?.is_empty() {
        bail!("this dev vendo already has sales; use a fresh one, e.g. `pisowifi --dev --instance 5 demo-data`");
    }
    let mut rng = rand::thread_rng();
    let t = now();
    let macs: Vec<Mac> = (0..PHONES.len())
        .map(|i| Mac([0xda, 0xa1, 0x19, rng.gen(), rng.gen(), i as u8]))
        .collect();
    for (i, (host, nick, ua)) in PHONES.iter().enumerate() {
        let first = t - rng.gen_range(5..110) * 86400;
        db.save_device(&DeviceRow {
            mac: macs[i],
            nickname: nick.to_string(),
            hostname: host.to_string(),
            user_agent: ua.to_string(),
            first_seen: first,
            last_seen: t - rng.gen_range(0..36) * 3600,
        })?;
    }
    let rates = db.rates()?;
    // Busy at lunch and after school/work; quiet at night.
    const HOURS: [u32; 24] = [1, 1, 0, 0, 0, 1, 2, 4, 5, 6, 7, 8, 10, 9, 7, 7, 8, 10, 12, 13, 12, 9, 5, 2];
    let total: u32 = HOURS.iter().sum();
    let mut box_since = 0u64;
    for day in (0..120i64).rev() {
        let start = t - t.rem_euclid(86400) - day * 86400 - 8 * 3600; // ~local midnight in the Philippines
        let weekend = matches!((start / 86400 + 4) % 7, 5 | 6);
        let sales = rng.gen_range(14..30) + if weekend { 10 } else { 0 } + (120 - day as u32) / 12;
        let mut day_pesos = 0u64;
        for _ in 0..sales {
            let mut pick = rng.gen_range(0..total);
            let hour = HOURS.iter().position(|w| if pick < *w { true } else { pick -= w; false }).unwrap_or(12) as i64;
            let ts = start + hour * 3600 + rng.gen_range(0..3600);
            if ts > t {
                continue;
            }
            // Most sales are one ₱5 or a few pisos; some are ₱10 or ₱20.
            let pesos = *[1, 2, 3, 5, 5, 5, 6, 10, 10, 15, 20, 20].get(rng.gen_range(0..12)).unwrap();
            let mac = macs[rng.gen_range(0..macs.len())];
            let r: f32 = rng.gen();
            if r < 0.05 {
                db.demo_sale(ts, Some(mac), 0, 120 * 60, "voucher")?;
            } else if r < 0.07 {
                db.demo_sale(ts, Some(mac), 0, 30 * 60, "manual")?;
            } else if r < 0.075 {
                db.demo_sale(ts, None, 1, 0, "unclaimed")?;
                day_pesos += 1;
            } else {
                db.demo_sale(ts, Some(mac), pesos, crate::rates::secs_for(pesos, &rates), "coin")?;
                day_pesos += pesos as u64;
            }
        }
        box_since += day_pesos;
        // Emptied about once a week, now and then a few pesos short.
        if day % 7 == 3 && day > 0 {
            let short = if rng.gen_bool(0.25) { rng.gen_range(1..6) } else { 0 };
            db.demo_collection(start + 86400 - 3600, box_since, box_since.saturating_sub(short))?;
            box_since = 0;
        }
    }
    db.set_setting("daily_goal", "300")?;
    println!("Added 120 days of sample sales and {} devices. Start it with --dev run (same --instance).", PHONES.len());
    Ok(())
}
