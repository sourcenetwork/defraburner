//! Phase 4 gate test: synthetic sustained load scales the fleet up, then
//! back down, within the Rust-side clamps (never trusting the policy's
//! output any further than the clamp step validates it). Proves the
//! drained cell disappears from both the live cluster (`/admin/status`)
//! and the durable manifest, and that its removal is recorded in the
//! decision log with the clamped action and `executed: true`.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const SDL: &str = "type Spike { name: String }";
const READY_DEADLINE: Duration = Duration::from_secs(60);
const READY_POLL_STEP: Duration = Duration::from_millis(250);
const EXIT_DEADLINE: Duration = Duration::from_secs(30);
const EXIT_POLL_STEP: Duration = Duration::from_millis(100);
const SCALE_UP_DEADLINE: Duration = Duration::from_secs(30);
const SCALE_DOWN_DEADLINE: Duration = Duration::from_secs(60);
const STATUS_POLL_STEP: Duration = Duration::from_millis(500);
/// Safety ceiling on the load generator threads, well past
/// [`SCALE_UP_DEADLINE`]: the test stops them explicitly as soon as
/// scale-up is observed, this only bounds the case where it never is.
const LOAD_MAX_DURATION: Duration = Duration::from_secs(25);
const LOAD_THREADS: usize = 3;

