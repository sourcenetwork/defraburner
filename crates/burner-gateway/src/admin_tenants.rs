//! Tenant admin control surface (console round, D23/D25): create (Phase 3,
//! unchanged, relocated here), drop (plain or `?retire=true`), rotate
//! token, and set a per-tenant admission override. Every mutation shares
//! `gateway::send_supervisor_command`; `admin_create_tenant` is the one
//! exception (unchanged from Phase 3): it never reaches `cell::ignite`
//! (tenants are placed onto cells that already exist), so it stays a
//! direct handler exactly as it always has.

use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, post, put};
use axum::{Json, Router};
use burner_cell::{
    AdmissionOverride, ClusterManifest, SupervisorCommand, TenantCommandError, TenantSpec,
    TenantStatus, is_valid_tenant_name,
};
use serde::{Deserialize, Serialize};

use crate::auth;
use crate::gateway::{
    GatewayState, bad_request, internal_error, is_valid_admin_token, not_found,
    publish_cell_change, send_supervisor_command, unauthorized,
};

pub(crate) fn router() -> Router<GatewayState> {
    Router::new()
        .route("/admin/tenants", post(admin_create_tenant))
        .route("/admin/tenants/{name}", delete(admin_drop_tenant))
        .route(
            "/admin/tenants/{name}/rotate-token",
            post(admin_rotate_tenant_token),
        )
        .route(
            "/admin/tenants/{name}/admission",
            put(admin_set_tenant_admission),
        )
        .route(
            "/admin/tenants/{name}/collections",
            post(admin_add_tenant_collections),
        )
}

#[derive(Deserialize)]
struct CreateTenantRequest {
    name: String,
    schema_sdl: String,
    #[serde(default = "default_replicas")]
    replicas: u8,
}

fn default_replicas() -> u8 {
    2
}

#[derive(Serialize)]
struct CreateTenantResponse {
    name: String,
    token: String,
}

