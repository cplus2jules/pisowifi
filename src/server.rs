//! A small, defensive HTTP/1.1 server loop around an axum Router:
//! connection cap, header read timeout (slowloris), TLS handshake timeout,
//! and the real TCP peer address attached to every request.

use axum::extract::ConnectInfo;
use axum::http::{header, HeaderValue, Request};
use axum::middleware::Next;
use axum::response::Response;
use axum::Router;
use hyper::body::Incoming;
use hyper_util::rt::{TokioIo, TokioTimer};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;
use tokio::sync::{watch, Semaphore};
use tokio_rustls::TlsAcceptor;
use tower::Service;

pub async fn serve(
    listener: TcpListener,
    router: Router,
    tls: Option<TlsAcceptor>,
    max_conns: usize,
    mut shutdown: watch::Receiver<bool>,
) {
    let limit = Arc::new(Semaphore::new(max_conns));
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
        let Ok(permit) = limit.clone().try_acquire_owned() else {
            continue; // over capacity: drop the connection
        };
        let _ = stream.set_nodelay(true);
        let router = router.clone();
        let tls = tls.clone();
        tokio::spawn(async move {
            let _permit = permit;
            match tls {
                None => serve_conn(stream, addr, router).await,
                Some(acc) => {
                    match tokio::time::timeout(Duration::from_secs(10), acc.accept(stream)).await {
                        Ok(Ok(s)) => serve_conn(s, addr, router).await,
                        _ => {}
                    }
                }
            }
        });
    }
}

async fn serve_conn<I>(io: I, addr: SocketAddr, router: Router)
where
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let svc = hyper::service::service_fn(move |mut req: Request<Incoming>| {
        req.extensions_mut().insert(ConnectInfo(addr));
        let mut r = router.clone();
        async move { r.call(req).await }
    });
    let conn = hyper::server::conn::http1::Builder::new()
        .timer(TokioTimer::new())
        .header_read_timeout(Duration::from_secs(10))
        .max_buf_size(64 * 1024)
        .serve_connection(TokioIo::new(io), svc);
    // Hard cap so idle keep-alive connections can't pile up.
    let _ = tokio::time::timeout(Duration::from_secs(120), conn).await;
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
             font-src 'self'; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'",
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
