//! Customer portal (plain HTTP on the LAN; it carries no secrets).

use crate::app::{self, asset, ApiError, ApiResult, Shared};
use crate::engine::{CoinStart, MeView, Resume};
use crate::hub::proto::{WalletKind, WalletSupport, UNKNOWN_PASS};
use crate::hubclient::{AfterPurchase, HubError, PassError};
use axum::extract::{ConnectInfo, DefaultBodyLimit, State};
use axum::http::StatusCode;
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
        .route("/", get(|| async {
            static HTML: std::sync::OnceLock<String> = std::sync::OnceLock::new();
            app::page(include_str!("../web/portal/index.html"), &HTML)
        }))
        .route("/app.js", get(|| async { asset("text/javascript; charset=utf-8", include_str!("../web/portal/app.js")) }))
        .route("/app.css", get(|| async { asset("text/css; charset=utf-8", include_str!("../web/portal/app.css")) }))
        .route("/icon.svg", get(|| async { asset("image/svg+xml", include_str!("../web/shared/icon.svg")) }))
        .route("/coins.js", get(|| async { asset("text/javascript; charset=utf-8", include_str!("../web/shared/coins.js")) }))
        .route("/captive-api", get(captive_api))
        .route("/brand/{name}", get(app::brand_image))
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

async fn fallback(State(st): State<Shared>) -> Response {
    Redirect::to(&st.portal_url()).into_response()
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
    #[serde(flatten)]
    me: MeView,
}

async fn me(State(st): State<Shared>, ConnectInfo(addr): ConnectInfo<SocketAddr>) -> ApiResult<Json<MeResponse>> {
    let (mac, ip) = st.identify(addr)?;
    let pass = PassInfo {
        enabled: st.hub.is_some(),
        linked: st.engine.pass_of(mac).is_some(),
        chain: st.hub.as_ref().and_then(|h| h.chain()),
        wallet: st.hub.as_ref().map(|h| h.status().wallet).unwrap_or_default(),
    };
    Ok(Json(MeResponse { brand: st.brand.get(), pass, me: st.engine.me(mac, ip) }))
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
    let r = match &st.hub {
        Some(hub) => hub.resume(&st.engine, mac, ip).await,
        None => st.engine.resume(mac, ip),
    };
    match r {
        Resume::Resumed => Ok(Json(json!({ "ok": true }))),
        Resume::Nothing => Err(ApiError::bad("No saved time to resume")),
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