/// Live tenant provisioning: writes the SDL file, appends a `Pending`
/// `TenantSpec` (with a freshly issued token already attached), saves the
/// manifest, then runs the same place+schema+wire pass `start`'s own
/// reconcile does (`burner_mesh::reconcile`) so the tenant is placed and
/// wired before this handler returns, and rebuilds the routing table so
/// the returned token is routable immediately.
async fn admin_create_tenant(
    State(state): State<GatewayState>,
    headers: HeaderMap,
    Json(request): Json<CreateTenantRequest>,
) -> Response {
    if !is_valid_admin_token(&state, &headers) {
        return unauthorized("missing or invalid admin token");
    }
    if !is_valid_tenant_name(&request.name) {
        return bad_request(&format!(
            "invalid tenant name '{}': must match [a-z0-9-]{{1,63}}",
            request.name
        ));
    }
    if request.replicas == 0 {
        return bad_request("replicas must be at least 1");
    }
    let collections = match query::parse_sdl(&request.schema_sdl) {
        Ok(collections) => collections,
        Err(error) => return bad_request(&format!("SDL parse error: {error}")),
    };
    if collections.is_empty() {
        return bad_request("schema_sdl declares no collections");
    }

    let mut manifest = match ClusterManifest::load(&state.data_root).await {
        Ok(manifest) => manifest,
        Err(error) => return internal_error(&format!("loading cluster manifest: {error}")),
    };
    if manifest.tenants.iter().any(|t| t.name == request.name) {
        return bad_request(&format!(
            "tenant '{}' already exists in the cluster manifest",
            request.name
        ));
    }

    let issued = match auth::issue() {
        Ok(issued) => issued,
        Err(error) => return internal_error(&format!("issuing tenant token: {error}")),
    };

    let sdl_path = burner_mesh::tenant_sdl_path(&state.data_root, &request.name);
    if let Some(parent) = sdl_path.parent()
        && let Err(error) = tokio::fs::create_dir_all(parent).await
    {
        return internal_error(&format!("creating tenants directory: {error}"));
    }
    if let Err(error) = tokio::fs::write(&sdl_path, &request.schema_sdl).await {
        return internal_error(&format!("writing tenant schema: {error}"));
    }

    manifest.tenants.push(TenantSpec {
        name: request.name.clone(),
        replicas: request.replicas,
        cells: Vec::new(),
        token_sha256: issued.digest_hex,
        status: TenantStatus::Pending,
        admission: None,
        health: Default::default(),
    });
    if let Err(error) = manifest.save(&state.data_root).await {
        return internal_error(&format!("saving cluster manifest: {error}"));
    }

    {
        let mut supervisor = state.supervisor.lock().await;
        let outcomes = match burner_mesh::reconcile(&mut supervisor, &state.data_root).await {
            Ok(outcomes) => outcomes,
            Err(error) => {
                return rollback_failed_tenant_creation(
                    &state,
                    &request.name,
                    &sdl_path,
                    &format!("{error:#}"),
                )
                .await;
            }
        };
        // Per-tenant isolation (bug-fix round, D25 addendum): only this
        // request's own tenant can trigger a rollback here. Another
        // tenant coming back degraded in the same reconcile pass is real,
        // visible state (`reconcile` already persisted it into the
        // manifest), never a reason to fail this unrelated create.
        match outcomes
            .iter()
            .find(|outcome| outcome.name() == request.name)
        {
            Some(burner_mesh::TenantOutcome::Ready(_)) => {}
            Some(burner_mesh::TenantOutcome::Degraded { reason, .. }) => {
                return rollback_failed_tenant_creation(&state, &request.name, &sdl_path, reason)
                    .await;
            }
            None => {
                return rollback_failed_tenant_creation(
                    &state,
                    &request.name,
                    &sdl_path,
                    "reconcile completed without reporting an outcome for this tenant",
                )
                .await;
            }
        }
        if let Err(error) = state.routing.rebuild(&state.data_root, &supervisor).await {
            return internal_error(&format!("rebuilding routing table: {error:#}"));
        }
    }

    (
        axum::http::StatusCode::CREATED,
        Json(CreateTenantResponse {
            name: request.name,
            token: issued.token_hex,
        }),
    )
        .into_response()
}

#[derive(Deserialize)]
struct AddCollectionsRequest {
    schema_sdl: String,
}

#[derive(Serialize)]
struct AddCollectionsResponse {
    name: String,
    added: Vec<String>,
    collections: Vec<String>,
}

