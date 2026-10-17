//! Owner dashboard (HTTPS only).

use crate::app::{self, asset, ApiError, ApiResult, Shared};
use crate::auth::{self, Auth, LoginCheck, SessionInfo};
use crate::db::{Rate, Spend};
use crate::devices::DeviceInfo;
use crate::engine::{AdminAdd, Resume};
use crate::net::Mac;
use crate::rates;
use axum::extract::{ConnectInfo, DefaultBodyLimit, Path, Query, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::{from_fn, from_fn_with_state, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::SocketAddr;

pub fn router(st: Shared) -> Router {
    let protected = Router::new()
        .route("/overview", get(overview))
        .route("/clients/{mac}/time", post(add_time))
        .route("/clients/{mac}/kick", post(kick))
        .route("/clients/{mac}/pause", post(pause_client))
        .route("/clients/{mac}/resume", post(resume_client))
        .route("/devices", get(list_devices))
        .route("/devices/{mac}", get(device_detail))
        .route("/devices/{mac}/name", put(rename_device))
        .route("/devices/{mac}/blocked", put(set_blocked))
        .route("/report", get(report))
        .route("/sales.csv", get(sales_csv))
        .route("/coinbox", get(coinbox))
        .route("/coinbox/collect", post(collect_coins))
        .route("/goal", put(set_goal))
        .route("/rates", get(get_rates).put(set_rates))
        .route("/vouchers", get(list_vouchers).post(create_vouchers))
        .route("/vouchers/{id}", delete(delete_voucher))
        .route("/branding", put(set_branding))
        .route(
            "/brand/{name}",
            put(upload_image).delete(remove_image).layer(DefaultBodyLimit::max(crate::brand::MAX_ASSET_BYTES + 1024)),
        )
        .route("/qr/{kind}", get(qr_code))
        .route("/password", post(change_password))
        .route("/audit", get(audit))
        .route("/test-coin", post(test_coin))
        .route("/update", get(update_status))
        .route("/update/check", post(update_check))
        .route("/update/install", post(update_install))
        .route(
            "/update/upload",
            post(update_upload).delete(update_cancel).layer(DefaultBodyLimit::max(crate::update::MAX_UPLOAD_BYTES)),
        )
        .route("/update/settings", put(update_settings))
        .route("/logout", post(logout))
        .layer(from_fn_with_state(st.clone(), require_session));
    let api = Router::new()
        .route("/session", get(session))
        .route("/login", post(login))
        .merge(protected);
    Router::new()
        .route("/", get(|| async {
            static HTML: std::sync::OnceLock<String> = std::sync::OnceLock::new();
            app::page(include_str!("../web/admin/index.html"), &HTML)
        }))
        .route("/admin.js", get(|| async { asset("text/javascript; charset=utf-8", include_str!("../web/admin/admin.js")) }))
        .route("/admin.css", get(|| async { asset("text/css; charset=utf-8", include_str!("../web/admin/admin.css")) }))
        .route("/icon.svg", get(|| async { asset("image/svg+xml", include_str!("../web/shared/icon.svg")) }))
        .route("/brand/{name}", get(app::brand_image))
        .route("/coins.js", get(|| async { asset("text/javascript; charset=utf-8", include_str!("../web/shared/coins.js")) }))
        .nest("/api", api)
        .layer(from_fn_with_state(st.clone(), app::admin_mac_guard))
        .layer(from_fn_with_state(st.clone(), app::admin_host_guard))
        .layer(from_fn(crate::server::security_headers))
        .layer(from_fn(hsts))
        .layer(DefaultBodyLimit::max(16 * 1024))
        .with_state(st)
}

/// What a device not on `admin.allow_macs` sees: why it's locked and how
/// the owner gets back in. Served before any login, from its own small pool.
pub fn locked_router(st: Shared) -> Router {
    Router::new().fallback(locked_page).layer(from_fn(hsts)).with_state(st)
}

async fn locked_page(State(st): State<Shared>, ConnectInfo(addr): ConnectInfo<SocketAddr>) -> Response {
    // Every value below is formatted by us (a MAC or our own URL), never raw input.
    let mac = st.identify(addr).ok().map(|(m, _)| m.to_string());
    let device = mac.clone().unwrap_or_else(|| "Couldn't tell which device this is. Reconnect to the WiFi and reload.".into());
    let html = include_str!("../web/admin/locked.html")
        .replace("{device}", &device)
        .replace("{mac}", mac.as_deref().unwrap_or("YOUR-PHONE-MAC"))
        .replace("{portal}", &st.portal_url());
    (
        StatusCode::FORBIDDEN,
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
            (
                header::CONTENT_SECURITY_POLICY,
                "default-src 'none'; style-src 'unsafe-inline'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'",
            ),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::REFERRER_POLICY, "no-referrer"),
        ],
        html,
    )
        .into_response()
}

