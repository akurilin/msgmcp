//! Dashboard: a web page on this Mac's loopback interface showing the service's
//! configuration and its audit log, rendered here with no scripts, and the same as JSON at
//! /api/status and /api/log. Both services start it. It has no key: anything that can
//! reach this Mac's loopback can read it, but web pages can't (the Host check stops DNS
//! rebinding, and without CORS headers other origins can't read responses), and `tailscale
//! serve` forwards only the gateway's port.

mod page;

use crate::audit;
use anyhow::{Result, bail};
use axum::{
    Router,
    extract::{ConnectInfo, Request, State},
    http::{HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
};
use serde_json::{Value, json};
use std::{net::SocketAddr, sync::Arc};

pub const DEFAULT_LISTEN: &str = "127.0.0.1:8766";
const DEFAULT_EVENTS: usize = 5_000;
/// The page summarizes and lists at most this many of the newest events.
const PAGE_EVENTS: usize = 20_000;
const MAX_EVENTS: usize = 50_000;
const SECURITY_HEADERS: [(header::HeaderName, &str); 5] = [
    (
        header::CONTENT_SECURITY_POLICY,
        "default-src 'none'; style-src 'self'; img-src 'self' data:; base-uri 'none'; \
         form-action 'self'; frame-ancestors 'none'",
    ),
    (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
    (header::X_FRAME_OPTIONS, "DENY"),
    (header::REFERRER_POLICY, "no-referrer"),
    (header::CACHE_CONTROL, "no-store"),
];

/// Builds the /api/status document; it may block, so it runs off the async runtime.
pub type Status = Arc<dyn Fn() -> Value + Send + Sync>;

/// The address to listen on, which must be on the loopback interface.
pub fn parse_listen(listen: &str) -> Result<SocketAddr> {
    let Ok(address) = listen.parse::<SocketAddr>() else {
        bail!("--dashboard needs an address and port such as {DEFAULT_LISTEN}, not {listen:?}");
    };
    if !address.ip().is_loopback() {
        bail!("The dashboard listens on a loopback address only, not {listen}");
    }
    Ok(address)
}

/// Refuse any Host but this listener's own names, so a web page that rebinds its domain to
/// 127.0.0.1 can't read the dashboard.
async fn check_host(State(hosts): State<Arc<[String]>>, request: Request, next: Next) -> Response {
    let host = request
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok());
    let mut response = if host.is_some_and(|h| hosts.iter().any(|allowed| allowed == h)) {
        next.run(request).await
    } else {
        let ip = request
            .extensions()
            .get::<ConnectInfo<SocketAddr>>()
            .map(|c| c.0.ip().to_string());
        audit::record(
            "rejected",
            json!({"reason": "dashboard: host not allowed", "status": 403, "caller": {"ip": ip}}),
        );
        (StatusCode::FORBIDDEN, "Host not allowed\n").into_response()
    };
    for (name, value) in SECURITY_HEADERS {
        response
            .headers_mut()
            .insert(name, HeaderValue::from_static(value));
    }
    response
}

fn file(content_type: &'static str, body: &'static str) -> Response {
    ([(header::CONTENT_TYPE, content_type)], body).into_response()
}

fn json_response(value: &Value) -> Response {
    (
        [(header::CONTENT_TYPE, "application/json")],
        value.to_string(),
    )
        .into_response()
}

async fn status(State(status): State<Status>) -> Response {
    match tokio::task::spawn_blocking(move || status()).await {
        Ok(value) => json_response(&value),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "Status failed\n").into_response(),
    }
}

fn query(request: &Request) -> Vec<(String, String)> {
    request
        .uri()
        .query()
        .map(|q| form_urlencoded::parse(q.as_bytes()).into_owned().collect())
        .unwrap_or_default()
}

/// The page, built here on each request: no scripts, so the browser runs no code from it.
async fn index(State(status): State<Status>, request: Request) -> Response {
    let query = query(&request);
    let rendered = tokio::task::spawn_blocking(move || {
        let (events, more) = audit::recent(PAGE_EVENTS);
        page::render(&status(), &events, more, &query, chrono::Local::now())
    })
    .await;
    match rendered {
        Ok(html) => ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], html).into_response(),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "Page failed\n").into_response(),
    }
}

/// `?limit=N`: the newest N events, newest first.
async fn log(request: Request) -> Response {
    let limit = query(&request)
        .into_iter()
        .find_map(|(key, value)| (key == "limit").then_some(value))
        .and_then(|n| n.parse::<usize>().ok())
        .unwrap_or(DEFAULT_EVENTS)
        .clamp(1, MAX_EVENTS);
    match tokio::task::spawn_blocking(move || audit::recent(limit)).await {
        Ok((events, more)) => json_response(&json!({"events": events, "more": more})),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "Log read failed\n").into_response(),
    }
}

/// Serve the dashboard in the background. A dashboard that can't start (its port is taken)
/// is reported on stderr and the service keeps serving MCP.
pub async fn start(address: SocketAddr, status_document: Status) {
    let listener = match tokio::net::TcpListener::bind(address).await {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("MSGMCP dashboard cannot listen on {address}: {error}");
            return;
        }
    };
    let Ok(address) = listener.local_addr() else {
        return;
    };
    let port = address.port();
    let hosts: Arc<[String]> = [
        format!("localhost:{port}"),
        format!("127.0.0.1:{port}"),
        format!("[::1]:{port}"),
        address.to_string(),
    ]
    .into();
    let app = Router::new()
        .route("/", get(index).with_state(status_document.clone()))
        .route(
            "/app.css",
            get(|| async { file("text/css; charset=utf-8", include_str!("dashboard/app.css")) }),
        )
        .route("/api/status", get(status).with_state(status_document))
        .route("/api/log", get(log))
        .layer(middleware::from_fn_with_state(hosts, check_host));
    eprintln!("MSGMCP dashboard listening on http://{address}/");
    tokio::spawn(async move {
        let served = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await;
        if let Err(error) = served {
            eprintln!("MSGMCP dashboard stopped: {error}");
        }
    });
}
