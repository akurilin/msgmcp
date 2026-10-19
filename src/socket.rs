//! Socket service: MCP on a Unix socket, for clients running as this user on this Mac.

use crate::{Server, caller};
use anyhow::{Context, Result, bail};
use rmcp::ServiceExt;
use std::{
    os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
};

const SOCKET_DIRECTORY_MODE: u32 = 0o700;
const SOCKET_FILE_MODE: u32 = 0o600;

// Keep the endpoint private to this account; all same-user clients can use it.
fn prepare_socket(path: &Path) -> Result<()> {
    let directory = path.parent().context("Socket needs a parent directory")?;
    if !directory.exists() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(SOCKET_DIRECTORY_MODE)
            .create(directory)?;
    }
    let metadata = std::fs::symlink_metadata(directory)?;
    if !metadata.is_dir() || metadata.mode() & 0o777 != SOCKET_DIRECTORY_MODE {
        bail!(
            "Socket directory must be a real directory with mode 0700: {}",
            directory.display()
        );
    }
    // Use the effective process identity, independent of environment variables.
    let uid = unsafe { libc::geteuid() };
    if metadata.uid() != uid {
        bail!("Socket directory belongs to another user");
    }
    if let Ok(metadata) = std::fs::symlink_metadata(path) {
        if !metadata.file_type().is_socket() || metadata.uid() != uid {
            bail!("Refusing to replace a non-owned socket: {}", path.display());
        }
        if std::os::unix::net::UnixStream::connect(path).is_ok() {
            bail!("MSGMCP socket service is already running");
        }
        std::fs::remove_file(path)?;
    }
    Ok(())
}

// Remove only the socket created by this service when it exits normally.
struct SocketCleanup(PathBuf);
impl Drop for SocketCleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

pub async fn serve(server: Server, socket: PathBuf) -> Result<()> {
    prepare_socket(&socket)?;
    let listener = tokio::net::UnixListener::bind(&socket)?;
    let _cleanup = SocketCleanup(socket.clone());
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(SOCKET_FILE_MODE))?;
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            _ = terminate.recv() => break,
            connection = listener.accept() => {
                // A failed or already-closed connection must not stop the service.
                let stream = match connection {
                    Ok((stream, _)) => stream,
                    Err(error) => {
                        eprintln!("Accept failed: {error}");
                        continue;
                    }
                };
                let Ok(credentials) = stream.peer_cred() else { continue };
                if credentials.uid() != unsafe { libc::geteuid() } {
                    continue;
                }
                let server = server.with_caller(caller::process(credentials.pid()));
                // Each client gets independent MCP initialization and connection state.
                tokio::spawn(async move {
                    match server.serve(stream).await {
                        Ok(connection) => { let _ = connection.waiting().await; },
                        Err(error) => eprintln!("MCP connection failed: {error}"),
                    }
                });
            }
        }
    }
    Ok(())
}
