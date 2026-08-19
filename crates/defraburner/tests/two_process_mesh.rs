//! Phase 2 gate test: two independently-started `defraburner` processes
//! mesh over loopback via `start --peers` (static cross-host peer
//! dialing). Real subprocesses, real ready-files: proves the mesh forms
//! across a process boundary, not just in-process
//! (`tests/tenants.rs` covers the in-process group-wiring path).

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use burner_cell::CellStatus;
use burner_mesh::PeerDialOutcome;
use serde::Deserialize;

const READY_DEADLINE: Duration = Duration::from_secs(60);
const READY_POLL_STEP: Duration = Duration::from_millis(250);
const EXIT_DEADLINE: Duration = Duration::from_secs(30);
const EXIT_POLL_STEP: Duration = Duration::from_millis(100);
const MAX_SPAWN_ATTEMPTS: u32 = 3;

#[derive(Debug, Deserialize)]
struct ReadyFile {
    cells: Vec<CellStatus>,
    static_peer_outcomes: Vec<PeerDialOutcome>,
}

#[test]
fn cross_process_mesh_dials_static_peers() {
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_defraburner"));
    let tmp = tempfile::tempdir().expect("tempdir");

    let (mut child_a, _data_root_a, ready_file_a) =
        spawn_with_retry(&binary, &tmp.path().join("a"), &[]);
    let ready_a = wait_for_ready_file(&ready_file_a, &mut child_a, READY_DEADLINE);
    assert_eq!(ready_a.cells.len(), 2, "process A should provision 2 cells");

    // Assemble A's cells' dialable addrs: <addr>/p2p/<peer-id> (the same
    // template `burner_cell::cell::RunningCell::dialable_addr` uses).
    let a_addrs: Vec<String> = ready_a
        .cells
        .iter()
        .filter_map(|cell| {
            cell.listen_addrs
                .first()
                .map(|addr| format!("{addr}/p2p/{}", cell.peer_id))
        })
        .collect();
    assert_eq!(
        a_addrs.len(),
        2,
        "both of A's cells should have a listen address; got {:?}",
        ready_a.cells
    );
    let a_peer_ids: Vec<String> = ready_a.cells.iter().map(|c| c.peer_id.clone()).collect();

    let (mut child_b, _data_root_b, ready_file_b) = spawn_with_retry(
        &binary,
        &tmp.path().join("b"),
        &["--peers".to_string(), a_addrs.join(",")],
    );
    let ready_b = wait_for_ready_file(&ready_file_b, &mut child_b, READY_DEADLINE);
    assert_eq!(ready_b.cells.len(), 2, "process B should provision 2 cells");

    // B's ready-file is written after `dial_static_peers` and
    // `status_with_connected_peers` both ran, so both of A's peer ids
    // should already show up somewhere across B's cells'
    // `connected_peers` (every B cell dials every configured peer; a given
    // A peer need not be connected from every B cell, just at least one).
    let all_b_connected: Vec<String> = ready_b
        .cells
        .iter()
        .flat_map(|cell| cell.connected_peers.iter().cloned())
        .collect();
    for a_peer in &a_peer_ids {
        assert!(
            all_b_connected
                .iter()
                .any(|connected| connected.contains(a_peer.as_str())),
            "B's ready-file connected_peers should include A's peer '{a_peer}'; got {all_b_connected:?}"
        );
    }

    // D19: `start` deadline-polls every successfully-dialed peer into
    // `connected_peers()` before writing the ready-file, so every outcome
    // whose dial succeeded must also be confirmed here -- never a bare
    // unpolled snapshot that merely happened to observe the connection in
    // time. This is a direct assertion on the polled `confirmed` field
    // itself, not just an inference from `connected_peers` above.
    assert_eq!(
        ready_b.static_peer_outcomes.len(),
        2 * a_addrs.len(),
        "every one of B's 2 cells should have attempted every one of A's addrs; got {:?}",
        ready_b.static_peer_outcomes
    );
    for outcome in &ready_b.static_peer_outcomes {
        assert!(
            outcome.ok,
            "dial from '{}' to '{}' should have succeeded; error={:?}",
            outcome.cell_id, outcome.peer_addr, outcome.error
        );
        assert!(
            outcome.confirmed,
            "dial from '{}' to '{}' succeeded but was never confirmed connected \
             within the deadline; note={:?}",
            outcome.cell_id, outcome.peer_addr, outcome.note
        );
    }

    send_sigterm(&child_a);
    send_sigterm(&child_b);
    let status_a = wait_for_exit(&mut child_a, EXIT_DEADLINE);
    let status_b = wait_for_exit(&mut child_b, EXIT_DEADLINE);
    assert!(
        status_a.success(),
        "process A should exit cleanly on SIGTERM, got {status_a:?}"
    );
    assert!(
        status_b.success(),
        "process B should exit cleanly on SIGTERM, got {status_b:?}"
    );
}

/// Spawns `start --cells 2` under a fresh subdirectory of `parent`, with
/// `extra_args` appended (the `--peers` case), retrying up to
/// [`MAX_SPAWN_ATTEMPTS`] times with a new random base port whenever the
/// child exits before producing a ready-file. Mirrors
/// `tests/recovery.rs`'s `spawn_first_run_with_retry`, generalized to
/// carry extra CLI args.
fn spawn_with_retry(
    binary: &Path,
    parent: &Path,
    extra_args: &[String],
) -> (Child, PathBuf, PathBuf) {
    let mut last_diagnostics = String::new();
    for attempt in 0..MAX_SPAWN_ATTEMPTS {
        let data_root = parent.join(format!("attempt-{attempt}"));
        std::fs::create_dir_all(&data_root).expect("create attempt data root");
        let ready_file = data_root.join("ready.json");
        let base_port = pick_base_port(attempt);
        // Each process needs its own gateway listener too: an ephemeral,
        // OS-assigned port (best-effort free, like `base_port`; a
        // collision here fails this attempt the same way a p2p port
        // collision does, and is retried).
        let gateway_addr = free_tcp_port();

        let mut child = spawn_start(
            binary,
            &data_root,
            &ready_file,
            base_port,
            gateway_addr,
            extra_args,
        );
        match wait_for_ready_or_exit(&ready_file, &mut child, READY_DEADLINE) {
            SpawnProbe::Ready => return (child, data_root, ready_file),
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

#[allow(clippy::too_many_arguments)]
fn spawn_start(
    binary: &Path,
    data_root: &Path,
    ready_file: &Path,
    base_port: u16,
    gateway_addr: u16,
    extra_args: &[String],
) -> Child {
    let stdout = File::create(data_root.join("stdout.log")).expect("create stdout.log");
    let stderr = File::create(data_root.join("stderr.log")).expect("create stderr.log");
    Command::new(binary)
        .arg("start")
        .arg("--data-root")
        .arg(data_root)
        .arg("--cells")
        .arg("2")
        .arg("--base-port")
        .arg(base_port.to_string())
        .arg("--gateway-addr")
        .arg(format!("127.0.0.1:{gateway_addr}"))
        .arg("--ready-file")
        .arg(ready_file)
        .args(extra_args)
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

/// Binds an ephemeral OS-assigned TCP port and immediately releases it,
/// for use as a (best-effort, not reserved) free `--gateway-addr` port.
fn free_tcp_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind probe port");
    listener.local_addr().expect("local_addr").port()
}

/// Picks a base port for a 2-cell cluster; mirrors `tests/recovery.rs`'s
/// helper of the same name (see its doc comment for the fast-path/fallback
/// rationale).
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
