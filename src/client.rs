//! Client commands: the stdio adapter MCP clients run, and `schema` and `call`.
//! None of them open a database or run a script; they only talk to a running service.

use anyhow::{Context, Result, bail};
use rmcp::{
    RoleClient, ServiceExt,
    model::{CallToolRequestParams, JsonObject},
    service::RunningService,
    transport::{
        StreamableHttpClientTransport, Transport, async_rw::AsyncRwTransport,
        streamable_http_client::StreamableHttpClientTransportConfig,
    },
};
use serde_json::json;
use std::path::PathBuf;

/// Where a client finds the service: the socket service or the gateway.
pub enum Endpoint {
    Socket(PathBuf),
    Gateway { url: String, token: String },
}

fn unreachable(endpoint: &Endpoint) -> String {
    match endpoint {
        Endpoint::Socket(path) => format!(
            "Cannot connect to MSGMCP at {}. Install or start the MSGMCP LaunchAgent.",
            path.display()
        ),
        Endpoint::Gateway { url, .. } => {
            format!("Cannot connect to MSGMCP at {url}. Install or start the MSGMCP LaunchAgent.")
        }
    }
}

fn http_transport(url: &str, token: &str) -> impl Transport<RoleClient> + 'static {
    StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(url).auth_header(token),
    )
}

// Forward protocol messages only, in order, between stdio and the service.
pub async fn adapter(endpoint: &Endpoint) -> Result<()> {
    match endpoint {
        Endpoint::Socket(path) => {
            let stream = tokio::net::UnixStream::connect(path)
                .await
                .with_context(|| unreachable(endpoint))?;
            let (mut reader, mut writer) = stream.into_split();
            let upload = async {
                tokio::io::copy(&mut tokio::io::stdin(), &mut writer).await?;
                tokio::io::AsyncWriteExt::shutdown(&mut writer).await
            };
            let download = async {
                tokio::io::copy(&mut reader, &mut tokio::io::stdout())
                    .await
                    .map(|_| ())
            };
            tokio::pin!(download);
            tokio::select! {
                result = upload => { result?; download.await?; },
                result = &mut download => result?,
            }
        }
        Endpoint::Gateway { url, token } => {
            let mut local = AsyncRwTransport::new_server(tokio::io::stdin(), tokio::io::stdout());
            let mut remote = http_transport(url, token);
            loop {
                tokio::select! {
                    message = local.receive() => match message {
                        Some(message) => remote.send(message).await.with_context(|| unreachable(endpoint))?,
                        None => break,
                    },
                    message = remote.receive() => match message {
                        Some(message) => local.send(message).await?,
                        None => bail!("{}", unreachable(endpoint)),
                    },
                }
            }
            // Dropping the transport stops its worker; the stateless gateway has no session
            // to end, and close() would wait for a handshake that never started.
            drop(remote);
        }
    }
    Ok(())
}

/// Connect to the service as an MCP client, for the `schema` and `call` commands.
async fn connect(endpoint: &Endpoint) -> Result<RunningService<RoleClient, ()>> {
    match endpoint {
        Endpoint::Socket(path) => {
            let stream = tokio::net::UnixStream::connect(path)
                .await
                .with_context(|| unreachable(endpoint))?;
            Ok(().serve(stream).await?)
        }
        Endpoint::Gateway { url, token } => Ok(
            ().serve(http_transport(url, token))
                .await
                .with_context(|| unreachable(endpoint))?,
        ),
    }
}

// Everything except `transport` comes from the running service, so it matches what MCP clients see.
pub async fn schema(endpoint: &Endpoint) -> Result<()> {
    let client = connect(endpoint).await?;
    let tools = client.list_all_tools().await?;
    let exe = std::env::current_exe()?;
    let mut transport = match endpoint {
        Endpoint::Socket(path) => json!({
            "socket": path,
            "framing": "MCP (JSON-RPC 2.0), one JSON message per line; send initialize and notifications/initialized first",
        }),
        Endpoint::Gateway { url, .. } => json!({
            "url": url,
            "framing": "MCP Streamable HTTP, stateless; send Authorization: Bearer <token>; a POST of tools/call needs no initialize",
        }),
    };
    transport["stdio_adapter"] = json!(exe);
    transport["cli"] = json!(format!(
        "{} call <tool> '<arguments JSON object>'",
        exe.display()
    ));
    let output = json!({
        "server": client.peer_info(),
        "transport": transport,
        "tools": tools,
    });
    client.cancel().await?;
    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}

pub async fn call(endpoint: &Endpoint, tool: String, arguments: Option<String>) -> Result<()> {
    let arguments: JsonObject = serde_json::from_str(arguments.as_deref().unwrap_or("{}"))
        .context("Tool arguments must be a JSON object")?;
    let client = connect(endpoint).await?;
    let result = client
        .call_tool(CallToolRequestParams::new(tool).with_arguments(arguments))
        .await;
    client.cancel().await?;
    let result = result?;
    let text = || {
        result
            .content
            .iter()
            .filter_map(|c| c.as_text().map(|t| t.text.as_str()))
            .collect::<Vec<_>>()
            .join("\n")
    };
    if result.is_error == Some(true) {
        bail!("{}", text());
    }
    match &result.structured_content {
        Some(value) => println!("{value}"),
        None => println!("{}", text()),
    }
    Ok(())
}
