//! The single gateway listener: bearer token -> tenant -> admission ->
//! per-cell router, plus the gateway's own `/health-check`, `/admin/status`,
//! and `/admin/*` control-surface endpoints (mounted from the sibling
//! `admin_cells`/`admin_tenants`/`admin_autoscaler` modules, all sharing
//! this module's `GatewayState`, `send_supervisor_command` (both
//! `pub(crate)`, not part of this crate's public API), and response
//! helpers: D25: "when two places must agree, they call one
//! function").
//!
//! D12: [`build`]'s fallible setup (routing table, admin token, the
//! listener bind) is awaited directly by `start.rs`, never spawned, so a
//! startup failure (e.g. the gateway port is already taken) surfaces as a
//! real `start` error; [`serve`] is spawned onto its own task once that
//! succeeds (see that call site's comment). Every admin/tenant-routing
//! handler here runs on axum's own per-connection task. None of this
//! reaches `burner_cell::cell::ignite` directly (a mutation that would --
//! provisioning a cell, which the admin command channel exists precisely
//! to keep off every axum handler task, see `command.rs` in `burner-cell`
//! and `defraburner::commands`'s executor), so none of it needs to avoid
//! being `Send`, unlike the ignition path itself.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use burner_cell::{Supervisor, Watchdog};
use burner_policy::autoscaler::AutoscalerControl;
use serde::Serialize;
use tokio::sync::{Mutex, mpsc, oneshot};
use tower::ServiceExt;

use crate::admission::{Admission, Decision};
use crate::auth;
use crate::routing::RoutingTable;
use crate::sse;
use crate::{admin_autoscaler, admin_cells, admin_fibers, admin_tenants};

/// Default gateway listen address.
pub const DEFAULT_GATEWAY_ADDR: &str = "127.0.0.1:9181";

/// Deadline for an admin command sent down the [`SupervisorCommand`]
/// channel to be picked up and answered by `defraburner::commands`'s
/// executor (D25): every admin mutation shares this one timeout via
/// [`send_supervisor_command`], surfacing as 503 rather than hanging the
/// HTTP request forever if the executor is somehow wedged.
///
/// [`SupervisorCommand`]: burner_cell::SupervisorCommand
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

/// Per-(tenant, cell) latency counters: count, sum, and max, in
/// microseconds. Honest minimal metrics (count+sum+max, not a full
/// histogram): enough for Phase 4's snapshot pipeline to compute a mean
/// and a worst-case without this gateway pretending to a percentile
/// breakdown it does not track.
#[derive(Default)]
struct LatencyCounters {
    count: AtomicU64,
    sum_micros: AtomicU64,
    max_micros: AtomicU64,
}

/// A point-in-time render of one `LatencyCounters` entry.
#[derive(Debug, Clone, Serialize)]
pub struct LatencySnapshot {
    pub tenant: String,
    pub cell_id: String,
    pub count: u64,
    pub mean_micros: u64,
    pub max_micros: u64,
}

/// A point-in-time render of one cell's aggregate request counters
/// (Phase 4, D17: feeds `burner-policy`'s `MetricsSnapshot.cells[].requests`
/// and `/admin/status`'s per-cell counters).
#[derive(Debug, Clone, Serialize)]
pub struct CellRequestSnapshot {
    pub cell_id: String,
    pub count: u64,
    pub sum_micros: u64,
    pub max_micros: u64,
}

/// Bounded by tenant/cell cardinality, not by request volume: one entry
/// per `(tenant, cell)` pair (or, in `per_cell`, per cell id) ever
/// observed, never per request. `pub(crate)` (not private): the
/// `admin_cells`/`admin_tenants`/`admin_autoscaler` sibling modules build
/// their own `GatewayState` fixtures in tests.
pub(crate) struct Metrics {
    entries: kovan_map::HopscotchMap<String, Arc<LatencyCounters>>,
    /// Per-cell-only view of the same counters (Phase 4, D17): recorded
    /// alongside `entries` at the same call site (`record`) rather than
    /// derived from it, so a future phase allowing shared-cell density
    /// (multiple tenants per cell) does not silently undercount a cell
    /// that only ever shows up paired with one tenant at a time in
    /// `entries`.
    per_cell: kovan_map::HopscotchMap<String, Arc<LatencyCounters>>,
}

impl Metrics {
    pub(crate) fn new() -> Self {
        Self {
            entries: kovan_map::HopscotchMap::new(),
            per_cell: kovan_map::HopscotchMap::new(),
        }
    }

    fn key(tenant: &str, cell_id: &str) -> String {
        format!("{tenant}\u{0}{cell_id}")
    }

    fn record(&self, tenant: &str, cell_id: &str, elapsed: Duration) {
        let key = Self::key(tenant, cell_id);
        let counters = match self.entries.get(&key) {
            Some(counters) => counters,
            None => {
                let fresh = Arc::new(LatencyCounters::default());
                self.entries.get_or_insert(key, fresh)
            }
        };
        let cell_counters = match self.per_cell.get(cell_id) {
            Some(counters) => counters,
            None => {
                let fresh = Arc::new(LatencyCounters::default());
                self.per_cell.get_or_insert(cell_id.to_string(), fresh)
            }
        };

        let micros = elapsed.as_micros().min(u128::from(u64::MAX)) as u64;
        for counters in [&counters, &cell_counters] {
            counters.count.fetch_add(1, Ordering::Relaxed);
            counters.sum_micros.fetch_add(micros, Ordering::Relaxed);
            counters.max_micros.fetch_max(micros, Ordering::Relaxed);
        }
    }

