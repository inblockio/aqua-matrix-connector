//! Inbound attachments fetched on demand (`fetch_attachment`).
//!
//! The bridge never downloads media automatically: an inbox entry only records
//! the attachment's metadata and its (possibly encrypted) media reference. A
//! session asks for it by `inbox_seq`; the daemon downloads it through the
//! authenticated media API (`/_matrix/client/v1/media/...`), decrypts and
//! verifies it, and stores it here.
//!
//! Layout: `<state dir>/attachments/` (mode 700) holds
//! - `<inbox_seq>-<sanitized name>`: the decrypted bytes, mode 600;
//! - `<inbox_seq>-<sanitized name>.meta.json`: event id, mime type, size and
//!   sha256 (mode 600). A cached file is only served when its event id matches
//!   the inbox entry and its bytes still hash to the recorded sha256, so a
//!   reused seq (inbox file deleted) or a modified file is re-fetched instead.
//!
//! Retention: files older than the configured number of days (mtime; default
//! [`DEFAULT_RETENTION_DAYS`]) are deleted at daemon start and after every
//! fetch. A pruned attachment is simply downloaded again on the next call.
//! Only regular files directly inside `attachments/` are ever deleted.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Default size cap for one fetched attachment (50 MiB).
pub const DEFAULT_MAX_BYTES: u64 = 50 * 1024 * 1024;
/// Default retention for fetched attachments, in days.
pub const DEFAULT_RETENTION_DAYS: u64 = 14;
/// Longest sanitized file name kept, in bytes (the extension is preserved).
pub const MAX_NAME_BYTES: usize = 100;

/// Inbox `kind`s that carry media.
pub const MEDIA_KINDS: [&str; 4] = ["file", "image", "audio", "video"];

/// Size cap and retention for fetched attachments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttachmentPolicy {
    pub max_bytes: u64,
    pub retention_days: u64,
}

impl Default for AttachmentPolicy {
    fn default() -> Self {
        Self {
            max_bytes: DEFAULT_MAX_BYTES,
            retention_days: DEFAULT_RETENTION_DAYS,
        }
    }
}

impl AttachmentPolicy {
    /// Refuse sizes above the cap (declared by the sender or actually downloaded).
    pub fn check_size(&self, bytes: u64, what: &str) -> Result<(), String> {
        if bytes > self.max_bytes {
            return Err(format!(
                "attachment {what} size is {bytes} bytes; the cap is {} bytes. Not fetched.",
                self.max_bytes
            ));
        }
        Ok(())
    }
}

/// A fetched attachment on disk.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Fetched {
    pub path: PathBuf,
    pub event_id: String,
    pub filename: String,
    pub mimetype: Option<String>,
    pub size: u64,
    pub sha256: String,
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// Reduce a sender-supplied file name to a safe basename: the last path
/// component only (both `/` and `\` split), control characters dropped,
/// anything outside `[A-Za-z0-9._+()-]` replaced by `_`, runs of `_`
/// collapsed, leading dots/underscores stripped (no hidden files, no `..`),
/// at most [`MAX_NAME_BYTES`] with the extension kept. Empty yields
/// `attachment`.
pub fn sanitize_filename(raw: &str) -> String {
    let base = raw.rsplit(['/', '\\']).next().unwrap_or("");
    let mut out = String::new();
    for ch in base.chars() {
        if ch.is_control() {
            continue;
        }
        let c = if ch.is_ascii_alphanumeric() || "._+()-".contains(ch) {
            ch
        } else {
            '_'
        };
        if c == '_' && out.ends_with('_') {
            continue;
        }
        out.push(c);
    }
    let out = out
        .trim_start_matches(['.', '_'])
        .trim_end_matches(['.', '_'])
        .to_string();
    let mut out = if out.is_empty() {
        "attachment".to_string()
    } else {
        out
    };
    if out.len() > MAX_NAME_BYTES {
        let ext = match out.rfind('.') {
            Some(i) if out.len() - i <= 12 => out[i..].to_string(),
            _ => String::new(),
        };
        let keep = MAX_NAME_BYTES - ext.len();
        out = format!("{}{ext}", &out[..keep]);
    }
    out
}

