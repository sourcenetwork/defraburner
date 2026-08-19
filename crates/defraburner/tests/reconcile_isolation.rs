//! Bug-fix round (D25 addendum) regression tests: `burner_mesh::reconcile`
//! isolates each tenant's own outcome from every other tenant's (bug 1),
//! treats an already-wired group as an idempotent ENSURE rather than
//! re-waiting on an event that cannot fire (bug 2), and degrades a
//! tenant whose assigned cell is not actually running instead of hanging
//! wiring on a dead peer (bug 3). In-process (real provisioned cells, a
//! hand-written manifest, `reconcile` called directly), the same pattern
//! `tests/tenants.rs` already establishes: faster and more precisely
//! targeted at `reconcile`'s own logic than spawning a full binary.

use std::path::Path;
use std::time::{Duration, Instant};

use burner_cell::{
    BackendKind, CellSpec, ClusterManifest, DEFAULT_MEM_BUDGET_BYTES, Supervisor, TenantHealth,
    TenantSpec, TenantStatus,
};

const SDL: &str = "type Spike { name: String }";

fn free_tcp_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

async fn provision_real_cell(supervisor: &mut Supervisor, data_root: &Path, id: &str) {
    supervisor
        .provision(CellSpec {
            signing_key_file: burner_cell::identity::key_path(data_root, id),
            id: id.to_string(),
            group: "default".to_string(),
            backend: BackendKind::Lark,
            p2p_port: free_tcp_port(),
            bind_addr: "127.0.0.1".parse().unwrap(),
            mem_budget_bytes: DEFAULT_MEM_BUDGET_BYTES,
        })
        .await
        .unwrap_or_else(|error| panic!("provisioning real cell '{id}' failed: {error}"));
}

/// A `CellSpec` entry for a cell id that is recorded in the manifest (so
/// `ClusterManifest::validate` is satisfied) but never actually
/// provisioned in any `Supervisor` -- simulating "a cell that is no
/// longer running" (bug 3) without needing to corrupt real cell data or
/// abort `Supervisor::recover` entirely (which aborts the *whole*
/// recovery on any single cell's ignition failure, the wrong shape for
/// testing one tenant's isolated degradation).
fn ghost_cell_spec(id: &str, data_root: &Path) -> CellSpec {
    CellSpec {
        signing_key_file: burner_cell::identity::key_path(data_root, id),
        id: id.to_string(),
        group: "default".to_string(),
        backend: BackendKind::Lark,
        p2p_port: free_tcp_port(),
        bind_addr: "127.0.0.1".parse().unwrap(),
        mem_budget_bytes: DEFAULT_MEM_BUDGET_BYTES,
    }
}

fn pending_tenant(name: &str, replicas: u8) -> TenantSpec {
    TenantSpec {
        name: name.to_string(),
        replicas,
        cells: Vec::new(),
        token_sha256: String::new(),
        status: TenantStatus::Pending,
        admission: None,
        health: TenantHealth::default(),
    }
}

async fn write_tenant_sdl(data_root: &Path, name: &str) {
    let sdl_path = burner_mesh::tenant_sdl_path(data_root, name);
    tokio::fs::create_dir_all(sdl_path.parent().unwrap())
        .await
        .expect("create tenants dir");
    tokio::fs::write(&sdl_path, SDL)
        .await
        .expect("write tenant SDL");
}