async fn hsts(req: Request, next: Next) -> Response {
    let mut res = next.run(req).await;
    res.headers_mut()
        .insert(header::STRICT_TRANSPORT_SECURITY, HeaderValue::from_static("max-age=31536000"));
    res
}

fn cookie_name(st: &Shared) -> &'static str {
    // The __Host- prefix forces Secure + Path=/ + no Domain. Dev mode is plain HTTP.
    if st.dev { "pw_admin" } else { "__Host-pw_admin" }
}

fn session_cookie(st: &Shared, token: &str, max_age: u64) -> HeaderValue {
    let secure = if st.dev { "" } else { "; Secure" };
    HeaderValue::from_str(&format!(
        "{}={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age={max_age}{secure}",
        cookie_name(st)
    ))
    .unwrap()
}

async fn require_session(
    State(st): State<Shared>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    mut req: Request,
    next: Next,
) -> Response {
    let info = app::cookie(req.headers(), cookie_name(&st)).and_then(|t| st.auth.check(t, addr.ip()));
    let Some(info) = info else {
        return ApiError::new(StatusCode::UNAUTHORIZED, "Please sign in").into_response();
    };
    if req.method() != Method::GET && req.method() != Method::HEAD {
        let token = req.headers().get("x-csrf-token").and_then(|v| v.to_str().ok());
        if !Auth::csrf_ok(&info, token) || !app::same_origin_write(req.headers()) {
            return ApiError::new(StatusCode::FORBIDDEN, "Security check failed. Reload the page.").into_response();
        }
    }
    req.extensions_mut().insert(std::sync::Arc::new(info));
    next.run(req).await
}

fn actor(addr: SocketAddr) -> String {
    format!("owner@{}", addr.ip())
}

// ---- session -------------------------------------------------------------

async fn session(State(st): State<Shared>, ConnectInfo(addr): ConnectInfo<SocketAddr>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    let needs_setup = st.db.get_setting(auth::PASSWORD_KEY)?.is_none();
    let info: Option<SessionInfo> =
        app::cookie(&headers, cookie_name(&st)).and_then(|t| st.auth.check(t, addr.ip()));
    Ok(Json(json!({
        "logged_in": info.is_some(),
        "csrf": info.map(|i| i.csrf),
        "needs_setup": needs_setup,
        "dev": st.dev,
    })))
}

#[derive(Deserialize)]
struct LoginReq {
    password: String,
}

async fn login(
    State(st): State<Shared>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(req): Json<LoginReq>,
) -> ApiResult<Response> {
    if !app::same_origin_write(&headers) {
        return Err(ApiError::new(StatusCode::FORBIDDEN, "Blocked"));
    }
    let ip = addr.ip();
    match st.auth.check_rate(ip) {
        LoginCheck::Allowed => {}
        LoginCheck::LockedOut(secs) => {
            return Err(ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                format!("Too many attempts. Try again in {} minutes.", secs.div_ceil(60)),
            ))
        }
        LoginCheck::Throttled(secs) => {
            return Err(ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                format!("Too many sign-in attempts on this box. Try again in {secs} seconds."),
            ))
        }
    }
    let Some(phc) = st.db.get_setting(auth::PASSWORD_KEY)? else {
        return Err(ApiError::bad("No password set yet. Run `sudo pisowifi set-password` on the device."));
    };
    if req.password.len() > 256 {
        return Err(ApiError::new(StatusCode::UNAUTHORIZED, "Wrong password"));
    }
    // One Argon2 at a time keeps a login flood from starving the box.
    let _permit = st.auth.hashing.acquire().await.map_err(|e| anyhow::anyhow!("{e}"))?;
    let pw = zeroize::Zeroizing::new(req.password);
    let (ok, upgraded) = tokio::task::spawn_blocking(move || auth::verify_and_upgrade(&pw, &phc))
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    if !ok {
        let slow_mode = st.auth.record_fail(ip);
        st.db.audit(&actor(addr), "login_failed", "");
        if slow_mode {
            tracing::warn!("many failed dashboard logins: box-wide slow mode on (1 try per 30 s)");
            st.db.audit("system", "login_throttled", "20+ failed sign-ins in 15 min; 1 attempt per 30 s");
        }
        return Err(ApiError::new(StatusCode::UNAUTHORIZED, "Wrong password"));
    }
    if let Some(hash) = upgraded {
        // Stored hash used older Argon2 settings: replace it now we know the password.
        st.db.set_setting(auth::PASSWORD_KEY, &hash)?;
    }
    let (token, csrf) = st.auth.create(ip);
    st.db.audit(&actor(addr), "login", "");
    let mut res = Json(json!({ "ok": true, "csrf": csrf })).into_response();
    res.headers_mut().insert(header::SET_COOKIE, session_cookie(&st, &token, st.auth.max_age_secs()));
    Ok(res)
}

