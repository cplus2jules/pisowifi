//! Customer portal (plain HTTP on the LAN; it carries no secrets).

use crate::app::{self, asset, ApiError, ApiResult, Shared};
use crate::engine::{CoinStart, MeView, Resume};
use crate::hub::proto::{WalletKind, WalletSupport, UNKNOWN_PASS};
use crate::hubclient::{AfterPurchase, HubError, PassError};
use axum::extract::{ConnectInfo, DefaultBodyLimit, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::{from_fn, from_fn_with_state};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::net::SocketAddr;

pub fn router(st: Shared) -> Router {
    let api = Router::new()
        .route("/me", get(me))
        .route("/coin/start", post(coin_start))
        .route("/coin/done", post(coin_done))
        .route("/pause", post(pause))
        .route("/resume", post(resume))
        .route("/voucher", post(voucher))
        .route("/pass/claim", post(pass_claim))
        .route("/pass/save", post(pass_save))
        .route("/pass/wallet", post(pass_wallet))
        .layer(from_fn(app::portal_csrf_guard));
    Router::new()
        .route("/", get(|State(st): State<Shared>| async move {
            // The owner's look is stamped into the page, so it never flashes the other one first.
            static HTML: std::sync::OnceLock<String> = std::sync::OnceLock::new();
            static GLASS: std::sync::OnceLock<String> = std::sync::OnceLock::new();
            const PAGE: &str = include_str!("../web/portal/index.html");
            let html = match st.brand.get().theme.as_str() {
                "glass" => GLASS.get_or_init(|| {
                    PAGE.replace("__V__", app::asset_version()).replacen("<html lang=\"en\">", "<html lang=\"en\" data-theme=\"glass\">", 1)
                }),
                _ => HTML.get_or_init(|| PAGE.replace("__V__", app::asset_version())),
            };
            app::html_page(html)
        }))
        .route("/app.js", get(|| async { asset("text/javascript; charset=utf-8", include_str!("../web/portal/app.js")) }))
        .route("/app.css", get(|| async { asset("text/css; charset=utf-8", include_str!("../web/portal/app.css")) }))
        .route("/icon.svg", get(|| async { asset("image/svg+xml", include_str!("../web/shared/icon.svg")) }))
        .route("/coins.js", get(|| async { asset("text/javascript; charset=utf-8", include_str!("../web/shared/coins.js")) }))
        .route("/add-to-google-wallet.svg", get(|| async { asset("image/svg+xml", include_str!("../web/portal/add-to-google-wallet.svg")) }))
        .route("/manifest.webmanifest", get(manifest))
        .route("/home-icon.png", get(|st: State<Shared>| home_icon(st, 180, false)))
        .route("/home-icon-192.png", get(|st: State<Shared>| home_icon(st, 192, true)))
        .route("/home-icon-512.png", get(|st: State<Shared>| home_icon(st, 512, true)))
        .route("/home-icon-maskable.png", get(|st: State<Shared>| home_icon(st, 512, false)))
        .route("/captive-api", get(captive_api))
        .route("/reminder.ics", get(reminder))
        .route("/brand/{name}", get(app::brand_image))
        .route("/fonts/{file}", get(crate::fonts::serve))
        .nest("/api", api)
        .fallback(fallback)
        .layer(from_fn_with_state(st.clone(), app::portal_host_guard))
        .layer(from_fn(crate::server::security_headers))
        .layer(DefaultBodyLimit::max(4 * 1024))
        .with_state(st)
}

/// RFC 8908 captive portal API. iOS and Android only trust it over HTTPS
/// with a publicly trusted certificate, so it isn't advertised over DHCP by
/// default; classic probe redirects handle sign-in detection.
async fn captive_api(State(st): State<Shared>, ConnectInfo(addr): ConnectInfo<SocketAddr>) -> Response {
    let (captive, secs) = match st.identify(addr) {
        Ok((mac, ip)) => {
            let me = st.engine.me(mac, ip);
            (!me.running, me.remaining_secs)
        }
        Err(_) => (true, 0),
    };
    let mut body = json!({ "captive": captive, "user-portal-url": st.portal_url() });
    if !captive {
        body["seconds-remaining"] = json!(secs);
        body["can-extend-session"] = json!(true);
    }
    ([(axum::http::header::CONTENT_TYPE, "application/captive+json")], body.to_string()).into_response()
}

/// Lets phones add the portal to the home screen, named and coloured like
/// the owner's page. Opening it with `?home` tells the page it was added.
async fn manifest(State(st): State<Shared>) -> Response {
    let b = st.brand.get();
    let icons = match (st.brand.asset("icon"), &b.icon) {
        (Some(a), Some(url)) => {
            let side = crate::homeicon::square_png_side(&a.data).unwrap_or(256);
            json!([{ "src": url, "sizes": format!("{side}x{side}"), "type": "image/png" }])
        }
        _ => json!([
            { "src": "/home-icon-192.png", "sizes": "192x192", "type": "image/png" },
            { "src": "/home-icon-512.png", "sizes": "512x512", "type": "image/png" },
            { "src": "/home-icon-maskable.png", "sizes": "512x512", "type": "image/png", "purpose": "maskable" },
        ]),
    };
    let body = json!({
        "id": "/",
        "name": b.name,
        "short_name": b.name,
        "start_url": "/?home",
        "scope": "/",
        "display": "standalone",
        "background_color": "#FFFFFF",
        "theme_color": "#FFFFFF",
        "icons": icons,
    });
    ([(header::CONTENT_TYPE, "application/manifest+json"), (header::CACHE_CONTROL, "no-cache")], body.to_string()).into_response()
}

/// Drawn icons by (accent, size, rounded corners).
type IconCache = std::sync::Mutex<Vec<((String, u32, bool), bytes::Bytes)>>;

/// The owner's logo icon when there is one, else the mark in their accent.
/// Unversioned URLs (the page's link is fixed), so phones check each time.
async fn home_icon(State(st): State<Shared>, size: u32, round: bool) -> Response {
    static CACHE: IconCache = std::sync::Mutex::new(Vec::new());
    let png = match st.brand.asset("icon") {
        Some(a) => a.data.clone(),
        None => {
            let key = (st.brand.get().accent, size, round);
            let hit = CACHE.lock().unwrap().iter().find(|(k, _)| *k == key).map(|(_, v)| v.clone());
            match hit {
                Some(png) => png,
                None => {
                    let png = bytes::Bytes::from(crate::homeicon::png(&key.0, size, round));
                    let mut c = CACHE.lock().unwrap();
                    if c.len() >= 8 {
                        c.remove(0);
                    }
                    c.push((key, png.clone()));
                    png
                }
            }
        }
    };
    ([(header::CONTENT_TYPE, "image/png"), (header::CACHE_CONTROL, "no-cache")], png).into_response()
}

async fn fallback(State(st): State<Shared>) -> Response {
    Redirect::to(&st.portal_url()).into_response()
}

#[derive(Deserialize)]
struct ReminderQuery {
    /// When the time runs out, in Unix seconds by the phone's clock. The
    /// vendo's own clock can be far off (no RTC), so it never decides this.
    end: i64,
}

/// A top-up reminder for the phone's calendar: an event for the last five
/// minutes of the customer's time, with an alert as it starts.
async fn reminder(State(st): State<Shared>, Query(q): Query<ReminderQuery>) -> Response {
    let Some(ics) = reminder_ics(&st.brand.get().name, &st.portal_url(), q.end) else {
        return (StatusCode::BAD_REQUEST, "That reminder time doesn't look right.").into_response();
    };
    (
        [
            (header::CONTENT_TYPE, "text/calendar; charset=utf-8"),
            (header::CONTENT_DISPOSITION, "inline; filename=\"wifi-reminder.ics\""),
            (header::CACHE_CONTROL, "no-store"),
        ],
        ics,
    )
        .into_response()
}

/// Minutes before the end that the reminder goes off.
const REMIND_LEAD_MINS: i64 = 5;

/// The calendar file (RFC 5545). None for an end time outside 2020–2100,
/// which no real phone clock sends.
fn reminder_ics(name: &str, portal_url: &str, end: i64) -> Option<String> {
    if !(1_577_836_800..4_102_444_800).contains(&end) {
        return None;
    }
    let stamp = |t: i64| chrono::DateTime::from_timestamp(t, 0).map(|d| d.format("%Y%m%dT%H%M%SZ").to_string());
    let (start, finish) = (stamp(end - REMIND_LEAD_MINS * 60)?, stamp(end)?);
    let now = stamp(chrono::Utc::now().timestamp().clamp(1_577_836_800, 4_102_444_800))?;
    let name = if name.trim().is_empty() { "Piso WiFi" } else { name.trim() };
    let summary = format!("{name}: {REMIND_LEAD_MINS} minutes left");
    let desc = format!("Your WiFi time ends in {REMIND_LEAD_MINS} minutes. Top up at {portal_url} or drop more coins.");
    let lines = [
        "BEGIN:VCALENDAR".to_string(),
        "VERSION:2.0".into(),
        "PRODID:-//PisoWiFi//Top-up reminder//EN".into(),
        "METHOD:PUBLISH".into(),
        "BEGIN:VEVENT".into(),
        format!("UID:{end}-pisowifi-reminder"),
        format!("DTSTAMP:{now}"),
        format!("DTSTART:{start}"),
        format!("DTEND:{finish}"),
        format!("SUMMARY:{}", ics_text(&summary)),
        format!("DESCRIPTION:{}", ics_text(&desc)),
        format!("URL:{portal_url}"),
        "TRANSP:TRANSPARENT".into(),
        "BEGIN:VALARM".into(),
        "ACTION:DISPLAY".into(),
        "TRIGGER:-PT0M".into(),
        format!("DESCRIPTION:{}", ics_text(&summary)),
        "END:VALARM".into(),
        "END:VEVENT".into(),
        "END:VCALENDAR".into(),
    ];
    Some(lines.iter().map(|l| ics_fold(l)).collect::<Vec<_>>().join("\r\n") + "\r\n")
}

/// Escapes a TEXT value (backslash, comma, semicolon, line breaks).
fn ics_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            ',' => out.push_str("\\,"),
            ';' => out.push_str("\\;"),
            '\n' => out.push_str("\\n"),
            '\r' => {}
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// Folds a content line at 75 bytes, never inside a character (a shop name
/// can hold emoji).
fn ics_fold(line: &str) -> String {
    let mut out = String::with_capacity(line.len() + 8);
    let mut used = 0;
    for c in line.chars() {
        let n = c.len_utf8();
        if used + n > 75 {
            out.push_str("\r\n ");
            used = 1;
        }
        out.push(c);
        used += n;
    }
    out
}

#[derive(Serialize)]
struct PassInfo {
    /// This vendo is connected to a chain, so passes work here.
    enabled: bool,
    /// This phone's time here belongs to a pass.
    linked: bool,
    /// The hub's id. The phone saves passes per chain: every owner's vendos
    /// share 10.0.0.1, and a pass only works on its own chain.
    chain: Option<String>,
    /// "Add to Google/Apple Wallet" buttons the hub supports.
    wallet: WalletSupport,
}

#[derive(Serialize)]
struct MeResponse {
    brand: crate::brand::Brand,
    pass: PassInfo,
    /// This phone's model or own name, when its browser or DHCP said.
    device_name: Option<String>,
    #[serde(flatten)]
    me: MeView,
}

async fn me(State(st): State<Shared>, ConnectInfo(addr): ConnectInfo<SocketAddr>, headers: HeaderMap) -> ApiResult<Json<MeResponse>> {
    let (mac, ip) = st.identify(addr)?;
    // So the owner sees "Galaxy A53 5G" instead of an address. Display only.
    let ua = headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok()).unwrap_or("");
    st.devices.saw_browser(&st.db, mac, ua);
    let pass = PassInfo {
        enabled: st.hub.is_some(),
        linked: st.engine.pass_of(mac).is_some(),
        chain: st.hub.as_ref().and_then(|h| h.chain()),
        wallet: st.hub.as_ref().map(|h| h.status().wallet).unwrap_or_default(),
    };
    Ok(Json(MeResponse { brand: st.brand.get(), pass, device_name: st.devices.own_name(mac), me: st.engine.me(mac, ip) }))
}