/// Bug 1 (and bug 3, from the `Placed` re-wiring side): two tenants, A and
/// B, both genuinely placed and wired; B is then broken (its manifest
/// assignment is edited to name a cell that was never provisioned, i.e.
/// no longer running) without touching A. A third tenant, C, is then
/// added and reconciled in the SAME pass: creating C must still succeed,
/// A must remain untouched, and B must show up degraded, naming the
/// missing cell -- never a 500-equivalent abort of the whole pass over
/// an unrelated tenant's problem.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_tenants_broken_wiring_never_blocks_another_tenants_creation() {
    let dir = tempfile::tempdir().expect("tempdir");
    let data_root = dir.path().to_path_buf();

    let mut supervisor = Supervisor::new(&data_root);
    provision_real_cell(&mut supervisor, &data_root, "cell-0").await;
    provision_real_cell(&mut supervisor, &data_root, "cell-1").await;
    provision_real_cell(&mut supervisor, &data_root, "cell-2").await;
    provision_real_cell(&mut supervisor, &data_root, "cell-3").await;

    write_tenant_sdl(&data_root, "tenant-a").await;
    write_tenant_sdl(&data_root, "tenant-b").await;

    let mut manifest = ClusterManifest::load(&data_root).await.unwrap();
    manifest.tenants.push(pending_tenant("tenant-a", 1));
    manifest.tenants.push(pending_tenant("tenant-b", 2));
    manifest.save(&data_root).await.unwrap();

    // First pass: both genuinely placed and wired for real.
    let first = burner_mesh::reconcile(&mut supervisor, &data_root)
        .await
        .expect("first reconcile pass should succeed");
    for outcome in &first {
        assert!(
            matches!(outcome, burner_mesh::TenantOutcome::Ready(_)),
            "'{}' should have placed and wired cleanly on the first pass: {outcome:?}",
            outcome.name()
        );
    }

    write_tenant_sdl(&data_root, "tenant-c").await;

    // Break tenant-b: swap one of its two real cells for a ghost id that
    // is recorded in the manifest but never provisioned in `supervisor`.
    let mut manifest = ClusterManifest::load(&data_root).await.unwrap();
    let tenant_b = manifest
        .tenants
        .iter_mut()
        .find(|t| t.name == "tenant-b")
        .unwrap();
    let surviving_cell = tenant_b.cells[0].clone();
    tenant_b.cells = vec![surviving_cell, "cell-ghost".to_string()];
    manifest
        .cells
        .push(ghost_cell_spec("cell-ghost", &data_root));
    manifest.tenants.push(pending_tenant("tenant-c", 1));
    manifest
        .save(&data_root)
        .await
        .expect("manifest with the ghost cell and tenant-c should still validate");

    let second = burner_mesh::reconcile(&mut supervisor, &data_root)
        .await
        .expect("reconcile itself must not abort just because one tenant is broken");

    let by_name = |name: &str| {
        second
            .iter()
            .find(|o| o.name() == name)
            .unwrap_or_else(|| panic!("'{name}' missing from the outcome list"))
    };

    assert!(
        matches!(by_name("tenant-c"), burner_mesh::TenantOutcome::Ready(_)),
        "creating tenant-c must succeed despite tenant-b being broken: {:?}",
        by_name("tenant-c")
    );
    assert!(
        matches!(by_name("tenant-a"), burner_mesh::TenantOutcome::Ready(_)),
        "tenant-a must be entirely unaffected by tenant-b's breakage: {:?}",
        by_name("tenant-a")
    );
    match by_name("tenant-b") {
        burner_mesh::TenantOutcome::Degraded { reason, .. } => {
            assert!(
                reason.contains("cell-ghost") && reason.contains("not running"),
                "tenant-b's degraded reason should name the missing cell: {reason}"
            );
        }
        other => panic!("expected tenant-b to be degraded, got {other:?}"),
    }

    // The degraded health is genuinely persisted, not just returned in
    // memory: /admin/api/overview reads it straight off the manifest.
    let manifest = ClusterManifest::load(&data_root).await.unwrap();
    let tenant_b = manifest
        .tenants
        .iter()
        .find(|t| t.name == "tenant-b")
        .unwrap();
    match &tenant_b.health {
        TenantHealth::Degraded { reason, .. } => assert!(reason.contains("cell-ghost")),
        TenantHealth::Ok => {
            panic!("tenant-b's degraded health should be persisted in the manifest")
        }
    }
    let tenant_c = manifest
        .tenants
        .iter()
        .find(|t| t.name == "tenant-c")
        .unwrap();
    assert_eq!(tenant_c.health, TenantHealth::Ok);

    supervisor.shutdown_all().await;
}