async fn logout(State(st): State<Shared>, headers: HeaderMap) -> Response {
    if let Some(t) = app::cookie(&headers, cookie_name(&st)) {
        st.auth.destroy(t);
    }
    let mut res = Json(json!({ "ok": true })).into_response();
    res.headers_mut().insert(header::SET_COOKIE, session_cookie(&st, "", 0));
    res
}

// ---- dashboard -----------------------------------------------------------

async fn overview(State(st): State<Shared>) -> ApiResult<Json<Value>> {
    let sales = st.db.sales_summary(14)?;
    let clients = st.engine.clients();
    let active = clients.iter().filter(|c| c.running).count();
    // Devices on the WiFi without time, so the owner can give them some.
    let mut lan_devices = crate::net::lan_devices(&st.cfg.network.lan_iface, &st.cfg.network.dhcp_leases, st.dev);
    st.devices.saw_hostnames(&st.db, lan_devices.iter().filter_map(|d| Some((d.mac, d.name.as_deref()?))));
    lan_devices.retain(|d| !clients.iter().any(|c| c.mac == d.mac.to_string()));
    lan_devices.truncate(200);
    let slot = st.engine.slot_admin();
    let devices = device_names(
        &st,
        clients
            .iter()
            .map(|c| c.mac.clone())
            .chain(lan_devices.iter().map(|d| d.mac.to_string()))
            .chain(sales.top_customers.iter().map(|c| c.mac.clone()))
            .chain(sales.recent.iter().filter_map(|s| s.mac.clone()))
            .chain(slot.mac.clone()),
    );
    Ok(Json(json!({
        "name": st.brand.get().name,
        "brand": st.brand.get(),
        "sales": sales,
        "clients": clients,
        "lan_devices": lan_devices,
        "devices": devices,
        "coinbox": st.db.coinbox()?,
        "goal": daily_goal(&st)?,
        "health": crate::health::read(&st.cfg.storage.data_dir, &st.cfg.network.wan_iface, st.dev),
        "active": active,
        "slot": slot,
        "uptime_secs": st.engine.uptime_secs(),
        "fingerprint": st.fingerprint,
        "portal_url": st.portal_url(),
        "hub": st.hub.as_ref().map(|h| h.status()),
        "version": env!("CARGO_PKG_VERSION"),
        "dev": st.dev,
        "server_time": crate::db::now(),
    })))
}

fn parse_mac(s: &str) -> ApiResult<Mac> {
    Mac::parse(s).map_err(|_| ApiError::bad("Bad device address"))
}

#[derive(Deserialize)]
struct MinutesReq {
    minutes: u32,
}

async fn add_time(
    State(st): State<Shared>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Path(mac): Path<String>,
    Json(req): Json<MinutesReq>,
) -> ApiResult<Json<Value>> {
    let mac = parse_mac(&mac)?;
    if req.minutes == 0 || req.minutes > 60 * 24 * 30 {
        return Err(ApiError::bad("Minutes must be between 1 and 43200"));
    }
    let secs = req.minutes as u64 * 60;
    // Where the device is on the LAN (only needed if it has no time yet).
    let seen_at = crate::net::ip_for_mac(mac, &st.cfg.network.lan_iface, st.dev)
        .or_else(|| {
            let leases = crate::net::read_leases(&st.cfg.network.dhcp_leases).ok()?;
            leases.into_iter().find(|l| l.mac == mac).map(|l| l.ip)
        })
        .filter(|ip| st.dev || crate::config::in_subnet(*ip, st.lan.0, st.lan.1));
    let how = st.engine.admin_add_time(mac, secs, seen_at);
    if how == AdminAdd::Unknown {
        return Err(ApiError::bad("That device isn't on the WiFi right now"));
    }
    if how == AdminAdd::Blocked {
        return Err(ApiError::bad("This device is blocked. Unblock it first."));
    }
    let paused = how == AdminAdd::Paused;
    st.db.add_sale(Some(mac), 0, secs, "manual")?;
    st.db.audit(&actor(addr), "add_time", &format!("{mac} +{}m{}", req.minutes, if paused { " (paused)" } else { "" }));
    Ok(Json(json!({ "ok": true, "paused": paused })))
}