#[test]
fn autoscaler_scales_up_then_down_within_guardrails() {
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_defraburner"));
    let tmp = tempfile::tempdir().expect("tempdir");
    let data_root = tmp.path().join("cluster");
    std::fs::create_dir_all(&data_root).expect("create data root");

    let schema_path = tmp.path().join("spike.graphql");
    std::fs::write(&schema_path, SDL).expect("write schema file");

    // --- tenant create via the real CLI, capturing the printed token ----
    let create_output = Command::new(&binary)
        .arg("tenant")
        .arg("create")
        .arg("--data-root")
        .arg(&data_root)
        .arg("--name")
        .arg("spike-co")
        .arg("--schema")
        .arg(&schema_path)
        .arg("--replicas")
        .arg("1")
        .output()
        .expect("run `defraburner tenant create`");
    assert!(
        create_output.status.success(),
        "tenant create failed: {}",
        String::from_utf8_lossy(&create_output.stderr)
    );
    let tenant_token = extract_token(
        &String::from_utf8_lossy(&create_output.stdout),
        "tenant spike-co token",
    )
    .expect("tenant create should print a token line");

    // --- start: 1 cell, aggressive autoscaler knobs -----------------------
    let gateway_port = free_tcp_port();
    let gateway_addr = format!("127.0.0.1:{gateway_port}");
    let base_url = format!("http://{gateway_addr}");
    let ready_file = data_root.join("ready.json");
    let base_port = pick_base_port();

    let mut child = Command::new(&binary)
        .arg("start")
        .arg("--data-root")
        .arg(&data_root)
        .arg("--cells")
        .arg("1")
        .arg("--min-cells")
        .arg("1")
        .arg("--max-cells")
        .arg("3")
        .arg("--tick-interval")
        .arg("1")
        .arg("--cooldown-secs")
        .arg("2")
        .arg("--base-port")
        .arg(base_port.to_string())
        .arg("--gateway-addr")
        .arg(&gateway_addr)
        .arg("--ready-file")
        .arg(&ready_file)
        .stdout(Stdio::from(
            File::create(data_root.join("stdout.log")).unwrap(),
        ))
        .stderr(Stdio::from(
            File::create(data_root.join("stderr.log")).unwrap(),
        ))
        .spawn()
        .expect("spawn `defraburner start`");
    wait_for_ready_file(&ready_file, &mut child, READY_DEADLINE);

    let admin_token = std::fs::read_to_string(data_root.join("admin.token"))
        .expect("read admin.token")
        .trim()
        .to_string();

    // --- fire sustained load through the gateway (2-3 threads, tight loop)
    let stop = Arc::new(AtomicBool::new(false));
    let load_handles: Vec<std::thread::JoinHandle<()>> = (0..LOAD_THREADS)
        .map(|_| {
            let base_url = base_url.clone();
            let tenant_token = tenant_token.clone();
            let stop = stop.clone();
            std::thread::spawn(move || {
                let deadline = Instant::now() + LOAD_MAX_DURATION;
                while !stop.load(Ordering::Relaxed) && Instant::now() < deadline {
                    let _ = ureq::post(&format!("{base_url}/api/v1/graphql"))
                        .set("Authorization", &format!("Bearer {tenant_token}"))
                        .send_json(serde_json::json!({ "query": "query { Spike { name } }" }));
                }
            })
        })
        .collect();

    // --- poll /admin/status until the fleet scales up ----------------------
    let scale_up_deadline = Instant::now() + SCALE_UP_DEADLINE;
    let mut cell_count = admin_cell_count(&base_url, &admin_token);
    while cell_count < 2 {
        if Instant::now() >= scale_up_deadline {
            stop.store(true, Ordering::Relaxed);
            for handle in load_handles {
                let _ = handle.join();
            }
            send_sigterm(&child);
            wait_for_exit(&mut child, EXIT_DEADLINE);
            panic!(
                "timed out after {SCALE_UP_DEADLINE:?} waiting for the autoscaler to scale up \
                 (last observed cell count: {cell_count})"
            );
        }
        std::thread::sleep(STATUS_POLL_STEP);
        cell_count = admin_cell_count(&base_url, &admin_token);
    }
    assert!(
        cell_count >= 2,
        "scale-up should have grown the fleet, got {cell_count} cell(s)"
    );

    // Removal is disabled by default (D41), because draining a cell now
    // destroys the wasm database it owns. This test is specifically about
    // the scale-down half of the guardrails, so it turns the knob on
    // through the same admin route an operator would use, which also
    // proves the knob actually reaches the clamp.
    let enable = ureq::put(&format!("{base_url}/admin/autoscaler"))
        .set("Authorization", &format!("Bearer {admin_token}"))
        .send_json(serde_json::json!({ "scale_down_enabled": true }));
    match enable {
        Ok(response) => {
            let body: serde_json::Value = response.into_json().expect("autoscaler response body");
            assert_eq!(
                body["scale_down_enabled"], true,
                "enabling scale-down should be reflected in the response: {body:?}"
            );
        }
        Err(error) => {
            send_sigterm(&child);
            wait_for_exit(&mut child, EXIT_DEADLINE);
            panic!("enabling scale_down failed: {error}");
        }
    }

    // --- stop the load, then poll until the fleet scales back down to 1 ---
    stop.store(true, Ordering::Relaxed);
    for handle in load_handles {
        handle
            .join()
            .expect("a load-generator thread should not panic");
    }

    let scale_down_deadline = Instant::now() + SCALE_DOWN_DEADLINE;
    let mut status = admin_status(&base_url, &admin_token);
    let mut cell_count = status["cells"].as_array().map(Vec::len).unwrap_or(0);
    while cell_count > 1 {
        if Instant::now() >= scale_down_deadline {
            send_sigterm(&child);
            wait_for_exit(&mut child, EXIT_DEADLINE);
            panic!(
                "timed out after {SCALE_DOWN_DEADLINE:?} waiting for the autoscaler to scale back \
                 down (last observed cell count: {cell_count})"
            );
        }
        std::thread::sleep(STATUS_POLL_STEP);
        status = admin_status(&base_url, &admin_token);
        cell_count = status["cells"].as_array().map(Vec::len).unwrap_or(0);
    }

    let cells_after = status["cells"]
        .as_array()
        .expect("cells array in /admin/status");
    assert_eq!(
        cells_after.len(),
        1,
        "fleet should be back at 1 cell after scale-down"
    );

    // --- the removal is logged, with the clamped action and executed=true -
    let decisions = status["decisions"]
        .as_array()
        .expect("decisions array in /admin/status");
    let scale_down_entry = decisions
        .iter()
        .rev()
        .find(|entry| {
            entry["clamped"]
                .as_array()
                .map(|actions| actions.iter().any(|action| action["kind"] == "scale_down"))
                .unwrap_or(false)
        })
        .unwrap_or_else(|| {
            panic!("expected a logged scale_down decision entry among: {decisions:?}")
        });
    assert_eq!(
        scale_down_entry["executed"], true,
        "scale_down entry should show executed=true: {scale_down_entry:?}"
    );
    assert_eq!(
        scale_down_entry["error"],
        serde_json::Value::Null,
        "scale_down entry should show no execution error: {scale_down_entry:?}"
    );
    let removed_cell_id = scale_down_entry["clamped"]
        .as_array()
        .unwrap()
        .iter()
        .find_map(|action| {
            (action["kind"] == "scale_down")
                .then(|| action["cell_id"].as_str())
                .flatten()
        })
        .expect("scale_down action should carry a cell_id")
        .to_string();

    // --- the drained cell disappeared from the manifest too, not just the
    // --- live supervisor status --------------------------------------------
    let manifest: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(data_root.join("cluster.json")).expect("read cluster.json"),
    )
    .expect("parse cluster.json");
    let manifest_cell_ids: Vec<&str> = manifest["cells"]
        .as_array()
        .expect("cells array in cluster.json")
        .iter()
        .filter_map(|cell| cell["id"].as_str())
        .collect();
    assert!(
        !manifest_cell_ids.contains(&removed_cell_id.as_str()),
        "removed cell '{removed_cell_id}' should no longer be in the manifest, got {manifest_cell_ids:?}"
    );
    let status_cell_ids: Vec<&str> = cells_after
        .iter()
        .filter_map(|cell| cell["id"].as_str())
        .collect();
    assert!(
        !status_cell_ids.contains(&removed_cell_id.as_str()),
        "removed cell '{removed_cell_id}' should no longer appear in /admin/status"
    );

    // --- clean shutdown -----------------------------------------------------
    send_sigterm(&child);
    let status = wait_for_exit(&mut child, EXIT_DEADLINE);
    assert!(
        status.success(),
        "defraburner should exit cleanly on SIGTERM, got {status:?}"
    );
}

