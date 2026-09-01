//! Cell admin control surface (console round, D21/D23/D25): provision,
//! drain, introspect, and dial. Every mutation shares
//! `gateway::send_supervisor_command`; `GET .../inspect` is read-only and
//! never touches the command channel.

use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use burner_cell::{DrainCellError, SupervisorCommand};
use serde::{Deserialize, Serialize};

use crate::gateway::{
    GatewayState, bad_request, conflict, internal_error, is_valid_admin_token, not_found,
    publish_cell_change, send_supervisor_command, unauthorized,
};

/// `POST /admin/cells` is capped at this many cells per request: a
/// generous single-request batch, never unbounded (D25: "count 1..=8; >8
/// is a 400 naming the cap").
const MAX_PROVISION_COUNT: usize = 8;

pub(crate) fn router() -> Router<GatewayState> {
    Router::new()
        .route("/admin/cells", post(admin_provision_cells))
        .route("/admin/cells/{id}", delete(admin_drain_cell))
        .route("/admin/cells/{id}/inspect", get(admin_inspect_cell))
        .route("/admin/cells/{id}/dial", post(admin_dial_cell))
}

#[derive(Deserialize)]
struct ProvisionCellsRequest {
    count: usize,
}

#[derive(Serialize)]
struct ProvisionCellsResponse {
    cells: Vec<burner_cell::ProvisionOutcome>,
}

/// `POST /admin/cells {count}`: provisions `count` fresh cells one at a
/// time, reporting each attempt's outcome independently.
async fn admin_provision_cells(
    State(state): State<GatewayState>,
    headers: HeaderMap,
    Json(request): Json<ProvisionCellsRequest>,
) -> Response {
    if !is_valid_admin_token(&state, &headers) {
        return unauthorized("missing or invalid admin token");
    }
    if request.count == 0 {
        return bad_request("count must be at least 1");
    }
    if request.count > MAX_PROVISION_COUNT {
        return bad_request(&format!(
            "count {} exceeds the per-request cap of {MAX_PROVISION_COUNT}",
            request.count
        ));
    }

    let outcomes =
        match send_supervisor_command(&state, |reply| SupervisorCommand::ProvisionCells {
            count: request.count,
            reply,
        })
        .await
        {
            Ok(outcomes) => outcomes,
            Err(response) => return response,
        };

    {
        let supervisor = state.supervisor.lock().await;
        if let Err(error) = state.routing.rebuild(&state.data_root, &supervisor).await {
            tracing::error!(error = %error, "rebuilding routing table after cell provision failed");
        }
    }
    publish_cell_change(&state).await;

    Json(ProvisionCellsResponse { cells: outcomes }).into_response()
}

/// `DELETE /admin/cells/{id}`: drains and erases one free cell.
async fn admin_drain_cell(
    State(state): State<GatewayState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    if !is_valid_admin_token(&state, &headers) {
        return unauthorized("missing or invalid admin token");
    }

    let outcome = match send_supervisor_command(&state, |reply| SupervisorCommand::DrainCell {
        id: id.clone(),
        reply,
    })
    .await
    {
        Ok(outcome) => outcome,
        Err(response) => return response,
    };

    match outcome {
        Ok(()) => {
            publish_cell_change(&state).await;
            axum::http::StatusCode::OK.into_response()
        }
        Err(DrainCellError::NotFound) => not_found(&format!("cell '{id}' not found")),
        Err(DrainCellError::AssignedToTenant(tenant)) => conflict(&format!(
            "cell '{id}' is assigned to tenant '{tenant}'; refusing to remove an in-use cell"
        )),
        Err(DrainCellError::Failed(message)) => internal_error(&message),
    }
}

#[derive(Serialize)]
struct CellInspectResponse {
    id: String,
    collections: Vec<String>,
    listen_addrs: Vec<String>,
    connected_peers: Vec<String>,
    sync_status: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    transaction_stats: Option<serde_json::Value>,
    storage_bytes: u64,
    /// The cell's configured memory budget (D11), for the dashboard's
    /// storage gauge. Named plainly, not "storage_limit": this is a
    /// memory-cache-sizing knob, not a disk quota (v1 has no storage
    /// cap), so the gauge that divides `storage_bytes` by this is an
    /// informational reference, not a real capacity percentage: the UI
    /// says so rather than implying a hard ceiling that does not exist.
    mem_budget_bytes: u64,
    watchdog: burner_cell::CellHealth,
    marker_ok: bool,
}

