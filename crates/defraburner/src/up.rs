//! `defraburner up` support (D21/D25): resolving the default data root,
//! finding a free port for a fresh single-cell provision, and deciding
//! whether to best-effort open the dashboard in a browser. `start::run`
//! does the actual provisioning/serving; this module is just the
//! `up`-specific decisions its CLI handler makes before calling into it.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};

/// Env var overriding the default data root, checked before falling back
/// to `$HOME/.local/share/defraburner` (D21).
const DATA_ENV_VAR: &str = "DEFRABURNER_DATA";

/// Resolves `up`'s data root: `--data-root` flag, then `DEFRABURNER_DATA`,
/// then `$HOME/.local/share/defraburner`.
pub fn resolve_data_root(flag: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) = flag {
        return Ok(path);
    }
    if let Some(env_path) = std::env::var_os(DATA_ENV_VAR) {
        return Ok(PathBuf::from(env_path));
    }
    let home = std::env::var_os("HOME")
        .context("HOME is not set; pass --data-root or set DEFRABURNER_DATA")?;
    Ok(PathBuf::from(home).join(".local/share/defraburner"))
}

/// Scans `[start, start + width)` for the first TCP port this host can
/// currently bind on `127.0.0.1`, releasing the probe bind immediately
/// (best-effort, not reserved: matches the same convention every gate
/// test's own `free_tcp_port` helper already uses). Fresh-provision only:
/// a recovered cluster always reuses each cell's own recorded `p2p_port`
/// from the manifest, never this scan.
pub fn find_free_port(start: u16, width: u16) -> Result<u16> {
    for offset in 0..width {
        let Some(port) = start.checked_add(offset) else {
            break;
        };
        if std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return Ok(port);
        }
    }
    bail!(
        "no free TCP port found scanning [{start}, {}) ({width} candidates)",
        start as u32 + width as u32
    );
}

/// Whether `up` should best-effort open the dashboard in a browser:
/// `!no_open` and a display server is present. Split into this pure
/// combinator (unit-tested directly) and [`has_display_env`] (the real
/// environment read, not unit-testable in isolation since env vars are
/// process-global) so the decision logic itself is exercised without
/// mutating process environment from a test.
pub fn should_open_browser(no_open: bool) -> bool {
    should_open_browser_with(no_open, has_display_env())
}

fn should_open_browser_with(no_open: bool, has_display: bool) -> bool {
    !no_open && has_display
}

fn has_display_env() -> bool {
    std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_data_root_prefers_the_explicit_flag() {
        let resolved = resolve_data_root(Some(PathBuf::from("/explicit/root"))).unwrap();
        assert_eq!(resolved, PathBuf::from("/explicit/root"));
    }

    #[test]
    fn find_free_port_returns_a_bindable_port_in_range() {
        // Bind an ephemeral OS-assigned port first so the scan has a
        // known-taken port to (potentially) skip past, then scan a tiny
        // window starting there: at minimum the immediately-following
        // ports are overwhelmingly likely free on a test host.
        let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let taken_port = taken.local_addr().unwrap().port();
        let found = find_free_port(taken_port, 64).expect("should find a free port nearby");
        assert!(found >= taken_port);
        // The found port must actually be bindable right now.
        std::net::TcpListener::bind(("127.0.0.1", found)).expect("found port should be bindable");
    }

    #[test]
    fn find_free_port_skips_an_occupied_port() {
        let occupied = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let occupied_port = occupied.local_addr().unwrap().port();
        let found = find_free_port(occupied_port, 64).expect("should find a free port");
        assert_ne!(
            found, occupied_port,
            "must not return the still-bound occupied port"
        );
        drop(occupied);
    }

    #[test]
    fn find_free_port_fails_loudly_when_the_whole_window_is_exhausted() {
        // A zero-width window can never find anything: proves the bound
        // is real (a scan, not an unconditional success) without needing
        // to actually occupy 64 real ports in a unit test.
        let error = find_free_port(9171, 0).unwrap_err();
        assert!(error.to_string().contains("no free TCP port"));
    }

    #[test]
    fn should_open_browser_requires_both_not_no_open_and_a_display() {
        assert!(should_open_browser_with(false, true));
        assert!(!should_open_browser_with(true, true), "no_open wins");
        assert!(
            !should_open_browser_with(false, false),
            "no display, no open"
        );
        assert!(!should_open_browser_with(true, false));
    }
}