    fn snapshot(&self) -> Vec<LatencySnapshot> {
        self.entries
            .iter()
            .map(|(key, counters)| {
                let (tenant, cell_id) = key.split_once('\u{0}').unwrap_or((key.as_str(), ""));
                let count = counters.count.load(Ordering::Relaxed);
                let sum = counters.sum_micros.load(Ordering::Relaxed);
                LatencySnapshot {
                    tenant: tenant.to_string(),
                    cell_id: cell_id.to_string(),
                    count,
                    mean_micros: sum.checked_div(count).unwrap_or(0),
                    max_micros: counters.max_micros.load(Ordering::Relaxed),
                }
            })
            .collect()
    }

    fn cell_snapshot(&self) -> Vec<CellRequestSnapshot> {
        self.per_cell
            .iter()
            .map(|(cell_id, counters)| CellRequestSnapshot {
                cell_id,
                count: counters.count.load(Ordering::Relaxed),
                sum_micros: counters.sum_micros.load(Ordering::Relaxed),
                max_micros: counters.max_micros.load(Ordering::Relaxed),
            })
            .collect()
    }
}

/// A cheap, cloneable handle to the gateway's live per-cell request and
/// per-tenant admission counters (Phase 4, D17), held by the autoscaler's
/// tick loop (via `defraburner`'s `start.rs`) so it can pull a fresh
/// snapshot each tick without reaching into the otherwise-private
/// `GatewayState`. `burner-policy` does not depend on this crate, so the
/// caller converts these into its own shapes; see `snapshot.rs`'s doc
/// comment in `burner-policy` for why.
#[derive(Clone)]
pub struct GatewayMetricsHandle {
    metrics: Arc<Metrics>,
    admission: Arc<Admission>,
}

impl GatewayMetricsHandle {
    pub fn cell_requests(&self) -> Vec<CellRequestSnapshot> {
        self.metrics.cell_snapshot()
    }

    pub fn tenant_admission(&self) -> Vec<crate::admission::TenantAdmissionSnapshot> {
        self.admission.per_tenant_snapshot()
    }
}

/// The shared afterburner policy runtime's live configuration and
/// registered packages (console round, operator directive): engine
/// lifecycle lives in `defraburner::runtime`, which this crate cannot see
/// (it would be a dependency cycle the other direction), so `start.rs`
/// builds this plain, serializable snapshot once at startup and hands it
/// in here. Immutable for the process's lifetime: the engine's knobs are
/// CLI-fixed and packages are registered once at startup, never mutated
/// afterward.
#[derive(Debug, Clone, Serialize)]
pub struct RuntimeInfo {
    /// Always `"wasm"` today (D6: policies never run native); carried as
    /// a real field, not a doc-comment claim, so the dashboard renders
    /// what the engine actually reports rather than a hardcoded label.
    pub mode: String,
    pub fuel: Option<u64>,
    pub memory_bytes: Option<usize>,
    pub timeout_ms: Option<u64>,
    pub registered_packages: Vec<burner_policy::RegisteredPackage>,
}

/// Shared state for every handler in this crate, including the
/// `admin_cells`/`admin_tenants`/`admin_autoscaler` sibling modules
/// (`pub(crate)` fields: this is an internal wiring struct, not part of
/// the crate's public API).
#[derive(Clone)]
pub(crate) struct GatewayState {
    pub(crate) supervisor: Arc<Mutex<Supervisor>>,
    pub(crate) routing: Arc<RoutingTable>,
    pub(crate) admission: Arc<Admission>,
    pub(crate) metrics: Arc<Metrics>,
    pub(crate) data_root: PathBuf,
    pub(crate) admin_token_digest: String,
    /// Live autoscaler policy-health status (Phase 4, D17): written by
    /// `burner_policy::autoscaler::run`, read here for `/admin/status`.
    /// Constructed by the caller (`defraburner`'s `start.rs`) before the
    /// autoscaler loop starts, so a fresh handle (honestly "no tick yet")
    /// is always available even before the first tick completes.
    pub(crate) policy_status: Arc<burner_policy::PolicyStatusHandle>,
    /// The dashboard's live event hub (Phase 5, extended D25): fans an
    /// overview snapshot out every second plus new decision entries and
    /// cell-lifecycle changes to every `/admin/api/stream` client.
    pub(crate) sse: Arc<sse::SseHub>,
    /// Read-only from every handler (console round, D25): the watchdog's
    /// per-cell probe counters, for `GET /admin/cells/{id}/inspect`.
    pub(crate) watchdog: Arc<Watchdog>,
    /// Read-only from every handler; mutated only through
    /// `SupervisorCommand::SetAutoscaler`/`ForceAutoscalerTick` on the
    /// executor's task (console round, D25).
    pub(crate) autoscaler_control: Arc<AutoscalerControl>,
    /// The admin command channel (console round, D25): every mutating
    /// admin handler enqueues a `SupervisorCommand` here and awaits its
    /// reply via [`send_supervisor_command`] instead of touching
    /// `supervisor` (or the manifest, or the autoscaler control) itself.
    pub(crate) command_tx: mpsc::Sender<burner_cell::SupervisorCommand>,
    /// Immutable for the process's lifetime (see [`RuntimeInfo`]'s own
    /// doc comment).
    pub(crate) runtime_info: Arc<RuntimeInfo>,
    /// The outcome of dialing every `--peers`-configured static peer at
    /// startup (console round, D23: the Mesh view). Computed once, in
    /// `start.rs`, before the gateway ever binds; immutable for the
    /// process's lifetime like `runtime_info` (a static peer list is a
    /// startup-time config knob, not something admin commands change
    /// live).
    pub(crate) static_peer_outcomes: Arc<Vec<burner_mesh::PeerDialOutcome>>,
    /// Why cells have no wasm database, when they do not (D40).
    ///
    /// The fibers themselves live on the supervisor, one per cell; this is
    /// only the explanation for their absence, so `/admin/cells/{id}/db`
    /// and the dashboard can say *why* instead of reporting a bare
    /// "not found" that reads as a missing cell.
    pub(crate) fiber_unavailable_reason: Option<String>,
}

