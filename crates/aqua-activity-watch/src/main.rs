//! aqua-activity-watch — host-side activity tracker for a containerised agent.
//!
//! Tails the agent's append-only inbound activity log on its mounted volume
//! (one JSON line per message the agent received from its peer, written by the
//! agent's `record_activity`), counts distinct messages, and alerts the
//! operator (a) on the peer's FIRST message and (b) on every Nth message
//! (default 10). Alerts are delivered by exec'ing the existing notify channel
//! (`notify-tim.sh`) — this binary deliberately contains no Matrix/crypto code,
//! so there is no second identity, store, or token to manage.
//!
//! Robustness: counting is offset-based (each complete line past the persisted
//! byte offset is one new message), with event-id de-dup as defence. State is
//! persisted atomically so a restart never re-counts or re-alerts. A fresh
//! volume (inode change / truncation) resets the count and replays first
//! contact (a new identity epoch). Partial trailing lines are held back until
//! newline-terminated; malformed lines are skipped. A failed notifier exec
//! leaves the milestone un-recorded so it retries — it never crash-loops.

use std::collections::VecDeque;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use serde::{Deserialize, Serialize};

/// Cap on the remembered recent event ids (de-dup defence). Bounds the state
/// file size; far larger than any plausible duplicate window.
const RECENT_IDS_CAP: usize = 512;
const STATE_SCHEMA: u32 = 1;

#[derive(Parser, Debug)]
#[command(name = "aqua-activity-watch", about = "Tail an agent activity log and alert on first + every-N messages")]
struct Args {
    /// Path to the agent's inbound activity JSONL on the host volume.
    #[arg(long, env = "AW_ACTIVITY_LOG")]
    activity_log: PathBuf,

    /// Instance label — names the state file and tags alerts.
    #[arg(long, env = "AW_LABEL", default_value = "garys")]
    label: String,

    /// Human label shown in alert text.
    #[arg(long, env = "AW_DISPLAY_LABEL", default_value = "Gary's Aqua Consultant")]
    display_label: String,

    /// Directory holding the persisted state file (default ~/.aqua-activity-watch).
    #[arg(long, env = "AW_STATE_DIR")]
    state_dir: Option<PathBuf>,

    /// Notifier script to exec for each alert (default ~/.aqua-matrix-notify/notify-tim.sh).
    #[arg(long, env = "AW_NOTIFIER")]
    notifier: Option<PathBuf>,

    /// Poll interval in seconds.
    #[arg(long, env = "AW_POLL_INTERVAL", default_value_t = 2)]
    poll_interval: u64,

    /// Milestone interval — alert on every Nth message.
    #[arg(long, env = "AW_MILESTONE", default_value_t = 10)]
    milestone: u64,

    /// Process one poll and exit (for tests/CI) instead of looping forever.
    #[arg(long, env = "AW_ONCE", default_value_t = false)]
    once: bool,
}

/// Persisted, atomically-written watcher state (one per label).
#[derive(Serialize, Deserialize, Debug, Default)]
struct State {
    schema: u32,
    label: String,
    activity_path: String,
    /// Inode of the activity file the offset/count refer to. 0 = none seen yet.
    inode: u64,
    /// Byte offset up to which the file has been consumed (newline-aligned).
    offset: u64,
    /// Distinct messages counted.
    count: u64,
    /// Whether the first-contact alert has been delivered for this epoch.
    first_contact_notified: bool,
    /// Highest milestone already alerted (monotonic; 0 = none).
    last_milestone: u64,
    /// Bounded ring of recent event ids, for duplicate-line defence.
    recent_event_ids: VecDeque<String>,
}

/// One activity record (only the field we need; extras ignored).
#[derive(Deserialize)]
struct InboundLine {
    event_id: String,
}

fn expand_tilde(p: &Path) -> PathBuf {
    if let Ok(stripped) = p.strip_prefix("~") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(stripped);
        }
    }
    p.to_path_buf()
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| ".".into()))
}

