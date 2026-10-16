//! Shared web state, errors and request guards.

use crate::auth::Auth;
use crate::brand::BrandStore;
use crate::config::{in_subnet, Config};
use crate::db::Db;
use crate::engine::Engine;
use crate::net::{mac_for_ip, Mac};
use axum::extract::{ConnectInfo, Path, Request, State};
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};
use axum::Json;
use serde_json::json;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

pub struct AppState {
    pub cfg: Config,
    pub dev: bool,
    pub engine: Arc<Engine>,
    pub db: Arc<Db>,
    pub auth: Auth,
    pub brand: BrandStore,
    /// Set when this vendo is part of a chain (Piso Pass enabled).
    pub hub: Option<Arc<crate::hubclient::HubClient>>,
    pub lan: (Ipv4Addr, u8),
    pub admin_macs: Vec<Mac>,
    pub fingerprint: String,
}

pub type Shared = Arc<AppState>;

/// An error for the browser or a vendo: a message for people and, where a
/// program needs to tell cases apart, a fixed `code`.
pub struct ApiError(pub StatusCode, pub String, pub Option<&'static str>);

impl ApiError {
    pub fn new(code: StatusCode, msg: impl Into<String>) -> Self {
        ApiError(code, msg.into(), None)
    }
    pub fn bad(msg: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, msg)
    }
    pub fn with_code(mut self, code: &'static str) -> Self {
        self.2 = Some(code);
        self
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        tracing::error!("{e:#}");
        ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "Internal error")
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = match self.2 {
            Some(code) => json!({ "error": self.1, "code": code }),
            None => json!({ "error": self.1 }),
        };
        (self.0, Json(body)).into_response()
    }
}

pub type ApiResult<T> = Result<T, ApiError>;

impl AppState {
    /// Who is this? Uses only the TCP peer address and the ARP table.
    pub fn identify(&self, addr: SocketAddr) -> ApiResult<(Mac, Ipv4Addr)> {
        let IpAddr::V4(ip) = addr.ip() else {
            return Err(ApiError::new(StatusCode::FORBIDDEN, "IPv6 is not supported"));
        };
        if !self.dev && !in_subnet(ip, self.lan.0, self.lan.1) {
            return Err(ApiError::new(StatusCode::FORBIDDEN, "Not on the WiFi network"));
        }
        let mac = mac_for_ip(ip, &self.cfg.network.lan_iface, self.dev).ok_or_else(|| {
            ApiError::new(StatusCode::FORBIDDEN, "Can't identify your device. Reconnect to the WiFi.")
        })?;
        Ok((mac, ip))
    }

    fn host_ok(&self, headers: &HeaderMap, port: u16) -> bool {
        let Some(host) = headers.get(header::HOST).and_then(|h| h.to_str().ok()) else {
            return false;
        };
        let ip = self.cfg.network.portal_ip.to_string();
        let mut ok = vec![format!("{ip}:{port}")];
        if port == 80 || port == 443 {
            ok.push(ip);
        }
        if self.dev {
            ok.push(format!("localhost:{port}"));
        }
        ok.iter().any(|h| h.eq_ignore_ascii_case(host))
    }

    pub fn portal_url(&self) -> String {
        let port = self.cfg.portal.listen.port();
        let ip = self.cfg.network.portal_ip;
        if port == 80 {
            format!("http://{ip}/")
        } else {
            format!("http://{ip}:{port}/")
        }
    }
}

/// Captive-portal behaviour: any request for another host (phones probing
/// connectivitycheck.gstatic.com, captive.apple.com, ...) is redirected to
/// the portal, which makes the "Sign in to network" sheet pop up.
pub async fn portal_host_guard(State(st): State<Shared>, req: Request, next: Next) -> Response {
    if !st.host_ok(req.headers(), st.cfg.portal.listen.port()) {
        return Redirect::to(&st.portal_url()).into_response();
    }
    next.run(req).await
}

/// Stops DNS-rebinding tricks against the dashboard.
pub async fn admin_host_guard(State(st): State<Shared>, req: Request, next: Next) -> Response {
    if !st.host_ok(req.headers(), st.cfg.admin.listen.port()) {
        return StatusCode::MISDIRECTED_REQUEST.into_response();
    }
    next.run(req).await
}

/// Optional owner-device allowlist for the dashboard.
pub async fn admin_mac_guard(
    State(st): State<Shared>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    req: Request,
    next: Next,
) -> Response {
    if !st.admin_macs.is_empty() {
        let allowed = st.identify(addr).map(|(mac, _)| st.admin_macs.contains(&mac)).unwrap_or(false);
        if !allowed {
            return StatusCode::NOT_FOUND.into_response();
        }
    }
    next.run(req).await
}

/// CSRF defence for the portal API: writes need a custom header (which a
/// cross-site page can't send without a CORS preflight we never approve)
/// and, when the browser sends one, a same-origin Origin header.
pub async fn portal_csrf_guard(req: Request, next: Next) -> Response {
    if req.method() != Method::GET && req.method() != Method::HEAD && !same_origin_write(req.headers()) {
        return ApiError::new(StatusCode::FORBIDDEN, "Blocked").into_response();
    }
    next.run(req).await
}

pub fn same_origin_write(h: &HeaderMap) -> bool {
    if h.get("x-pisowifi").map(|v| v.as_bytes()) != Some(b"1") {
        return false;
    }
    match (h.get(header::ORIGIN), h.get(header::HOST)) {
        (None, _) => true,
        (Some(o), Some(host)) => {
            let o = o.to_str().unwrap_or("");
            let host = host.to_str().unwrap_or("");
            o == format!("http://{host}") || o == format!("https://{host}")
        }
        _ => false,
    }
}

pub fn cookie<'a>(h: &'a HeaderMap, name: &str) -> Option<&'a str> {
    h.get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v)
}

/// Owner-uploaded logo/poster. URLs carry a version, so they cache forever.
pub async fn brand_image(State(st): State<Shared>, Path(name): Path<String>) -> Response {
    match st.brand.asset(&name) {
        Some(a) => (
            [(header::CONTENT_TYPE, a.mime), (header::CACHE_CONTROL, "public, max-age=31536000, immutable")],
            a.data.clone(),
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Short hash of every page file. Pages reference assets as `?v=<hash>`, so
/// a new build is never hidden behind a browser's cached copy, and assets
/// can be cached for a year (a big win on slow phones).
pub fn asset_version() -> &'static str {
    static V: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    V.get_or_init(|| {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        for part in [
            include_str!("../web/portal/index.html"),
            include_str!("../web/portal/app.js"),
            include_str!("../web/portal/app.css"),
            include_str!("../web/admin/index.html"),
            include_str!("../web/admin/admin.js"),
            include_str!("../web/admin/admin.css"),
            include_str!("../web/shared/coins.js"),
            include_str!("../web/shared/icon.svg"),
        ] {
            h.update(part.as_bytes());
        }
        hex::encode(&h.finalize()[..6])
    })
}

/// An HTML page with its asset links stamped with the current version.
pub fn page(html: &'static str, cell: &'static std::sync::OnceLock<String>) -> Response {
    let body = cell.get_or_init(|| html.replace("__V__", asset_version()));
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8"), (header::CACHE_CONTROL, "no-cache")],
        body.as_str(),
    )
        .into_response()
}

pub fn asset(content_type: &'static str, body: &'static str) -> Response {
    (
        [(header::CONTENT_TYPE, content_type), (header::CACHE_CONTROL, "public, max-age=31536000, immutable")],
        body,
    )
        .into_response()
}