/// Runs the gateway listener until the process is torn down (this future
/// never returns `Ok` under normal operation; it is one branch of
/// `start.rs`'s `select!`, or the task it is spawned onto).
///
/// Split into [`build`] (everything fallible: routing table, admin token,
/// the actual listener bind) and [`serve`] (the long-running loop) quite
/// deliberately, not as one `run`. `start.rs` awaits `build` directly, on
/// its own task, before the ready-file is written: a bind failure (e.g.
/// the gateway port is already taken) is then a genuine startup error
/// that fails `start` loudly with a real exit code, not something a
/// `tokio::spawn`ed task could only report by logging and quietly
/// triggering the same graceful-shutdown path a real runtime error would.
/// Only `serve`, which cannot fail before the listener already exists, is
/// spawned and raced against the watchdog/autoscaler/command-executor
/// `select!`.
///
/// Returns the gateway's raw admin bearer token alongside the listener,
/// router, and metrics handle (console round, D25) so `up`'s post-
/// readiness banner can print the dashboard URL with the token already
/// attached, whether this run just issued it or loaded it from a prior
/// run's `admin.token` file.
#[allow(clippy::too_many_arguments)]
pub async fn build(
    gateway_addr: SocketAddr,
    data_root: PathBuf,
    supervisor: Arc<Mutex<Supervisor>>,
    policy_status: Arc<burner_policy::PolicyStatusHandle>,
    watchdog: Arc<Watchdog>,
    autoscaler_control: Arc<AutoscalerControl>,
    command_tx: mpsc::Sender<burner_cell::SupervisorCommand>,
    runtime_info: RuntimeInfo,
    static_peer_outcomes: Vec<burner_mesh::PeerDialOutcome>,
    fiber_unavailable_reason: Option<String>,
) -> Result<(
    tokio::net::TcpListener,
    Router,
    GatewayMetricsHandle,
    String,
)> {
    let routing = Arc::new(RoutingTable::new());
    {
        let supervisor = supervisor.lock().await;
        routing
            .rebuild(&data_root, &supervisor)
            .await
            .context("building the initial routing table")?;
    }

    let admin_token = load_or_issue_admin_token(&data_root).await?;
    let metrics = Arc::new(Metrics::new());
    let admission = Arc::new(Admission::default());
    apply_admission_overrides(&admission, &data_root).await;
    let metrics_handle = GatewayMetricsHandle {
        metrics: metrics.clone(),
        admission: admission.clone(),
    };
    let state = GatewayState {
        supervisor,
        routing,
        admission,
        metrics,
        data_root,
        admin_token_digest: auth::digest_hex(&admin_token),
        policy_status,
        sse: Arc::new(sse::SseHub::new()),
        watchdog,
        autoscaler_control,
        command_tx,
        runtime_info: Arc::new(runtime_info),
        static_peer_outcomes: Arc::new(static_peer_outcomes),
        fiber_unavailable_reason,
    };

    // Safe to `tokio::spawn` (D12): only ever reads supervisor/manifest/
    // decision-log state, exactly like `admin_status` above; never reaches
    // `cell::ignite`. Outlives no explicit handle (dies with the process,
    // same as any of axum's own per-connection tasks): there is nothing
    // it holds that needs an orderly shutdown.
    tokio::spawn(run_sse_publisher(state.clone()));

    let router = Router::new()
        .route("/health-check", get(health_check))
        .route("/admin/status", get(admin_status))
        .route("/admin/api/overview", get(admin_status))
        .route("/admin/api/stream", get(admin_api_stream))
        .merge(admin_tenants::router())
        .merge(admin_cells::router())
        .merge(admin_autoscaler::router())
        .merge(admin_fibers::router())
        .merge(burner_dashboard::router())
        .fallback(route_to_tenant)
        .with_state(state);

    let listener = bind_gateway_listener(gateway_addr).await?;
    let bound_addr = listener
        .local_addr()
        .context("reading the gateway listener's bound address")?;
    tracing::info!(address = %bound_addr, "gateway listening");
    Ok((listener, router, metrics_handle, admin_token))
}

