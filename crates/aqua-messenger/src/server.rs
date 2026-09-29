//! Serve an [`Engine`] on a unix socket (mode 600): one JSON request line per
//! connection, one JSON response line back. Used by the host daemon and by
//! embedded agents so their stdio MCP servers (`aqua-messenger-mcp`,
//! `aqua-system-bridge-mcp`) never touch Matrix or a crypto store.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use crate::engine::{Engine, Transport};
use crate::proto::{self, Request, Response};

/// Largest request line accepted on the socket.
const MAX_REQUEST_BYTES: u64 = 256 * 1024;

/// Bind the socket (mode 600). Refuses when another process answers on it
/// (single owner per identity); removes a stale socket file.
pub fn bind_socket(path: &Path) -> anyhow::Result<UnixListener> {
    if path.exists() {
        if std::os::unix::net::UnixStream::connect(path).is_ok() {
            anyhow::bail!(
                "another messenger backend is already listening on {}; refusing to start a second owner of this identity",
                path.display()
            );
        }
        std::fs::remove_file(path)?;
    }
    let l = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(l)
}

/// Accept loop; runs until the task is aborted.
pub async fn serve<T: Transport>(listener: UnixListener, engine: Arc<Engine<T>>) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let engine = engine.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_conn(stream, engine).await {
                        tracing::warn!("socket connection error: {e:#}");
                    }
                });
            }
            Err(e) => {
                tracing::error!("socket accept failed: {e}");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
}

async fn handle_conn<T: Transport>(
    stream: UnixStream,
    engine: Arc<Engine<T>>,
) -> anyhow::Result<()> {
    let (rd, mut wr) = stream.into_split();
    let mut reader = BufReader::new(rd.take(MAX_REQUEST_BYTES));
    let mut line = String::new();
    reader.read_line(&mut line).await?;
    let resp = match serde_json::from_str::<Request>(line.trim()) {
        Ok(req) => engine.handle(req).await,
        Err(e) => Response::err(format!("bad request: {e}")),
    };
    wr.write_all(proto::encode_line(&resp).as_bytes()).await?;
    wr.flush().await?;
    Ok(())
}
