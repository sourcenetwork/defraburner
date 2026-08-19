//! Phase 2 gate test: live interop smoke against the real Go DefraDB
//! checkout at `~/projects/defradb-go`. Starts the Go binary (building it
//! with the vendored Go toolchain if no prebuilt binary exists), drives it
//! purely over its HTTP API, then pulls one document from it into one of
//! OUR cells via the explicit sync path (`connect_peer` +
//! `add_collections` + `sync_documents`, D5's explicit path -- mirroring
//! Phase 1 recovery re-wiring, not the pubsub-subscription path, so this
//! test is not also exercising D13's topic-ready wait).
//!
//! Fails loud, never skips silently: if neither a prebuilt `defradb`
//! binary nor a working Go toolchain is found, or if the P2P handshake
//! itself fails (e.g. real wire version skew between defradb.rs's
//! v1.0.0-rc1 compatibility target and the local Go `develop` checkout),
//! this test fails with the exact cause in the panic message rather than
//! being marked passing or skipped.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use burner_cell::{BackendKind, CellSpec, DEFAULT_MEM_BUDGET_BYTES, Supervisor};

const SDL: &str = "type Spike { name: String }";
const READY_MARKER: &str = "Providing HTTP API at ";
const GO_READY_DEADLINE: Duration = Duration::from_secs(30);
const SYNC_DEADLINE: Duration = Duration::from_secs(30);
/// Fallback Go toolchain checked when `go` is missing from PATH (per the
/// plan's Phase 2 gate: "also check defradb.rs/.tooling for a go
/// toolchain").
const VENDORED_GO: &str = "/home/vcq/projects/defradb.rs/.tooling/go/bin/go";
const GO_REPO: &str = "/home/vcq/projects/defradb-go";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn go_interop_pulls_a_document_via_explicit_sync() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let go_binary = find_or_build_go_binary(tmp.path());

    let go_rootdir = tmp.path().join("go-node");
    std::fs::create_dir_all(&go_rootdir).expect("create go node rootdir");
    let (mut go_child, base_url) = spawn_go_node(&go_binary, &go_rootdir);

    go_add_schema(&base_url, SDL);
    let go_doc_id = go_create_doc(
        &base_url,
        r#"mutation { add_Spike(input: {name: "go-interop"}) { _docID name } }"#,
    );
    let go_addrs = go_p2p_info(&base_url);
    let go_dialable_addr = go_addrs
        .iter()
        .find(|addr| addr.contains("/p2p/"))
        .unwrap_or_else(|| panic!("go node's p2p/info returned no /p2p/ address: {go_addrs:?}"));

    // One of OUR cells, built in-process (no manifest/tenant machinery
    // needed for this smoke test).
    let data_root = tmp.path().join("rust-cell");
    let mut supervisor = Supervisor::new(&data_root);
    let port = free_tcp_port();
    supervisor
        .provision(CellSpec {
            signing_key_file: burner_cell::identity::key_path(&data_root, "cell-0"),
            id: "cell-0".to_string(),
            group: "default".to_string(),
            backend: BackendKind::Lark,
            p2p_port: port,
            bind_addr: "127.0.0.1".parse().unwrap(),
            mem_budget_bytes: DEFAULT_MEM_BUDGET_BYTES,
        })
        .await
        .expect("provision our cell");

    let node = supervisor
        .node_handle("cell-0")
        .expect("cell-0 should be running");
    node.add_schema(SDL).await.expect("add_schema on our cell");

    let p2p = node.p2p().expect("our cell has a p2p system").clone();

    // A real wire-compatibility failure here (version skew between
    // defradb.rs's v1.0.0-rc1 compat target and the local Go `develop`
    // checkout) is a finding, not a bug to paper over: the panic message
    // carries the upstream error verbatim, and this test is correctly
    // failing, not skipping, in that case.
    p2p.ops()
        .connect_peer(go_dialable_addr)
        .await
        .map_err(|error| anyhow::anyhow!(error))
        .unwrap_or_else(|error| {
            panic!("connect_peer from our cell to the go node ({go_dialable_addr}) failed: {error}")
        });

    p2p.ops()
        .add_collections(vec!["Spike".to_string()])
        .await
        .map_err(|error| anyhow::anyhow!(error))
        .unwrap_or_else(|error| panic!("add_collections on our cell failed: {error}"));

    p2p.ops()
        .sync_documents("Spike", vec![go_doc_id.clone()], Some(SYNC_DEADLINE))
        .await
        .map_err(|error| anyhow::anyhow!(error))
        .unwrap_or_else(|error| {
            panic!("sync_documents pulling '{go_doc_id}' from the go node failed: {error}")
        });

    let deadline = Instant::now() + SYNC_DEADLINE;
    loop {
        let response = node.execute("query { Spike { name } }").await;
        assert!(
            !response.has_errors(),
            "query on our cell returned errors: {:?}",
            response.errors
        );
        if response_contains_name(&response, "go-interop") {
            break;
        }
        if Instant::now() >= deadline {
            panic!(
                "document '{go_doc_id}' created on the go node did not arrive on our cell \
                 within {SYNC_DEADLINE:?} of sync_documents"
            );
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    supervisor.shutdown_all().await;
    let _ = go_child.kill();
    let _ = go_child.wait();
}

fn response_contains_name(response: &query::QueryResponse, name: &str) -> bool {
    response
        .data
        .as_ref()
        .and_then(|data| data.get("Spike"))
        .and_then(|docs| docs.as_array())
        .map(|docs| {
            docs.iter()
                .any(|doc| doc.get("name").and_then(|v| v.as_str()) == Some(name))
        })
        .unwrap_or(false)
}

/// Discovery order (Phase 2 plan): (1) an existing build at the repo root
/// or `build/`, (2) build it with a working Go toolchain (PATH, else the
/// vendored one). Fails loud (panics naming the missing prerequisite) if
/// neither is available: this test never skips silently.
fn find_or_build_go_binary(scratch_dir: &Path) -> PathBuf {
    let repo = Path::new(GO_REPO);
    for candidate in [repo.join("defradb"), repo.join("build").join("defradb")] {
        if candidate.is_file() {
            tracing::info!(path = %candidate.display(), "using prebuilt go defradb binary");
            return candidate;
        }
    }

    let Some(go) = resolve_go_binary() else {
        panic!(
            "go_interop test requires either a prebuilt `defradb` binary at {} or {}, or a \
             working Go toolchain; found neither `go` on PATH nor {VENDORED_GO}",
            repo.join("defradb").display(),
            repo.join("build/defradb").display(),
        );
    };

    let output_path = scratch_dir.join("defradb");
    let status = Command::new(&go)
        .arg("build")
        .arg("-o")
        .arg(&output_path)
        .arg("./cmd/defradb")
        .current_dir(repo)
        .status()
        .unwrap_or_else(|error| panic!("launching `{} build` failed: {error}", go.display()));
    assert!(
        status.success(),
        "`{} build -o {} ./cmd/defradb` (cwd {}) exited with {status:?}",
        go.display(),
        output_path.display(),
        repo.display(),
    );
    output_path
}

/// `go` on PATH if it works, else the vendored toolchain at
/// [`VENDORED_GO`], else `None`.
fn resolve_go_binary() -> Option<PathBuf> {
    if Command::new("go")
        .arg("version")
        .output()
        .is_ok_and(|out| out.status.success())
    {
        return Some(PathBuf::from("go"));
    }
    let fallback = PathBuf::from(VENDORED_GO);
    fallback.is_file().then_some(fallback)
}

/// Spawns `defradb start` with ephemeral HTTP and P2P ports and waits for
/// its `"Providing HTTP API at "` stdout line, returning the child and the
/// parsed base URL (e.g. `http://127.0.0.1:54321`).
fn spawn_go_node(binary: &Path, rootdir: &Path) -> (Child, String) {
    let stdout_path = rootdir.join("stdout.log");
    let stderr_path = rootdir.join("stderr.log");
    let stdout_file = File::create(&stdout_path).expect("create go node stdout.log");
    let stderr_file = File::create(&stderr_path).expect("create go node stderr.log");

    let mut child = Command::new(binary)
        .arg("start")
        .arg("--rootdir")
        .arg(rootdir)
        .arg("--url")
        .arg("127.0.0.1:0")
        .arg("--p2paddr")
        .arg("/ip4/127.0.0.1/tcp/0")
        .arg("--no-keyring")
        .arg("--development")
        .arg("--no-telemetry")
        .stdout(Stdio::from(stdout_file))
        .stderr(Stdio::from(stderr_file))
        .spawn()
        .unwrap_or_else(|error| panic!("spawning `{}` failed: {error}", binary.display()));

    let base_url =
        wait_for_http_api_line(&stdout_path, &stderr_path, &mut child, GO_READY_DEADLINE);
    (child, base_url)
}

fn wait_for_http_api_line(
    stdout_path: &Path,
    stderr_path: &Path,
    child: &mut Child,
    deadline: Duration,
) -> String {
    let start = Instant::now();
    loop {
        // corelog (defradb-go's structured logger,
        // github.com/sourcenetwork/corelog@v0.0.9 handler.go:62-67)
        // defaults to stderr, not stdout, so the ready line has to be
        // looked for on both streams.
        let stdout = std::fs::read_to_string(stdout_path).unwrap_or_default();
        let stderr = std::fs::read_to_string(stderr_path).unwrap_or_default();
        for stream in [&stdout, &stderr] {
            if let Some(pos) = stream.find(READY_MARKER) {
                let after = &stream[pos + READY_MARKER.len()..];
                if let Some(url) = after.split_whitespace().next() {
                    return url.to_string();
                }
            }
        }
        if let Some(status) = child.try_wait().expect("try_wait") {
            panic!(
                "go defradb exited unexpectedly ({status:?}) before printing its HTTP API \
                 address; stdout:\n{stdout}\nstderr:\n{stderr}"
            );
        }
        if start.elapsed() >= deadline {
            let _ = child.kill();
            panic!(
                "timed out after {deadline:?} waiting for go defradb's HTTP API address; \
                 stdout so far:\n{stdout}\nstderr so far:\n{stderr}"
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn go_add_schema(base_url: &str, sdl: &str) {
    let response = ureq::post(&format!("{base_url}/api/v0/collections")).send_string(sdl);
    match response {
        Ok(response) => assert_eq!(
            response.status(),
            200,
            "go node rejected the schema (unexpected non-200 with an Ok response)"
        ),
        Err(error) => panic!("POST /api/v0/collections to the go node failed: {error}"),
    }
}

fn go_create_doc(base_url: &str, mutation: &str) -> String {
    let body = serde_json::json!({ "query": mutation });
    let response = ureq::post(&format!("{base_url}/api/v0/graphql"))
        .send_json(body)
        .unwrap_or_else(|error| panic!("POST /api/v0/graphql to the go node failed: {error}"));
    let json: serde_json::Value = response
        .into_json()
        .expect("go node graphql response should be valid JSON");
    if let Some(errors) = json.get("errors") {
        panic!("go node add_Spike mutation returned errors: {errors}");
    }
    extract_doc_id(&json)
        .unwrap_or_else(|| panic!("go node add_Spike response missing _docID: {json}"))
}

/// Extracts `_docID` from an `add_Spike` GraphQL response, tolerant of
/// either an array-wrapped result (this codebase's own dialect elsewhere,
/// e.g. `tests/tenants.rs`) or a single object, in case the Go dialect's
/// exact response shape differs in a way unrelated to the P2P wire
/// compatibility this test actually exercises.
fn extract_doc_id(json: &serde_json::Value) -> Option<String> {
    let add_spike = json.get("data")?.get("add_Spike")?;
    let doc = match add_spike.as_array() {
        Some(items) => items.first()?,
        None => add_spike,
    };
    doc.get("_docID")?.as_str().map(str::to_string)
}

fn go_p2p_info(base_url: &str) -> Vec<String> {
    let response = ureq::get(&format!("{base_url}/api/v0/p2p/info"))
        .call()
        .unwrap_or_else(|error| panic!("GET /api/v0/p2p/info from the go node failed: {error}"));
    response
        .into_json()
        .unwrap_or_else(|error| panic!("go node p2p/info response should be a JSON array: {error}"))
}

/// Binds an ephemeral OS-assigned TCP port and immediately releases it,
/// for use as a (best-effort, not reserved) free p2p_port.
fn free_tcp_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}
