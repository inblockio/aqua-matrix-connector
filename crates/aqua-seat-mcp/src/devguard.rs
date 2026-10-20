//! The spike is dev only. Both service URLs are checked at startup and on
//! every attach; a prod host is refused, not warned about.

use anyhow::{bail, Result};

/// Default siwx-oidc provider: DEV.
pub const DEFAULT_SIWX_URL: &str = "https://dev.siwx.inblock.io";
/// Default Matrix homeserver: DEV.
pub const DEFAULT_MATRIX_URL: &str = "https://dev.matrix.inblock.io";

/// Host part of an http(s) URL, lowercased, without port or userinfo.
fn host_of(url: &str) -> Option<String> {
    let rest = url.trim().split_once("://")?.1;
    let authority = rest.split(['/', '?', '#']).next()?;
    let host_port = authority.rsplit('@').next()?;
    let host = if let Some(v6) = host_port.strip_prefix('[') {
        v6.split(']').next()?
    } else {
        host_port.split(':').next()?
    };
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

/// Accept only dev endpoints: `dev.<...>.inblock.io`, or loopback for local
/// test servers. Anything else (in particular `matrix.inblock.io`,
/// `siwx-oidc.inblock.io`, `agentic.inblock.io`) is an error.
pub fn require_dev(url: &str) -> Result<()> {
    let Some(host) = host_of(url) else {
        bail!("not an http(s) URL: {url:?}");
    };
    let loopback = host == "localhost" || host == "127.0.0.1" || host == "::1";
    let dev = host.starts_with("dev.") && host.ends_with(".inblock.io");
    if loopback || dev {
        Ok(())
    } else {
        bail!("REFUSED: {host} is not a dev endpoint; Claude seats run on dev.*.inblock.io only (spike scope)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_dev() {
        require_dev(DEFAULT_SIWX_URL).unwrap();
        require_dev(DEFAULT_MATRIX_URL).unwrap();
        assert!(DEFAULT_SIWX_URL.contains("dev.siwx.inblock.io"));
        assert!(DEFAULT_MATRIX_URL.contains("dev.matrix.inblock.io"));
    }

    #[test]
    fn prod_hosts_are_refused() {
        for u in [
            "https://matrix.inblock.io",
            "https://siwx-oidc.inblock.io/",
            "https://agentic.inblock.io",
            "https://inblock.io",
            "https://dev.evil.example.com",
            "https://matrix.inblock.io.evil.dev.example.com",
            "https://dev.matrix.inblock.io@matrix.inblock.io",
            "ftp",
            "",
        ] {
            assert!(require_dev(u).is_err(), "{u:?}");
        }
    }

    #[test]
    fn dev_and_loopback_pass() {
        for u in [
            "https://dev.matrix.inblock.io",
            "https://dev.siwx.inblock.io:443/x",
            "http://localhost:8008",
            "http://127.0.0.1:9000/",
            "https://DEV.MATRIX.INBLOCK.IO",
        ] {
            require_dev(u).unwrap_or_else(|e| panic!("{u}: {e}"));
        }
    }
}
