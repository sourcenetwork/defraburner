//! Shared helpers for every `defraburner` integration test that spawns
//! the real binary and drives it over HTTP: `console.rs`,
//! `console_coverage.rs`, and `data_plane.rs`. One canonical
//! implementation per concept (spawn, deadline-poll a ready-file, read
//! the banner's bound address, common admin calls) so the three test
//! binaries can never silently drift from each other on what "wait for
//! ready" or "read the admin token" means. `tests/common/mod.rs` is
//! Cargo's own convention for a helper module shared across integration
//! test binaries without itself becoming one (unlike a bare
//! `tests/common.rs`, which Cargo would compile and run as its own,
//! empty test crate).

#![allow(dead_code)] // not every test binary uses every helper here.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

pub const READY_DEADLINE: Duration = Duration::from_secs(60);
pub const READY_POLL_STEP: Duration = Duration::from_millis(250);
pub const EXIT_DEADLINE: Duration = Duration::from_secs(30);
pub const EXIT_POLL_STEP: Duration = Duration::from_millis(100);
pub const STATUS_POLL_STEP: Duration = Duration::from_millis(300);
pub const CONDITION_DEADLINE: Duration = Duration::from_secs(30);

#[derive(serde::Deserialize)]
pub struct ReadyFile {
    pub cells: Vec<burner_cell::CellStatus>,
}

pub fn spawn_up(data_root: &Path, ready_file: &Path, extra_args: &[&str]) -> Child {
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_defraburner"));
    std::fs::create_dir_all(data_root).expect("create data root");
    let stdout = File::create(data_root.join("stdout.log")).expect("create stdout.log");
    let stderr = File::create(data_root.join("stderr.log")).expect("create stderr.log");
    Command::new(&binary)
        .arg("up")
        .arg("--data-root")
        .arg(data_root)
        .arg("--ready-file")
        .arg(ready_file)
        .args(extra_args)
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .spawn()
        .expect("spawn `defraburner up`")
}

pub enum SpawnProbe {
    Ready,
    Exited(ExitStatus),
}

pub fn wait_for_ready_or_exit(
    ready_file: &Path,
    child: &mut Child,
    deadline: Duration,
) -> SpawnProbe {
    let start = Instant::now();
    loop {
        if ready_file.exists() {
            return SpawnProbe::Ready;
        }
        if let Some(status) = child.try_wait().expect("try_wait") {
            return SpawnProbe::Exited(status);
        }
        if start.elapsed() >= deadline {
            let _ = child.kill();
            panic!(
                "timed out after {deadline:?} waiting for ready-file {}",
                ready_file.display()
            );
        }
        std::thread::sleep(READY_POLL_STEP);
    }
}

pub fn wait_for_ready_file(ready_file: &Path, child: &mut Child, deadline: Duration) -> ReadyFile {
    match wait_for_ready_or_exit(ready_file, child, deadline) {
        SpawnProbe::Ready => {
            let bytes = std::fs::read(ready_file).expect("read ready-file");
            serde_json::from_slice(&bytes).expect("parse ready-file JSON")
        }
        SpawnProbe::Exited(status) => {
            let stderr = std::fs::read_to_string(ready_file.with_file_name("stderr.log"))
                .unwrap_or_default();
            panic!(
                "defraburner exited unexpectedly with {status:?} before writing a ready-file; stderr:\n{stderr}"
            )
        }
    }
}

pub fn wait_for_exit(child: &mut Child, deadline: Duration) -> ExitStatus {
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            return status;
        }
        if start.elapsed() >= deadline {
            let _ = child.kill();
            panic!("process did not exit within {deadline:?} after SIGTERM");
        }
        std::thread::sleep(EXIT_POLL_STEP);
    }
}

pub fn send_sigterm(child: &Child) {
    let status = Command::new("kill")
        .arg("-TERM")
        .arg(child.id().to_string())
        .status()
        .expect("run `kill -TERM`");
    assert!(status.success(), "`kill -TERM` itself failed: {status:?}");
}

pub fn read_admin_token(data_root: &Path) -> String {
    std::fs::read_to_string(data_root.join("admin.token"))
        .expect("read admin.token")
        .trim()
        .to_string()
}

/// The gateway binds `127.0.0.1:0` in every test here (an OS-assigned
/// port); the actual bound address is recovered from the banner printed
/// to stdout rather than re-deriving it, so this is the single source of
/// truth for "what did the gateway actually bind" every test uses. Also
/// exercises the zero-config contract's own promise in passing: the
/// banner always names the *actual* bound address, even when the
/// gateway's bind resilience scanned past a requested port that was busy.
pub fn read_gateway_base_url(data_root: &Path) -> String {
    let stdout = std::fs::read_to_string(data_root.join("stdout.log")).expect("read stdout.log");
    let line = stdout
        .lines()
        .find(|line| line.trim_start().starts_with("gateway:"))
        .expect("banner should contain a 'gateway:' line");
    let url = line
        .split_whitespace()
        .last()
        .expect("gateway banner line should end with the URL");
    url.trim_end_matches('/').to_string()
}

pub fn admin_status(base_url: &str, admin_token: &str) -> serde_json::Value {
    ureq::get(&format!("{base_url}/admin/status"))
        .set("Authorization", &format!("Bearer {admin_token}"))
        .call()
        .expect("GET /admin/status should succeed")
        .into_json()
        .expect("valid JSON")
}

pub fn admin_cell_count(base_url: &str, admin_token: &str) -> usize {
    admin_status(base_url, admin_token)["cells"]
        .as_array()
        .map(Vec::len)
        .unwrap_or(0)
}

pub fn create_tenant(
    base_url: &str,
    admin_token: &str,
    name: &str,
    schema_sdl: &str,
    replicas: u8,
) -> serde_json::Value {
    let response = match ureq::post(&format!("{base_url}/admin/tenants"))
        .set("Authorization", &format!("Bearer {admin_token}"))
        .send_json(serde_json::json!({
            "name": name,
            "schema_sdl": schema_sdl,
            "replicas": replicas,
        })) {
        Ok(response) => response,
        // `ureq::Error`'s own `Display` omits the response body (exactly
        // the text an honest 500 puts the real cause in), so every
        // caller of this helper gets a real diagnosis instead of a bare
        // status code.
        Err(ureq::Error::Status(status, response)) => {
            let body = response.into_string().unwrap_or_default();
            panic!("creating tenant '{name}' should succeed, got {status}: {body}")
        }
        Err(error) => panic!("creating tenant '{name}' failed outright: {error}"),
    };
    assert_eq!(response.status(), 201);
    response.into_json().expect("valid JSON")
}

/// Deadline-polls `condition` on a fixed step until it returns `true`, or
/// panics once `deadline` has elapsed. Deadline-plus-step, never a bare
/// fixed sleep.
pub fn wait_until(deadline: Duration, step: Duration, mut condition: impl FnMut() -> bool) {
    let start = Instant::now();
    loop {
        if condition() {
            return;
        }
        if start.elapsed() >= deadline {
            panic!("condition not met within {deadline:?}");
        }
        std::thread::sleep(step);
    }
}