async fn kick(State(st): State<Shared>, ConnectInfo(addr): ConnectInfo<SocketAddr>, Path(mac): Path<String>) -> ApiResult<Json<Value>> {
    let mac = parse_mac(&mac)?;
    if !st.engine.kick(mac) {
        return Err(ApiError::bad("Device is not connected"));
    }
    st.db.audit(&actor(addr), "kick", &mac.to_string());
    Ok(Json(json!({ "ok": true })))
}

/// Where a device with time is, for pausing and resuming it from here.
fn client_ip(st: &Shared, mac: Mac) -> ApiResult<std::net::Ipv4Addr> {
    let m = mac.to_string();
    st.engine
        .clients()
        .into_iter()
        .find(|c| c.mac == m)
        .and_then(|c| c.ip.parse().ok())
        .ok_or_else(|| ApiError::bad("That device has no time right now"))
}

async fn pause_client(State(st): State<Shared>, ConnectInfo(addr): ConnectInfo<SocketAddr>, Path(mac): Path<String>) -> ApiResult<Json<Value>> {
    let mac = parse_mac(&mac)?;
    if !st.engine.pause(mac, client_ip(&st, mac)?) {
        return Err(ApiError::bad("Its time is already paused"));
    }
    st.db.audit(&actor(addr), "pause", &mac.to_string());
    Ok(Json(json!({ "ok": true })))
}

async fn resume_client(State(st): State<Shared>, ConnectInfo(addr): ConnectInfo<SocketAddr>, Path(mac): Path<String>) -> ApiResult<Json<Value>> {
    let mac = parse_mac(&mac)?;
    match st.engine.resume(mac, client_ip(&st, mac)?) {
        Resume::Resumed => {}
        Resume::Nothing => return Err(ApiError::bad("Its time is already running")),
        Resume::Blocked => return Err(ApiError::bad("This device is blocked. Unblock it first.")),
        Resume::NeedsHub => {
            return Err(ApiError::bad("This is Piso Pass time and the hub hasn't answered lately. Try again in a minute."))
        }
    }
    st.db.audit(&actor(addr), "resume", &mac.to_string());
    Ok(Json(json!({ "ok": true })))
}

// ---- devices ---------------------------------------------------------------

/// A device as the dashboard shows it: its name and model, and whether it's blocked.
fn device_json(st: &Shared, mac: Mac) -> Value {
    let mut v = json!(st.devices.info(mac));
    v["blocked"] = json!(st.engine.is_blocked(mac));
    v
}

/// What to call each device the page is about to show.
fn device_names(st: &Shared, macs: impl IntoIterator<Item = String>) -> serde_json::Map<String, Value> {
    let mut out = serde_json::Map::new();
    for m in macs {
        if out.contains_key(&m) {
            continue;
        }
        if let Ok(mac) = Mac::parse(&m) {
            out.insert(m, device_json(st, mac));
        }
    }
    out
}

#[derive(Serialize)]
struct DeviceItem {
    mac: String,
    #[serde(flatten)]
    info: DeviceInfo,
    spend: Spend,
    /// Has time here now (running or paused).
    has_time: bool,
    blocked: bool,
    running: bool,
    remaining_secs: u64,
}

/// Every device that has opened the customer page or bought time here.
async fn list_devices(State(st): State<Shared>) -> ApiResult<Json<Value>> {
    let spend = st.db.device_spend()?;
    let clients = st.engine.clients();
    let blocked = st.engine.blocked();
    let mut seen: HashMap<Mac, DeviceInfo> = st.devices.all().into_iter().collect();
    for m in spend.keys().map(String::as_str).chain(clients.iter().map(|c| c.mac.as_str())) {
        if let Ok(mac) = Mac::parse(m) {
            seen.entry(mac).or_insert_with(|| st.devices.info(mac));
        }
    }
    for mac in &blocked {
        seen.entry(*mac).or_insert_with(|| st.devices.info(*mac));
    }
    let mut list: Vec<DeviceItem> = seen
        .into_iter()
        .map(|(mac, info)| {
            let m = mac.to_string();
            let client = clients.iter().find(|c| c.mac == m);
            DeviceItem {
                spend: spend.get(&m).copied().unwrap_or_default(),
                has_time: client.is_some(),
                blocked: blocked.contains(&mac),
                running: client.is_some_and(|c| c.running),
                remaining_secs: client.map_or(0, |c| c.remaining_secs),
                mac: m,
                info,
            }
        })
        .collect();
    let recent = |d: &DeviceItem| d.info.last_seen.unwrap_or(0).max(d.spend.last);
    list.sort_by(|a, b| b.has_time.cmp(&a.has_time).then(recent(b).cmp(&recent(a))));
    list.truncate(500);
    Ok(Json(json!({ "devices": list, "server_time": crate::db::now() })))
}

