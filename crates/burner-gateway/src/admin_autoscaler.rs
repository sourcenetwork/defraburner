//! Autoscaler admin control surface (console round, D23/D25): live
//! min/max/cooldown/tick-interval/pause config and a force-tick trigger.
//! Both mutations share `gateway::send_supervisor_command`.

use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::routing::{post, put};
use axum::{Json, Router};
use burner_cell::{AutoscalerPatch, SupervisorCommand};
use serde::Serialize;

use crate::gateway::{
    GatewayState, bad_request, is_valid_admin_token, send_supervisor_command, unauthorized,
};

pub(crate) fn router() -> Router<GatewayState> {
    Router::new()
        .route("/admin/autoscaler", put(admin_set_autoscaler))
        .route("/admin/autoscaler/tick", post(admin_force_tick))
}

#[derive(Serialize)]
struct AutoscalerConfigResponse {
    min_cells: usize,
    max_cells: usize,
    cooldown_secs: u64,
    tick_interval_secs: u64,
    paused: bool,
    scale_down_enabled: bool,
}

/// `PUT /admin/autoscaler {min_cells?, max_cells?, cooldown_secs?,
/// tick_interval_secs?, paused?, scale_down_enabled?}`: merges the given fields into the live
/// autoscaler override layer (persisted in the manifest), rejecting a
/// patch whose resulting config would be nonsensical (400).
async fn admin_set_autoscaler(
    State(state): State<GatewayState>,
    headers: HeaderMap,
    Json(patch): Json<AutoscalerPatch>,
) -> Response {
    if !is_valid_admin_token(&state, &headers) {
        return unauthorized("missing or invalid admin token");
    }

    let outcome = match send_supervisor_command(&state, |reply| SupervisorCommand::SetAutoscaler {
        patch,
        reply,
    })
    .await
    {
        Ok(outcome) => outcome,
        Err(response) => return response,
    };

    match outcome {
        Ok(()) => {
            let effective = state.autoscaler_control.effective().await;
            Json(AutoscalerConfigResponse {
                min_cells: effective.min_cells,
                max_cells: effective.max_cells,
                cooldown_secs: effective.cooldown_secs,
                tick_interval_secs: effective.tick_interval.as_secs(),
                paused: state.autoscaler_control.is_paused().await,
                scale_down_enabled: effective.scale_down_enabled,
            })
            .into_response()
        }
        Err(message) => bad_request(&message),
    }
}

/// `POST /admin/autoscaler/tick`: forces one tick outside the normal
/// cadence. Acknowledges once the signal has been sent, not once the
/// forced tick has actually finished running: a tick's own work (a
/// live sync_status query per cell, a manifest load/save) should never
/// gate this endpoint's response latency.
async fn admin_force_tick(State(state): State<GatewayState>, headers: HeaderMap) -> Response {
    if !is_valid_admin_token(&state, &headers) {
        return unauthorized("missing or invalid admin token");
    }

    match send_supervisor_command(&state, |reply| SupervisorCommand::ForceAutoscalerTick {
        reply,
    })
    .await
    {
        Ok(()) => axum::http::StatusCode::ACCEPTED.into_response(),
        Err(response) => response,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{StatusCode, header};
    use burner_cell::Supervisor;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    #[tokio::test]
    async fn set_autoscaler_requires_the_admin_token() {
        let dir = tempfile::tempdir().unwrap();
        let (state, _command_rx) = crate::gateway::test_support::state(
            Arc::new(Mutex::new(Supervisor::new(dir.path()))),
            dir.path().to_path_buf(),
            "test-admin-token",
        );
        let response = admin_set_autoscaler(
            State(state),
            HeaderMap::new(),
            Json(AutoscalerPatch::default()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn force_tick_requires_the_admin_token() {
        let dir = tempfile::tempdir().unwrap();
        let (state, _command_rx) = crate::gateway::test_support::state(
            Arc::new(Mutex::new(Supervisor::new(dir.path()))),
            dir.path().to_path_buf(),
            "test-admin-token",
        );
        let response = admin_force_tick(State(state), HeaderMap::new()).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    /// D25: with the command channel receiver dropped (simulating the
    /// executor not running), a well-authed request still surfaces as a
    /// clean 503, never a hang or a panic.
    #[tokio::test]
    async fn set_autoscaler_surfaces_503_when_the_executor_is_not_running() {
        let dir = tempfile::tempdir().unwrap();
        let (state, command_rx) = crate::gateway::test_support::state(
            Arc::new(Mutex::new(Supervisor::new(dir.path()))),
            dir.path().to_path_buf(),
            "test-admin-token",
        );
        drop(command_rx);

        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            "Bearer test-admin-token".parse().unwrap(),
        );
        let response = admin_set_autoscaler(
            State(state),
            headers,
            Json(AutoscalerPatch {
                paused: Some(true),
                ..Default::default()
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }
}
