//! Offline checks of the front-end binaries: `--version` everywhere, and
//! `aqua-seat-listen` against an unreachable daemon.

use std::process::Command;

#[test]
fn every_binary_answers_version() {
    for (bin, name) in [
        (env!("CARGO_BIN_EXE_aqua-seat-mcp"), "aqua-seat-mcp"),
        (env!("CARGO_BIN_EXE_aqua-seat-listen"), "aqua-seat-listen"),
        (env!("CARGO_BIN_EXE_aqua-seat-stress"), "aqua-seat-stress"),
    ] {
        let out = Command::new(bin).arg("--version").output().unwrap();
        assert!(out.status.success(), "{name}");
        let s = String::from_utf8_lossy(&out.stdout);
        assert!(s.starts_with(&format!("{name} ")), "{name}: {s}");
    }
}

#[test]
fn listen_exits_1_when_the_daemon_is_unreachable_and_nudge_stays_silent() {
    let t = tempfile::tempdir().unwrap();
    let env = |c: &mut Command| {
        c.env("CLAUDE_SEATS_STATE", t.path())
            .env("CLAUDE_SEATS_SOCK", t.path().join("none.sock"))
            .env("CLAUDE_SEATS_NO_SPAWN", "1")
            .env_remove("CLAUDE_SEAT");
    };
    let mut c = Command::new(env!("CARGO_BIN_EXE_aqua-seat-listen"));
    c.args(["--seat", "seat-cli-test", "--timeout", "5"]);
    env(&mut c);
    let out = c.output().unwrap();
    assert_eq!(out.status.code(), Some(1), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(out.stdout.is_empty());
    assert!(String::from_utf8_lossy(&out.stderr).contains("cannot reach aqua-seatd"));

    let mut c = Command::new(env!("CARGO_BIN_EXE_aqua-seat-listen"));
    c.args(["--seat", "seat-cli-test", "--nudge"]);
    env(&mut c);
    let out = c.output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert!(out.stdout.is_empty(), "nudge prints nothing when nothing is unread");
}

#[test]
fn a_second_listener_exits_0_at_once() {
    let t = tempfile::tempdir().unwrap();
    // hold the lock the way a running listener would
    let paths = aqua_seat::paths::SeatPaths::new(t.path(), "seat-cli-lock").unwrap();
    let held = aqua_seat::listen::acquire_lock(&paths).unwrap();
    assert!(matches!(held, aqua_seat::listen::Lock::Held(_)));
    let t0 = std::time::Instant::now();
    let out = Command::new(env!("CARGO_BIN_EXE_aqua-seat-listen"))
        .args(["--seat", "seat-cli-lock"])
        .env("CLAUDE_SEATS_STATE", t.path())
        .env("CLAUDE_SEATS_SOCK", t.path().join("none.sock"))
        .env("CLAUDE_SEATS_NO_SPAWN", "1")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert!(out.stdout.is_empty() && out.stderr.is_empty());
    assert!(t0.elapsed() < std::time::Duration::from_secs(5));
}