async fn device_detail(State(st): State<Shared>, Path(mac): Path<String>) -> ApiResult<Json<Value>> {
    let mac = parse_mac(&mac)?;
    let m = mac.to_string();
    let client = st.engine.clients().into_iter().find(|c| c.mac == m);
    Ok(Json(json!({
        "mac": m,
        "device": device_json(&st, mac),
        "spend": st.db.device_spend()?.get(&m).copied().unwrap_or_default(),
        "sales": st.db.device_sales(mac, 60)?,
        "client": client,
        "server_time": crate::db::now(),
    })))
}

#[derive(Deserialize)]
struct NameReq {
    name: String,
}

async fn rename_device(
    State(st): State<Shared>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Path(mac): Path<String>,
    Json(req): Json<NameReq>,
) -> ApiResult<Json<Value>> {
    let mac = parse_mac(&mac)?;
    let name = crate::brand::clean_text(&req.name, 32);
    st.devices.set_nickname(&st.db, mac, &name)?;
    st.db.audit(&actor(addr), "rename_device", &format!("{mac} → {}", if name.is_empty() { "(no name)" } else { &name }));
    Ok(Json(json!({ "ok": true, "device": device_json(&st, mac) })))
}

#[derive(Deserialize)]
struct BlockReq {
    blocked: bool,
}

/// Blocks a device (its time here ends; it can't buy or use time) or lets it back.
async fn set_blocked(
    State(st): State<Shared>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Path(mac): Path<String>,
    Json(req): Json<BlockReq>,
) -> ApiResult<Json<Value>> {
    let mac = parse_mac(&mac)?;
    if req.blocked {
        if st.admin_macs.contains(&mac) {
            return Err(ApiError::bad("That's one of your own dashboard devices"));
        }
        if st.engine.block(mac)? == crate::engine::Block::Inserting {
            return Err(ApiError::bad("It's inserting coins right now. Try again once they tap Start."));
        }
        st.db.audit(&actor(addr), "block_device", &mac.to_string());
    } else if st.engine.unblock(mac)? {
        st.db.audit(&actor(addr), "unblock_device", &mac.to_string());
    }
    Ok(Json(json!({ "ok": true, "device": device_json(&st, mac) })))
}

// ---- sales -----------------------------------------------------------------

#[derive(Deserialize)]
struct PeriodQuery {
    #[serde(default)]
    days: u32,
}

fn period(q: &PeriodQuery) -> u32 {
    if [1, 7, 30, 90, 365].contains(&q.days) { q.days } else { 30 }
}

async fn report(State(st): State<Shared>, Query(q): Query<PeriodQuery>) -> ApiResult<Json<Value>> {
    Ok(Json(json!(st.db.report(period(&q))?)))
}

/// Every sale in the period as a spreadsheet (CSV, opens in Excel or Sheets).
async fn sales_csv(State(st): State<Shared>, Query(q): Query<PeriodQuery>) -> ApiResult<Response> {
    use chrono::{Local, TimeZone};
    let days = period(&q);
    let today = Local::now().date_naive();
    let first = today - chrono::Days::new(days as u64 - 1);
    let since = Local
        .from_local_datetime(&first.and_hms_opt(0, 0, 0).unwrap())
        .earliest()
        .map_or(0, |t| t.timestamp());
    let mut names: HashMap<String, String> = HashMap::new();
    // A byte-order mark so Excel reads the names as UTF-8.
    let mut out = String::from("\u{FEFF}Date,Time,Device,Device name,How,Pesos,Minutes\n");
    for s in st.db.sales_since(since)? {
        let at = Local.timestamp_opt(s.ts, 0).single();
        let name = match &s.mac {
            Some(m) => names
                .entry(m.clone())
                .or_insert_with(|| Mac::parse(m).map(|mac| st.devices.info(mac).name).unwrap_or_default())
                .clone(),
            None => String::new(),
        };
        let how = match s.source.as_str() {
            "coin" => "Coins",
            "voucher" => "Voucher",
            "manual" => "Added by owner",
            "unclaimed" => "Unclaimed coins",
            other => other,
        };
        let row = [
            at.map(|t| t.format("%Y-%m-%d").to_string()).unwrap_or_default(),
            at.map(|t| t.format("%H:%M").to_string()).unwrap_or_default(),
            s.mac.clone().unwrap_or_default(),
            name,
            how.to_string(),
            s.pesos.to_string(),
            (s.secs / 60).to_string(),
        ];
        out.push_str(&row.iter().map(|f| csv_field(f)).collect::<Vec<_>>().join(","));
        out.push('\n');
    }
    let file = format!("attachment; filename=\"sales-{}-{}days.csv\"", today.format("%Y-%m-%d"), days);
    Ok((
        [
            (header::CONTENT_TYPE, HeaderValue::from_static("text/csv; charset=utf-8")),
            (header::CONTENT_DISPOSITION, HeaderValue::from_str(&file).map_err(|e| anyhow::anyhow!("{e}"))?),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-store")),
        ],
        out,
    )
        .into_response())
}