/// Bounded range scanned for a free gateway port when `requested` is
/// already taken (operator directive: "ports never block startup": a
/// second `up`, against a different data root, must come up alongside a
/// first one already holding the default port). Unlike
/// `defraburner::up::find_free_port`'s best-effort probe-then-release
/// scan for a fresh cell's p2p port, this binds and keeps the winning
/// listener directly: no separate release-then-rebind race window, since
/// nothing else needs that exact port reserved in between.
const GATEWAY_PORT_SCAN_WIDTH: u16 = 64;

async fn bind_gateway_listener(requested: SocketAddr) -> Result<tokio::net::TcpListener> {
    let mut last_error: Option<(SocketAddr, std::io::Error)> = None;
    for offset in 0..GATEWAY_PORT_SCAN_WIDTH {
        let Some(port) = requested.port().checked_add(offset) else {
            break;
        };
        let candidate = SocketAddr::new(requested.ip(), port);
        match tokio::net::TcpListener::bind(candidate).await {
            Ok(listener) => {
                if offset > 0 {
                    tracing::warn!(
                        requested = %requested,
                        bound = %candidate,
                        "requested gateway address was in use; bound a nearby free port instead"
                    );
                }
                return Ok(listener);
            }
            Err(error) => last_error = Some((candidate, error)),
        }
    }
    match last_error {
        Some((last_candidate, error)) => Err(error).with_context(|| {
            format!(
                "binding gateway listener: no free port found scanning [{}, {}) (last attempt {last_candidate})",
                requested.port(),
                requested.port() as u32 + GATEWAY_PORT_SCAN_WIDTH as u32
            )
        }),
        None => anyhow::bail!("gateway port scan width is zero; no bind was attempted"),
    }
}

/// Reapplies every tenant's persisted admission override (if any) onto a
/// freshly built `Admission` (console round, D23): `PUT
/// /admin/tenants/{name}/admission` persists in the manifest, so a
/// restart must honor it, not silently fall back to the process default.
/// A manifest load failure here is logged and otherwise ignored (mirrors
/// `RoutingTable::rebuild`'s own degrade-don't-fail posture at `build`
/// time): the gateway still comes up with the process-wide default,
/// rather than failing `start` over a cosmetic personalization detail.
async fn apply_admission_overrides(admission: &Admission, data_root: &Path) {
    let manifest = match burner_cell::ClusterManifest::load(data_root).await {
        Ok(manifest) => manifest,
        Err(_) => return,
    };
    for tenant in &manifest.tenants {
        if let Some(override_) = tenant.admission {
            admission.set_override(
                &tenant.name,
                Some((override_.rate_per_sec, override_.burst)),
            );
        }
    }
}

/// SSE overview push cadence (Phase 5, tightened to 1s in the console
/// round, D25: "non stop" realtime, not a slow poll): every tick, gathers
/// one overview and pushes it to every connected client, then pushes any
/// decision-log entries not yet seen as individual `decision` events.
/// Cell-lifecycle changes are pushed immediately by the mutating admin
/// handlers themselves (`admin_cells`/`admin_tenants`), not discovered
/// here on a delay.
const SSE_OVERVIEW_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Serialize)]
struct SseOverviewPayload {
    /// The latest known autoscaler tick (the autoscaler's own
    /// `PolicyStatusHandle::last_ok_tick`, or `0` before any tick has
    /// completed: an honest "not yet", never omitted): every SSE event
    /// type carries a `tick` field, matching `DecisionLogEntry`'s own.
    tick: u64,
    overview: AdminStatusResponse,
}

/// The background publisher loop (Phase 5): every [`SSE_OVERVIEW_INTERVAL`],
/// gathers one overview and pushes it to every connected client, then
/// pushes any decision-log entries not yet seen as individual `decision`
/// events (D17: "decision entries as they happen").
async fn run_sse_publisher(state: GatewayState) {
    let mut ticker = tokio::time::interval(SSE_OVERVIEW_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_seen_tick: Option<u64> = None;

    // The first tick fires immediately; skip it (mirrors
    // `burner_cell::watchdog::Watchdog::run`'s own doc comment on the same
    // point) so this task's first real work: a supervisor lock
    // acquisition plus a live `sync_status` query per cell: lands one
    // interval after the gateway starts, not the instant `build` spawns
    // it. `build` spawns this before `start.rs` itself still has to
    // acquire that same supervisor lock to snapshot `connected_peers` for
    // the ready-file; contending with that snapshot at the exact instant
    // it happens is pure added risk for zero benefit (nobody can be
    // subscribed to `/admin/api/stream` yet either way).
    ticker.tick().await;

    loop {
        ticker.tick().await;
        let overview = match gather_overview(&state).await {
            Ok(overview) => overview,
            Err(error) => {
                tracing::warn!(error = %error, "sse publisher: gathering overview failed");
                continue;
            }
        };

        for entry in &overview.decisions {
            if last_seen_tick.is_none_or(|seen| entry.tick > seen) {
                state.sse.publish("decision", entry);
            }
        }
        if let Some(max_tick) = overview.decisions.iter().map(|entry| entry.tick).max() {
            last_seen_tick = Some(last_seen_tick.map_or(max_tick, |seen| seen.max(max_tick)));
        }

        let tick = overview.policy.last_ok_tick.unwrap_or(0);
        state
            .sse
            .publish("overview", &SseOverviewPayload { tick, overview });
    }
}

async fn admin_api_stream(State(state): State<GatewayState>, headers: HeaderMap) -> Response {
    if !is_valid_admin_token(&state, &headers) {
        return unauthorized("missing or invalid admin token");
    }
    sse::stream_response(&state.sse)
}

/// Serves an already-[`build`]-bound gateway listener until the process
/// is torn down (or a real runtime I/O error occurs).
pub async fn serve(listener: tokio::net::TcpListener, router: Router) -> Result<()> {
    axum::serve(listener, router)
        .await
        .context("gateway server error")
}

async fn health_check() -> &'static str {
    "OK"
}