fn load_state(path: &Path) -> State {
    match std::fs::read_to_string(path) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
        Err(_) => State::default(),
    }
}

/// Write state atomically: temp file in the same dir, then rename.
fn save_state(path: &Path, state: &State) -> Result<()> {
    let tmp = path.with_extension("json.tmp");
    let json = serde_json::to_string_pretty(state)?;
    std::fs::write(&tmp, json).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("rename into {}", path.display()))?;
    Ok(())
}

/// Multiples of `step` in the half-open interval `(after, count]`.
fn crossed_milestones(after: u64, count: u64, step: u64) -> Vec<u64> {
    if step == 0 {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut m = (after / step + 1) * step;
    while m <= count {
        out.push(m);
        m += step;
    }
    out
}

/// Exec the notifier. Returns Ok(true) if it exited 0.
fn notify(notifier: &Path, title: &str, body: &str) -> Result<bool> {
    let status = Command::new(notifier)
        .arg("-s")
        .arg("INFO")
        .arg("-t")
        .arg(title)
        .arg(body)
        .status()
        .with_context(|| format!("exec notifier {}", notifier.display()))?;
    Ok(status.success())
}

struct Watcher {
    activity_log: PathBuf,
    state_path: PathBuf,
    notifier: PathBuf,
    label: String,
    display_label: String,
    milestone: u64,
    state: State,
}

impl Watcher {
    /// Read newly-appended complete lines, updating count/offset/recent ids.
    /// Returns true if any state field changed (so we know to persist).
    fn ingest(&mut self) -> Result<bool> {
        let meta = match std::fs::metadata(&self.activity_log) {
            Ok(m) => m,
            Err(_) => return Ok(false), // file not created yet — nothing to do
        };
        let ino = meta.ino();
        let size = meta.len();

        let mut dirty = false;

        // Fresh volume (new inode) or in-place truncation → reset this epoch.
        if ino != self.state.inode || size < self.state.offset {
            tracing::info!(
                "activity file reset (inode {} -> {}, size {} < offset {}); re-reading from 0",
                self.state.inode, ino, size, self.state.offset
            );
            self.state.inode = ino;
            self.state.offset = 0;
            self.state.count = 0;
            self.state.first_contact_notified = false;
            self.state.last_milestone = 0;
            self.state.recent_event_ids.clear();
            dirty = true;
        }

        if size <= self.state.offset {
            return Ok(dirty); // no new bytes
        }

        let mut f = std::fs::File::open(&self.activity_log)
            .with_context(|| format!("open {}", self.activity_log.display()))?;
        f.seek(SeekFrom::Start(self.state.offset))?;
        let to_read = (size - self.state.offset) as usize;
        let mut buf = vec![0u8; to_read];
        let n = f.read(&mut buf)?;
        buf.truncate(n);

        // Only process up to the last newline; hold back any partial trailing line.
        let last_nl = match buf.iter().rposition(|&b| b == b'\n') {
            Some(i) => i,
            None => return Ok(dirty), // no complete line yet
        };
        let complete = &buf[..=last_nl];

        for raw in complete.split(|&b| b == b'\n') {
            if raw.is_empty() {
                continue;
            }
            let line = match std::str::from_utf8(raw) {
                Ok(s) => s.trim(),
                Err(_) => continue,
            };
            if line.is_empty() {
                continue;
            }
            match serde_json::from_str::<InboundLine>(line) {
                Ok(rec) => {
                    if self.state.recent_event_ids.iter().any(|e| e == &rec.event_id) {
                        continue; // duplicate event id — already counted
                    }
                    self.state.count += 1;
                    self.state.recent_event_ids.push_back(rec.event_id);
                    if self.state.recent_event_ids.len() > RECENT_IDS_CAP {
                        self.state.recent_event_ids.pop_front();
                    }
                }
                Err(e) => {
                    tracing::debug!("skipping malformed activity line: {e}");
                }
            }
        }

        // Advance the offset past everything we consumed (the complete prefix).
        // We reached a complete line, so state changed (offset and likely count).
        self.state.offset += (last_nl + 1) as u64;
        Ok(true)
    }

    /// Deliver any due alerts (first-contact, then milestones). Each alert is
    /// sent at most once; a failed send is NOT recorded, so it retries next
    /// poll. Returns true if any milestone/flag advanced (persist needed).
    fn deliver(&mut self) -> bool {
        let mut advanced = false;

        if !self.state.first_contact_notified && self.state.count >= 1 {
            let body = format!(
                "🟢 New user activity — first message has arrived for \"{}\".",
                self.display_label
            );
            match notify(&self.notifier, &format!("{} activity", self.label), &body) {
                Ok(true) => {
                    self.state.first_contact_notified = true;
                    advanced = true;
                    tracing::info!("delivered first-contact alert (count=1)");
                }
                Ok(false) => tracing::warn!("notifier returned non-zero for first-contact; will retry"),
                Err(e) => tracing::warn!("first-contact notify failed: {e:#}; will retry"),
            }
        }

        let due = crossed_milestones(self.state.last_milestone, self.state.count, self.milestone);
        if let Some(&highest) = due.last() {
            let body = if due.len() == 1 {
                format!(
                    "📈 \"{}\" reached {} messages (now at {}).",
                    self.display_label, due[0], self.state.count
                )
            } else {
                let list = due
                    .iter()
                    .map(|m| m.to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                format!(
                    "📈 \"{}\" is now at {} messages (crossed milestones: {}).",
                    self.display_label, self.state.count, list
                )
            };
            match notify(&self.notifier, &format!("{} activity", self.label), &body) {
                Ok(true) => {
                    self.state.last_milestone = highest;
                    advanced = true;
                    tracing::info!("delivered milestone alert up to {highest} (count={})", self.state.count);
                }
                Ok(false) => tracing::warn!("notifier returned non-zero for milestone; will retry"),
                Err(e) => tracing::warn!("milestone notify failed: {e:#}; will retry"),
            }
        }

        advanced
    }

    fn poll(&mut self) -> Result<()> {
        let changed = self.ingest()?;
        let advanced = self.deliver();
        if changed || advanced {
            save_state(&self.state_path, &self.state)?;
        }
        Ok(())
    }
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,aqua_activity_watch=info".into()),
        )
        .init();

    let args = Args::parse();

    let activity_log = expand_tilde(&args.activity_log);
    let state_dir = args
        .state_dir
        .map(|p| expand_tilde(&p))
        .unwrap_or_else(|| home().join(".aqua-activity-watch"));
    let notifier = args
        .notifier
        .map(|p| expand_tilde(&p))
        .unwrap_or_else(|| home().join(".aqua-matrix-notify").join("notify-tim.sh"));

    std::fs::create_dir_all(&state_dir)
        .with_context(|| format!("create state dir {}", state_dir.display()))?;
    let state_path = state_dir.join(format!("state-{}.json", args.label));

    let mut state = load_state(&state_path);
    // (Re)stamp identity fields; if the configured path changed, start clean.
    if state.activity_path != activity_log.to_string_lossy() {
        state = State::default();
    }
    state.schema = STATE_SCHEMA;
    state.label = args.label.clone();
    state.activity_path = activity_log.to_string_lossy().into_owned();

    tracing::info!(
        "watching {} (label={}, milestone every {}, state={}, notifier={})",
        activity_log.display(),
        args.label,
        args.milestone,
        state_path.display(),
        notifier.display(),
    );

    let mut watcher = Watcher {
        activity_log,
        state_path,
        notifier,
        label: args.label,
        display_label: args.display_label,
        milestone: args.milestone,
        state,
    };

    if args.once {
        return watcher.poll();
    }

    loop {
        if let Err(e) = watcher.poll() {
            tracing::warn!("poll error: {e:#}");
        }
        std::thread::sleep(Duration::from_secs(args.poll_interval.max(1)));
    }
}
