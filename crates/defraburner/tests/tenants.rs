//! Phase 2 gate test (in-process): tenant placement, schema application,
//! and group wiring converge a write across a tenant's replica group, and
//! stay disjoint from every other cell (D14): a cell never assigned to the
//! tenant never sees its data.

use std::net::TcpListener;
use std::time::Duration;

use burner_cell::{
    BackendKind, CellSpec, ClusterManifest, DEFAULT_MEM_BUDGET_BYTES, Supervisor, TenantSpec,
    TenantStatus,
};
use tokio::time::Instant;

const SDL: &str = "type Spike { name: String }";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tenant_write_converges_within_group_and_stays_disjoint() {
    let dir = tempfile::tempdir().expect("tempdir");
    let data_root = dir.path().to_path_buf();

    let mut supervisor = Supervisor::new(&data_root);
    for i in 0..3 {
        let id = format!("cell-{i}");
        let port = free_tcp_port();
        supervisor
            .provision(CellSpec {
                signing_key_file: burner_cell::identity::key_path(&data_root, &id),
                id: id.clone(),
                group: "default".to_string(),
                backend: BackendKind::Lark,
                p2p_port: port,
                bind_addr: "127.0.0.1".parse().unwrap(),
                mem_budget_bytes: DEFAULT_MEM_BUDGET_BYTES,
            })
            .await
            .unwrap_or_else(|error| panic!("provisioning '{id}' failed: {error}"));
    }

    // Create the tenant via the manifest API path directly (write the SDL
    // file, append a Pending TenantSpec, save), not through the CLI: this
    // test is in-process, so it drives the same API `tenant create` does.
    let mut manifest = ClusterManifest::load(&data_root)
        .await
        .expect("load manifest after provisioning cells");
    let sdl_path = burner_mesh::tenant_sdl_path(&data_root, "acme-co");
    tokio::fs::create_dir_all(sdl_path.parent().unwrap())
        .await
        .expect("create tenants dir");
    tokio::fs::write(&sdl_path, SDL)
        .await
        .expect("write tenant SDL");
    manifest.tenants.push(TenantSpec {
        name: "acme-co".to_string(),
        replicas: 2,
        cells: Vec::new(),
        token_sha256: String::new(),
        status: TenantStatus::Pending,
        admission: None,
        health: Default::default(),
    });
    manifest
        .save(&data_root)
        .await
        .expect("save manifest with pending tenant");

    let outcomes = burner_mesh::reconcile(&mut supervisor, &data_root)
        .await
        .expect("reconcile should place and wire the tenant");
    assert_eq!(outcomes.len(), 1);
    let tenant_ready = match &outcomes[0] {
        burner_mesh::TenantOutcome::Ready(ready) => ready,
        burner_mesh::TenantOutcome::Degraded { name, reason } => {
            panic!("expected tenant '{name}' to reconcile cleanly, got degraded: {reason}")
        }
    };
    assert_eq!(tenant_ready.name, "acme-co");
    assert_eq!(
        tenant_ready.cells.len(),
        2,
        "replicas: 2 should place 2 cells"
    );
    assert_eq!(tenant_ready.collections, vec!["Spike".to_string()]);

    let unassigned_cell_id = supervisor
        .cell_ids()
        .into_iter()
        .find(|id| !tenant_ready.cells.contains(id))
        .expect("exactly one of the 3 cells should be unassigned");

    let first = &tenant_ready.cells[0];
    let second = &tenant_ready.cells[1];
    let first_node = supervisor
        .node_handle(first)
        .expect("first assigned cell should be running");
    let second_node = supervisor
        .node_handle(second)
        .expect("second assigned cell should be running");
    let unassigned_node = supervisor
        .node_handle(&unassigned_cell_id)
        .expect("unassigned cell should still be running");

    let response = first_node
        .execute(r#"mutation { add_Spike(input: {name: "hello"}) { _docID } }"#)
        .await;
    assert!(
        !response.has_errors(),
        "add_Spike on '{first}' returned errors: {:?}",
        response.errors
    );

    // Convergence on the second assigned cell: deadline plus bounded step,
    // never a bare fixed sleep.
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let response = second_node.execute("query { Spike { name } }").await;
        assert!(
            !response.has_errors(),
            "query on '{second}' returned errors: {:?}",
            response.errors
        );
        if response_contains_name(&response, "hello") {
            break;
        }
        if Instant::now() >= deadline {
            panic!("tenant write did not converge onto the second assigned cell within 20s");
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // Disjointness (D14): the unassigned cell must never see this
    // tenant's data. A bounded negative window cannot prove absence for
    // all time in general (a bug that merely delays delivery would look
    // identical to true absence at any fixed point), but it is meaningful
    // here specifically because there is no delivery mechanism at all
    // between the unassigned cell and the tenant's group: `wire_group`
    // ran with exactly the two assigned cells, so the unassigned cell was
    // never `connect_peer`'d, `add_collections`'d, or schema'd for
    // "Spike" in the first place. A positive hit after this wait could
    // only come from a real placement/wiring bug (e.g. a third cell
    // wired in error), never from ordinary replication latency, so a
    // short bound (well under the 20s convergence deadline used for the
    // real replication path above) is sufficient, not a compromise on
    // rigor.
    tokio::time::sleep(Duration::from_secs(3)).await;
    let response = unassigned_node.execute("query { Spike { name } }").await;
    assert!(
        !response_contains_name(&response, "hello"),
        "disjointness violated: unassigned cell '{unassigned_cell_id}' has tenant data \
         (response: {:?}, errors: {:?})",
        response.data,
        response.errors
    );

    supervisor.shutdown_all().await;
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

/// Binds an ephemeral OS-assigned TCP port and immediately releases it,
/// for use as a (best-effort, not reserved) free p2p_port; mirrors
/// `tests/recovery.rs`'s helper of the same name.
fn free_tcp_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}