async fn coin_start(State(st): State<Shared>, ConnectInfo(addr): ConnectInfo<SocketAddr>) -> ApiResult<Response> {
    let (mac, ip) = st.identify(addr)?;
    match st.engine.coin_start(mac, ip) {
        CoinStart::Ok => Ok(Json(json!({ "ok": true })).into_response()),
        CoinStart::Busy(secs) => Ok((
            StatusCode::CONFLICT,
            Json(json!({ "error": "Someone else is inserting coins. Please wait.", "secs_left": secs })),
        )
            .into_response()),
        CoinStart::Offline => Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "The coin slot isn't working right now. Please don't insert coins; tell the owner.",
        )),
        CoinStart::Wait(secs) => Err(ApiError::new(StatusCode::TOO_MANY_REQUESTS, format!("Please wait {secs} s, then tap Insert coin again."))),
        CoinStart::Blocked => Err(blocked()),
    }
}

#[derive(Deserialize, Default)]
struct CoinDoneReq {
    /// The Piso Pass saved on this phone for this chain, if any.
    #[serde(default)]
    pass: Option<String>,
}

async fn coin_done(
    State(st): State<Shared>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(req): Json<CoinDoneReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let (mac, ip) = st.identify(addr)?;
    // A coin still dropping (a ₱20 is 20 pulses, ~2 s) counts before we close.
    for _ in 0..80 {
        if !st.engine.counting() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let credited = st.engine.coin_done(mac);
    // In a chain: the coins join this phone's pass, or it gets its first one.
    if let (Some(hub), Some((pesos, _))) = (&st.hub, credited) {
        return Ok(Json(match hub.after_purchase(&st.engine, mac, ip, req.pass, pesos).await {
            AfterPurchase::NewPass(code) => json!({ "ok": true, "pass_code": code }),
            AfterPurchase::Claimed(secs) => json!({ "ok": true, "pass_secs": secs }),
            AfterPurchase::UnknownPass => json!({ "ok": true, "pass_unknown": true }),
            AfterPurchase::AlreadyLinked | AfterPurchase::Offline => json!({ "ok": true }),
        }));
    }
    Ok(Json(json!({ "ok": true })))
}

fn blocked() -> ApiError {
    ApiError::new(StatusCode::FORBIDDEN, crate::engine::BLOCKED).with_code("blocked")
}

/// Asked before anything goes to the hub, so a pass's time never moves to a blocked phone.
fn refuse_blocked(st: &Shared, mac: crate::net::Mac) -> ApiResult<()> {
    if st.engine.is_blocked(mac) {
        return Err(blocked());
    }
    Ok(())
}

fn invalid_pass() -> ApiError {
    ApiError::bad("That Piso Pass code isn't valid.").with_code(UNKNOWN_PASS)
}

fn pass_offline(e: HubError) -> ApiError {
    tracing::warn!("Piso Pass: {e}");
    ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "Piso Pass is offline right now. Try again in a minute.")
}