/// `POST /admin/tenants/{name}/collections {schema_sdl}`: adds the
/// collections `schema_sdl` declares to a tenant that is already placed
/// and serving, applying them on every cell in its group, wiring them for
/// replication, and appending them to its stored SDL.
///
/// Every request-shaped failure is rejected here, before
/// `burner_mesh::add_collections` runs, so that anything it returns `Err`
/// for is a genuine execution failure and maps to a 500: an SDL that does
/// not parse or declares nothing, a name the tenant already has, an
/// unknown tenant, and a tenant that is not `Placed` yet are all 400s
/// naming the specific problem.
///
/// Existing documents are untouched; this only ever adds. There is
/// deliberately no matching remove: dropping a collection would destroy
/// data, and the two destructive paths that exist (`DELETE
/// /admin/tenants/{name}` and its `?retire=true` form) already say
/// plainly what they erase.
async fn admin_add_tenant_collections(
    State(state): State<GatewayState>,
    Path(name): Path<String>,
    headers: HeaderMap,
    // `Json` consumes the body, so axum requires it last.
    Json(request): Json<AddCollectionsRequest>,
) -> Response {
    if !is_valid_admin_token(&state, &headers) {
        return unauthorized("missing or invalid admin token");
    }
    let added: Vec<String> = match query::parse_sdl(&request.schema_sdl) {
        Ok(collections) => collections.into_iter().map(|c| c.name).collect(),
        Err(error) => return bad_request(&format!("SDL parse error: {error}")),
    };
    if added.is_empty() {
        return bad_request("schema_sdl declares no collections");
    }

    let manifest = match ClusterManifest::load(&state.data_root).await {
        Ok(manifest) => manifest,
        Err(error) => return internal_error(&format!("loading cluster manifest: {error}")),
    };
    let Some(tenant) = manifest.tenants.iter().find(|t| t.name == name) else {
        return not_found(&format!("no tenant '{name}' in the cluster manifest"));
    };
    if tenant.status != TenantStatus::Placed {
        return bad_request(&format!(
            "tenant '{name}' is not placed yet, so it has no cells to add a collection to"
        ));
    }

    let sdl_path = burner_mesh::tenant_sdl_path(&state.data_root, &name);
    let existing_sdl = match tokio::fs::read_to_string(&sdl_path).await {
        Ok(sdl) => sdl,
        Err(error) => {
            return internal_error(&format!("reading tenant '{name}' schema: {error}"));
        }
    };
    let existing: Vec<String> = match query::parse_sdl(&existing_sdl) {
        Ok(collections) => collections.into_iter().map(|c| c.name).collect(),
        Err(error) => {
            return internal_error(&format!(
                "tenant '{name}' has a stored schema that no longer parses: {error}"
            ));
        }
    };
    if let Some(clash) = added.iter().find(|c| existing.contains(c)) {
        return bad_request(&format!(
            "tenant '{name}' already has a collection named '{clash}'"
        ));
    }

    let result = {
        let mut supervisor = state.supervisor.lock().await;
        burner_mesh::add_collections(
            &mut supervisor,
            &state.data_root,
            &name,
            &request.schema_sdl,
        )
        .await
    };
    match result {
        Ok(added) => {
            let mut collections = existing;
            collections.extend(added.iter().cloned());
            Json(AddCollectionsResponse {
                name,
                added,
                collections,
            })
            .into_response()
        }
        Err(error) => internal_error(&format!(
            "adding collections to tenant '{name}' failed: {error:#}"
        )),
    }
}

/// Rolls back a tenant creation that failed at the reconcile (place +
/// schema + wire) step (D17c): removes the just-appended `TenantSpec`
/// and deletes the SDL file just written, re-saves the manifest, then
/// returns 500 with `reason` (D25 addendum: this tenant's own
/// `TenantOutcome::Degraded` reason, already the full alternate-format
/// error chain from `reconcile`) in the body. Leaving either behind would
/// strand a `Pending` tenant carrying an issued-but-never-returned token:
/// the caller has no way to retry (`tenant create`/this same endpoint
/// refuse a name that already exists) or clean it up themselves.
///
/// A rollback sub-step failing in turn (manifest reload/save, SDL
/// delete) is logged loudly rather than silently swallowed, but does not
/// replace the original reconcile reason in the response: that is still
/// the actionable cause, and hiding it behind a rollback-plumbing error
/// would be strictly less honest.
async fn rollback_failed_tenant_creation(
    state: &GatewayState,
    tenant_name: &str,
    sdl_path: &std::path::Path,
    reason: &str,
) -> Response {
    match ClusterManifest::load(&state.data_root).await {
        Ok(mut manifest) => {
            manifest.tenants.retain(|tenant| tenant.name != tenant_name);
            if let Err(save_error) = manifest.save(&state.data_root).await {
                tracing::error!(
                    tenant = tenant_name,
                    error = %save_error,
                    "rollback: failed to save cluster manifest after removing tenant"
                );
            }
        }
        Err(load_error) => {
            tracing::error!(
                tenant = tenant_name,
                error = %load_error,
                "rollback: failed to reload cluster manifest to remove tenant"
            );
        }
    }

    if let Err(remove_error) = tokio::fs::remove_file(sdl_path).await
        && remove_error.kind() != std::io::ErrorKind::NotFound
    {
        tracing::error!(
            tenant = tenant_name,
            error = %remove_error,
            "rollback: failed to delete tenant SDL file"
        );
    }

    internal_error(&format!(
        "creating tenant '{tenant_name}' failed while placing and wiring it: {reason}"
    ))
}