/// `GET /admin/cells/{id}/inspect`: collections, listen addrs, connected
/// peers, sync status, transaction stats (when the backend tracks them),
/// storage bytes, watchdog counters, and the recovery marker's health.
/// Read-only; never touches the command channel.
async fn admin_inspect_cell(
    State(state): State<GatewayState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    if !is_valid_admin_token(&state, &headers) {
        return unauthorized("missing or invalid admin token");
    }

    // Only the node handle and the plain fields needed below are cloned
    // out while the lock is held; every subsequent `.await` (the P2P
    // queries) runs after it is released, so an inspect request never
    // blocks a concurrent admin mutation or another request's routing
    // lookup for the duration of those queries.
    let (node, listen_addrs, marker_ok, mem_budget_bytes) = {
        let supervisor = state.supervisor.lock().await;
        let Some(node) = supervisor.node_handle(&id) else {
            return not_found(&format!("cell '{id}' not found"));
        };
        let marker_ok = supervisor
            .status()
            .into_iter()
            .find(|status| status.id == id)
            .map(|status| status.marker_ok)
            .unwrap_or(false);
        let (listen_addrs, mem_budget_bytes) = supervisor
            .running_cell(&id)
            .map(|cell| (cell.listen_addrs.clone(), cell.spec.mem_budget_bytes))
            .unwrap_or_default();
        (node, listen_addrs, marker_ok, mem_budget_bytes)
    };

    let collections = burner_cell::cell::cell_collections(&node).unwrap_or_default();
    let transaction_stats = burner_cell::cell::cell_transaction_stats(&node);
    let (connected_peers, sync_status) = match node.p2p() {
        Some(p2p) => {
            let connected_peers = p2p.ops().connected_peers().await.unwrap_or_default();
            let sync_status = p2p
                .ops()
                .sync_status()
                .await
                .unwrap_or(serde_json::Value::Null);
            (connected_peers, sync_status)
        }
        None => (Vec::new(), serde_json::Value::Null),
    };
    let storage_bytes = burner_policy::snapshot::storage_bytes_for_cells(
        &state.data_root,
        std::slice::from_ref(&id),
    )
    .await
    .get(&id)
    .copied()
    .unwrap_or(0);
    let watchdog = state
        .watchdog
        .counters()
        .await
        .get(&id)
        .copied()
        .unwrap_or_default();

    Json(CellInspectResponse {
        id,
        collections,
        listen_addrs,
        connected_peers,
        sync_status,
        transaction_stats,
        storage_bytes,
        mem_budget_bytes,
        watchdog,
        marker_ok,
    })
    .into_response()
}

#[derive(Deserialize)]
struct DialRequest {
    addr: String,
}