#[derive(Deserialize)]
struct PassReq {
    code: String,
    /// The customer asked (typed the code or confirmed a link): start the
    /// clock. Automatic claims leave paused time paused.
    #[serde(default)]
    start: bool,
}

/// Brings a Piso Pass's time to this location.
async fn pass_claim(
    State(st): State<Shared>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(req): Json<PassReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let (mac, ip) = st.identify(addr)?;
    let Some(hub) = st.hub.clone() else {
        return Err(ApiError::bad("Piso Pass isn't set up at this location."));
    };
    refuse_blocked(&st, mac)?;
    st.engine.code_attempt_allowed(mac).map_err(ApiError::bad)?;
    if crate::hub::normalize_code(&req.code).len() != 16 {
        st.engine.record_code_fail(mac);
        return Err(invalid_pass());
    }
    match hub.claim_pass(&st.engine, mac, ip, req.code, req.start).await {
        Ok(c) => Ok(Json(json!({ "ok": true, "secs": c.secs, "total": c.total, "running": c.running }))),
        Err(PassError::AlreadyHere) => Err(ApiError::bad("Your Piso Pass time is already on this phone.")),
        Err(PassError::Unknown) => {
            st.engine.record_code_fail(mac);
            Err(invalid_pass())
        }
        Err(PassError::Hub(e)) => Err(pass_offline(e)),
        Err(PassError::HasPass | PassError::NoTime) => Err(ApiError::bad("Something went wrong. Please try again.")),
    }
}

