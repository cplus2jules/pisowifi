//! Customer portal (plain HTTP on the LAN; it carries no secrets).

use crate::app::{self, asset, ApiError, ApiResult, Shared};
use crate::engine::{CoinStart, MeView};
use crate::hub::proto::{WalletKind, WalletSupport};
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
    }
}

async fn coin_done(State(st): State<Shared>, ConnectInfo(addr): ConnectInfo<SocketAddr>) -> ApiResult<Json<serde_json::Value>> {
    let (mac, _) = st.identify(addr)?;
    let credited = st.engine.coin_done(mac);
    // First purchase in a chain: give this phone a Piso Pass.
    if let (Some(hub), Some((pesos, secs))) = (&st.hub, credited) {
        if st.engine.pass_of(mac).is_none() {
            return Ok(Json(match new_pass(&st, hub, mac, Some((pesos, secs))).await {
                Ok(code) => json!({ "ok": true, "pass_code": code }),
                Err(_) => json!({ "ok": true }),
            }));
        }
    }
    Ok(Json(json!({ "ok": true })))
}

async fn new_pass(
    st: &Shared,
    hub: &std::sync::Arc<crate::hubclient::HubClient>,
    mac: crate::net::Mac,
    sale: Option<(u32, u64)>,
) -> Result<String, ApiError> {
    match hub.create_pass().await {
        Ok(p) => {
            st.engine.link_pass(mac, p.pass_id.clone());
            if let Some((pesos, secs)) = sale {
                let _ = st.db.outbox_add(&p.pass_id, pesos, secs);
            }
            Ok(p.code)
        }
        Err(e) => {
            tracing::warn!("creating pass: {e}");
            Err(ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "Piso Pass is offline right now. Your time still works here."))
        }
    }
}

#[derive(Deserialize)]
struct PassReq {
    code: String,
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
        return Err(ApiError::bad("That Piso Pass code isn't valid."));
    }
    // Checked before asking the hub, so time is never moved and then lost:
    // a phone whose pass still has time here can't swap in another pass.
    if st.engine.holds_pass_time(mac) {
        return Err(ApiError::bad("Your Piso Pass time is already on this phone."));
    }
    match hub.claim(req.code).await {
        Ok(r) => {
            let total = st.engine.claim_into(mac, ip, r.pass_id, r.secs);
            Ok(Json(json!({ "ok": true, "secs": r.secs, "total": total })))
        }
        Err(crate::hubclient::HubError::NotFound) => {
            st.engine.record_code_fail(mac);
            Err(ApiError::bad("That Piso Pass code isn't valid."))
        }
        Err(e) => {
            tracing::warn!("pass claim: {e}");
            Err(ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "Piso Pass is offline right now. Try again in a minute."))
        }
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
        return Err(ApiError::bad("That Piso Pass code isn't valid."));
    }
    match hub.wallet(req.code, req.kind).await {
        Ok(url) => Ok(Json(json!({ "ok": true, "url": url }))),
        Err(crate::hubclient::HubError::NotFound) => {
            st.engine.record_code_fail(mac);
            Err(ApiError::bad("That Piso Pass code isn't valid."))
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
    if st.engine.pass_of(mac).is_some() {
        return Err(ApiError::bad("This phone already has a Piso Pass."));
    }
    let code = new_pass(&st, &hub, mac, None).await?;
    Ok(Json(json!({ "ok": true, "pass_code": code })))
}

async fn pause(State(st): State<Shared>, ConnectInfo(addr): ConnectInfo<SocketAddr>) -> ApiResult<Json<serde_json::Value>> {
    let (mac, _) = st.identify(addr)?;
    if !st.engine.pause(mac) {
        return Err(ApiError::bad("Nothing to pause"));
    }
    Ok(Json(json!({ "ok": true })))
}

async fn resume(State(st): State<Shared>, ConnectInfo(addr): ConnectInfo<SocketAddr>) -> ApiResult<Json<serde_json::Value>> {
    let (mac, ip) = st.identify(addr)?;
    if !st.engine.resume(mac, ip) {
        return Err(ApiError::bad("No saved time to resume"));
    }
    Ok(Json(json!({ "ok": true })))
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
