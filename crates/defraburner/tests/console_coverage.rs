//! Completeness contract (operator directive: "nothing must be half
//! wired, I want to control everything perfectly from the dashboard").
//! One const table is the single source of truth for every control-plane
//! capability this console claims to offer. Per row, three checks:
//!
//!   (a) the route pattern appears in the gateway's router source (read
//!       via `include_str!`, so the table cannot silently drift from the
//!       real routes -- a renamed or removed route fails this check, not
//!       a passing green test that no longer means anything);
//!   (b) a marker specific to that capability's own UI control appears in
//!       the shipped dashboard JS (so no endpoint ships without a UI
//!       path to reach it);
//!   (c) against one live spawned `defraburner up`, the endpoint answers
//!       with something other than 404/405 for the admin token (so a
//!       route is not merely referenced in source, it is actually
//!       mounted and reachable).
//!
//! A fourth, inverse check scans the shipped JS for every `adminFetch`/
//! `fetch` call against an `/admin/*` path and fails if that path has no
//! matching row in the table (catches the UI calling something the table
//! -- and therefore this test -- does not know about).

mod common;
use common::*;

// ===== Router sources (check a) ========================================
// Read via `include_str!` from this crate, not duplicated by hand: if a
// route is renamed in the gateway, this table's check (a) breaks at test
// time, not silently.
const GATEWAY_RS: &str = include_str!("../../burner-gateway/src/gateway.rs");
const ADMIN_CELLS_RS: &str = include_str!("../../burner-gateway/src/admin_cells.rs");
const ADMIN_TENANTS_RS: &str = include_str!("../../burner-gateway/src/admin_tenants.rs");
const ADMIN_AUTOSCALER_RS: &str = include_str!("../../burner-gateway/src/admin_autoscaler.rs");

fn router_source() -> String {
    [
        GATEWAY_RS,
        ADMIN_CELLS_RS,
        ADMIN_TENANTS_RS,
        ADMIN_AUTOSCALER_RS,
    ]
    .concat()
}

// ===== Shipped JS (checks b and the inverse scan) ======================
const DASHBOARD_JS_FILES: &[(&str, &str)] = &[
    (
        "core.js",
        include_str!("../../burner-dashboard/assets/core.js"),
    ),
    (
        "charts.js",
        include_str!("../../burner-dashboard/assets/charts.js"),
    ),
    (
        "main.js",
        include_str!("../../burner-dashboard/assets/main.js"),
    ),
    (
        "view-overview.js",
        include_str!("../../burner-dashboard/assets/view-overview.js"),
    ),
    (
        "view-cells.js",
        include_str!("../../burner-dashboard/assets/view-cells.js"),
    ),
    (
        "view-tenants.js",
        include_str!("../../burner-dashboard/assets/view-tenants.js"),
    ),
    (
        "view-autoscaler.js",
        include_str!("../../burner-dashboard/assets/view-autoscaler.js"),
    ),
    (
        "view-mesh.js",
        include_str!("../../burner-dashboard/assets/view-mesh.js"),
    ),
    (
        "view-console.js",
        include_str!("../../burner-dashboard/assets/view-console.js"),
    ),
];

fn dashboard_js() -> String {
    DASHBOARD_JS_FILES
        .iter()
        .map(|(_, content)| *content)
        .collect::<Vec<_>>()
        .concat()
}

/// State a probe may need: the live gateway, the admin token, and one
/// already-running fixture cell/tenant every probe can read without
/// standing up its own (a mutating probe that needs a *disposable*
/// cell/tenant provisions its own instead of touching the fixture).
struct ProbeCtx {
    base_url: String,
    admin_token: String,
    fixture_cell_id: String,
    fixture_tenant: String,
    fixture_tenant_token: String,
}