/// One spreadsheet cell. Device names come from customers' phones, and
/// spreadsheet apps run cells that start with = + - @ as formulas.
fn csv_field(s: &str) -> String {
    let s = if s.starts_with(['=', '+', '-', '@', '\t', '\r']) { format!("'{s}") } else { s.to_string() };
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s
    }
}

// ---- coin box and goal -------------------------------------------------------

async fn coinbox(State(st): State<Shared>) -> ApiResult<Json<Value>> {
    Ok(Json(json!(st.db.coinbox()?)))
}

#[derive(Deserialize)]
struct CollectReq {
    /// What the owner counted; missing means they didn't count.
    #[serde(default)]
    counted: Option<u64>,
    #[serde(default)]
    note: String,
}

async fn collect_coins(
    State(st): State<Shared>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(req): Json<CollectReq>,
) -> ApiResult<Json<Value>> {
    if req.counted.is_some_and(|c| c > 10_000_000) {
        return Err(ApiError::bad("That's more than a coin box holds"));
    }
    let note = crate::brand::clean_text(&req.note, 80);
    let c = st.db.collect(req.counted, &note)?;
    st.db.audit(&actor(addr), "collect_coins", &format!("counted ₱{}, expected ₱{}", c.counted, c.expected));
    Ok(Json(json!({ "collection": c, "coinbox": st.db.coinbox()? })))
}

fn daily_goal(st: &Shared) -> ApiResult<u32> {
    Ok(st.db.get_setting("daily_goal")?.and_then(|v| v.parse().ok()).unwrap_or(0))
}

#[derive(Deserialize)]
struct GoalReq {
    pesos: u32,
}

async fn set_goal(State(st): State<Shared>, ConnectInfo(addr): ConnectInfo<SocketAddr>, Json(req): Json<GoalReq>) -> ApiResult<Json<Value>> {
    if req.pesos > 1_000_000 {
        return Err(ApiError::bad("Pick a goal under ₱1,000,000 a day"));
    }
    st.db.set_setting("daily_goal", &req.pesos.to_string())?;
    st.db.audit(&actor(addr), "set_goal", &format!("₱{} a day", req.pesos));
    Ok(Json(json!({ "ok": true, "goal": req.pesos })))
}

async fn get_rates(State(st): State<Shared>) -> ApiResult<Json<Vec<Rate>>> {
    Ok(Json(st.db.rates()?))
}

async fn set_rates(
    State(st): State<Shared>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(mut req): Json<Vec<Rate>>,
) -> ApiResult<Json<Vec<Rate>>> {
    rates::validate(&req).map_err(ApiError::bad)?;
    req.sort_by_key(|r| r.pesos);
    st.db.set_rates(&req)?;
    st.engine.set_rates(req.clone());
    let detail = req.iter().map(|r| format!("₱{}={}m", r.pesos, r.minutes)).collect::<Vec<_>>().join(", ");
    st.db.audit(&actor(addr), "set_rates", &detail);
    Ok(Json(req))
}

async fn list_vouchers(State(st): State<Shared>) -> ApiResult<Json<Value>> {
    Ok(Json(json!({ "vouchers": st.db.list_vouchers()? })))
}

#[derive(Deserialize)]
struct VoucherReq {
    count: u32,
    minutes: u32,
    #[serde(default)]
    label: String,
}

async fn create_vouchers(
    State(st): State<Shared>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(req): Json<VoucherReq>,
) -> ApiResult<Json<Value>> {
    if !(1..=100).contains(&req.count) {
        return Err(ApiError::bad("Create 1–100 vouchers at a time"));
    }
    if req.minutes == 0 || req.minutes > 60 * 24 * 30 {
        return Err(ApiError::bad("Minutes must be between 1 and 43200"));
    }
    let label: String = req.label.trim().chars().filter(|c| !c.is_control()).take(40).collect();
    let codes = st.db.create_vouchers(req.count, req.minutes as u64 * 60, &label)?;
    st.db.audit(&actor(addr), "create_vouchers", &format!("{} × {}m {label}", req.count, req.minutes));
    Ok(Json(json!({ "codes": codes, "minutes": req.minutes, "label": label })))
}

