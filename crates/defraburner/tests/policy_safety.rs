//! Phase 4 gate test: a broken policy package never wedges the cluster.
//! `testonly-malformed` (packages/testonly-malformed, a real,
//! AOT-compiled, syntactically valid sealed package that always answers
//! `{"nonsense": 1}`) is loaded as a `--packages-dir` override laid out
//! under a directory named `autoscale-default`, so it replaces the
//! embedded default entirely: the cluster must still start, still serve
//! GraphQL traffic, a positive `/admin/status` `policy.consecutive_errors`
//! count must show up, and the cell count must never change across a
//! bounded observation window. A second test proves the opposite failure
//! shape: a genuinely corrupt override (a truncated file, not a real
//! `.afb` archive) makes `start` itself exit nonzero with a loud,
//! actionable error -- a startup configuration error, never a silent
//! fallback.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

const SDL: &str = "type Spike { name: String }";
const READY_DEADLINE: Duration = Duration::from_secs(60);
const READY_POLL_STEP: Duration = Duration::from_millis(250);
const EXIT_DEADLINE: Duration = Duration::from_secs(30);
const EXIT_POLL_STEP: Duration = Duration::from_millis(100);
/// Ticks observed before asserting the cell count never moved and a
/// policy error was recorded.
const OBSERVATION_TICKS: u64 = 10;
const TICK_INTERVAL_SECS: u64 = 1;
const SAMPLE_STEP: Duration = Duration::from_millis(300);

#[test]
fn malformed_policy_override_never_wedges_or_scales_the_cluster() {
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_defraburner"));
    let tmp = tempfile::tempdir().expect("tempdir");
    let data_root = tmp.path().join("cluster");
    std::fs::create_dir_all(&data_root).expect("create data root");

    // Lay testonly-malformed's real, already-`burn compile`d .afb out
    // under a directory literally named `autoscale-default`, so
    // burner-policy's override scan (keyed by directory name, not the
    // archive's own namespace/name) replaces the embedded default with
    // it.
    let packages_dir = tmp.path().join("packages-override");
    let override_dir = packages_dir.join("autoscale-default");
    std::fs::create_dir_all(&override_dir).expect("create override dir");
    let malformed_afb = testonly_malformed_afb_path();
    std::fs::copy(&malformed_afb, override_dir.join("override.afb")).unwrap_or_else(|e| {
        panic!(
            "copying {} (run `just packages` first): {e}",
            malformed_afb.display()
        )
    });

    let schema_path = tmp.path().join("spike.graphql");
    std::fs::write(&schema_path, SDL).expect("write schema file");
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

    let gateway_port = free_tcp_port();
    let gateway_addr = format!("127.0.0.1:{gateway_port}");
    let base_url = format!("http://{gateway_addr}");
    let ready_file = data_root.join("ready.json");

    let mut child = Command::new(&binary)
        .arg("start")
        .arg("--data-root")
        .arg(&data_root)
        .arg("--cells")
        .arg("1")
        .arg("--tick-interval")
        .arg(TICK_INTERVAL_SECS.to_string())
        .arg("--base-port")
        .arg(free_tcp_port().to_string())
        .arg("--gateway-addr")
        .arg(&gateway_addr)
        .arg("--ready-file")
        .arg(&ready_file)
        .arg("--packages-dir")
        .arg(&packages_dir)
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

    // --- the cluster still serves real GraphQL traffic ---------------------
    let add_response = ureq::post(&format!("{base_url}/api/v1/graphql"))
        .set("Authorization", &format!("Bearer {tenant_token}"))
        .send_json(serde_json::json!({
            "query": r#"mutation { add_Spike(input: {name: "still-serving"}) { _docID name } }"#
        }))
        .expect("POST /api/v1/graphql (add_Spike) should succeed despite the broken policy");
    assert_eq!(add_response.status(), 200);
    let add_json: serde_json::Value = add_response.into_json().expect("valid JSON");
    assert!(
        add_json.get("errors").is_none(),
        "add_Spike returned errors: {add_json}"
    );

    // --- observe for OBSERVATION_TICKS worth of time: cell count never ----
    // --- moves, and a policy error eventually shows up ---------------------
    let initial_cell_count = admin_status(&base_url, &admin_token)["cells"]
        .as_array()
        .map(Vec::len)
        .expect("cells array");
    assert_eq!(initial_cell_count, 1);

    let window = Duration::from_secs(OBSERVATION_TICKS * TICK_INTERVAL_SECS + 3);
    let deadline = Instant::now() + window;
    let mut saw_policy_error = false;
    while Instant::now() < deadline {
        let status = admin_status(&base_url, &admin_token);
        let cell_count = status["cells"]
            .as_array()
            .map(Vec::len)
            .unwrap_or(usize::MAX);
        assert_eq!(
            cell_count, initial_cell_count,
            "cell count must never change while the autoscale policy is malformed, got {status:?}"
        );
        if status["policy"]["consecutive_errors"].as_u64().unwrap_or(0) > 0 {
            saw_policy_error = true;
        }
        // Nothing crashed.
        assert!(
            child.try_wait().expect("try_wait").is_none(),
            "defraburner should still be running under a malformed policy"
        );
        std::thread::sleep(SAMPLE_STEP);
    }

    assert!(
        saw_policy_error,
        "expected /admin/status policy.consecutive_errors > 0 within the observation window"
    );
    let final_status = admin_status(&base_url, &admin_token);
    assert!(
        final_status["policy"]["last_error"].is_string(),
        "expected policy.last_error to carry a message: {final_status:?}"
    );

    send_sigterm(&child);
    let status = wait_for_exit(&mut child, EXIT_DEADLINE);
    assert!(
        status.success(),
        "defraburner should still exit cleanly on SIGTERM, got {status:?}"
    );
}

