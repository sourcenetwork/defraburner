//! Phase 1 golden test: kill -9 recovery against the real `defraburner`
//! binary. Proves the claim `docs/plans/defraburner.md`'s Phase 1 gate
//! requires: N cells ignite, a SIGKILL followed by a restart recovers every
//! cell with the same peer id and its data intact, and the graceful
//! (SIGTERM) shutdown path also exits cleanly.

use std::collections::HashMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use burner_cell::CellStatus;
use serde::Deserialize;

const READY_DEADLINE: Duration = Duration::from_secs(60);
const READY_POLL_STEP: Duration = Duration::from_millis(250);
const EXIT_DEADLINE: Duration = Duration::from_secs(30);
const EXIT_POLL_STEP: Duration = Duration::from_millis(100);
const MAX_SPAWN_ATTEMPTS: u32 = 3;

#[derive(Debug, Deserialize)]
struct ReadyFile {
    cells: Vec<CellStatus>,
}

#[test]
fn kill_9_recovery_preserves_peer_ids_and_data() {
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_defraburner"));
    let tmp = tempfile::tempdir().expect("tempdir");

    // --- (a) first spawn, with port-collision retry -----------------------
    //
    // Two OS-assigned free ports are not guaranteed adjacent (ephemeral
    // port assignment is not sequential), so the real mechanism for
    // avoiding a bind collision on the second cell's fixed port is
    // retrying against a fresh port (and a fresh, never-provisioned data
    // root, since a failed second cell still leaves the first cell fully
    // provisioned and its manifest saved) rather than trying to
    // atomically reserve two adjacent ports up front.
    let spawn_started = Instant::now();
    let (mut child, data_root, ready_file, base_port) =
        spawn_first_run_with_retry(&binary, tmp.path());

    // --- (b) wait for the ready-file; assert marker_ok for every cell ------
    let first_ready = wait_for_ready_file(&ready_file, &mut child, READY_DEADLINE);
    println!(
        "RECOVERY first_provision_ready_ms={}",
        spawn_started.elapsed().as_millis()
    );
    assert_eq!(first_ready.cells.len(), 2, "expected 2 provisioned cells");
    for cell in &first_ready.cells {
        assert!(
            cell.marker_ok,
            "cell '{}' marker_ok should be true right after provisioning",
            cell.id
        );
    }
    let first_by_id = index_by_id(&first_ready.cells);

    // --- (c) SIGKILL: kill -9 semantics -------------------------------------
    child.kill().expect("SIGKILL the running cluster");
    child.wait().expect("reap SIGKILL'd process");

    // --- (d) delete the ready-file, respawn with the same data-root --------
    std::fs::remove_file(&ready_file).expect("remove stale ready-file");
    let recovery_started = Instant::now();
    let mut child = spawn_start(&binary, &data_root, &ready_file, base_port, 2);

    // --- (e) wait for the new ready-file ------------------------------------
    let second_ready = wait_for_ready_file(&ready_file, &mut child, READY_DEADLINE);
    println!(
        "RECOVERY sigkill_recovery_ready_ms={}",
        recovery_started.elapsed().as_millis()
    );
    let second_by_id = index_by_id(&second_ready.cells);

    // --- (f) same cell ids, same peer_id, marker_ok, same listen_addrs -----
    assert_eq!(
        first_by_id.len(),
        second_by_id.len(),
        "cell count should be unchanged across recovery"
    );
    for (id, before) in &first_by_id {
        let after = second_by_id
            .get(id)
            .unwrap_or_else(|| panic!("cell '{id}' missing after recovery"));
        assert_eq!(
            before.peer_id, after.peer_id,
            "cell '{id}' peer id must be stable across a SIGKILL + restart (D7)"
        );
        assert!(
            after.marker_ok,
            "cell '{id}' data must survive a SIGKILL (marker_ok)"
        );
        assert_eq!(
            before.listen_addrs, after.listen_addrs,
            "cell '{id}' listen addresses must be identical across restart (fixed ports)"
        );
    }

    // --- (g) SIGTERM: graceful shutdown, clean exit 0 ----------------------
    send_sigterm(&child);
    let status = wait_for_exit(&mut child, EXIT_DEADLINE);
    assert!(
        status.success(),
        "graceful shutdown should exit 0, got {status:?}"
    );
}

fn index_by_id(cells: &[CellStatus]) -> HashMap<String, CellStatus> {
    cells
        .iter()
        .map(|cell| (cell.id.clone(), cell.clone()))
        .collect()
}

