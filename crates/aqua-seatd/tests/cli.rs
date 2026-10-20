//! Offline checks of the daemon binary: `--version`, and `--print-host-did`
//! printing only the public DID (D8) without starting anything.

use std::os::unix::fs::PermissionsExt;
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_aqua-seatd");

#[test]
fn version_flag_works() {
    let out = Command::new(BIN).arg("--version").output().unwrap();
    assert!(out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.starts_with("aqua-seatd "), "{s}");
}

#[test]
fn print_host_did_prints_only_the_public_did_and_is_stable() {
    let t = tempfile::tempdir().unwrap();
    let run = || {
        Command::new(BIN)
            .arg("--print-host-did")
            .env("CLAUDE_SEATS_STATE", t.path())
            .env("CLAUDE_SEATS_SOCK", t.path().join("nope.sock"))
            .output()
            .unwrap()
    };
    let a = run();
    assert!(a.status.success(), "{}", String::from_utf8_lossy(&a.stderr));
    let did = String::from_utf8(a.stdout).unwrap();
    let did = did.trim();
    assert!(did.starts_with("did:key:z6Mk") && !did.contains(char::is_whitespace), "{did}");
    let pem = t.path().join("host.pem");
    assert_eq!(std::fs::metadata(&pem).unwrap().permissions().mode() & 0o777, 0o600);
    let b = String::from_utf8(run().stdout).unwrap();
    assert_eq!(b.trim(), did, "the host DID never changes");
    // nothing else was started or created: no socket, no seats, no lock
    assert!(!t.path().join("nope.sock").exists());
    assert!(!t.path().join("seats").exists());
    assert!(!t.path().join("seatd.lock").exists());
    // and no key material on stdout
    assert!(!String::from_utf8_lossy(&run().stdout).contains("PRIVATE"));
}

#[test]
fn prod_endpoints_are_refused_at_startup() {
    let t = tempfile::tempdir().unwrap();
    for (flag, url) in [
        ("--matrix-url", "https://matrix.inblock.io"),
        ("--siwx-url", "https://siwx-oidc.inblock.io"),
    ] {
        let out = Command::new(BIN)
            .args([flag, url])
            .env("CLAUDE_SEATS_STATE", t.path())
            .env("CLAUDE_SEATS_SOCK", t.path().join("s.sock"))
            .output()
            .unwrap();
        assert!(!out.status.success());
        assert!(String::from_utf8_lossy(&out.stderr).contains("not a dev endpoint"));
        assert!(!t.path().join("s.sock").exists());
    }
}

#[test]
fn hub_flag_accepts_one_from_the_environment() {
    let t = tempfile::tempdir().unwrap();
    // --print-host-did exits before anything starts; it only proves the flag parses.
    for v in ["1", "true", "0"] {
        let out = Command::new(BIN)
            .arg("--print-host-did")
            .env("CLAUDE_SEATS_HUB", v)
            .env("CLAUDE_SEATS_STATE", t.path())
            .output()
            .unwrap();
        assert!(out.status.success(), "CLAUDE_SEATS_HUB={v}: {}", String::from_utf8_lossy(&out.stderr));
    }
}