/// Entries shown in `/admin/status`'s (and `/admin/api/overview`'s)
/// decision tail (Phase 4, D17).
const STATUS_DECISION_TAIL: usize = 20;

/// Per-cell detail beyond what the live `CellStatus` snapshot carries
/// (Phase 5, D17): tenant assignment (a join against the manifest,
/// mirroring `burner-policy`'s own snapshot), storage size, and live
/// `sync_status`, for the dashboard's Cells view.
#[derive(Serialize)]
struct CellDetail {
    id: String,
    tenant: Option<String>,
    storage_bytes: u64,
    sync_status: serde_json::Value,
}

/// One `(cell_id, peer_id)` pair whose replication was positively
/// confirmed via an observed topic-join event in this process (D25 "the
/// real bug" fix): consumed by the dashboard's mesh panel to distinguish
/// a link with positive evidence of being broken ("missing", dashed) from
/// one simply never observed either way ("unconfirmed", dotted): see
/// `burner_mesh::wiring::wire_group`'s doc comment. Collection-agnostic
/// by design: a tenant's whole cell group confirms every collection
/// together in one batch, so which specific collection a triple names
/// does not matter for this dashboard-facing signal.
#[derive(Serialize)]
struct ConfirmedReplicationPair {
    cell_id: String,
    peer_id: String,
}

/// The live autoscaler control's current state (console round, D23), for
/// the dashboard's Autoscaler view controls card.
#[derive(Serialize)]
struct AutoscalerControlView {
    min_cells: usize,
    max_cells: usize,
    cooldown_secs: u64,
    tick_interval_secs: u64,
    paused: bool,
}

#[derive(Serialize)]
struct AdminStatusResponse {
    cells: Vec<burner_cell::CellStatus>,
    tenants: Vec<burner_cell::TenantSpec>,
    admission: crate::admission::AdmissionCounters,
    tenant_admission: Vec<crate::admission::TenantAdmissionSnapshot>,
    latency: Vec<LatencySnapshot>,
    cell_requests: Vec<CellRequestSnapshot>,
    cell_details: Vec<CellDetail>,
    confirmed_replication_pairs: Vec<ConfirmedReplicationPair>,
    policy: burner_policy::PolicyStatusSnapshot,
    autoscaler_control: AutoscalerControlView,
    decisions: Vec<burner_policy::log::DecisionLogEntry>,
    runtime: Arc<RuntimeInfo>,
    static_peer_outcomes: Arc<Vec<burner_mesh::PeerDialOutcome>>,
}

/// Gathers the full admin/dashboard overview: shared by `/admin/status`,
/// `/admin/api/overview`, and the SSE publisher (D17: when two places
/// must agree on a shape, they call one function).
async fn gather_overview(state: &GatewayState) -> Result<AdminStatusResponse> {
    let (cells, sync_statuses, confirmed_topic_joins) = {
        let supervisor = state.supervisor.lock().await;
        let cells = supervisor.status_with_connected_peers().await;
        let sync_statuses = supervisor.sync_status_snapshot().await;
        let confirmed_topic_joins = supervisor.confirmed_topic_joins_snapshot();
        (cells, sync_statuses, confirmed_topic_joins)
    };
    // Collection-agnostic (see `ConfirmedReplicationPair`'s doc comment):
    // dedup via a plain tuple set before mapping to the response struct.
    let confirmed_replication_pairs: Vec<ConfirmedReplicationPair> = confirmed_topic_joins
        .into_iter()
        .map(|(cell_id, _collection, peer_id)| (cell_id, peer_id))
        .collect::<std::collections::HashSet<(String, String)>>()
        .into_iter()
        .map(|(cell_id, peer_id)| ConfirmedReplicationPair { cell_id, peer_id })
        .collect();

    let manifest = burner_cell::ClusterManifest::load(&state.data_root)
        .await
        .context("loading cluster manifest")?;

    let cell_ids: Vec<String> = cells.iter().map(|cell| cell.id.clone()).collect();
    let storage_bytes =
        burner_policy::snapshot::storage_bytes_for_cells(&state.data_root, &cell_ids).await;
    let tenant_of: HashMap<&str, &str> = manifest
        .tenants
        .iter()
        .flat_map(|tenant| {
            tenant
                .cells
                .iter()
                .map(move |cell_id| (cell_id.as_str(), tenant.name.as_str()))
        })
        .collect();
    let cell_details = cells
        .iter()
        .map(|cell| CellDetail {
            id: cell.id.clone(),
            tenant: tenant_of.get(cell.id.as_str()).map(|t| t.to_string()),
            storage_bytes: storage_bytes.get(&cell.id).copied().unwrap_or(0),
            sync_status: sync_statuses
                .get(&cell.id)
                .cloned()
                .unwrap_or(serde_json::Value::Null),
        })
        .collect();

    // A missing/not-yet-created decision log (before the first autoscaler
    // tick ever runs) is normal, not an error: degrade to an empty tail
    // rather than failing the whole overview over it.
    let decisions = match burner_policy::log::tail(&state.data_root, STATUS_DECISION_TAIL).await {
        Ok(entries) => entries,
        Err(error) => {
            tracing::warn!(error = %error, "reading decision log tail for the overview failed");
            Vec::new()
        }
    };

    let effective = state.autoscaler_control.effective().await;
    let autoscaler_control = AutoscalerControlView {
        min_cells: effective.min_cells,
        max_cells: effective.max_cells,
        cooldown_secs: effective.cooldown_secs,
        tick_interval_secs: effective.tick_interval.as_secs(),
        paused: state.autoscaler_control.is_paused().await,
    };

    Ok(AdminStatusResponse {
        cells,
        tenants: manifest.tenants,
        admission: state.admission.counters(),
        tenant_admission: state.admission.per_tenant_snapshot(),
        latency: state.metrics.snapshot(),
        cell_requests: state.metrics.cell_snapshot(),
        cell_details,
        confirmed_replication_pairs,
        policy: state.policy_status.snapshot(),
        autoscaler_control,
        decisions,
        runtime: state.runtime_info.clone(),
        static_peer_outcomes: state.static_peer_outcomes.clone(),
    })
}