async fn delete_voucher(State(st): State<Shared>, ConnectInfo(addr): ConnectInfo<SocketAddr>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    if !st.db.delete_unused_voucher(&id)? {
        return Err(ApiError::bad("Voucher not found or already used"));
    }
    st.db.audit(&actor(addr), "delete_voucher", &id);
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
struct BrandingReq {
    name: String,
    accent: String,
    #[serde(default)]
    poster_alt: String,
    #[serde(default)]
    wifi_name: String,
    /// Left out: unchanged.
    sounds: Option<bool>,
    after_url: Option<String>,
    notice: Option<String>,
}

async fn set_branding(
    State(st): State<Shared>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(req): Json<BrandingReq>,
) -> ApiResult<Json<Value>> {
    use crate::brand::{clean_text, valid_accent};
    let name = clean_text(&req.name, 40);
    if name.is_empty() {
        return Err(ApiError::bad("Give your WiFi a name"));
    }
    if !valid_accent(&req.accent) {
        return Err(ApiError::bad("Pick a colour like #5A43E6"));
    }
    let (alt, wifi) = (clean_text(&req.poster_alt, 120), clean_text(&req.wifi_name, 32));
    let now = st.brand.get();
    let after_url = match &req.after_url {
        Some(u) => crate::brand::clean_after_url(u).ok_or_else(|| ApiError::bad("Use a web address like google.com"))?,
        None => now.after_url,
    };
    st.brand.set_online(&st.db, req.sounds.unwrap_or(now.sounds), &after_url)?;
    if let Some(notice) = &req.notice {
        st.brand.set_notice(&st.db, &clean_text(notice, 140))?;
    }
    st.brand.set_text(&st.db, &name, &req.accent.to_uppercase(), &alt, &wifi)?;
    st.db.audit(&actor(addr), "branding", &format!("{name} · {}", req.accent));
    Ok(Json(json!({ "ok": true, "brand": st.brand.get() })))
}

async fn upload_image(
    State(st): State<Shared>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Path(name): Path<String>,
    body: bytes::Bytes,
) -> ApiResult<Json<Value>> {
    let size = body.len();
    match st.brand.set_asset(&st.db, &name, body.to_vec())? {
        Ok(()) => {
            st.db.audit(&actor(addr), "upload_image", &format!("{name} {} KB", size / 1024));
            Ok(Json(json!({ "ok": true, "brand": st.brand.get() })))
        }
        Err(msg) => Err(ApiError::bad(msg)),
    }
}

async fn remove_image(State(st): State<Shared>, ConnectInfo(addr): ConnectInfo<SocketAddr>, Path(name): Path<String>) -> ApiResult<Json<Value>> {
    if !st.brand.remove_asset(&st.db, &name)? {
        return Err(ApiError::bad("Nothing to remove"));
    }
    st.db.audit(&actor(addr), "remove_image", &name);
    Ok(Json(json!({ "ok": true, "brand": st.brand.get() })))
}

/// QR codes for the printable sign: "wifi" joins the network straight from
/// the phone camera; "portal" opens the customer page with onboarding.
async fn qr_code(State(st): State<Shared>, Path(kind): Path<String>) -> ApiResult<Response> {
    let text = match kind.as_str() {
        "portal" => format!("{}?welcome=1", st.portal_url()),
        "wifi" => {
            let ssid = st.brand.get().wifi_name;
            if ssid.is_empty() {
                return Err(ApiError::bad("Set your WiFi name first"));
            }
            let esc: String = ssid.chars().flat_map(|c| if "\\;,:\"".contains(c) { vec!['\\', c] } else { vec![c] }).collect();
            format!("WIFI:S:{esc};T:nopass;;")
        }
        _ => return Err(ApiError::new(StatusCode::NOT_FOUND, "Unknown QR code")),
    };
    let code = qrcode::QrCode::with_error_correction_level(text.as_bytes(), qrcode::EcLevel::M).map_err(|e| anyhow::anyhow!("{e}"))?;
    let svg = code
        .render::<qrcode::render::svg::Color>()
        .min_dimensions(240, 240)
        .quiet_zone(true)
        .dark_color(qrcode::render::svg::Color("#1E1F2E"))
        .light_color(qrcode::render::svg::Color("#FFFFFF"))
        .build();
    Ok(([(header::CONTENT_TYPE, "image/svg+xml"), (header::CACHE_CONTROL, "no-store")], svg).into_response())
}

#[derive(Deserialize)]
struct PasswordReq {
    current: String,
    new: String,
}

async fn change_password(
    State(st): State<Shared>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(req): Json<PasswordReq>,
) -> ApiResult<Response> {
    let Some(phc) = st.db.get_setting(auth::PASSWORD_KEY)? else {
        return Err(ApiError::bad("No password set"));
    };
    if req.current.len() > 256 || req.new.len() > 256 {
        return Err(ApiError::bad("Password too long"));
    }
    let _permit = st.auth.hashing.acquire().await.map_err(|e| anyhow::anyhow!("{e}"))?;
    let (cur, new) = (zeroize::Zeroizing::new(req.current), zeroize::Zeroizing::new(req.new));
    let b = st.brand.get();
    let words = [b.name, b.wifi_name];
    let res = tokio::task::spawn_blocking(move || {
        if !auth::verify_password(&cur, &phc) {
            return Ok(None);
        }
        auth::hash_password(&new, &[words[0].as_str(), words[1].as_str()]).map(Some)
    })
    .await
    .map_err(|e| anyhow::anyhow!("{e}"))?;
    match res {
        Ok(Some(hash)) => {
            st.db.set_setting(auth::PASSWORD_KEY, &hash)?;
            st.auth.destroy_all();
            st.db.audit(&actor(addr), "password_changed", "");
            let mut r = Json(json!({ "ok": true })).into_response();
            r.headers_mut().insert(header::SET_COOKIE, session_cookie(&st, "", 0));
            Ok(r)
        }
        Ok(None) => Err(ApiError::new(StatusCode::UNAUTHORIZED, "Current password is wrong")),
        Err(e) => Err(ApiError::bad(e.to_string())),
    }
}

async fn audit(State(st): State<Shared>) -> ApiResult<Json<Value>> {
    Ok(Json(json!({ "entries": st.db.list_audit(100)? })))
}

#[derive(Deserialize)]
struct TestCoinReq {
    pesos: u32,
}

/// Simulated coins, only in dev mode (real boxes must use real coins so
/// the sales ledger can't be padded from the web).
async fn test_coin(State(st): State<Shared>, Json(req): Json<TestCoinReq>) -> ApiResult<Json<Value>> {
    if !st.dev {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "Not available"));
    }
    if !(1..=50).contains(&req.pesos) {
        return Err(ApiError::bad("1–50 pesos"));
    }
    st.engine.test_coin(req.pesos);
    Ok(Json(json!({ "ok": true })))
}