#[derive(Deserialize)]
struct WalletReq {
    code: String,
    kind: WalletKind,
}

/// "Add to Google/Apple Wallet": the hub builds the pass and returns a
/// one-time link the phone opens directly (customers are online once they've paid).
async fn pass_wallet(
    State(st): State<Shared>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(req): Json<WalletReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let (mac, _) = st.identify(addr)?;
    let Some(hub) = st.hub.clone() else {
        return Err(ApiError::bad("Piso Pass isn't set up at this location."));
    };
    if !hub.status().wallet.has(req.kind) {
        return Err(ApiError::bad("Adding to your wallet isn't available here yet."));
    }
    st.engine.code_attempt_allowed(mac).map_err(ApiError::bad)?;
    if crate::hub::normalize_code(&req.code).len() != 16 {
        st.engine.record_code_fail(mac);
        return Err(invalid_pass());
    }
    match hub.wallet(req.code, req.kind).await {
        Ok(url) => Ok(Json(json!({ "ok": true, "url": url }))),
        Err(HubError::NotFound) => {
            st.engine.record_code_fail(mac);
            Err(invalid_pass())
        }
        Err(e) => {
            tracing::warn!("wallet pass: {e}");
            Err(ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "Couldn't make your wallet pass right now. Try again in a minute."))
        }
    }
}