async fn admin_status(State(state): State<GatewayState>, headers: HeaderMap) -> Response {
    if !is_valid_admin_token(&state, &headers) {
        return unauthorized("missing or invalid admin token");
    }
    match gather_overview(&state).await {
        Ok(overview) => Json(overview).into_response(),
        Err(error) => internal_error(&format!("gathering overview: {error:#}")),
    }
}

/// The tenant request pipeline: extract bearer token -> resolve tenant
/// (401 unknown) -> admission (429) -> route (proxy into the per-cell
/// router via `tower::ServiceExt::oneshot`). Mounted as the router's
/// fallback, so it handles every path not claimed by `/health-check` or
/// `/admin/*`.
///
/// The request is passed through unchanged *except* for the `Authorization`
/// header, which is stripped before proxying. It has to be: it already
/// did its job (gateway-level tenant authentication, above) and is an
/// opaque token in this gateway's own namespace, not a DefraDB identity
/// JWT. Forwarding it verbatim hits the per-cell router's own
/// `auth_middleware`, which treats any Bearer value that fails JWT
/// parsing as an invalid identity token and rejects with 403 (verified:
/// `defradb.rs/crates/http/src/identity_extractor.rs:169-171`, matching
/// Go DefraDB's own behavior): an entirely different, cell-local
/// identity system this gateway's tenant tokens were never meant to
/// satisfy. Stripping it leaves that request anonymous to the cell (the
/// same as any of this codebase's own direct, header-free `node.execute`
/// calls), which is correct: DefraDB-level ACP identity is a separate,
/// later concern from gateway-level tenant admission and routing.
async fn route_to_tenant(State(state): State<GatewayState>, mut request: Request) -> Response {
    let Some(token) = extract_bearer_token(request.headers()) else {
        return unauthorized("missing bearer token");
    };
    let Some(tenant) = state.routing.resolve_tenant(&token) else {
        return unauthorized("unknown token");
    };

    if let Decision::Reject { retry_after_secs } = state.admission.check(&tenant, Instant::now()) {
        return too_many_requests(retry_after_secs);
    }

    let supervisor = state.supervisor.lock().await;
    let routed = state.routing.route(&tenant, &token, &supervisor);
    drop(supervisor);
    let (cell_id, router) = match routed {
        Ok(v) => v,
        Err(error) => return service_unavailable(&error.to_string()),
    };

    request.headers_mut().remove(header::AUTHORIZATION);

    let start = Instant::now();
    let response = match router.oneshot(request).await {
        Ok(response) => response,
        // axum's Router is an infallible tower::Service: errors are
        // already converted to HTTP responses upstream of this Result.
        Err(never) => match never {},
    };
    state.metrics.record(&tenant, &cell_id, start.elapsed());
    response
}

fn extract_bearer_token(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let token = value
        .strip_prefix("Bearer ")
        .or_else(|| value.strip_prefix("bearer "))?
        .trim();
    (!token.is_empty()).then(|| token.to_string())
}

pub(crate) fn is_valid_admin_token(state: &GatewayState, headers: &HeaderMap) -> bool {
    match extract_bearer_token(headers) {
        Some(token) => auth::digests_match(&auth::digest_hex(&token), &state.admin_token_digest),
        None => false,
    }
}

pub(crate) fn unauthorized(message: &str) -> Response {
    (StatusCode::UNAUTHORIZED, message.to_string()).into_response()
}

pub(crate) fn bad_request(message: &str) -> Response {
    (StatusCode::BAD_REQUEST, message.to_string()).into_response()
}