#[test]
fn corrupt_wasm_override_makes_start_exit_nonzero_with_a_loud_error() {
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_defraburner"));
    let tmp = tempfile::tempdir().expect("tempdir");
    let data_root = tmp.path().join("cluster");
    std::fs::create_dir_all(&data_root).expect("create data root");

    let packages_dir = tmp.path().join("packages-override");
    let override_dir = packages_dir.join("autoscale-default");
    std::fs::create_dir_all(&override_dir).expect("create override dir");
    // A genuinely truncated/corrupt file: not valid zstd, let alone a
    // valid tar-of-a-precompiled-package.
    std::fs::write(
        override_dir.join("truncated.afb"),
        b"not a real afb archive, deliberately corrupt",
    )
    .expect("write a corrupt .afb");

    let output = Command::new(&binary)
        .arg("start")
        .arg("--data-root")
        .arg(&data_root)
        .arg("--cells")
        .arg("1")
        .arg("--base-port")
        .arg(free_tcp_port().to_string())
        .arg("--gateway-addr")
        .arg(format!("127.0.0.1:{}", free_tcp_port()))
        .arg("--packages-dir")
        .arg(&packages_dir)
        .output()
        .expect("run `defraburner start`");

    assert!(
        !output.status.success(),
        "start should exit nonzero when a --packages-dir override is corrupt"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.to_lowercase().contains("policy"),
        "stderr should carry an actionable message naming the policy load failure, got: {stderr}"
    );
    assert!(
        !ready_file_exists(&data_root),
        "no ready-file should ever be written when startup fails this early"
    );
}

fn ready_file_exists(data_root: &Path) -> bool {
    data_root.join("ready.json").exists()
}

fn admin_status(base_url: &str, admin_token: &str) -> serde_json::Value {
    ureq::get(&format!("{base_url}/admin/status"))
        .set("Authorization", &format!("Bearer {admin_token}"))
        .call()
        .expect("GET /admin/status should succeed")
        .into_json()
        .expect("valid JSON")
}

/// The real, already-`burn compile`d `.afb` for `packages/testonly-malformed`,
/// found by extension rather than a hardcoded versioned filename.
fn testonly_malformed_afb_path() -> PathBuf {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/testonly-malformed");
    std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("reading {} (run `just packages` first): {e}", dir.display()))
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .find(|path| path.extension().and_then(|ext| ext.to_str()) == Some("afb"))
        .unwrap_or_else(|| {
            panic!(
                "no .afb file under {} (run `just packages` first)",
                dir.display()
            )
        })
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