/// For time bought while the hub was unreachable: put it on a new pass.
async fn pass_save(State(st): State<Shared>, ConnectInfo(addr): ConnectInfo<SocketAddr>) -> ApiResult<Json<serde_json::Value>> {
    let (mac, _) = st.identify(addr)?;
    let Some(hub) = st.hub.clone() else {
        return Err(ApiError::bad("Piso Pass isn't set up at this location."));
    };
    refuse_blocked(&st, mac)?;
    match hub.save_pass(&st.engine, mac).await {
        Ok(code) => Ok(Json(json!({ "ok": true, "pass_code": code }))),
        Err(PassError::HasPass) => Err(ApiError::bad("This phone already has a Piso Pass.")),
        Err(PassError::NoTime) => Err(ApiError::bad("There's no time on this phone to save.")),
        Err(PassError::Hub(e)) => Err(pass_offline(e)),
        Err(PassError::Unknown | PassError::AlreadyHere) => Err(ApiError::bad("Something went wrong. Please try again.")),
    }
}

async fn pause(State(st): State<Shared>, ConnectInfo(addr): ConnectInfo<SocketAddr>) -> ApiResult<Json<serde_json::Value>> {
    let (mac, ip) = st.identify(addr)?;
    if !st.engine.pause(mac, ip) {
        return Err(ApiError::bad("Nothing to pause"));
    }
    Ok(Json(json!({ "ok": true })))
}

async fn resume(State(st): State<Shared>, ConnectInfo(addr): ConnectInfo<SocketAddr>) -> ApiResult<Json<serde_json::Value>> {
    let (mac, ip) = st.identify(addr)?;
    refuse_blocked(&st, mac)?;
    let r = match &st.hub {
        Some(hub) => hub.resume(&st.engine, mac, ip).await,
        None => st.engine.resume(mac, ip),
    };
    match r {
        Resume::Resumed => Ok(Json(json!({ "ok": true }))),
        Resume::Nothing => Err(ApiError::bad("No saved time to resume")),
        Resume::Blocked => Err(blocked()),
        Resume::NeedsHub => Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "This location can't reach Piso Pass right now, so saved pass time can't restart here yet. Try again in a few minutes.",
        )),
    }
}

#[derive(Deserialize)]
struct VoucherReq {
    code: String,
}

async fn voucher(
    State(st): State<Shared>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(req): Json<VoucherReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let (mac, ip) = st.identify(addr)?;
    if req.code.len() > 32 {
        return Err(ApiError::bad("That code is invalid or already used."));
    }
    match st.engine.redeem_voucher(mac, ip, &req.code) {
        Ok(secs) => Ok(Json(json!({ "ok": true, "secs": secs }))),
        Err(msg) => Err(ApiError::bad(msg)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reminder_is_a_valid_calendar_event() {
        // 2026-11-09 12:00:00 UTC
        let ics = reminder_ics("Overly PisoWiFi 💜, the best; really", "http://10.0.0.1/", 1_794_225_600).unwrap();
        assert!(ics.starts_with("BEGIN:VCALENDAR\r\n") && ics.ends_with("END:VCALENDAR\r\n"));
        assert!(ics.contains("DTSTART:20261109T115500Z\r\n"), "five minutes before the end");
        assert!(ics.contains("DTEND:20261109T120000Z\r\n"));
        assert!(ics.contains("TRIGGER:-PT0M"));
        assert!(ics.contains("SUMMARY:Overly PisoWiFi 💜\\, the best\\; really: 5 minutes left"));
        for line in ics.split("\r\n") {
            assert!(line.len() <= 75, "folded: {line:?}");
        }
        // Unfolding gives back the description whole.
        let unfolded = ics.replace("\r\n ", "");
        assert!(unfolded.contains("DESCRIPTION:Your WiFi time ends in 5 minutes. Top up at http://10.0.0.1/ or drop more coins."));
    }

    #[test]
    fn reminder_refuses_silly_times_and_names_the_page() {
        assert!(reminder_ics("x", "http://10.0.0.1/", 0).is_none());
        assert!(reminder_ics("x", "http://10.0.0.1/", 9_999_999_999).is_none());
        assert!(reminder_ics("  ", "http://10.0.0.1/", 1_794_225_600).unwrap().contains("SUMMARY:Piso WiFi: 5 minutes left"));
    }

    #[test]
    fn folding_never_splits_a_character() {
        let long = format!("SUMMARY:{}", "💜".repeat(40));
        let folded = ics_fold(&long);
        assert!(folded.split("\r\n").all(|l| l.len() <= 75 && std::str::from_utf8(l.as_bytes()).is_ok()));
        assert_eq!(folded.replace("\r\n ", ""), long);
    }
}