fn admin_status(base_url: &str, admin_token: &str) -> serde_json::Value {
    ureq::get(&format!("{base_url}/admin/status"))
        .set("Authorization", &format!("Bearer {admin_token}"))
        .call()
        .expect("GET /admin/status should succeed")
        .into_json()
        .expect("valid JSON")
}

fn admin_cell_count(base_url: &str, admin_token: &str) -> usize {
    admin_status(base_url, admin_token)["cells"]
        .as_array()
        .map(Vec::len)
        .unwrap_or(0)
}

/// Finds the line starting with `prefix` and returns its last
/// whitespace-delimited token (the printed token itself). Mirrors
/// `tests/gateway.rs`'s helper of the same name.
fn extract_token(output: &str, prefix: &str) -> Option<String> {
    output
        .lines()
        .find(|line| line.starts_with(prefix))
        .and_then(|line| line.rsplit(' ').next())
        .map(str::to_string)
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

fn wait_for_ready_file(ready_file: &Path, child: &mut Child, deadline: Duration) {
    match wait_for_ready_or_exit(ready_file, child, deadline) {
        SpawnProbe::Ready => {}
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

fn send_sigterm(child: &Child) {
    let status = Command::new("kill")
        .arg("-TERM")
        .arg(child.id().to_string())
        .status()
        .expect("run `kill -TERM`");
    assert!(status.success(), "`kill -TERM` itself failed: {status:?}");
}

/// Binds an ephemeral OS-assigned TCP port and immediately releases it,
/// for use as a (best-effort, not reserved) free port.
fn free_tcp_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind probe port");
    listener.local_addr().expect("local_addr").port()
}

/// Picks a base port for up to a 3-cell cluster (`--max-cells 3`, so the
/// autoscaler may provision `base_port`, `base_port + 1`, and
/// `base_port + 2`): three OS-assigned free ports if they happen to be
/// consecutive (the fast path, then all three confirmed free), else a
/// pseudo-random high port (a genuine bind failure would still surface
/// loudly via `gateway::build`'s fail-loud bind behavior). Mirrors
/// `tests/gateway.rs`'s `pick_base_port` helper, extended from 2 ports to
/// 3.
fn pick_base_port() -> u16 {
    let a = std::net::TcpListener::bind("127.0.0.1:0").expect("bind probe port a");
    let b = std::net::TcpListener::bind("127.0.0.1:0").expect("bind probe port b");
    let c = std::net::TcpListener::bind("127.0.0.1:0").expect("bind probe port c");
    let port_a = a.local_addr().expect("local_addr a").port();
    let port_b = b.local_addr().expect("local_addr b").port();
    let port_c = c.local_addr().expect("local_addr c").port();
    drop(a);
    drop(b);
    drop(c);
    if port_b == port_a + 1 && port_c == port_a + 2 {
        return port_a;
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    20000 + ((nanos ^ std::process::id()) % 20000) as u16
}
