//! `/admin/cells/{id}/db`: a cell's wasm DefraDB.
//!
//! A fiber is not a separate kind of thing from a cell (D40): every cell
//! owns exactly one, sharing its id and its lifetime. It is spawned when
//! the cell ignites and shut down when the cell drains, so these routes
//! have no ignite or drain of their own; `/admin/cells` already owns that
//! lifecycle. What lives here is what you do *to* a cell's database:
//! apply schema, list collections, query, mutate.

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use burner_fiber::Request as FiberRequest;
use serde::Deserialize;
use serde_json::json;

use crate::gateway::{GatewayState, is_valid_admin_token};

pub(crate) fn router() -> Router<GatewayState> {
    Router::new()
        .route("/admin/cells/{id}/db", get(db_collections))
        .route("/admin/cells/{id}/db/schema", post(db_schema))
        .route("/admin/cells/{id}/db/query", post(db_query))
}

#[derive(Deserialize)]
struct SchemaBody {
    sdl: String,
}

#[derive(Deserialize)]
struct QueryBody {
    graphql: String,
    /// `true` routes to the guest's mutation path. Explicit rather than
    /// sniffed from the string: a caller that means to write should say
    /// so, and a heuristic on "mutation" would misroute a query whose
    /// field name merely contains it.
    #[serde(default)]
    mutate: bool,
}

async fn db_collections(
    State(state): State<GatewayState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    admin(&headers, &state)?;
    call(&state, &id, FiberRequest::ListCollections).await
}

async fn db_schema(
    State(state): State<GatewayState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<SchemaBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    admin(&headers, &state)?;
    call(&state, &id, FiberRequest::AddSchema { sdl: body.sdl }).await
}

async fn db_query(
    State(state): State<GatewayState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<QueryBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    admin(&headers, &state)?;
    let request = if body.mutate {
        FiberRequest::Mutate {
            graphql: body.graphql,
        }
    } else {
        FiberRequest::Query {
            graphql: body.graphql,
        }
    };
    call(&state, &id, request).await
}

/// Sends one request to a cell's wasm database.
///
/// Distinguishes the three ways this can legitimately have no fiber to
/// talk to, because they need different fixes and collapsing them into
/// one "not found" would send an operator hunting the wrong problem: the
/// cell is not running, the process has no wasm image at all, or the cell
/// is running but was ignited before an image was available.
async fn call(
    state: &GatewayState,
    id: &str,
    request: FiberRequest,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let (fiber, cell_exists, image_loaded) = {
        let supervisor = state.supervisor.lock().await;
        (
            supervisor.cell_fiber(id),
            supervisor.cell_ids().iter().any(|c| c == id),
            supervisor.has_fiber_image(),
        )
    };

    let fiber = match fiber {
        Some(fiber) => fiber,
        None if !cell_exists => {
            return Err((
                StatusCode::NOT_FOUND,
                Json(json!({ "error": format!("no running cell '{id}'") })),
            ));
        }
        None if !image_loaded => {
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({
                    "error": state
                        .fiber_unavailable_reason
                        .clone()
                        .unwrap_or_else(|| "no wasm database image is loaded".to_string())
                })),
            ));
        }
        None => {
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({
                    "error": format!(
                        "cell '{id}' is running without a wasm database; it was ignited \
                         before the package was available. Re-ignite it to attach one."
                    )
                })),
            ));
        }
    };

    let outcome = tokio::task::spawn_blocking(move || {
        let mut guard = fiber.blocking_lock();
        guard.request(&request)
    })
    .await
    .map_err(|error| internal(format!("the wasm database task panicked: {error}")))?
    .map_err(|error| internal(format!("cell '{id}' database request failed: {error:#}")))?;

    match outcome.into_data() {
        Ok(data) => Ok(Json(json!({ "status": "ok", "data": data }))),
        // The guest reported a failure (bad SDL, unparseable query). That
        // is the caller's error, not the server's: 400, with the guest's
        // own message, rather than a 500 that would read as a host fault.
        Err(error) => Err((
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": format!("{error:#}") })),
        )),
    }
}

fn admin(
    headers: &HeaderMap,
    state: &GatewayState,
) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    if is_valid_admin_token(state, headers) {
        Ok(())
    } else {
        Err((
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "admin authentication required" })),
        ))
    }
}

fn internal(message: String) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": message })),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_query_body_defaults_to_read_not_write() {
        let body: QueryBody = serde_json::from_str(r#"{"graphql":"{ A { b } }"}"#).unwrap();
        assert!(
            !body.mutate,
            "omitting `mutate` must default to a read; defaulting to a write \
             would turn a malformed read into an unintended mutation"
        );
    }

    #[test]
    fn a_mutation_body_is_explicit() {
        let body: QueryBody =
            serde_json::from_str(r#"{"graphql":"mutation { x }","mutate":true}"#).unwrap();
        assert!(body.mutate);
    }
}
