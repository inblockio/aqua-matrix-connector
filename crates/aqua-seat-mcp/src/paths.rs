//! Where seat state and the daemon socket live, and the path-safety rules.
//!
//! State root `~/.local/state/claude-seats/` (0700), override with
//! `CLAUDE_SEATS_STATE` (tests). Per seat `seats/<seat>/` (0700) holds
//! `agent.pem` (0600), `store/`, `meta.toml`, `inbox.jsonl`, ... The daemon
//! socket is `$XDG_RUNTIME_DIR/claude-seats/seatd.sock` (dir 0700, socket
//! 0600), override with `CLAUDE_SEATS_SOCK`. Never under `/tmp` by default
//! (a RAM-backed tmpfs on this host).

use std::fs;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};

use crate::seatname;

/// Env var overriding the state root.
pub const STATE_ENV: &str = "CLAUDE_SEATS_STATE";
/// Env var overriding the daemon socket path.
pub const SOCK_ENV: &str = "CLAUDE_SEATS_SOCK";

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// `$CLAUDE_SEATS_STATE`, else `~/.local/state/claude-seats`.
pub fn state_root() -> PathBuf {
    match std::env::var_os(STATE_ENV) {
        Some(d) if !d.is_empty() => PathBuf::from(d),
        _ => home().join(".local/state/claude-seats"),
    }
}

/// `$CLAUDE_SEATS_SOCK`, else `$XDG_RUNTIME_DIR/claude-seats/seatd.sock`, else
/// `<state root>/seatd.sock`.
pub fn sock_path() -> PathBuf {
    if let Some(s) = std::env::var_os(SOCK_ENV) {
        if !s.is_empty() {
            return PathBuf::from(s);
        }
    }
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(d) if !d.is_empty() => PathBuf::from(d).join("claude-seats/seatd.sock"),
        _ => state_root().join("seatd.sock"),
    }
}

/// The files of one seat. Build it only with [`SeatPaths::new`], which refuses
/// a name that was not sanitised.
#[derive(Debug, Clone)]
pub struct SeatPaths {
    pub seat: String,
    pub dir: PathBuf,
}

impl SeatPaths {
    /// `root/seats/<seat>`. Errors unless `seat` is already a valid seat name
    /// (see [`seatname::is_valid`]); nothing is created.
    pub fn new(root: &Path, seat: &str) -> Result<Self> {
        if !seatname::is_valid(seat) {
            bail!("invalid seat name {seat:?}: use [a-z0-9_-], 1..={} chars, starting and ending alphanumeric, not a reserved word", seatname::MAX_SEAT_LEN);
        }
        let dir = root.join("seats").join(seat);
        // Belt and braces: the name has no separator, so the join cannot leave
        // `root/seats`.
        debug_assert_eq!(dir.parent(), Some(root.join("seats").as_path()));
        Ok(Self {
            seat: seat.to_string(),
            dir,
        })
    }

    /// The Ed25519 key (0600). Never read or printed by anything but the
    /// connector's own key loader.
    pub fn key(&self) -> PathBuf {
        self.dir.join("agent.pem")
    }
    /// The matrix-sdk crypto/state store.
    pub fn store(&self) -> PathBuf {
        self.dir.join("store")
    }
    /// Non-secret identity facts: seat, did, mxid, device id.
    pub fn meta(&self) -> PathBuf {
        self.dir.join("meta.toml")
    }
    pub fn inbox(&self) -> PathBuf {
        self.dir.join("inbox.jsonl")
    }
    /// Event ids of direct-address messages (side table of the inbox).
    pub fn addressed(&self) -> PathBuf {
        self.dir.join("addressed.log")
    }
    /// Last seq `aqua-seat-listen` was woken for.
    pub fn listen_cursor(&self) -> PathBuf {
        self.dir.join("listen.cursor")
    }
    /// flock single-instance lock of `aqua-seat-listen`.
    pub fn listen_lock(&self) -> PathBuf {
        self.dir.join("listen.lock")
    }

    /// Create the seat directory (0700), refusing a symlink in its place.
    pub fn ensure_dir(&self) -> Result<()> {
        if let Ok(md) = fs::symlink_metadata(&self.dir) {
            if md.file_type().is_symlink() {
                bail!("{} is a symlink; refusing to use it", self.dir.display());
            }
        }
        ensure_private_dir(&self.dir)
    }
}

/// `root/room.toml`.
pub fn room_file(root: &Path) -> PathBuf {
    root.join("room.toml")
}

