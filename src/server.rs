//! A small, defensive HTTP/1.1 server loop around an axum Router:
//! connection caps (total and per address), header read timeout (slowloris),
//! a whole-request timeout (slow bodies), TLS handshake timeout, and the real
//! TCP peer address attached to every request.
//!
//! An optional gate sorts connections right after accept, before any TLS
//! work: those it refuses get a fixed page from a small pool of their own,
//! so they can never use up the connections the full router has.

use axum::extract::ConnectInfo;
use axum::http::{header, HeaderValue, Request};
use axum::middleware::Next;
use axum::response::Response;
use axum::Router;
use hyper::body::Incoming;
use hyper_util::rt::{TokioIo, TokioTimer};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;
use tokio::sync::{watch, Semaphore};
use tokio_rustls::TlsAcceptor;
use tower::Service;

pub struct Limits {
    /// Open connections, in total.
    pub max_conns: usize,
    /// Open connections from one address (a phone's browser opens ~6).
    pub per_ip: usize,
    /// Longest a single request may take, body included.
    pub request_timeout: Duration,
}

/// Who may use the full router (checked per connection, before TLS).
pub struct Gate {
    pub allow: Box<dyn Fn(SocketAddr) -> bool + Send + Sync>,
    /// Served to everyone else: one request per connection.
    pub locked: Router,
}

/// Connections refused by the gate, in total and per address.
const LOCKED_CONNS: usize = 4;
const LOCKED_PER_IP: usize = 1;

/// Open connections per address; the guard gives the slot back.
#[derive(Clone, Default)]
struct PerIp(Arc<Mutex<HashMap<IpAddr, usize>>>);

struct IpSlot(PerIp, IpAddr);

impl PerIp {
    fn take(&self, ip: IpAddr, max: usize) -> Option<IpSlot> {
        let mut m = self.0.lock().unwrap();
        let n = m.entry(ip).or_insert(0);
        if *n >= max {
            return None;
        }
        *n += 1;
        Some(IpSlot(self.clone(), ip))
    }
}

impl Drop for IpSlot {
    fn drop(&mut self) {
        let mut m = (self.0).0.lock().unwrap();
        if let Some(n) = m.get_mut(&self.1) {
            *n -= 1;
            if *n == 0 {
                m.remove(&self.1);
            }
        }
    }
}

pub async fn serve(
    listener: TcpListener,
    router: Router,
    tls: Option<TlsAcceptor>,
    limits: Limits,
    gate: Option<Gate>,
    mut shutdown: watch::Receiver<bool>,
) {
    let limit = Arc::new(Semaphore::new(limits.max_conns));
    let per_ip = PerIp::default();
    let locked_limit = Arc::new(Semaphore::new(LOCKED_CONNS));
    let locked_per_ip = PerIp::default();
    loop {
        let (stream, addr) = tokio::select! {
            r = listener.accept() => match r {
                Ok(x) => x,
                Err(e) => {
                    tracing::warn!("accept: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            },
            _ = shutdown.changed() => return,
        };
        let (pool, pool_ip, per_ip_max, router, locked) = match &gate {
            Some(g) if !(g.allow)(addr) => (&locked_limit, &locked_per_ip, LOCKED_PER_IP, g.locked.clone(), true),
            _ => (&limit, &per_ip, limits.per_ip, router.clone(), false),
        };
        let Some(ip_slot) = pool_ip.take(addr.ip(), per_ip_max) else {
            continue; // this address already has plenty open
        };
        let Ok(permit) = pool.clone().try_acquire_owned() else {
            continue; // over capacity: drop the connection
        };
        let timeout = if locked { Duration::from_secs(5) } else { limits.request_timeout };
        let _ = stream.set_nodelay(true);
        let tls = tls.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let _ip_slot = ip_slot;
            match tls {
                None => serve_conn(stream, addr, router, timeout, locked).await,
                Some(acc) => {
                    match tokio::time::timeout(Duration::from_secs(10), acc.accept(stream)).await {
                        Ok(Ok(s)) => serve_conn(s, addr, router, timeout, locked).await,
                        _ => {}
                    }
                }
            }
        });
    }
}

/// `locked`: a single request, then the connection closes.
async fn serve_conn<I>(io: I, addr: SocketAddr, router: Router, timeout: Duration, locked: bool)
where
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let svc = hyper::service::service_fn(move |mut req: Request<Incoming>| {
        req.extensions_mut().insert(ConnectInfo(addr));
        let mut r = router.clone();
        async move {
            match tokio::time::timeout(timeout, r.call(req)).await {
                Ok(res) => res,
                Err(_) => {
                    let mut res = Response::new(axum::body::Body::from("Request took too long"));
                    *res.status_mut() = http::StatusCode::REQUEST_TIMEOUT;
                    Ok(res)
                }
            }
        }
    });
    let conn = hyper::server::conn::http1::Builder::new()
        .timer(TokioTimer::new())
        .header_read_timeout(Duration::from_secs(if locked { 5 } else { 10 }))
        .keep_alive(!locked)
        .max_buf_size(64 * 1024)
        .serve_connection(TokioIo::new(io), svc);
    // Hard cap so idle keep-alive connections can't pile up.
    let _ = tokio::time::timeout(Duration::from_secs(if locked { 15 } else { 120 }), conn).await;
}

/// Security headers for every response.
pub async fn security_headers(req: Request<axum::body::Body>, next: Next) -> Response {
    let is_api = req.uri().path().starts_with("/api/");
    let mut res = next.run(req).await;
    let h = res.headers_mut();
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; \
             font-src 'self'; connect-src 'self'; manifest-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'",
        ),
    );
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    h.insert("cross-origin-opener-policy", HeaderValue::from_static("same-origin"));
    h.insert("cross-origin-resource-policy", HeaderValue::from_static("same-origin"));
    h.insert("permissions-policy", HeaderValue::from_static("camera=(), microphone=(), geolocation=()"));
    if is_api || !h.contains_key(header::CACHE_CONTROL) {
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    res
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::get;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn get_page(addr: SocketAddr) -> String {
        let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
        s.write_all(b"GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").await.unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).await.unwrap();
        out
    }

    #[tokio::test]
    async fn refused_devices_get_the_locked_page_and_cant_use_up_the_owners_slots() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let open = Arc::new(AtomicBool::new(false));
        let o = open.clone();
        let gate = Gate {
            allow: Box::new(move |_| o.load(Ordering::SeqCst)),
            locked: Router::new().fallback(|| async { (http::StatusCode::FORBIDDEN, "locked") }),
        };
        let (_stop, rx) = watch::channel(false);
        let limits = Limits { max_conns: 1, per_ip: 1, request_timeout: Duration::from_secs(5) };
        let router = Router::new().route("/", get(|| async { "owner" }));
        tokio::spawn(serve(listener, router, None, limits, Some(gate), rx));

        let page = get_page(addr).await;
        assert!(page.starts_with("HTTP/1.1 403") && page.ends_with("locked"), "{page}");
        // Refused devices hold idle connections open...
        let mut idle = vec![];
        for _ in 0..LOCKED_CONNS + 2 {
            idle.push(tokio::net::TcpStream::connect(addr).await.unwrap());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        // ...and the owner still gets in (the full pool here is one connection).
        open.store(true, Ordering::SeqCst);
        let page = get_page(addr).await;
        assert!(page.ends_with("owner"), "{page}");
    }
}