/// The on-disk file name for an inbox entry's attachment.
pub fn stored_name(inbox_seq: u64, raw_name: &str) -> String {
    format!("{inbox_seq}-{}", sanitize_filename(raw_name))
}

pub struct AttachmentStore {
    dir: PathBuf,
}

impl AttachmentStore {
    /// `dir` is `<state dir>/attachments`; created (mode 700) on first write.
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn paths(&self, inbox_seq: u64, raw_name: &str) -> (PathBuf, PathBuf) {
        let name = stored_name(inbox_seq, raw_name);
        (
            self.dir.join(&name),
            self.dir.join(format!("{name}.meta.json")),
        )
    }

    /// The cached attachment for this entry, if present, for the same event,
    /// and intact (bytes still match the recorded sha256).
    pub fn lookup(&self, inbox_seq: u64, event_id: &str, raw_name: &str) -> Option<Fetched> {
        let (file, meta) = self.paths(inbox_seq, raw_name);
        let rec: Fetched = serde_json::from_slice(&std::fs::read(&meta).ok()?).ok()?;
        if rec.event_id != event_id {
            return None;
        }
        let bytes = std::fs::read(&file).ok()?;
        if bytes.len() as u64 != rec.size || sha256_hex(&bytes) != rec.sha256 {
            return None;
        }
        Some(Fetched { path: file, ..rec })
    }

    /// Write the decrypted bytes (mode 600, temp file + rename) and the
    /// metadata sidecar. Enforces the size cap once more on the real bytes.
    pub fn store(
        &self,
        policy: &AttachmentPolicy,
        inbox_seq: u64,
        event_id: &str,
        raw_name: &str,
        mimetype: Option<String>,
        bytes: &[u8],
    ) -> Result<Fetched, String> {
        policy.check_size(bytes.len() as u64, "downloaded")?;
        ensure_private_dir(&self.dir)
            .map_err(|e| format!("cannot create {}: {e}", self.dir.display()))?;
        let (file, meta) = self.paths(inbox_seq, raw_name);
        let rec = Fetched {
            path: file.clone(),
            event_id: event_id.to_string(),
            filename: sanitize_filename(raw_name),
            mimetype,
            size: bytes.len() as u64,
            sha256: sha256_hex(bytes),
        };
        write_private_atomic(&file, bytes)
            .map_err(|e| format!("cannot write {}: {e}", file.display()))?;
        let meta_json = serde_json::to_vec_pretty(&rec).map_err(|e| e.to_string())?;
        write_private_atomic(&meta, &meta_json)
            .map_err(|e| format!("cannot write {}: {e}", meta.display()))?;
        Ok(rec)
    }

    /// Delete regular files in the store older than `retention_days`
    /// (by mtime). Returns how many were removed. Missing dir = 0.
    pub fn prune(&self, retention_days: u64, now: SystemTime) -> usize {
        let Ok(rd) = std::fs::read_dir(&self.dir) else {
            return 0;
        };
        let max_age = Duration::from_secs(retention_days.saturating_mul(86_400));
        let mut removed = 0;
        for ent in rd.flatten() {
            let Ok(md) = ent.metadata() else { continue };
            // symlink_metadata semantics: DirEntry::metadata does not follow links.
            if !md.is_file() {
                continue;
            }
            let old = md
                .modified()
                .ok()
                .and_then(|m| now.duration_since(m).ok())
                .is_some_and(|age| age > max_age);
            if old && std::fs::remove_file(ent.path()).is_ok() {
                removed += 1;
            }
        }
        removed
    }
}

fn ensure_private_dir(p: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(p)?;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700))
}