#[derive(Deserialize)]
struct DropTenantQuery {
    #[serde(default)]
    retire: bool,
}

#[derive(Serialize)]
struct DropTenantResponse {
    name: String,
    /// Always states plainly whether data was left behind, so the caller
    /// never has to infer it (D23: "response states data remains on
    /// cells").
    data_remains_on_cells: Vec<String>,
    retired_cells: Vec<String>,
}

/// `DELETE /admin/tenants/{name}`: unsubscribes the tenant's collections
/// on its cells, removes its placement and the tenant itself from the
/// manifest (revoking its token), and, with `?retire=true`, also drains
/// and erases its cells (including their data directories).
async fn admin_drop_tenant(
    State(state): State<GatewayState>,
    Path(name): Path<String>,
    Query(query): Query<DropTenantQuery>,
    headers: HeaderMap,
) -> Response {
    if !is_valid_admin_token(&state, &headers) {
        return unauthorized("missing or invalid admin token");
    }

    let outcome = match send_supervisor_command(&state, |reply| SupervisorCommand::DropTenant {
        name: name.clone(),
        retire: query.retire,
        reply,
    })
    .await
    {
        Ok(outcome) => outcome,
        Err(response) => return response,
    };

    match outcome {
        Ok(dropped) => {
            {
                let supervisor = state.supervisor.lock().await;
                if let Err(error) = state.routing.rebuild(&state.data_root, &supervisor).await {
                    tracing::error!(
                        error = %error,
                        tenant = %name,
                        "rebuilding routing table after tenant drop failed"
                    );
                }
            }
            if !dropped.retired_cells.is_empty() {
                publish_cell_change(&state).await;
            }
            Json(DropTenantResponse {
                name: dropped.name,
                data_remains_on_cells: dropped.data_remains_on_cells,
                retired_cells: dropped.retired_cells,
            })
            .into_response()
        }
        Err(TenantCommandError::NotFound(name)) => not_found(&format!("tenant '{name}' not found")),
        Err(TenantCommandError::Failed(message)) => internal_error(&message),
    }
}

#[derive(Serialize)]
struct RotateTokenResponse {
    name: String,
    /// Shown once: the caller must save it now, exactly like `tenant
    /// create`'s printed token.
    token: String,
}