pub(crate) fn not_found(message: &str) -> Response {
    (StatusCode::NOT_FOUND, message.to_string()).into_response()
}

pub(crate) fn conflict(message: &str) -> Response {
    (StatusCode::CONFLICT, message.to_string()).into_response()
}

pub(crate) fn internal_error(message: &str) -> Response {
    tracing::error!(error = %message, "gateway admin handler failed");
    (StatusCode::INTERNAL_SERVER_ERROR, message.to_string()).into_response()
}

pub(crate) fn service_unavailable(message: &str) -> Response {
    (StatusCode::SERVICE_UNAVAILABLE, message.to_string()).into_response()
}

fn too_many_requests(retry_after_secs: u64) -> Response {
    let mut response = (StatusCode::TOO_MANY_REQUESTS, "admission rejected").into_response();
    let value = header::HeaderValue::from_str(&retry_after_secs.to_string())
        .unwrap_or_else(|_| header::HeaderValue::from_static("1"));
    response.headers_mut().insert(header::RETRY_AFTER, value);
    response
}

/// Sends `command` (built by `build_command`, which receives the reply
/// half) down `state.command_tx` and awaits its reply within
/// [`COMMAND_TIMEOUT`], converting every failure mode (executor not
/// running, reply channel dropped, timeout) into a 503 (D25: "all
/// admin-token authed, via a shared `send_supervisor_command` helper with
/// 30s timeout -> 503"). The one function every mutating admin handler in
/// `admin_cells`/`admin_tenants`/`admin_autoscaler` calls, so they never
/// duplicate this error-mapping three ways.
///
/// The `Err` variant is an `axum::Response`, which trips
/// `clippy::result_large_err` at its 128-byte threshold. Boxing it, the
/// lint's suggested remedy, would add an allocation on every failure and
/// force a deref at every `?` in the admin handlers, all to shrink a type
/// that each caller immediately returns as-is: the error IS the response.
#[allow(clippy::result_large_err)]
pub(crate) async fn send_supervisor_command<T>(
    state: &GatewayState,
    build_command: impl FnOnce(oneshot::Sender<T>) -> burner_cell::SupervisorCommand,
) -> std::result::Result<T, Response> {
    let (reply_tx, reply_rx) = oneshot::channel();
    let command = build_command(reply_tx);
    if state.command_tx.send(command).await.is_err() {
        return Err(service_unavailable(
            "the admin command executor is not running",
        ));
    }
    match tokio::time::timeout(COMMAND_TIMEOUT, reply_rx).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(_)) => Err(service_unavailable(
            "the admin command executor dropped the reply channel",
        )),
        Err(_) => Err(service_unavailable(
            "timed out waiting for the admin command executor",
        )),
    }
}

/// Payload for the SSE `cell_change` event (console round, D25): pushed
/// immediately by a mutating admin handler right after a cell-topology
/// change (provision, drain, tenant retirement) takes effect, rather than
/// waiting for the next periodic `overview` tick.
#[derive(Serialize)]
struct SseCellChangePayload {
    cells: Vec<burner_cell::CellStatus>,
}

/// Publishes the current cell list as an immediate `cell_change` SSE
/// event. Called by `admin_cells`/`admin_tenants` handlers after any
/// command that adds or removes a cell.
pub(crate) async fn publish_cell_change(state: &GatewayState) {
    let cells = state.supervisor.lock().await.status();
    state
        .sse
        .publish("cell_change", &SseCellChangePayload { cells });
}

/// Path to the gateway's own admin bearer token file.
fn admin_token_path(data_root: &Path) -> PathBuf {
    data_root.join("admin.token")
}

/// Loads the gateway's admin token, generating and persisting a fresh one
/// (printed once, mode 0600) if this is the first `start` against
/// `data_root`.
async fn load_or_issue_admin_token(data_root: &Path) -> Result<String> {
    let path = admin_token_path(data_root);
    if path.exists() {
        let token = tokio::fs::read_to_string(&path)
            .await
            .with_context(|| format!("reading admin token {}", path.display()))?;
        return Ok(token.trim().to_string());
    }

    let issued = auth::issue().context("issuing gateway admin token")?;
    let path_owned = path.clone();
    let token_hex = issued.token_hex.clone();
    tokio::task::spawn_blocking(move || write_admin_token_file(&path_owned, &token_hex))
        .await
        .context("admin token write task panicked")??;
    println!(
        "gateway admin token (save this, shown once): {}",
        issued.token_hex
    );
    Ok(issued.token_hex)
}

/// Mirrors `burner_cell::identity`'s `write_seed`: `create_new` so a
/// concurrent second writer can never clobber an already-issued token,
/// `sync_all` before the permission change, 0600 throughout.
fn write_admin_token_file(path: &Path, token: &str) -> Result<()> {
    use std::io::Write;

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("creating admin token file {}", path.display()))?;
    file.write_all(token.as_bytes())
        .with_context(|| format!("writing admin token file {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("fsyncing admin token file {}", path.display()))?;
    drop(file);

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("setting permissions on {}", path.display()))?;
    Ok(())
}