/// `root/seatd.lock`: held by the one `aqua-seatd` that owns this state root.
pub fn daemon_lock(root: &Path) -> PathBuf {
    root.join("seatd.lock")
}

/// Create `dir` and every missing parent as 0700, and force the final
/// component to 0700 even if it already existed.
pub fn ensure_private_dir(dir: &Path) -> Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("creating {}", dir.display()))?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("chmod 700 {}", dir.display()))?;
    Ok(())
}

/// Write `bytes` to `path` as a new 0600 file; `Ok(false)` when it already
/// exists (nothing is overwritten).
pub fn create_private_file_new(path: &Path, bytes: &[u8]) -> Result<bool> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt;
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
    {
        Ok(mut f) => {
            f.write_all(bytes)?;
            f.sync_all()?;
            Ok(true)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(anyhow!(e)).with_context(|| format!("creating {}", path.display())),
    }
}

/// Atomically replace `path` with `bytes` (temp file 0600 + rename).
pub fn write_private_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = path.with_extension("tmp");
    {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .with_context(|| format!("writing {}", tmp.display()))?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path).with_context(|| format!("renaming into {}", path.display()))?;
    Ok(())
}

/// Mode bits (permission part) of `path`.
pub fn mode_of(path: &Path) -> Result<u32> {
    Ok(fs::metadata(path)?.permissions().mode() & 0o777)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seat_paths_stay_under_the_seats_dir() {
        let root = Path::new("/state/root");
        let p = SeatPaths::new(root, "seat-test-a").unwrap();
        assert_eq!(p.dir, Path::new("/state/root/seats/seat-test-a"));
        assert_eq!(p.key(), Path::new("/state/root/seats/seat-test-a/agent.pem"));
        assert!(p.store().starts_with(&p.dir));
        assert!(p.meta().starts_with(&p.dir));
    }

    #[test]
    fn seat_paths_refuse_anything_unsanitised() {
        let root = Path::new("/state/root");
        for bad in [
            "",
            "..",
            ".",
            "../x",
            "x/..",
            "a/b",
            "/etc",
            "a\0b",
            "Seat",
            "-x",
            "x-",
            "room",
            "a b",
            &"a".repeat(41),
        ] {
            assert!(SeatPaths::new(root, bad).is_err(), "{bad:?} must be refused");
        }
    }

    #[test]
    fn ensure_dir_makes_0700_and_refuses_a_symlink() {
        let t = tempfile::tempdir().unwrap();
        let p = SeatPaths::new(t.path(), "seat-a").unwrap();
        p.ensure_dir().unwrap();
        assert_eq!(mode_of(&p.dir).unwrap(), 0o700);
        assert_eq!(mode_of(&t.path().join("seats")).unwrap(), 0o700);
        // idempotent
        p.ensure_dir().unwrap();

        let q = SeatPaths::new(t.path(), "seat-b").unwrap();
        let elsewhere = t.path().join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &q.dir).unwrap();
        assert!(q.ensure_dir().is_err());
    }

    #[test]
    fn private_files_are_0600_and_not_overwritten() {
        let t = tempfile::tempdir().unwrap();
        let f = t.path().join("k");
        assert!(create_private_file_new(&f, b"one").unwrap());
        assert_eq!(mode_of(&f).unwrap(), 0o600);
        assert!(!create_private_file_new(&f, b"two").unwrap());
        assert_eq!(fs::read(&f).unwrap(), b"one");
        write_private_atomic(&f, b"three").unwrap();
        assert_eq!(fs::read(&f).unwrap(), b"three");
        assert_eq!(mode_of(&f).unwrap(), 0o600);
    }

    #[test]
    fn env_overrides_are_honoured() {
        // Run in one test: env vars are process-global.
        std::env::set_var(STATE_ENV, "/some/where");
        std::env::set_var(SOCK_ENV, "/some/where/s.sock");
        assert_eq!(state_root(), Path::new("/some/where"));
        assert_eq!(sock_path(), Path::new("/some/where/s.sock"));
        std::env::remove_var(SOCK_ENV);
        std::env::set_var("XDG_RUNTIME_DIR", "/run/user/9");
        assert_eq!(sock_path(), Path::new("/run/user/9/claude-seats/seatd.sock"));
        std::env::remove_var(STATE_ENV);
        std::env::remove_var("XDG_RUNTIME_DIR");
        assert!(state_root().ends_with(".local/state/claude-seats"));
    }
}
