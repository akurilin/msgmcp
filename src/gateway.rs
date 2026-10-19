//! Gateway service: MCP over Streamable HTTP, for clients that cannot reach the Unix socket,
//! such as agents in cloud sandboxes connected through a private network.

use crate::{
    Server, audit,
    caller::{self, Caller},
};
use anyhow::{Context, Result, bail};
use axum::{
    Router,
    extract::{ConnectInfo, Request, State},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::never::NeverSessionManager,
};
use serde_json::json;
use std::{
    io::{Read, Write},
    net::{IpAddr, SocketAddr},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::Arc,
};

pub const DEFAULT_LISTEN: &str = "127.0.0.1:8765";
pub const MCP_PATH: &str = "/mcp";
const TOKEN_BYTES: usize = 32;
const TOKEN_DIRECTORY_MODE: u32 = 0o700;
const TOKEN_FILE_MODE: u32 = 0o600;

pub struct Options {
    pub listen: String,
    pub token_file: PathBuf,
    /// Host names accepted in addition to loopback, such as a tailnet name.
    pub allowed_hosts: Vec<String>,
}

/// Read the bearer token, refusing a file that other users could read or replace.
pub fn read_token(path: &Path) -> Result<String> {
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .with_context(|| format!("Cannot read the gateway token at {}", path.display()))?;
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
    {
        bail!(
            "Gateway token must be a regular file you own with mode 0600: {}",
            path.display()
        );
    }
    let mut token = String::new();
    file.read_to_string(&mut token)?;
    let token = token.trim().to_owned();
    if token.is_empty() {
        bail!("Gateway token is empty: {}", path.display());
    }
    Ok(token)
}

/// Create a random token on first start, so the gateway never runs without one.
fn create_token(path: &Path) -> Result<String> {
    if let Some(directory) = path.parent() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(TOKEN_DIRECTORY_MODE)
            .create(directory)?;
    }
    let mut bytes = [0u8; TOKEN_BYTES];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(TOKEN_FILE_MODE)
        .open(path)
        .with_context(|| format!("Cannot create the gateway token at {}", path.display()))?;
    writeln!(file, "{token}")?;
    Ok(token)
}

// Compare without an early exit, so response timing doesn't reveal a matching prefix.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The caller's address, for the audit log. Behind `tailscale serve` the TCP peer is this
/// Mac, and the tailnet address is the last X-Forwarded-For entry: serve appends it, so
/// anything before it came from the client.
fn caller_address(request: &Request) -> Option<IpAddr> {
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0.ip());
    let forwarded = request
        .headers()
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.rsplit(',').next())
        .and_then(|v| v.trim().parse::<IpAddr>().ok());
    match (peer, forwarded) {
        (Some(peer), Some(forwarded)) if peer.is_loopback() => Some(forwarded),
        (peer, forwarded) => peer.or(forwarded),
    }
}

async fn authorize(State(token): State<Arc<str>>, mut request: Request, next: Next) -> Response {
    let ip = caller_address(&request);
    let caller = Caller {
        ip: ip.map(|ip| ip.to_string()),
        device: match ip {
            Some(ip) => caller::device(ip).await,
            None => None,
        },
        ..Caller::default()
    };
    request.extensions_mut().insert(caller.clone());
    let presented = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    let rejected = |reason: &str, status: StatusCode| {
        audit::record(
            "rejected",
            json!({"reason": reason, "status": status.as_u16(), "caller": caller}),
        );
    };
    if presented.is_some_and(|p| same(p.as_bytes(), token.as_bytes())) {
        // Host and Origin checks happen inside the MCP service; log what they refuse.
        let response = next.run(request).await;
        if response.status() == StatusCode::FORBIDDEN {
            rejected("host or origin not allowed", response.status());
        }
        return response;
    }
    rejected(
        if presented.is_some() {
            "wrong token"
        } else {
            "missing token"
        },
        StatusCode::UNAUTHORIZED,
    );
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Bearer")],
        "Missing or invalid bearer token\n",
    )
        .into_response()
}

pub async fn serve(server: Server, options: Options) -> Result<()> {
    let token = match options.token_file.try_exists() {
        Ok(false) => create_token(&options.token_file)?,
        _ => read_token(&options.token_file)?,
    };
    let listener = tokio::net::TcpListener::bind(&options.listen)
        .await
        .with_context(|| format!("Cannot listen on {}", options.listen))?;
    let address = listener.local_addr()?;
    let mut hosts = vec!["localhost".to_owned(), "127.0.0.1".into(), "::1".into()];
    if !address.ip().is_unspecified() {
        hosts.push(address.ip().to_string());
    }
    hosts.extend(options.allowed_hosts);
    // Stateless JSON responses, so a plain POST of tools/call works without a session.
    // Requests from browsers (any Origin header) are refused.
    let config = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_json_response(true)
        .with_allowed_hosts(hosts)
        .enforce_origin_validation();
    let cancel = config.cancellation_token.clone();
    let service = StreamableHttpService::new(
        move || Ok(server.clone()),
        Arc::new(NeverSessionManager::default()),
        config,
    );
    let app = Router::new()
        .route_service(MCP_PATH, service)
        .layer(middleware::from_fn_with_state(
            Arc::<str>::from(token),
            authorize,
        ));
    eprintln!("MSGMCP gateway listening on http://{address}{MCP_PATH}");
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {},
            _ = terminate.recv() => {},
        }
        cancel.cancel();
    })
    .await?;
    Ok(())
}