#[derive(Serialize)]
struct DialResponse {
    cell_id: String,
    addr: String,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// `POST /admin/cells/{id}/dial {addr}`: dials a peer multiaddr (must
/// carry a `/p2p/<peer-id>` suffix) from the named cell.
async fn admin_dial_cell(
    State(state): State<GatewayState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(request): Json<DialRequest>,
) -> Response {
    if !is_valid_admin_token(&state, &headers) {
        return unauthorized("missing or invalid admin token");
    }

    let outcome = match send_supervisor_command(&state, |reply| SupervisorCommand::DialPeer {
        cell_id: id.clone(),
        addr: request.addr.clone(),
        reply,
    })
    .await
    {
        Ok(outcome) => outcome,
        Err(response) => return response,
    };

    match outcome {
        Ok(()) => Json(DialResponse {
            cell_id: id,
            addr: request.addr,
            ok: true,
            error: None,
        })
        .into_response(),
        Err(message) => Json(DialResponse {
            cell_id: id,
            addr: request.addr,
            ok: false,
            error: Some(message),
        })
        .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{StatusCode, header};
    use burner_cell::Supervisor;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    /// D25: a request over the per-request provision cap must 400 and
    /// name the cap, without ever touching the command channel (the
    /// dropped receiver would otherwise turn a wrongly-permitted request
    /// into a 503, masking the real bug this test is for).
    #[tokio::test]
    async fn provision_cells_over_the_cap_is_a_400_naming_the_cap() {
        let dir = tempfile::tempdir().unwrap();
        let (state, _command_rx) = crate::gateway::test_support::state(
            Arc::new(Mutex::new(Supervisor::new(dir.path()))),
            dir.path().to_path_buf(),
            "test-admin-token",
        );
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            "Bearer test-admin-token".parse().unwrap(),
        );

        let response = admin_provision_cells(
            State(state),
            headers,
            Json(ProvisionCellsRequest {
                count: MAX_PROVISION_COUNT + 1,
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body_text = String::from_utf8(body.to_vec()).unwrap();
        assert!(
            body_text.contains(&MAX_PROVISION_COUNT.to_string()),
            "400 body should name the cap: {body_text}"
        );
    }

    #[tokio::test]
    async fn provision_cells_of_zero_is_a_400() {
        let dir = tempfile::tempdir().unwrap();
        let (state, _command_rx) = crate::gateway::test_support::state(
            Arc::new(Mutex::new(Supervisor::new(dir.path()))),
            dir.path().to_path_buf(),
            "test-admin-token",
        );
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            "Bearer test-admin-token".parse().unwrap(),
        );

        let response = admin_provision_cells(
            State(state),
            headers,
            Json(ProvisionCellsRequest { count: 0 }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn inspect_of_an_unknown_cell_is_404() {
        let dir = tempfile::tempdir().unwrap();
        let (state, _command_rx) = crate::gateway::test_support::state(
            Arc::new(Mutex::new(Supervisor::new(dir.path()))),
            dir.path().to_path_buf(),
            "test-admin-token",
        );
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            "Bearer test-admin-token".parse().unwrap(),
        );

        let response =
            admin_inspect_cell(State(state), Path("ghost-cell".to_string()), headers).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    fn free_tcp_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    /// Completeness contract (operator directive): a successful inspect
    /// carries every documented key, not just the ones a particular
    /// backend happens to populate: proven against a real, ignited
    /// cell in-process (mirrors `admin_tenants.rs`'s own
    /// `admin_create_tenant_rolls_back_on_reconcile_failure`, the
    /// established pattern for a real-cell test here), not a mock.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn inspect_of_a_real_cell_carries_every_documented_key() {
        let dir = tempfile::tempdir().unwrap();
        let data_root = dir.path().to_path_buf();

        let mut supervisor = Supervisor::new(&data_root);
        supervisor
            .provision(burner_cell::CellSpec {
                signing_key_file: burner_cell::identity::key_path(&data_root, "cell-0"),
                id: "cell-0".to_string(),
                group: "default".to_string(),
                backend: burner_cell::BackendKind::Regolith,
                p2p_port: free_tcp_port(),
                bind_addr: "127.0.0.1".parse().unwrap(),
                mem_budget_bytes: burner_cell::DEFAULT_MEM_BUDGET_BYTES,
            })
            .await
            .expect("provision a real cell");

        let admin_token = "test-admin-token";
        let supervisor = Arc::new(Mutex::new(supervisor));
        let (state, _command_rx) =
            crate::gateway::test_support::state(supervisor.clone(), data_root, admin_token);
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            format!("Bearer {admin_token}").parse().unwrap(),
        );

        let response = admin_inspect_cell(State(state), Path("cell-0".to_string()), headers).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

        for key in [
            "id",
            "collections",
            "listen_addrs",
            "connected_peers",
            "sync_status",
            "storage_bytes",
            "mem_budget_bytes",
            "watchdog",
            "marker_ok",
        ] {
            assert!(
                json.get(key).is_some(),
                "inspect response missing documented key '{key}': {json}"
            );
        }
        assert_eq!(json["id"], "cell-0");
        assert_eq!(json["marker_ok"], true);
        // transaction_stats is `skip_serializing_if = "Option::is_none"`,
        // so its honest absence (not every backend tracks it) is itself
        // the documented contract, not a missing key.

        supervisor.lock().await.shutdown_all().await;
    }

    #[tokio::test]
    async fn every_admin_cells_endpoint_requires_the_admin_token() {
        let dir = tempfile::tempdir().unwrap();
        let (state, _command_rx) = crate::gateway::test_support::state(
            Arc::new(Mutex::new(Supervisor::new(dir.path()))),
            dir.path().to_path_buf(),
            "test-admin-token",
        );

        let response = admin_provision_cells(
            State(state.clone()),
            HeaderMap::new(),
            Json(ProvisionCellsRequest { count: 1 }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        let response = admin_drain_cell(
            State(state.clone()),
            Path("cell-0".to_string()),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        let response = admin_inspect_cell(
            State(state.clone()),
            Path("cell-0".to_string()),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        let response = admin_dial_cell(
            State(state),
            Path("cell-0".to_string()),
            HeaderMap::new(),
            Json(DialRequest {
                addr: "/ip4/127.0.0.1/tcp/9171/p2p/12D3Koo".to_string(),
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
}
