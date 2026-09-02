//! Phase 3 gate test: end-to-end GraphQL through the gateway against a
//! placed tenant (created via the real CLI flow), wrong-token rejection,
//! admission overload with `Retry-After`, `/admin/status`, and a
//! gateway-overhead measurement (direct `node.execute` vs via-gateway
//! HTTP, p50 over ~50 sequential queries each).

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use burner_cell::{BackendKind, CellSpec, DEFAULT_MEM_BUDGET_BYTES, Supervisor};

const SDL: &str = "type Spike { name: String }";
const READY_DEADLINE: Duration = Duration::from_secs(60);
const READY_POLL_STEP: Duration = Duration::from_millis(250);
const EXIT_DEADLINE: Duration = Duration::from_secs(30);
const EXIT_POLL_STEP: Duration = Duration::from_millis(100);
/// Default admission is 200 rps / burst 100
/// (`burner_gateway::admission::DEFAULT_*`). Fired concurrently (see the
/// overload section below for why), 150 requests against a burst of 100
/// leaves ~50 requests of headroom past the burst regardless of machine
/// speed, since a concurrent burst's admitted count is governed by
/// `burst`, not by how fast the client can drive a request/response
/// round trip.
const OVERLOAD_REQUESTS: u32 = 150;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gateway_end_to_end() {
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
        .arg("2")
        .output()
        .expect("run `defraburner tenant create`");
    assert!(
        create_output.status.success(),
        "tenant create failed: {}",
        String::from_utf8_lossy(&create_output.stderr)
    );
    let create_stdout = String::from_utf8_lossy(&create_output.stdout);
    let tenant_token = extract_token(&create_stdout, "tenant spike-co token")
        .expect("tenant create should print a token line");

    // --- start: 2 cells, gateway on an ephemeral port -------------------
    let gateway_port = free_tcp_port();
    let gateway_addr = format!("127.0.0.1:{gateway_port}");
    let base_url = format!("http://{gateway_addr}");
    let ready_file = data_root.join("ready.json");
    let base_port = pick_base_port();
    let mut child = spawn_start(&binary, &data_root, &ready_file, base_port, &gateway_addr);
    wait_for_ready_file(&ready_file, &mut child, READY_DEADLINE);

    let admin_token = std::fs::read_to_string(data_root.join("admin.token"))
        .expect("read admin.token")
        .trim()
        .to_string();

    // --- (a) GraphQL through the gateway with the tenant token ----------
    let add_response = ureq::post(&format!("{base_url}/api/v1/graphql"))
        .set("Authorization", &format!("Bearer {tenant_token}"))
        .send_json(serde_json::json!({
            "query": r#"mutation { add_Spike(input: {name: "via-gateway"}) { _docID name } }"#
        }))
        .expect("POST /api/v1/graphql (add_Spike) should succeed");
    assert_eq!(add_response.status(), 200);
    let add_json: serde_json::Value = add_response.into_json().expect("valid JSON");
    assert!(
        add_json.get("errors").is_none(),
        "add_Spike returned errors: {add_json}"
    );

    let query_response = ureq::post(&format!("{base_url}/api/v1/graphql"))
        .set("Authorization", &format!("Bearer {tenant_token}"))
        .send_json(serde_json::json!({ "query": "query { Spike { name } }" }))
        .expect("POST /api/v1/graphql (query) should succeed");
    let query_json: serde_json::Value = query_response.into_json().expect("valid JSON");
    assert!(
        response_contains_name(&query_json, "via-gateway"),
        "query result missing the doc just written: {query_json}"
    );

    // --- (e) gateway overhead: measured now, before overload burns the --
    // --- tenant's admission budget below (measurement only, no ----------
    // --- threshold assertion beyond both completing) --------------------
    let direct_p50 = measure_direct_p50(tmp.path()).await;
    let gateway_p50 = measure_gateway_p50(&base_url, &tenant_token);
    println!("GATE_MS direct_p50={direct_p50:.3} gateway_p50={gateway_p50:.3}");

    // --- (b) wrong token -> 401 ------------------------------------------
    let wrong_token_result = ureq::post(&format!("{base_url}/api/v1/graphql"))
        .set("Authorization", "Bearer not-a-real-token")
        .send_json(serde_json::json!({ "query": "query { Spike { name } }" }));
    match wrong_token_result {
        Ok(response) => panic!("expected 401 for a wrong token, got {}", response.status()),
        Err(ureq::Error::Status(401, _)) => {}
        Err(other) => panic!("expected 401 for a wrong token, got {other}"),
    }

    // --- (d) admin status lists the tenant -------------------------------
    let status_response = ureq::get(&format!("{base_url}/admin/status"))
        .set("Authorization", &format!("Bearer {admin_token}"))
        .call()
        .expect("GET /admin/status should succeed");
    assert_eq!(status_response.status(), 200);
    let status_json: serde_json::Value = status_response.into_json().expect("valid JSON");
    let tenant_names: Vec<&str> = status_json["tenants"]
        .as_array()
        .expect("tenants array in /admin/status response")
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    assert!(
        tenant_names.contains(&"spike-co"),
        "admin status should list the tenant: {status_json}"
    );

    // --- (c) overload: concurrent requests past burst, assert some 429s -
    // --- with Retry-After ---------------------------------------------------
    //
    // Fired CONCURRENTLY (`thread::scope`), not in a sequential loop. A
    // strictly serial client can never sustain more than roughly
    // 1/round-trip-time requests per second, and on this real path (real
    // GraphQL parse, a real embedded defradb query, real HTTP framing)
    // `gateway_p50` measured just above lands within a millisecond or two
    // of `period_ns` at the default 200 rps (5ms) -- observed as low as
    // 3.0ms and as high as 5.2ms across runs on this machine. A serial
    // loop at 5.2ms/request never exceeds the 200 rps limit at all, so it
    // can NEVER trip the limiter no matter how many requests it sends;
    // even at 3.0ms/request the ~2ms/request margin only accumulates
    // ~400ms of the 500ms burst window across a 150-request loop, missing
    // by a hair. This was a real, reproducible test bug (confirmed by
    // rerunning the old sequential version standalone, not just under
    // gate load): it does not indicate admission is broken -- the GCRA
    // math itself is covered against a synthetic clock in
    // `burner_gateway::admission::tests`. Firing every request at once
    // sidesteps the client's own round-trip time entirely: they land
    // within a single short window (bounded by the server's own
    // concurrency, not by waiting for each response in turn), so
    // `burst`'s ~100-request headroom is what gates admission, exactly
    // as intended, regardless of hardware speed. It also better matches
    // what "overload" means for a rate limiter: a real concurrent burst,
    // not a slow serial trickle.
    enum OverloadOutcome {
        Allowed,
        Rejected { retry_after: bool },
        UnexpectedStatus(u16),
        Transport(String),
    }
    let results: Vec<OverloadOutcome> = thread::scope(|scope| {
        let handles: Vec<_> = (0..OVERLOAD_REQUESTS)
            .map(|_| {
                scope.spawn(|| {
                    let result = ureq::post(&format!("{base_url}/api/v1/graphql"))
                        .set("Authorization", &format!("Bearer {tenant_token}"))
                        .send_json(serde_json::json!({ "query": "query { Spike { name } }" }));
                    match result {
                        Ok(_) => OverloadOutcome::Allowed,
                        Err(ureq::Error::Status(429, response)) => OverloadOutcome::Rejected {
                            retry_after: response.header("Retry-After").is_some(),
                        },
                        Err(ureq::Error::Status(code, _)) => {
                            OverloadOutcome::UnexpectedStatus(code)
                        }
                        Err(ureq::Error::Transport(transport)) => {
                            OverloadOutcome::Transport(transport.to_string())
                        }
                    }
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| {
                handle
                    .join()
                    .expect("overload request thread should not panic")
            })
            .collect()
    });
    for outcome in &results {
        match outcome {
            OverloadOutcome::UnexpectedStatus(code) => {
                panic!("an overload request got an unexpected status {code}")
            }
            OverloadOutcome::Transport(message) => {
                panic!("an overload request hit a transport error: {message}")
            }
            OverloadOutcome::Allowed | OverloadOutcome::Rejected { .. } => {}
        }
    }
    let rejected = results
        .iter()
        .filter(|o| matches!(o, OverloadOutcome::Rejected { .. }))
        .count();
    let saw_retry_after = results
        .iter()
        .any(|o| matches!(o, OverloadOutcome::Rejected { retry_after: true }));
    assert!(
        rejected > 0,
        "expected at least one 429 under overload ({OVERLOAD_REQUESTS} requests, burst 100), got 0"
    );
    assert!(
        saw_retry_after,
        "429 responses should carry a Retry-After header"
    );

    // --- clean shutdown ---------------------------------------------------
    send_sigterm(&child);
    let status = wait_for_exit(&mut child, EXIT_DEADLINE);
    assert!(
        status.success(),
        "gateway process should exit cleanly on SIGTERM, got {status:?}"
    );
}

/// Finds the line starting with `prefix` and returns its last
/// whitespace-delimited token (the printed token itself).
fn extract_token(output: &str, prefix: &str) -> Option<String> {
    output
        .lines()
        .find(|line| line.starts_with(prefix))
        .and_then(|line| line.rsplit(' ').next())
        .map(str::to_string)
}

fn response_contains_name(json: &serde_json::Value, name: &str) -> bool {
    json["data"]["Spike"]
        .as_array()
        .map(|docs| {
            docs.iter()
                .any(|doc| doc.get("name").and_then(|v| v.as_str()) == Some(name))
        })
        .unwrap_or(false)
}

/// Builds one throwaway in-process cell (separate from the spawned
/// binary's cluster; the gateway's cells have no HTTP surface of their
/// own to compare against directly, only via the gateway), schemas it the
/// same way, and measures `node.execute` p50 over 50 sequential queries.
async fn measure_direct_p50(tmp_path: &Path) -> f64 {
    let data_root = tmp_path.join("direct-node");
    let mut supervisor = Supervisor::new(&data_root);
    let port = free_tcp_port();
    supervisor
        .provision(CellSpec {
            signing_key_file: burner_cell::identity::key_path(&data_root, "direct-cell"),
            id: "direct-cell".to_string(),
            group: "default".to_string(),
            backend: BackendKind::Regolith,
            p2p_port: port,
            bind_addr: "127.0.0.1".parse().unwrap(),
            mem_budget_bytes: DEFAULT_MEM_BUDGET_BYTES,
        })
        .await
        .expect("provision direct-comparison cell");

    let node = supervisor
        .node_handle("direct-cell")
        .expect("direct-cell should be running");
    node.add_schema(SDL)
        .await
        .expect("add_schema on direct cell");
    let seed = node
        .execute(r#"mutation { add_Spike(input: {name: "direct"}) { _docID } }"#)
        .await;
    assert!(
        !seed.has_errors(),
        "seed mutation errored: {:?}",
        seed.errors
    );

    let mut samples = Vec::with_capacity(50);
    for _ in 0..50 {
        let start = Instant::now();
        let response = node.execute("query { Spike { name } }").await;
        assert!(
            !response.has_errors(),
            "direct query errored: {:?}",
            response.errors
        );
        samples.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    supervisor.shutdown_all().await;
    percentile_50(&mut samples)
}

/// Measures via-gateway HTTP p50 over 50 sequential queries.
fn measure_gateway_p50(base_url: &str, token: &str) -> f64 {
    let mut samples = Vec::with_capacity(50);
    for _ in 0..50 {
        let start = Instant::now();
        let response = ureq::post(&format!("{base_url}/api/v1/graphql"))
            .set("Authorization", &format!("Bearer {token}"))
            .send_json(serde_json::json!({ "query": "query { Spike { name } }" }))
            .expect("gateway query should succeed during p50 measurement");
        assert_eq!(response.status(), 200);
        samples.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    percentile_50(&mut samples)
}

fn percentile_50(samples: &mut [f64]) -> f64 {
    samples.sort_by(|a, b| a.partial_cmp(b).expect("latency samples are finite"));
    samples[samples.len() / 2]
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

fn spawn_start(
    binary: &Path,
    data_root: &Path,
    ready_file: &Path,
    base_port: u16,
    gateway_addr: &str,
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
        .arg(gateway_addr)
        .arg("--ready-file")
        .arg(ready_file)
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .spawn()
        .expect("spawn `defraburner start`")
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

/// Picks a base port for a 2-cell cluster: two OS-assigned free ports if
/// they happen to be adjacent (the fast path, then both confirmed free),
/// else a pseudo-random high port (the caller's own port-bind failure, if
/// any, would surface loudly rather than silently, per `gateway::build`'s
/// fail-loud bind behavior).
fn pick_base_port() -> u16 {
    let a = std::net::TcpListener::bind("127.0.0.1:0").expect("bind probe port a");
    let b = std::net::TcpListener::bind("127.0.0.1:0").expect("bind probe port b");
    let port_a = a.local_addr().expect("local_addr a").port();
    let port_b = b.local_addr().expect("local_addr b").port();
    drop(a);
    drop(b);
    if port_b == port_a + 1 {
        return port_a;
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    20000 + ((nanos ^ std::process::id()) % 20000) as u16
}