/// Shared `GatewayState` test fixture builder (D25), used by this
/// module's own tests and by the `admin_cells`/`admin_tenants`/
/// `admin_autoscaler` sibling modules' tests: every one of them otherwise
/// repeats the same handful of empty/default collaborators (routing
/// table, admission, metrics, policy status, SSE hub, watchdog,
/// autoscaler control), differing only in the supervisor, data root, and
/// admin token a given test actually cares about.
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    /// Returns a fresh `GatewayState` plus the receiving half of its
    /// command channel: a test either drives that receiver itself (to
    /// answer commands the handler under test sends) or drops it (any
    /// command the handler sends then surfaces as a 503, exactly as it
    /// would if the real executor task had died).
    pub(crate) fn state(
        supervisor: Arc<Mutex<Supervisor>>,
        data_root: PathBuf,
        admin_token: &str,
    ) -> (GatewayState, mpsc::Receiver<burner_cell::SupervisorCommand>) {
        let (command_tx, command_rx) = mpsc::channel(burner_cell::COMMAND_CHANNEL_CAPACITY);
        let control = AutoscalerControl::new(
            burner_policy::autoscaler::AutoscalerConfig {
                min_cells: 1,
                max_cells: 8,
                cooldown_secs: 60,
                tick_interval: Duration::from_secs(5),
                bind_addr: "127.0.0.1".parse().unwrap(),
                base_port: 9171,
            },
            burner_cell::AutoscalerSpec::default(),
        );
        let state = GatewayState {
            supervisor,
            routing: Arc::new(RoutingTable::new()),
            admission: Arc::new(Admission::default()),
            metrics: Arc::new(Metrics::new()),
            data_root,
            admin_token_digest: auth::digest_hex(admin_token),
            policy_status: Arc::new(burner_policy::PolicyStatusHandle::new()),
            sse: Arc::new(sse::SseHub::new()),
            watchdog: Arc::new(Watchdog::new()),
            autoscaler_control: Arc::new(control),
            command_tx,
            runtime_info: Arc::new(RuntimeInfo {
                mode: "wasm".to_string(),
                fuel: None,
                memory_bytes: None,
                timeout_ms: None,
                registered_packages: Vec::new(),
            }),
            static_peer_outcomes: Arc::new(Vec::new()),
            // Test supervisors carry no wasm image: these tests exercise
            // the gateway's own routing and auth, and compiling a 5 MiB
            // module per test would buy nothing. The reason string is what
            // `/admin/cells/{id}/db` reports in that state.
            fiber_unavailable_reason: Some("no fiber package in tests".to_string()),
        };
        (state, command_rx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_bearer_token_accepts_bearer_prefix_case_insensitively() {
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, "Bearer abc123".parse().unwrap());
        assert_eq!(extract_bearer_token(&headers).as_deref(), Some("abc123"));

        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, "bearer abc123".parse().unwrap());
        assert_eq!(extract_bearer_token(&headers).as_deref(), Some("abc123"));
    }

    #[test]
    fn extract_bearer_token_rejects_missing_or_empty() {
        assert_eq!(extract_bearer_token(&HeaderMap::new()), None);

        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, "Bearer ".parse().unwrap());
        assert_eq!(extract_bearer_token(&headers), None);

        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, "Basic abc123".parse().unwrap());
        assert_eq!(extract_bearer_token(&headers), None);
    }

    /// Zero-config contract (operator directive): the gateway's own bind
    /// must scan past an already-occupied port rather than failing `up`.
    #[tokio::test]
    async fn bind_gateway_listener_scans_past_an_occupied_port() {
        let occupied = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let occupied_port = occupied.local_addr().unwrap().port();
        let requested = SocketAddr::new("127.0.0.1".parse().unwrap(), occupied_port);

        let bound = bind_gateway_listener(requested)
            .await
            .expect("should scan past the occupied port to a free one");
        assert_ne!(bound.local_addr().unwrap().port(), occupied_port);
        drop(occupied);
    }

    #[tokio::test]
    async fn bind_gateway_listener_uses_the_requested_port_when_free() {
        // Bind and release to learn a genuinely free port, then ask for
        // exactly that port: offset 0 must win, no scanning needed.
        let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let free_port = probe.local_addr().unwrap().port();
        drop(probe);

        let requested = SocketAddr::new("127.0.0.1".parse().unwrap(), free_port);
        let bound = bind_gateway_listener(requested)
            .await
            .expect("should bind the requested port directly");
        assert_eq!(bound.local_addr().unwrap().port(), free_port);
    }

    #[test]
    fn metrics_snapshot_reports_count_mean_and_max() {
        let metrics = Metrics::new();
        metrics.record("acme-co", "cell-0", Duration::from_micros(100));
        metrics.record("acme-co", "cell-0", Duration::from_micros(300));
        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].tenant, "acme-co");
        assert_eq!(snapshot[0].cell_id, "cell-0");
        assert_eq!(snapshot[0].count, 2);
        assert_eq!(snapshot[0].mean_micros, 200);
        assert_eq!(snapshot[0].max_micros, 300);
    }

    #[test]
    fn metrics_keeps_distinct_cells_separate() {
        let metrics = Metrics::new();
        metrics.record("acme-co", "cell-0", Duration::from_micros(100));
        metrics.record("acme-co", "cell-1", Duration::from_micros(500));
        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.len(), 2);
    }
}