/// Spawns `start --cells 2` against a fresh subdirectory of `tmp_root`,
/// retrying up to [`MAX_SPAWN_ATTEMPTS`] times with a new random base port
/// (and a new, never-touched data root) whenever the child exits before
/// producing a ready-file. Returns the running child, the data root it
/// settled on, its ready-file path, and the base port that worked.
fn spawn_first_run_with_retry(binary: &Path, tmp_root: &Path) -> (Child, PathBuf, PathBuf, u16) {
    let mut last_diagnostics = String::new();
    for attempt in 0..MAX_SPAWN_ATTEMPTS {
        let data_root = tmp_root.join(format!("attempt-{attempt}"));
        std::fs::create_dir_all(&data_root).expect("create attempt data root");
        let ready_file = data_root.join("ready.json");
        let base_port = pick_base_port(attempt);

        let mut child = spawn_start(binary, &data_root, &ready_file, base_port, 2);
        match wait_for_ready_or_exit(&ready_file, &mut child, READY_DEADLINE) {
            SpawnProbe::Ready => return (child, data_root, ready_file, base_port),
            SpawnProbe::Exited(status) => {
                last_diagnostics = format!(
                    "attempt {attempt} (base_port {base_port}) exited early ({status:?}); \
                     stderr:\n{}",
                    read_log(&data_root.join("stderr.log"))
                );
            }
        }
    }
    panic!(
        "failed to start a 2-cell cluster after {MAX_SPAWN_ATTEMPTS} attempts \
         (likely repeated port collisions): {last_diagnostics}"
    );
}

enum SpawnProbe {
    Ready,
    Exited(ExitStatus),
}

/// Polls for either the ready-file appearing or the child exiting early,
/// whichever happens first. Used both to detect a successful start and (on
/// the initial retry-prone spawn) a port-collision failure.
fn wait_for_ready_or_exit(ready_file: &Path, child: &mut Child, deadline: Duration) -> SpawnProbe {
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

/// Same wait as [`wait_for_ready_or_exit`], but an early exit is a hard
/// test failure (used once we are past the retry-prone first spawn: any
/// exit here is a real bug, not a port collision to retry past).
fn wait_for_ready_file(ready_file: &Path, child: &mut Child, deadline: Duration) -> ReadyFile {
    match wait_for_ready_or_exit(ready_file, child, deadline) {
        SpawnProbe::Ready => {
            let bytes = std::fs::read(ready_file).expect("read ready-file");
            serde_json::from_slice(&bytes).expect("parse ready-file JSON")
        }
        SpawnProbe::Exited(status) => {
            panic!("defraburner exited unexpectedly with {status:?} before writing a ready-file")
        }
    }
}

fn wait_for_exit(child: &mut Child, deadline: Duration) -> ExitStatus {
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

fn spawn_start(
    binary: &Path,
    data_root: &Path,
    ready_file: &Path,
    base_port: u16,
    cells: u32,
) -> Child {
    let stdout = File::create(data_root.join("stdout.log")).expect("create stdout.log");
    let stderr = File::create(data_root.join("stderr.log")).expect("create stderr.log");
    Command::new(binary)
        .arg("start")
        .arg("--data-root")
        .arg(data_root)
        .arg("--cells")
        .arg(cells.to_string())
        .arg("--base-port")
        .arg(base_port.to_string())
        .arg("--ready-file")
        .arg(ready_file)
        // Redirected to files, not piped: an unread pipe can fill its OS
        // buffer and deadlock the child once tracing output exceeds it.
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .spawn()
        .expect("spawn `defraburner start`")
}

fn read_log(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|error| format!("<could not read log: {error}>"))
}

/// Picks a base port for a 2-cell cluster. On the first attempt, tries two
/// OS-assigned free ports and uses them directly if they happen to be
/// adjacent (the fast path: both ports are then confirmed free). On any
/// attempt (including the first, when the fast path misses), falls back to
/// a pseudo-random high port; the caller retries against the actual bind
/// outcome, so this does not need to be guaranteed-free, only likely-free
/// and different across attempts.
fn pick_base_port(attempt: u32) -> u16 {
    if attempt == 0 {
        let a = std::net::TcpListener::bind("127.0.0.1:0").expect("bind probe port a");
        let b = std::net::TcpListener::bind("127.0.0.1:0").expect("bind probe port b");
        let port_a = a.local_addr().expect("local_addr a").port();
        let port_b = b.local_addr().expect("local_addr b").port();
        drop(a);
        drop(b);
        if port_b == port_a + 1 {
            return port_a;
        }
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let seed = nanos ^ std::process::id() ^ attempt.wrapping_mul(104_729);
    20000 + (seed % 20000) as u16
}

fn send_sigterm(child: &Child) {
    let status = Command::new("kill")
        .arg("-TERM")
        .arg(child.id().to_string())
        .status()
        .expect("run `kill -TERM`");
    assert!(status.success(), "`kill -TERM` itself failed: {status:?}");
}