/// Bug 2: re-reconciling an already-wired, unchanged group must be fast
/// (well under the 15s topic-join deadline) and must not re-wait on a
/// subscription this process already confirmed. Two replicas so there is
/// genuine cross-cell wiring to redo, not a trivially-fast single-cell
/// no-op.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn re_reconciling_an_already_wired_group_does_not_re_wait() {
    let dir = tempfile::tempdir().expect("tempdir");
    let data_root = dir.path().to_path_buf();

    let mut supervisor = Supervisor::new(&data_root);
    provision_real_cell(&mut supervisor, &data_root, "cell-0").await;
    provision_real_cell(&mut supervisor, &data_root, "cell-1").await;
    write_tenant_sdl(&data_root, "tenant-a").await;

    let mut manifest = ClusterManifest::load(&data_root).await.unwrap();
    manifest.tenants.push(pending_tenant("tenant-a", 2));
    manifest.save(&data_root).await.unwrap();

    let first = burner_mesh::reconcile(&mut supervisor, &data_root)
        .await
        .expect("first reconcile pass should genuinely place and wire the group");
    assert!(matches!(first[0], burner_mesh::TenantOutcome::Ready(_)));

    // Nothing changed; this mirrors what an unrelated tenant's own
    // create/drop now triggers (a full reconcile pass over every
    // tenant, tenant-a included) every time it happens.
    let start = Instant::now();
    let second = burner_mesh::reconcile(&mut supervisor, &data_root)
        .await
        .expect("re-reconciling an unchanged, already-wired group should succeed");
    let elapsed = start.elapsed();

    assert!(
        matches!(second[0], burner_mesh::TenantOutcome::Ready(_)),
        "re-reconcile should still report the group ready: {:?}",
        second[0]
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "a re-reconcile of an already-confirmed group should be near-instant \
         (no re-wait on an event that cannot fire), took {elapsed:?}"
    );

    supervisor.shutdown_all().await;
}

/// Bug 3, from the `Pending` placement side: a tenant whose assignment
/// names a cell that was never provisioned (the exact "cell is no longer
/// running" shape, constructed directly rather than via a corrupted
/// recovery) is reported degraded, naming the missing cell -- and a
/// second, genuinely fine tenant in the very same pass is entirely
/// unaffected.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_tenant_assigned_to_a_cell_that_is_not_running_is_degraded_by_name() {
    let dir = tempfile::tempdir().expect("tempdir");
    let data_root = dir.path().to_path_buf();

    let mut supervisor = Supervisor::new(&data_root);
    provision_real_cell(&mut supervisor, &data_root, "cell-0").await;
    write_tenant_sdl(&data_root, "tenant-broken").await;
    write_tenant_sdl(&data_root, "tenant-fine").await;

    let mut manifest = ClusterManifest::load(&data_root).await.unwrap();
    manifest
        .cells
        .push(ghost_cell_spec("cell-missing", &data_root));
    // Pre-assigned (not Pending-with-empty-cells): `placement::place` is
    // idempotent and returns an already-populated `cells` unchanged, so
    // this exercises `resolve_running_cells` failing during *initial*
    // placement, the `reconcile_pending` code path -- distinct from the
    // other test's `reconcile_placed` (re-wiring) path.
    let mut broken = pending_tenant("tenant-broken", 1);
    broken.cells = vec!["cell-missing".to_string()];
    manifest.tenants.push(broken);
    manifest.tenants.push(pending_tenant("tenant-fine", 1));
    manifest
        .save(&data_root)
        .await
        .expect("manifest referencing the not-yet-running cell should still validate");

    let outcomes = burner_mesh::reconcile(&mut supervisor, &data_root)
        .await
        .expect("reconcile itself must not abort over one tenant's missing cell");

    let broken_outcome = outcomes
        .iter()
        .find(|o| o.name() == "tenant-broken")
        .unwrap();
    match broken_outcome {
        burner_mesh::TenantOutcome::Degraded { reason, .. } => {
            assert!(
                reason.contains("cell-missing") && reason.contains("not running"),
                "reason should name the missing cell: {reason}"
            );
        }
        other => panic!("expected tenant-broken to be degraded, got {other:?}"),
    }
    let fine_outcome = outcomes.iter().find(|o| o.name() == "tenant-fine").unwrap();
    assert!(
        matches!(fine_outcome, burner_mesh::TenantOutcome::Ready(_)),
        "an unrelated, genuinely placeable tenant in the same pass must not be blocked: {fine_outcome:?}"
    );

    // The broken tenant must stay Pending (never silently re-placed onto
    // a different cell on its own -- the operator decides that, via the
    // existing controls), with its cells assignment untouched.
    let manifest = ClusterManifest::load(&data_root).await.unwrap();
    let tenant_broken = manifest
        .tenants
        .iter()
        .find(|t| t.name == "tenant-broken")
        .unwrap();
    assert_eq!(tenant_broken.status, TenantStatus::Pending);
    assert_eq!(tenant_broken.cells, vec!["cell-missing".to_string()]);

    supervisor.shutdown_all().await;
}
