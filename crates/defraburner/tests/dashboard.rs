//! Phase 5 gate test: the embedded dashboard shell serves from the real
//! binary, its data API enforces the same admin-token auth as
//! `/admin/*`, and its SSE stream delivers a real, parseable event.

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

const READY_DEADLINE: Duration = Duration::from_secs(60);
const READY_POLL_STEP: Duration = Duration::from_millis(250);
const EXIT_DEADLINE: Duration = Duration::from_secs(30);
const EXIT_POLL_STEP: Duration = Duration::from_millis(100);
const SSE_READ_DEADLINE: Duration = Duration::from_secs(15);
/// Safety cap on lines read while looking for the first `data:` line, so a
/// stream that sends many keep-alive comments still cannot loop forever.
const SSE_MAX_LINES: u32 = 200;

#[test]
fn dashboard_shell_overview_and_stream_all_work_against_the_real_binary() {
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_defraburner"));
    let tmp = tempfile::tempdir().expect("tempdir");
    let data_root = tmp.path().join("cluster");
    std::fs::create_dir_all(&data_root).expect("create data root");

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
        .arg("--base-port")
        .arg(free_tcp_port().to_string())
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

    // --- GET /dashboard: the shell, no auth, no data -----------------------
    let dashboard_response = ureq::get(&format!("{base_url}/dashboard"))
        .call()
        .expect("GET /dashboard should succeed with no auth");
    assert_eq!(dashboard_response.status(), 200);
    assert!(
        dashboard_response
            .header("Content-Type")
            .unwrap_or_default()
            .contains("text/html"),
        "expected a text/html content type"
    );
    let dashboard_html = dashboard_response.into_string().expect("valid UTF-8 body");
    assert!(
        dashboard_html.contains("defraburner"),
        "shell should name defraburner"
    );
    assert!(
        dashboard_html.to_lowercase().contains("no data yet"),
        "shell should render the honest no-data marker before any live data arrives"
    );

    // --- GET /admin/api/overview: 401 without token, 200 with -------------
    let unauthorized = ureq::get(&format!("{base_url}/admin/api/overview")).call();
    match unauthorized {
        Ok(response) => panic!("expected 401 without a token, got {}", response.status()),
        Err(ureq::Error::Status(401, _)) => {}
        Err(other) => panic!("expected 401 without a token, got {other}"),
    }

    let overview_response = ureq::get(&format!("{base_url}/admin/api/overview"))
        .set("Authorization", &format!("Bearer {admin_token}"))
        .call()
        .expect("GET /admin/api/overview should succeed with the admin token");
    assert_eq!(overview_response.status(), 200);
    let overview: serde_json::Value = overview_response.into_json().expect("valid JSON");
    assert!(
        overview.get("cells").is_some(),
        "overview missing 'cells': {overview}"
    );
    assert!(
        overview.get("tenants").is_some(),
        "overview missing 'tenants': {overview}"
    );
    assert!(
        overview.get("policy").is_some(),
        "overview missing 'policy': {overview}"
    );

    // --- GET /admin/api/stream: a real, parseable SSE event ----------------
    let agent = ureq::AgentBuilder::new()
        .timeout_read(SSE_READ_DEADLINE)
        .build();
    let stream_response = agent
        .get(&format!("{base_url}/admin/api/stream"))
        .set("Authorization", &format!("Bearer {admin_token}"))
        .call()
        .expect("GET /admin/api/stream should succeed");
    assert_eq!(stream_response.status(), 200);

    let mut reader = BufReader::new(stream_response.into_reader());
    let mut data_line = None;
    for _ in 0..SSE_MAX_LINES {
        let mut line = String::new();
        let bytes_read = reader
            .read_line(&mut line)
            .expect("reading an SSE line within the read timeout");
        if bytes_read == 0 {
            break; // EOF: unexpected, but stop rather than loop forever
        }
        if let Some(rest) = line.strip_prefix("data:") {
            data_line = Some(rest.trim().to_string());
            break;
        }
    }
    let data_line =
        data_line.expect("expected a 'data:' line from /admin/api/stream within the deadline");
    let parsed: serde_json::Value = serde_json::from_str(&data_line).unwrap_or_else(|error| {
        panic!("SSE data line should be valid JSON ({error}): {data_line}")
    });
    assert!(
        parsed.get("tick").is_some(),
        "expected a 'tick' field in the SSE event: {parsed}"
    );
    drop(reader); // disconnects the SSE client, releasing its capacity slot

    // --- clean shutdown -----------------------------------------------------
    send_sigterm(&child);
    let status = wait_for_exit(&mut child, EXIT_DEADLINE);
    assert!(
        status.success(),
        "defraburner should exit cleanly on SIGTERM, got {status:?}"
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