struct Row {
    /// The route as it appears in the gateway's router source (axum's
    /// curly-brace param style), or -- for a capability with no fixed
    /// route of its own (the tenant data plane is a transparent proxy;
    /// "runtime"/"autoscaler get" are fields inside the overview
    /// response, not their own endpoint) -- a distinctive source
    /// substring proving the mechanism that serves it exists.
    path_pattern: &'static str,
    /// A literal substring proving a specific UI control reaches this
    /// capability, not just that its URL is spelled somewhere.
    js_marker: &'static str,
    human_name: &'static str,
    /// Calls the capability against a live spawned binary; returns the
    /// resulting HTTP status code.
    probe: fn(&ProbeCtx) -> u16,
}

fn get(url: &str, token: &str) -> u16 {
    match ureq::get(url)
        .set("Authorization", &format!("Bearer {token}"))
        .call()
    {
        Ok(response) => response.status(),
        Err(ureq::Error::Status(status, _)) => status,
        Err(other) => panic!("request to {url} failed outright (not even an HTTP status): {other}"),
    }
}
/// Reads `response`'s body as text, then tries to parse it as JSON; a
/// non-JSON body (a plain-text 500, the common shape every
/// `internal_error`/`bad_request` helper in this codebase returns) is
/// preserved as a JSON string rather than silently collapsing to `Null`
/// -- losing the real diagnostic text is exactly what made an earlier
/// failure in this same test file look like an opaque `Null` instead of
/// the real, actionable error body.
fn body_value(response: ureq::Response) -> serde_json::Value {
    let text = response.into_string().unwrap_or_default();
    serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text))
}
fn post_json(url: &str, token: &str, body: serde_json::Value) -> (u16, serde_json::Value) {
    match ureq::post(url)
        .set("Authorization", &format!("Bearer {token}"))
        .send_json(body)
    {
        Ok(response) => {
            let status = response.status();
            (status, body_value(response))
        }
        Err(ureq::Error::Status(status, response)) => (status, body_value(response)),
        Err(other) => panic!("request to {url} failed outright (not even an HTTP status): {other}"),
    }
}
fn put_json(url: &str, token: &str, body: serde_json::Value) -> u16 {
    match ureq::put(url)
        .set("Authorization", &format!("Bearer {token}"))
        .send_json(body)
    {
        Ok(response) => response.status(),
        Err(ureq::Error::Status(status, _)) => status,
        Err(other) => panic!("request to {url} failed outright (not even an HTTP status): {other}"),
    }
}
fn delete(url: &str, token: &str) -> u16 {
    match ureq::delete(url)
        .set("Authorization", &format!("Bearer {token}"))
        .call()
    {
        Ok(response) => response.status(),
        Err(ureq::Error::Status(status, _)) => status,
        Err(other) => panic!("request to {url} failed outright (not even an HTTP status): {other}"),
    }
}

/// Creates a fresh, disposable, single-replica tenant named `name` on
/// whatever free cell is available, returning its `{name, token}` body.
/// Several probes need their own throwaway tenant so they do not
/// interfere with the shared fixture (e.g. drop/retire actually delete
/// the tenant they act on).
fn make_disposable_tenant(ctx: &ProbeCtx, name: &str) -> serde_json::Value {
    let (status, body) = post_json(
        &format!("{}/admin/tenants", ctx.base_url),
        &ctx.admin_token,
        serde_json::json!({ "name": name, "schema_sdl": "type Widget { name: String }", "replicas": 1 }),
    );
    assert_eq!(
        status, 201,
        "creating disposable tenant '{name}' should succeed: {body:?}"
    );
    body
}

/// Provisions one throwaway cell (so a mutating probe never risks the
/// fixture cell every other probe still needs), returning its id.
fn make_disposable_cell(ctx: &ProbeCtx) -> String {
    let (status, body) = post_json(
        &format!("{}/admin/cells", ctx.base_url),
        &ctx.admin_token,
        serde_json::json!({ "count": 1 }),
    );
    assert_eq!(
        status, 200,
        "provisioning a disposable cell should succeed: {body:?}"
    );
    // `POST /admin/cells` answers 200 with a per-cell outcome, so a
    // failed attempt arrives as `{"error": ...}` with no id rather than a
    // non-200. Surface that error text: a bare "expected an id" tells a
    // future reader nothing about why the cell could not be provisioned.
    body["cells"][0]["id"]
        .as_str()
        .unwrap_or_else(|| {
            panic!(
                "provisioning a disposable cell returned no id; outcome: {}",
                body["cells"][0]
            )
        })
        .to_string()
}