/// `POST /admin/tenants/{name}/rotate-token`: issues a fresh bearer token
/// for `name`, replacing (and immediately invalidating) its previous one.
async fn admin_rotate_tenant_token(
    State(state): State<GatewayState>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Response {
    if !is_valid_admin_token(&state, &headers) {
        return unauthorized("missing or invalid admin token");
    }

    let outcome =
        match send_supervisor_command(&state, |reply| SupervisorCommand::RotateTenantToken {
            name: name.clone(),
            reply,
        })
        .await
        {
            Ok(outcome) => outcome,
            Err(response) => return response,
        };

    match outcome {
        Ok(token) => {
            let supervisor = state.supervisor.lock().await;
            if let Err(error) = state.routing.rebuild(&state.data_root, &supervisor).await {
                tracing::error!(
                    error = %error,
                    tenant = %name,
                    "rebuilding routing table after token rotation failed"
                );
            }
            drop(supervisor);
            Json(RotateTokenResponse { name, token }).into_response()
        }
        Err(TenantCommandError::NotFound(name)) => not_found(&format!("tenant '{name}' not found")),
        Err(TenantCommandError::Failed(message)) => internal_error(&message),
    }
}

#[derive(Deserialize)]
struct SetAdmissionRequest {
    /// `None` (or the field omitted) clears the override, reverting the
    /// tenant to the process-wide default rate/burst.
    #[serde(default)]
    rate_per_sec: Option<u64>,
    #[serde(default)]
    burst: Option<u64>,
}

#[derive(Serialize)]
struct SetAdmissionResponse {
    name: String,
    admission: Option<AdmissionOverride>,
}

/// `PUT /admin/tenants/{name}/admission`: sets (both fields present) or
/// clears (both absent) a tenant's per-tenant GCRA admission override,
/// persisted in the manifest and applied to the live `Admission` bucket
/// immediately.
async fn admin_set_tenant_admission(
    State(state): State<GatewayState>,
    Path(name): Path<String>,
    headers: HeaderMap,
    Json(request): Json<SetAdmissionRequest>,
) -> Response {
    if !is_valid_admin_token(&state, &headers) {
        return unauthorized("missing or invalid admin token");
    }

    let admission = match (request.rate_per_sec, request.burst) {
        (Some(rate_per_sec), Some(burst)) => Some(AdmissionOverride {
            rate_per_sec,
            burst,
        }),
        (None, None) => None,
        _ => {
            return bad_request(
                "rate_per_sec and burst must be given together (or both omitted to clear the override)",
            );
        }
    };

    let outcome =
        match send_supervisor_command(&state, |reply| SupervisorCommand::SetTenantAdmission {
            name: name.clone(),
            admission,
            reply,
        })
        .await
        {
            Ok(outcome) => outcome,
            Err(response) => return response,
        };

    match outcome {
        Ok(()) => {
            state
                .admission
                .set_override(&name, admission.map(|a| (a.rate_per_sec, a.burst)));
            Json(SetAdmissionResponse { name, admission }).into_response()
        }
        Err(TenantCommandError::NotFound(name)) => not_found(&format!("tenant '{name}' not found")),
        Err(TenantCommandError::Failed(message)) => internal_error(&message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{StatusCode, header};
    use burner_cell::Supervisor;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    fn free_tcp_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    /// D17c: a `POST /admin/tenants` whose reconcile step fails (here,
    /// forced by asking for more replicas than free cells exist) must not
    /// strand a `Pending` `TenantSpec` plus an issued-but-unreturned
    /// token. Drives the real handler in-process (one real provisioned
    /// cell, a real manifest on disk) rather than mocking reconcile, so
    /// this proves the actual rollback code path, not just its intent.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn admin_create_tenant_rolls_back_on_reconcile_failure() {
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
            .expect("provision the only free cell");

        let admin_token = "test-admin-token";
        let supervisor = Arc::new(Mutex::new(supervisor));
        let (state, _command_rx) =
            crate::gateway::test_support::state(supervisor.clone(), data_root.clone(), admin_token);

        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            format!("Bearer {admin_token}").parse().unwrap(),
        );
        let request = CreateTenantRequest {
            name: "acme-co".to_string(),
            schema_sdl: "type Spike { name: String }".to_string(),
            // Only 1 cell is provisioned; asking for 2 replicas forces
            // burner_mesh::reconcile's placement step to fail.
            replicas: 2,
        };

        let response = admin_create_tenant(State(state), headers, Json(request)).await;
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("reading response body");
        let body_text = String::from_utf8(body.to_vec()).expect("response body is UTF-8");
        assert!(
            body_text.contains("acme-co"),
            "error body should name the failed tenant: {body_text}"
        );
        assert!(
            body_text.contains("free cell"),
            "error body should carry the error chain's root cause: {body_text}"
        );

        let manifest = ClusterManifest::load(&data_root).await.unwrap();
        assert!(
            manifest.tenants.is_empty(),
            "the failed tenant must be rolled back out of the manifest, got: {:?}",
            manifest.tenants
        );
        let sdl_path = burner_mesh::tenant_sdl_path(&data_root, "acme-co");
        assert!(
            !sdl_path.exists(),
            "the SDL file must be rolled back (deleted)"
        );

        supervisor.lock().await.shutdown_all().await;
    }
}