// ---- software updates ----------------------------------------------------

async fn update_status(State(st): State<Shared>) -> Json<Value> {
    Json(st.updater.view())
}

async fn update_check(State(st): State<Shared>) -> Json<Value> {
    st.updater.clear_error();
    st.updater.check().await;
    Json(st.updater.view())
}

/// The owner's file (for a vendo with no internet): checked now, installed
/// only after they confirm with Install now.
async fn update_upload(State(st): State<Shared>, ConnectInfo(addr): ConnectInfo<SocketAddr>, body: bytes::Bytes) -> ApiResult<Json<Value>> {
    let u = st.updater.clone();
    let checked = tokio::task::spawn_blocking(move || u.upload(&body)).await.map_err(|e| anyhow::anyhow!("{e}"))?;
    match checked {
        Ok(m) => st.db.audit(&actor(addr), "update_uploaded", &format!("version {}", m.version)),
        Err(msg) => {
            st.db.audit(&actor(addr), "update_refused", &msg);
            return Err(ApiError::bad(msg));
        }
    }
    Ok(Json(st.updater.view()))
}

async fn update_cancel(State(st): State<Shared>) -> Json<Value> {
    st.updater.cancel_upload();
    Json(st.updater.view())
}

/// The uploaded file, or (with online checks on) the newest version online;
/// the card follows the progress via /update.
async fn update_install(State(st): State<Shared>, ConnectInfo(addr): ConnectInfo<SocketAddr>) -> ApiResult<Json<Value>> {
    st.updater.start_install(&actor(addr), false).map_err(ApiError::bad)?;
    Ok(Json(st.updater.view()))
}

#[derive(Deserialize)]
struct UpdateSettingsReq {
    online: bool,
    auto: bool,
    /// Another update list address; empty means the default one.
    #[serde(default)]
    url: String,
}

async fn update_settings(
    State(st): State<Shared>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(req): Json<UpdateSettingsReq>,
) -> ApiResult<Json<Value>> {
    st.updater.set_settings(req.online, req.auto, &req.url).map_err(ApiError::bad)?;
    let detail = match st.updater.url() {
        Some(u) => format!("online checks at {u}; install at 3 AM {}", if st.updater.auto() { "on" } else { "off" }),
        None => "online checks off".into(),
    };
    st.db.audit(&actor(addr), "update_settings", &detail);
    if req.online {
        let u = st.updater.clone();
        tokio::spawn(async move { u.check().await });
    }
    Ok(Json(st.updater.view()))
}