// ===== Probes (check c) =================================================

fn probe_cells_spawn(ctx: &ProbeCtx) -> u16 {
    let (status, _) = post_json(
        &format!("{}/admin/cells", ctx.base_url),
        &ctx.admin_token,
        serde_json::json!({ "count": 1 }),
    );
    status
}
fn probe_cells_drain(ctx: &ProbeCtx) -> u16 {
    let disposable = make_disposable_cell(ctx);
    delete(
        &format!("{}/admin/cells/{disposable}", ctx.base_url),
        &ctx.admin_token,
    )
}
fn probe_cells_inspect(ctx: &ProbeCtx) -> u16 {
    get(
        &format!(
            "{}/admin/cells/{}/inspect",
            ctx.base_url, ctx.fixture_cell_id
        ),
        &ctx.admin_token,
    )
}
fn probe_cells_dial(ctx: &ProbeCtx) -> u16 {
    let (status, _) = post_json(
        &format!("{}/admin/cells/{}/dial", ctx.base_url, ctx.fixture_cell_id),
        &ctx.admin_token,
        serde_json::json!({ "addr": "/ip4/127.0.0.1/tcp/1/p2p/12D3KooWCoverageProbeUnreachable" }),
    );
    status // dialing an unreachable bogus peer still 200s (ok:false in the body); only the route's own mounting is under test here.
}

fn probe_tenants_create(ctx: &ProbeCtx) -> u16 {
    let (status, _) = post_json(
        &format!("{}/admin/tenants", ctx.base_url),
        &ctx.admin_token,
        serde_json::json!({ "name": "cov-create-probe", "schema_sdl": "type Widget { name: String }", "replicas": 1 }),
    );
    status
}
fn probe_tenants_drop(ctx: &ProbeCtx) -> u16 {
    make_disposable_tenant(ctx, "cov-drop-probe");
    delete(
        &format!("{}/admin/tenants/cov-drop-probe", ctx.base_url),
        &ctx.admin_token,
    )
}
fn probe_tenants_drop_retire(ctx: &ProbeCtx) -> u16 {
    make_disposable_tenant(ctx, "cov-retire-probe");
    delete(
        &format!(
            "{}/admin/tenants/cov-retire-probe?retire=true",
            ctx.base_url
        ),
        &ctx.admin_token,
    )
}
fn probe_tenants_rotate_token(ctx: &ProbeCtx) -> u16 {
    // Its own disposable tenant, not the shared fixture: rotating the
    // fixture's token here would invalidate it out from under the later
    // graphql-data-plane probe, which still holds the token captured at
    // fixture-creation time.
    make_disposable_tenant(ctx, "cov-rotate-probe");
    let (status, _) = post_json(
        &format!(
            "{}/admin/tenants/cov-rotate-probe/rotate-token",
            ctx.base_url
        ),
        &ctx.admin_token,
        serde_json::Value::Null,
    );
    status
}
fn probe_tenants_admission_override(ctx: &ProbeCtx) -> u16 {
    put_json(
        &format!(
            "{}/admin/tenants/{}/admission",
            ctx.base_url, ctx.fixture_tenant
        ),
        &ctx.admin_token,
        serde_json::json!({ "rate_per_sec": 500, "burst": 500 }),
    )
}

