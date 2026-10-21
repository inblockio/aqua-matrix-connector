//! Talking to `aqua-seatd`: one JSON line per connection, optionally starting
//! the daemon first (it is one per host and refuses a second owner of the same
//! state root, so a racing second spawn is harmless).

use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use crate::paths;
use crate::proto::{encode_line, Request, Response};

/// Timeout for requests that answer at once.
pub const QUICK_TIMEOUT: Duration = Duration::from_secs(150);
/// A socket that does not accept within this is treated as unreachable.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(1);
/// Env var that stops front ends starting `aqua-seatd` themselves.
pub const NO_SPAWN_ENV: &str = "CLAUDE_SEATS_NO_SPAWN";

#[derive(Debug, Clone)]
pub struct SeatClient {
    sock: PathBuf,
    spawn: bool,
}

impl SeatClient {
    pub fn new(sock: PathBuf) -> Self {
        Self { sock, spawn: false }
    }

    /// The default socket; starts `aqua-seatd` when nothing listens, unless
    /// `CLAUDE_SEATS_NO_SPAWN` is set.
    pub fn from_env() -> Self {
        Self {
            sock: paths::sock_path(),
            spawn: std::env::var_os(NO_SPAWN_ENV).is_none(),
        }
    }

    /// Never start the daemon from this client (hooks: a hook must not spawn or wait).
    pub fn without_spawn(mut self) -> Self {
        self.spawn = false;
        self
    }

    pub fn sock(&self) -> &Path {
        &self.sock
    }

    /// One request. `Ok(data)` on `ok: true`, `Err(text)` for a daemon error,
    /// a transport error or a timeout. `timeout` bounds the whole exchange.
    pub async fn call(&self, req: &Request, timeout: Duration) -> Result<Value, String> {
        let mut stream = self.connect().await?;
        let fut = async {
            stream
                .write_all(encode_line(req).as_bytes())
                .await
                .map_err(|e| format!("write to aqua-seatd: {e}"))?;
            let _ = stream.flush().await;
            let mut line = String::new();
            let n = BufReader::new(&mut stream)
                .read_line(&mut line)
                .await
                .map_err(|e| format!("read from aqua-seatd: {e}"))?;
            if n == 0 {
                return Err("aqua-seatd closed the connection with no reply".to_string());
            }
            serde_json::from_str::<Response>(&line).map_err(|e| format!("malformed reply: {e}"))
        };
        let resp = match tokio::time::timeout(timeout, fut).await {
            Ok(r) => r?,
            Err(_) => {
                return Err(format!(
                    "no reply from aqua-seatd within {}s; the outcome is unknown",
                    timeout.as_secs()
                ))
            }
        };
        if resp.ok {
            Ok(resp.data)
        } else {
            Err(resp
                .error
                .unwrap_or_else(|| "aqua-seatd reported an unspecified error".into()))
        }
    }

    async fn connect(&self) -> Result<UnixStream, String> {
        let first = match tokio::time::timeout(CONNECT_TIMEOUT, UnixStream::connect(&self.sock)).await
        {
            Ok(Ok(s)) => return Ok(s),
            Ok(Err(e)) => e,
            Err(_) => std::io::Error::new(std::io::ErrorKind::TimedOut, "connect timed out"),
        };
        if !self.spawn {
            return Err(self.unreachable(&first));
        }
        spawn_daemon(&self.sock).map_err(|e| {
            format!(
                "{}; starting aqua-seatd failed too: {e}",
                self.unreachable(&first)
            )
        })?;
        for _ in 0..100 {
            tokio::time::sleep(Duration::from_millis(150)).await;
            if let Ok(s) = UnixStream::connect(&self.sock).await {
                return Ok(s);
            }
        }
        Err(format!(
            "{} (started aqua-seatd, but it did not open the socket within 15s; see {})",
            self.unreachable(&first),
            paths::state_root().join("seatd.log").display()
        ))
    }

    fn unreachable(&self, e: &std::io::Error) -> String {
        format!(
            "cannot reach aqua-seatd at {} ({e}). Start it with `aqua-seatd`.",
            self.sock.display()
        )
    }
}

/// Where `aqua-seatd` is: next to this executable, else on `PATH`.
fn seatd_binary() -> PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let p = dir.join("aqua-seatd");
            if p.exists() {
                return p;
            }
        }
    }
    PathBuf::from("aqua-seatd")
}

/// Start `aqua-seatd` detached (own process group, output to
/// `<state root>/seatd.log`). Returns once it is spawned, not once it listens.
pub fn spawn_daemon(sock: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let root = paths::state_root();
    paths::ensure_private_dir(&root).map_err(std::io::Error::other)?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(root.join("seatd.log"))?;
    let err = log.try_clone()?;
    std::process::Command::new(seatd_binary())
        .env(paths::SOCK_ENV, sock)
        .stdin(std::process::Stdio::null())
        .stdout(log)
        .stderr(err)
        .process_group(0)
        .spawn()
        .map(|_| ())
}
