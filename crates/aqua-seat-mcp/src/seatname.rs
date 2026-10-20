//! Seat names (decision D1): a seat is a lane or track, not a process. The name
//! is the tmux session name (the same key `tim-report` uses), else the env var
//! `CLAUDE_SEAT`, else `adhoc-<8 hex of the session id>`.
//!
//! The charset is `[a-z0-9_-]`, 1..=40 characters, and the name is used as a
//! directory name, so it is sanitised before it touches a path and re-checked
//! ([`is_valid`]) by every path builder.

/// Longest seat name.
pub const MAX_SEAT_LEN: usize = 40;

/// Names that mean something in the `to` argument of `seat_send` and so cannot
/// be a seat.
pub const RESERVED: [&str; 3] = ["room", "roster", "claude-seats"];

/// Where a derived seat name came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Env,
    Tmux,
    Adhoc,
}

/// A derived seat name and its origin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Derived {
    pub name: String,
    pub source: Source,
}

fn is_name_char(c: char) -> bool {
    c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_'
}

/// Turn arbitrary text (a tmux session name, a user's env var) into a seat
/// name: lowercased, every run of characters outside `[a-z0-9_-]` becomes one
/// `-`, leading and trailing `-`/`_` are dropped, cut at [`MAX_SEAT_LEN`].
/// `None` when nothing usable is left or the result is reserved.
pub fn sanitize(raw: &str) -> Option<String> {
    let mut out = String::with_capacity(raw.len().min(MAX_SEAT_LEN + 4));
    let mut last_dash = false;
    for c in raw.trim().chars() {
        let c = c.to_ascii_lowercase();
        if is_name_char(c) && c != '-' {
            out.push(c);
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    let trimmed = out.trim_matches(|c| c == '-' || c == '_');
    let mut cut: String = trimmed.chars().take(MAX_SEAT_LEN).collect();
    while cut.ends_with(['-', '_']) {
        cut.pop();
    }
    if cut.is_empty() || RESERVED.contains(&cut.as_str()) {
        return None;
    }
    Some(cut)
}

/// Whether `name` is exactly what [`sanitize`] would produce for itself. Every
/// path builder demands this, so a name that was never sanitised cannot reach
/// the filesystem.
pub fn is_valid(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_SEAT_LEN
        && name.chars().all(is_name_char)
        && name.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && name.chars().last().is_some_and(|c| c.is_ascii_alphanumeric())
        && !RESERVED.contains(&name)
}

/// The inputs of [`derive`], separated from the environment so the rule is
/// testable.
#[derive(Debug, Clone, Default)]
pub struct Inputs {
    /// `CLAUDE_SEAT`.
    pub env_seat: Option<String>,
    /// The tmux session name of `$TMUX_PANE`, when the process runs in tmux.
    pub tmux_session: Option<String>,
    /// `CLAUDE_CODE_SESSION_ID`, or any random string if there is none.
    pub session_id: Option<String>,
}

impl Inputs {
    /// Read the real environment. Runs `tmux display` only when `$TMUX_PANE`
    /// is set.
    pub fn from_env() -> Self {
        let tmux_session = std::env::var("TMUX_PANE")
            .ok()
            .filter(|p| !p.trim().is_empty())
            .and_then(|pane| {
                let out = std::process::Command::new("tmux")
                    .args(["display", "-p", "-t", &pane, "#S"])
                    .stderr(std::process::Stdio::null())
                    .output()
                    .ok()?;
                out.status
                    .success()
                    .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
            });
        Self {
            env_seat: std::env::var("CLAUDE_SEAT").ok(),
            tmux_session,
            session_id: std::env::var("CLAUDE_CODE_SESSION_ID").ok(),
        }
    }
}

/// D1: `CLAUDE_SEAT`, else the tmux session name, else `adhoc-<8 hex>`.
/// A value that sanitises to nothing is skipped, not an error.
pub fn derive(i: &Inputs) -> Derived {
    // The explicit env var wins; the tmux session name is the lane key
    // `tim-report` uses.
    if let Some(name) = i.env_seat.as_deref().and_then(sanitize) {
        return Derived {
            name,
            source: Source::Env,
        };
    }
    if let Some(name) = i.tmux_session.as_deref().and_then(sanitize) {
        return Derived {
            name,
            source: Source::Tmux,
        };
    }
    Derived {
        name: format!("adhoc-{}", adhoc_hex(i.session_id.as_deref())),
        source: Source::Adhoc,
    }
}

/// The first 8 hex digits of the session id (dashes ignored); 8 random hex
/// digits when there is no usable id.
fn adhoc_hex(session_id: Option<&str>) -> String {
    let hex: String = session_id
        .unwrap_or("")
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .take(8)
        .map(|c| c.to_ascii_lowercase())
        .collect();
    if hex.len() == 8 {
        hex
    } else {
        uuid::Uuid::new_v4().simple().to_string()[..8].to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_maps_to_the_charset() {
        assert_eq!(sanitize("Claude Id Comms").as_deref(), Some("claude-id-comms"));
        assert_eq!(sanitize("  Foo.Bar/baz  ").as_deref(), Some("foo-bar-baz"));
        assert_eq!(sanitize("a__b--c").as_deref(), Some("a__b-c"));
        assert_eq!(sanitize("seat-test-a").as_deref(), Some("seat-test-a"));
        assert_eq!(sanitize("Ünïcödé").as_deref(), Some("n-c-d"));
    }

    #[test]
    fn sanitize_strips_edges_and_rejects_empty() {
        assert_eq!(sanitize("--x--").as_deref(), Some("x"));
        assert_eq!(sanitize("_x_").as_deref(), Some("x"));
        assert_eq!(sanitize(""), None);
        assert_eq!(sanitize("   "), None);
        assert_eq!(sanitize("../.."), None);
        assert_eq!(sanitize("日本語"), None);
    }

    #[test]
    fn sanitize_cuts_at_forty_and_never_ends_in_a_separator() {
        let long = "a".repeat(100);
        assert_eq!(sanitize(&long).unwrap().len(), MAX_SEAT_LEN);
        let edge = format!("{}-b", "a".repeat(39));
        let s = sanitize(&edge).unwrap();
        assert_eq!(s, "a".repeat(39));
        assert!(is_valid(&s));
    }

    #[test]
    fn reserved_names_are_not_seats() {
        for r in RESERVED {
            assert_eq!(sanitize(r), None, "{r}");
            assert!(!is_valid(r), "{r}");
        }
        assert_eq!(sanitize("Roster"), None);
    }

    #[test]
    fn path_hostile_input_cannot_survive() {
        for bad in [
            "../../etc/passwd",
            "/abs/path",
            "a/b",
            "a\\b",
            "a\0b",
            "..",
            ".",
            ".hidden",
            "-rf",
            "x y",
        ] {
            if let Some(s) = sanitize(bad) {
                assert!(is_valid(&s), "{bad:?} -> {s:?}");
                assert!(!s.contains(['/', '\\', '.', '\0', ' ']), "{bad:?} -> {s:?}");
                assert!(!s.starts_with('-'), "{bad:?} -> {s:?}");
            }
            assert!(!is_valid(bad), "{bad:?} must not validate unsanitised");
        }
    }

    #[test]
    fn is_valid_is_strict() {
        assert!(is_valid("seat-test-a"));
        assert!(is_valid("a"));
        assert!(is_valid("0"));
        assert!(is_valid(&"a".repeat(40)));
        assert!(!is_valid(""));
        assert!(!is_valid(&"a".repeat(41)));
        assert!(!is_valid("Upper"));
        assert!(!is_valid("-lead"));
        assert!(!is_valid("trail-"));
        assert!(!is_valid("under_"));
        assert!(!is_valid("dot.ted"));
    }

    #[test]
    fn derive_prefers_env_then_tmux_then_adhoc() {
        let i = Inputs {
            env_seat: Some("My Lane".into()),
            tmux_session: Some("tmuxname".into()),
            session_id: Some("0f3a9c21-aaaa-bbbb-cccc-1234567890ab".into()),
        };
        assert_eq!(
            derive(&i),
            Derived {
                name: "my-lane".into(),
                source: Source::Env
            }
        );
        let i = Inputs {
            env_seat: Some("   ".into()),
            ..i
        };
        assert_eq!(
            derive(&i),
            Derived {
                name: "tmuxname".into(),
                source: Source::Tmux
            }
        );
        let i = Inputs {
            tmux_session: None,
            ..i
        };
        assert_eq!(
            derive(&i),
            Derived {
                name: "adhoc-0f3a9c21".into(),
                source: Source::Adhoc
            }
        );
    }

    #[test]
    fn adhoc_without_a_session_id_is_still_valid() {
        let d = derive(&Inputs::default());
        assert_eq!(d.source, Source::Adhoc);
        assert!(d.name.starts_with("adhoc-") && d.name.len() == 14, "{}", d.name);
        assert!(is_valid(&d.name));
        let d = derive(&Inputs {
            session_id: Some("zz-not-hex".into()),
            ..Default::default()
        });
        assert!(is_valid(&d.name));
    }
}