fn probe_autoscaler_get(ctx: &ProbeCtx) -> u16 {
    get(
        &format!("{}/admin/api/overview", ctx.base_url),
        &ctx.admin_token,
    )
}
fn probe_autoscaler_update_knobs(ctx: &ProbeCtx) -> u16 {
    put_json(
        &format!("{}/admin/autoscaler", ctx.base_url),
        &ctx.admin_token,
        serde_json::json!({ "cooldown_secs": 61 }),
    )
}
fn probe_autoscaler_pause(ctx: &ProbeCtx) -> u16 {
    put_json(
        &format!("{}/admin/autoscaler", ctx.base_url),
        &ctx.admin_token,
        serde_json::json!({ "paused": true }),
    )
}
fn probe_autoscaler_resume(ctx: &ProbeCtx) -> u16 {
    put_json(
        &format!("{}/admin/autoscaler", ctx.base_url),
        &ctx.admin_token,
        serde_json::json!({ "paused": false }),
    )
}
fn probe_autoscaler_force_tick(ctx: &ProbeCtx) -> u16 {
    let (status, _) = post_json(
        &format!("{}/admin/autoscaler/tick", ctx.base_url),
        &ctx.admin_token,
        serde_json::Value::Null,
    );
    status
}

fn probe_overview(ctx: &ProbeCtx) -> u16 {
    get(
        &format!("{}/admin/api/overview", ctx.base_url),
        &ctx.admin_token,
    )
}
fn probe_stream(ctx: &ProbeCtx) -> u16 {
    // SSE never ends its body on its own; only the status/headers are
    // under test here, so the response (and its lazy body reader) is
    // dropped immediately without ever reading the stream, closing the
    // connection rather than blocking on it.
    match ureq::get(&format!("{}/admin/api/stream", ctx.base_url))
        .set("Authorization", &format!("Bearer {}", ctx.admin_token))
        .call()
    {
        Ok(response) => response.status(),
        Err(ureq::Error::Status(status, _)) => status,
        Err(other) => panic!("SSE probe failed outright: {other}"),
    }
}
fn probe_runtime_block(ctx: &ProbeCtx) -> u16 {
    let status = get(
        &format!("{}/admin/api/overview", ctx.base_url),
        &ctx.admin_token,
    );
    // Stronger than the generic floor: the runtime block is honestly
    // present, not merely a 200 that happens to omit it.
    if status == 200 {
        let body: serde_json::Value = ureq::get(&format!("{}/admin/api/overview", ctx.base_url))
            .set("Authorization", &format!("Bearer {}", ctx.admin_token))
            .call()
            .expect("overview should succeed")
            .into_json()
            .expect("valid JSON");
        assert!(
            body.get("runtime").is_some(),
            "overview response should carry a 'runtime' block"
        );
    }
    status
}

fn probe_tenant_data_plane_graphql(ctx: &ProbeCtx) -> u16 {
    match ureq::post(&format!("{}/api/v0/graphql", ctx.base_url))
        .set(
            "Authorization",
            &format!("Bearer {}", ctx.fixture_tenant_token),
        )
        .send_json(serde_json::json!({ "query": "query { __typename }" }))
    {
        Ok(response) => response.status(),
        Err(ureq::Error::Status(status, _)) => status,
        Err(other) => panic!("tenant data-plane graphql probe failed outright: {other}"),
    }
}

// ===== The table =========================================================