fn write_private_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    {
        let mut f = crate::inbox::open_private(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(tag: &str) -> PathBuf {
        // Tests write under ~/.cache (disk), never /tmp (RAM-backed tmpfs here).
        let base = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let d = base
            .join(".cache")
            .join(format!("asb-attach-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn names_are_sanitized() {
        assert_eq!(sanitize_filename("report.pdf"), "report.pdf");
        assert_eq!(sanitize_filename("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_filename("..\\..\\win.ini"), "win.ini");
        assert_eq!(sanitize_filename(".bashrc"), "bashrc");
        assert_eq!(sanitize_filename("..."), "attachment");
        assert_eq!(sanitize_filename(""), "attachment");
        assert_eq!(sanitize_filename("a/.."), "attachment");
        assert_eq!(sanitize_filename("my file (1).PNG"), "my_file_(1).PNG");
        assert_eq!(
            sanitize_filename("x\n\u{0}y;rm -rf $HOME.txt"),
            "xy_rm_-rf_HOME.txt"
        );
        assert_eq!(sanitize_filename("Grüße.md"), "Gr_e.md");
        let long = format!("{}.tar.gz", "a".repeat(300));
        let s = sanitize_filename(&long);
        assert_eq!(s.len(), MAX_NAME_BYTES);
        assert!(s.ends_with(".gz"));
        assert_eq!(stored_name(7, "../x.md"), "7-x.md");
        for bad in ["..", "/", "a/../../b", ".", "\\.."] {
            let s = sanitize_filename(bad);
            assert!(
                !s.contains('/') && !s.starts_with('.') && s != "..",
                "{bad:?} -> {s:?}"
            );
        }
    }

    #[test]
    fn size_cap_is_enforced() {
        let p = AttachmentPolicy {
            max_bytes: 10,
            retention_days: 1,
        };
        assert!(p.check_size(10, "declared").is_ok());
        assert!(p
            .check_size(11, "declared")
            .unwrap_err()
            .contains("cap is 10"));
        let store = AttachmentStore::new(tmp_dir("cap"));
        assert!(store.store(&p, 1, "$e", "a.bin", None, &[0u8; 11]).is_err());
        assert!(!store.dir().join("1-a.bin").exists());
        let _ = std::fs::remove_dir_all(store.dir());
    }

    #[test]
    fn store_lookup_is_idempotent_private_and_tamper_evident() {
        use std::os::unix::fs::PermissionsExt;
        let store = AttachmentStore::new(tmp_dir("store"));
        let p = AttachmentPolicy::default();
        let f = store
            .store(
                &p,
                3,
                "$ev",
                "../r.md",
                Some("text/markdown".into()),
                b"hello",
            )
            .unwrap();
        assert_eq!(f.path, store.dir().join("3-r.md"));
        assert_eq!(f.sha256, sha256_hex(b"hello"));
        assert_eq!(
            std::fs::metadata(&f.path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(store.dir()).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(store.lookup(3, "$ev", "../r.md"), Some(f.clone()));
        // Different event under the same seq (inbox reset): not served.
        assert_eq!(store.lookup(3, "$other", "../r.md"), None);
        // Modified bytes: not served.
        std::fs::write(&f.path, b"HELLO").unwrap();
        assert_eq!(store.lookup(3, "$ev", "../r.md"), None);
        let _ = std::fs::remove_dir_all(store.dir());
    }

    #[test]
    fn prune_removes_only_old_files() {
        let store = AttachmentStore::new(tmp_dir("prune"));
        let p = AttachmentPolicy::default();
        store.store(&p, 1, "$a", "a.txt", None, b"a").unwrap();
        let now = SystemTime::now();
        assert_eq!(store.prune(1, now), 0);
        assert_eq!(store.prune(1, now + Duration::from_secs(2 * 86_400)), 2);
        assert!(store.lookup(1, "$a", "a.txt").is_none());
        let _ = std::fs::remove_dir_all(store.dir());
    }
}