const TABLE: &[Row] = &[
    Row {
        path_pattern: "/admin/cells",
        js_marker: "cells-spawn",
        human_name: "cells: spawn",
        probe: probe_cells_spawn,
    },
    Row {
        path_pattern: "/admin/cells/{id}",
        js_marker: "data-drain-btn",
        human_name: "cells: drain",
        probe: probe_cells_drain,
    },
    Row {
        path_pattern: "/admin/cells/{id}/inspect",
        js_marker: "/inspect",
        human_name: "cells: inspect",
        probe: probe_cells_inspect,
    },
    Row {
        path_pattern: "/admin/cells/{id}/dial",
        js_marker: "data-dial-btn",
        human_name: "cells: dial",
        probe: probe_cells_dial,
    },
    Row {
        path_pattern: "/admin/tenants",
        js_marker: "tenant-create-form",
        human_name: "tenants: create",
        probe: probe_tenants_create,
    },
    Row {
        path_pattern: "/admin/tenants/{name}",
        js_marker: "data-drop",
        human_name: "tenants: drop",
        probe: probe_tenants_drop,
    },
    Row {
        path_pattern: "/admin/tenants/{name}",
        js_marker: "?retire=true",
        human_name: "tenants: drop with retire",
        probe: probe_tenants_drop_retire,
    },
    Row {
        path_pattern: "/admin/tenants/{name}/rotate-token",
        js_marker: "rotate-token",
        human_name: "tenants: rotate token",
        probe: probe_tenants_rotate_token,
    },
    Row {
        path_pattern: "/admin/tenants/{name}/admission",
        js_marker: "data-admission-save",
        human_name: "tenants: admission override",
        probe: probe_tenants_admission_override,
    },
    Row {
        path_pattern: "autoscaler_control: AutoscalerControlView",
        js_marker: "autoscaler_control",
        human_name: "autoscaler: get (via overview)",
        probe: probe_autoscaler_get,
    },
    Row {
        path_pattern: "/admin/autoscaler",
        js_marker: "autoscaler-save",
        human_name: "autoscaler: update knobs",
        probe: probe_autoscaler_update_knobs,
    },
    Row {
        path_pattern: "/admin/autoscaler",
        js_marker: "autoscaler-pause-toggle",
        human_name: "autoscaler: pause",
        probe: probe_autoscaler_pause,
    },
    Row {
        path_pattern: "/admin/autoscaler",
        js_marker: "autoscaler-pause-toggle",
        human_name: "autoscaler: resume",
        probe: probe_autoscaler_resume,
    },
    Row {
        path_pattern: "/admin/autoscaler/tick",
        js_marker: "autoscaler-force-tick",
        human_name: "autoscaler: force tick",
        probe: probe_autoscaler_force_tick,
    },
    Row {
        path_pattern: "/admin/api/overview",
        js_marker: "admin/api/overview",
        human_name: "overview",
        probe: probe_overview,
    },
    Row {
        path_pattern: "/admin/api/stream",
        js_marker: "admin/api/stream",
        human_name: "stream",
        probe: probe_stream,
    },
    Row {
        path_pattern: "runtime: Arc<RuntimeInfo>",
        js_marker: "registered_packages",
        human_name: "runtime block",
        probe: probe_runtime_block,
    },
    Row {
        path_pattern: "fallback(route_to_tenant)",
        js_marker: "/api/v0/graphql",
        human_name: "tenant data plane: graphql",
        probe: probe_tenant_data_plane_graphql,
    },
];

/// The completeness contract itself: every row's three checks, run
/// against one shared live binary (cheap-ish: one process for all 18
/// rows, not 18 processes) plus the inverse fetch-call scan.
#[test]
fn every_control_plane_capability_is_routed_wired_and_mounted() {
    let source = router_source();
    let js = dashboard_js();

    // Checks (a) and (b) need no live process: run them first so a
    // source/JS drift fails fast without paying for a binary spawn.
    let mut failures = Vec::new();
    for row in TABLE {
        if !source.contains(row.path_pattern) {
            failures.push(format!(
                "{}: route pattern '{}' not found in the gateway router source",
                row.human_name, row.path_pattern
            ));
        }
        if !js.contains(row.js_marker) {
            failures.push(format!(
                "{}: js marker '{}' not found in the shipped dashboard JS",
                row.human_name, row.js_marker
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "coverage table check (a)/(b) failures:\n{}",
        failures.join("\n")
    );

    // Inverse scan: every /admin/* path the JS actually fetches must be
    // covered by at least one row's pattern.
    let called_admin_paths = extract_admin_fetch_path_prefixes(&js);
    let mut uncovered = Vec::new();
    for path in &called_admin_paths {
        let trimmed = path.trim_end_matches('/');
        let covered = TABLE.iter().any(|row| {
            row.path_pattern.starts_with(trimmed) || trimmed.starts_with(row.path_pattern)
        });
        if !covered {
            uncovered.push(path.clone());
        }
    }
    assert!(
        uncovered.is_empty(),
        "the dashboard JS calls admin path(s) with no matching row in the coverage table: {uncovered:?}"
    );

    // Check (c): spin up one real binary and probe every row against it.
    let tmp = tempfile::tempdir().expect("tempdir");
    let data_root = tmp.path().join("data");
    let ready_file = tmp.path().join("ready.json");
    let mut child = spawn_up(
        &data_root,
        &ready_file,
        &[
            "--no-open",
            "--gateway-addr",
            "127.0.0.1:0",
            "--max-cells",
            "20",
        ],
    );
    wait_for_ready_file(&ready_file, &mut child, READY_DEADLINE);

    let admin_token = read_admin_token(&data_root);
    let base_url = read_gateway_base_url(&data_root);

    // Headroom for every tenant-creating probe below (the fixture tenant
    // plus cov-create/drop/retire/rotate-probe, five single-replica
    // tenants total): provisioned up front, before anything claims a
    // cell, so no probe run later in table order can starve on free
    // cells depending on how many earlier rows happened to consume.
    let (headroom_status, headroom_body) = post_json(
        &format!("{base_url}/admin/cells"),
        &admin_token,
        serde_json::json!({ "count": 6 }),
    );
    assert_eq!(
        headroom_status, 200,
        "provisioning headroom cells should succeed: {headroom_body:?}"
    );

    let fixture_cell_id = admin_status(&base_url, &admin_token)["cells"][0]["id"]
        .as_str()
        .expect("fixture cell id")
        .to_string();
    let fixture_tenant_body = make_disposable_tenant(
        &ProbeCtx {
            base_url: base_url.clone(),
            admin_token: admin_token.clone(),
            fixture_cell_id: fixture_cell_id.clone(),
            fixture_tenant: String::new(),
            fixture_tenant_token: String::new(),
        },
        "cov-fixture-tenant",
    );
    let ctx = ProbeCtx {
        base_url,
        admin_token,
        fixture_cell_id,
        fixture_tenant: "cov-fixture-tenant".to_string(),
        fixture_tenant_token: fixture_tenant_body["token"]
            .as_str()
            .expect("fixture tenant token")
            .to_string(),
    };

    let mut live_failures = Vec::new();
    for row in TABLE {
        let status = (row.probe)(&ctx);
        if status == 404 || status == 405 {
            live_failures.push(format!(
                "{}: live probe returned {status} (not mounted)",
                row.human_name
            ));
        }
    }
    assert!(
        live_failures.is_empty(),
        "coverage table check (c) failures:\n{}",
        live_failures.join("\n")
    );

    send_sigterm(&child);
    wait_for_exit(&mut child, EXIT_DEADLINE);
}

/// Visual pass, TESTS requirement: the mesh panel's own data dependencies
/// (per-cell tenant assignment, connected_peers, static peer outcomes)
/// are genuinely present in a live `/admin/api/overview` response, not
/// just referenced by name in the panel's own JS (that source-level half
/// is `burner_dashboard`'s own
/// `mesh_panel_javascript_reads_the_fields_its_live_counterpart_test_verifies_the_backend_sends`,
/// which cannot reach a live server since that crate has no dependency
/// on burner-gateway). Creates one real tenant so `cell_details[].tenant`
/// has something non-trivial to report, not just an all-free cluster.
#[test]
fn overview_payload_carries_every_field_the_mesh_panel_needs() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let data_root = tmp.path().join("data");
    let ready_file = tmp.path().join("ready.json");
    let mut child = spawn_up(
        &data_root,
        &ready_file,
        &["--no-open", "--gateway-addr", "127.0.0.1:0"],
    );
    wait_for_ready_file(&ready_file, &mut child, READY_DEADLINE);

    let admin_token = read_admin_token(&data_root);
    let base_url = read_gateway_base_url(&data_root);
    create_tenant(
        &base_url,
        &admin_token,
        "cov-mesh-tenant",
        "type Widget { name: String }",
        1,
    );

    let overview = admin_status(&base_url, &admin_token);
    let cells = overview["cells"]
        .as_array()
        .expect("overview should carry a cells array");
    assert!(
        !cells.is_empty(),
        "a fresh cluster should have at least one cell"
    );
    for cell in cells {
        assert!(
            cell.get("connected_peers").is_some_and(|v| v.is_array()),
            "every cell should carry a connected_peers array: {cell:?}"
        );
        assert!(
            cell.get("peer_id").is_some_and(|v| v.is_string()),
            "every cell should carry its own peer_id: {cell:?}"
        );
    }

    let details = overview["cell_details"]
        .as_array()
        .expect("overview should carry cell_details");
    assert!(!details.is_empty());
    let assigned = details
        .iter()
        .any(|d| d.get("tenant").is_some_and(|t| !t.is_null()));
    assert!(
        assigned,
        "at least one cell_details entry should carry the tenant it is assigned to: {details:?}"
    );

    assert!(
        overview
            .get("static_peer_outcomes")
            .is_some_and(|v| v.is_array()),
        "overview should carry static_peer_outcomes (empty array when no --peers, never absent)"
    );
    assert!(
        overview.get("tenants").is_some_and(|v| v.is_array()),
        "overview should carry the tenants array the mesh panel clusters by"
    );

    send_sigterm(&child);
    wait_for_exit(&mut child, EXIT_DEADLINE);
}

/// Best-effort extraction of every literal (non-interpolated) prefix
/// passed to `fetch(`/`adminFetch(` that starts with `/admin/` -- not a
/// full JS template-literal parser (the least code that does the job:
/// this only needs to catch a call whose static prefix names a path,
/// which is exactly how every admin call site in this dashboard is
/// written).
fn extract_admin_fetch_path_prefixes(js: &str) -> Vec<String> {
    let mut paths = Vec::new();
    for marker in ["adminFetch(`", "adminFetch(\"", "fetch(`", "fetch(\""] {
        let mut cursor = 0usize;
        while let Some(found) = js[cursor..].find(marker) {
            let start = cursor + found + marker.len();
            let after = &js[start..];
            let end = ["${", "`", "\""]
                .iter()
                .filter_map(|delim| after.find(delim))
                .min()
                .unwrap_or(after.len());
            let literal = &after[..end];
            if literal.starts_with("/admin/") {
                paths.push(literal.to_string());
            }
            cursor = start + end.max(1);
        }
    }
    paths
}

#[cfg(test)]
mod unit_tests {
    use super::*;

    #[test]
    fn extract_admin_fetch_path_prefixes_finds_a_backtick_templated_call() {
        let js = "await B.adminFetch(`/admin/cells/${id}/dial`, {method:'POST'});";
        assert_eq!(extract_admin_fetch_path_prefixes(js), vec!["/admin/cells/"]);
    }

    #[test]
    fn extract_admin_fetch_path_prefixes_finds_a_plain_quoted_call() {
        let js = r#"fetch("/admin/api/overview", { headers });"#;
        assert_eq!(
            extract_admin_fetch_path_prefixes(js),
            vec!["/admin/api/overview"]
        );
    }

    #[test]
    fn extract_admin_fetch_path_prefixes_ignores_non_admin_paths() {
        let js = r#"fetch("/api/v0/graphql", { headers });"#;
        assert!(extract_admin_fetch_path_prefixes(js).is_empty());
    }

    #[test]
    fn table_rows_are_all_non_empty_and_distinct_by_human_name() {
        let mut seen = std::collections::HashSet::new();
        for row in TABLE {
            assert!(!row.path_pattern.is_empty());
            assert!(!row.js_marker.is_empty());
            assert!(
                seen.insert(row.human_name),
                "duplicate human_name '{}' in the table",
                row.human_name
            );
        }
    }
}
