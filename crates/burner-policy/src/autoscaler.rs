//! The autoscale + placement control loop (D9/D12/D17). Each tick: build
//! a [`snapshot::MetricsSnapshot`], run the autoscale policy, parse,
//! clamp, execute; then, only when the manifest has a genuinely unplaced
//! (`Pending`, no cells yet) tenant, run the placement policy, parse,
//! clamp, and reconcile-place through `burner-mesh`. A policy error never
//! aborts the loop: the last-known-good plan holds (no action that step),
//! the failure is logged loudly (`tracing::error` and the decision log)
//! and counted in [`PolicyStatusHandle`].
//!
//! Driven directly on the caller's task from `defraburner::start`'s
//! `select!`, never spawned (D12): `execute_scale_up` reaches
//! `Supervisor::provision`, which reaches `cell::ignite`, whose returned
//! future is not `Send` whenever libp2p is configured (see
//! `burner_cell::watchdog::Watchdog::run`'s doc comment for the same
//! constraint, verified the same way).

use std::collections::HashMap;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use burner_cell::{
    AutoscalerPatch, AutoscalerSpec, BackendKind, CellSpec, ClusterManifest,
    DEFAULT_MEM_BUDGET_BYTES, Supervisor,
};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex, Notify, RwLock};

use crate::clamp::{
    self, AutoscaleClampContext, ClampedAction, ClampedPlan, PlacementClampContext,
};
use crate::decision::{AutoscaleDecision, PlacementDecision};
use crate::engine::{AUTOSCALE_DEFAULT_NAME, PLACEMENT_DEFAULT_NAME, PolicyEngine};
use crate::log::{self, DecisionLogEntry};
use crate::snapshot::{self, GatewayMetrics, LastAction, MetricsSnapshot, SnapshotInputs};

/// Autoscaler tuning knobs, CLI-plumbed by `defraburner`'s `main.rs`. This
/// is the process's base configuration; [`AutoscalerControl`] layers live
/// admin overrides ([`AutoscalerSpec`]) on top of it.
#[derive(Debug, Clone)]
pub struct AutoscalerConfig {
    pub min_cells: usize,
    pub max_cells: usize,
    pub cooldown_secs: u64,
    pub tick_interval: Duration,
    /// Bind address for a freshly scaled-up cell's libp2p transport
    /// (matches `provision_fresh`'s existing IPv4-only convention).
    pub bind_addr: IpAddr,
    pub base_port: u16,
}

/// Live, shared autoscaler control (console round, D23): the CLI-derived
/// [`AutoscalerConfig`] plus an admin-adjustable [`AutoscalerSpec`]
/// override layer, and a manual force-tick signal. Shared as
/// `Arc<AutoscalerControl>` between [`run`]'s tick loop (reads
/// [`AutoscalerControl::effective`] and [`AutoscalerControl::is_paused`]
/// every tick) and the admin command executor
/// (`defraburner::commands`, `PUT /admin/autoscaler` /
/// `POST /admin/autoscaler/tick`).
pub struct AutoscalerControl {
    base: AutoscalerConfig,
    overrides: RwLock<AutoscalerSpec>,
    force_tick: Notify,
}

impl AutoscalerControl {
    pub fn new(base: AutoscalerConfig, overrides: AutoscalerSpec) -> Self {
        Self {
            base,
            overrides: RwLock::new(overrides),
            force_tick: Notify::new(),
        }
    }

    /// The effective config for the next tick: every override field
    /// present wins over `base`; every absent field falls back to it.
    pub async fn effective(&self) -> AutoscalerConfig {
        let overrides = *self.overrides.read().await;
        AutoscalerConfig {
            min_cells: overrides.min_cells.unwrap_or(self.base.min_cells),
            max_cells: overrides.max_cells.unwrap_or(self.base.max_cells),
            cooldown_secs: overrides.cooldown_secs.unwrap_or(self.base.cooldown_secs),
            tick_interval: overrides
                .tick_interval_secs
                .map(Duration::from_secs)
                .unwrap_or(self.base.tick_interval),
            bind_addr: self.base.bind_addr,
            base_port: self.base.base_port,
        }
    }

    pub async fn is_paused(&self) -> bool {
        self.overrides.read().await.paused
    }

    /// The current override layer, exactly as persisted in the cluster
    /// manifest's `autoscaler` section.
    pub async fn spec_snapshot(&self) -> AutoscalerSpec {
        *self.overrides.read().await
    }

    /// Merges `patch` into the override layer, rejecting a patch whose
    /// resulting effective config would be nonsensical (min above max, a
    /// zero min, or a zero tick interval -- `tokio::time::interval` panics
    /// on a zero period) before it ever takes effect.
    pub async fn apply_patch(&self, patch: AutoscalerPatch) -> std::result::Result<(), String> {
        let mut overrides = self.overrides.write().await;
        let mut candidate = *overrides;
        if let Some(v) = patch.min_cells {
            candidate.min_cells = Some(v);
        }
        if let Some(v) = patch.max_cells {
            candidate.max_cells = Some(v);
        }
        if let Some(v) = patch.cooldown_secs {
            candidate.cooldown_secs = Some(v);
        }
        if let Some(v) = patch.tick_interval_secs {
            candidate.tick_interval_secs = Some(v);
        }
        if let Some(v) = patch.paused {
            candidate.paused = v;
        }

        let effective_min = candidate.min_cells.unwrap_or(self.base.min_cells);
        let effective_max = candidate.max_cells.unwrap_or(self.base.max_cells);
        if effective_min == 0 {
            return Err("min_cells must be at least 1".to_string());
        }
        if effective_min > effective_max {
            return Err(format!(
                "min_cells ({effective_min}) would exceed max_cells ({effective_max})"
            ));
        }
        if candidate.tick_interval_secs == Some(0) {
            return Err("tick_interval_secs must be at least 1".to_string());
        }

        *overrides = candidate;
        Ok(())
    }

    /// Signals the tick loop to run one extra tick right away, outside its
    /// normal cadence.
    pub fn force_tick(&self) {
        self.force_tick.notify_one();
    }

    /// Waits for either `ticker`'s normal cadence or a manual
    /// [`AutoscalerControl::force_tick`], whichever comes first.
    async fn wait_for_tick(&self, ticker: &mut tokio::time::Interval) {
        tokio::select! {
            _ = ticker.tick() => {}
            _ = self.force_tick.notified() => {}
        }
    }
}

/// At most one scale action is ever authorized per tick. Enforced by
/// [`AutoscaleDecision`]'s own shape (a single `action`, not a list), not
/// by a runtime truncation; carried here only as the informational value
/// reported in [`MetricsSnapshot::limits`].
pub const MAX_ACTIONS_PER_TICK: usize = 1;

/// Live, shared policy-health status: written by the tick loop, read by
/// `/admin/status` and the dashboard (via `burner-gateway`, which depends
/// on this crate). A fresh handle honestly represents "no tick has
/// completed yet" (`last_ok_tick: None`), never a fabricated zero.
pub struct PolicyStatusHandle {
    last_ok_tick: AtomicU64,
    has_ok_tick: AtomicBool,
    consecutive_errors: AtomicU64,
    last_error: std::sync::Mutex<Option<String>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PolicyStatusSnapshot {
    pub last_ok_tick: Option<u64>,
    pub consecutive_errors: u64,
    pub last_error: Option<String>,
}

impl Default for PolicyStatusHandle {
    fn default() -> Self {
        Self::new()
    }
}

impl PolicyStatusHandle {
    pub fn new() -> Self {
        Self {
            last_ok_tick: AtomicU64::new(0),
            has_ok_tick: AtomicBool::new(false),
            consecutive_errors: AtomicU64::new(0),
            last_error: std::sync::Mutex::new(None),
        }
    }

    pub fn snapshot(&self) -> PolicyStatusSnapshot {
        PolicyStatusSnapshot {
            last_ok_tick: self
                .has_ok_tick
                .load(Ordering::Relaxed)
                .then(|| self.last_ok_tick.load(Ordering::Relaxed)),
            consecutive_errors: self.consecutive_errors.load(Ordering::Relaxed),
            last_error: lock_last_error(&self.last_error).clone(),
        }
    }

    fn record_ok(&self, tick: u64) {
        self.last_ok_tick.store(tick, Ordering::Relaxed);
        self.has_ok_tick.store(true, Ordering::Relaxed);
        self.consecutive_errors.store(0, Ordering::Relaxed);
    }

    fn record_error(&self, message: String) {
        self.consecutive_errors.fetch_add(1, Ordering::Relaxed);
        *lock_last_error(&self.last_error) = Some(message);
    }
}

fn lock_last_error(
    mutex: &std::sync::Mutex<Option<String>>,
) -> std::sync::MutexGuard<'_, Option<String>> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// What one policy step (autoscale or placement) contributed to this
/// tick's policy-health signal. Execution failures (a clamped,
/// host-approved action that failed to actually run, e.g. a port
/// conflict) are always logged (decision log + `tracing::error`)
/// regardless of this outcome, but only a genuine [`StepOutcome::PolicyError`]
/// -- the engine call or the decision parse itself failing -- affects
/// [`PolicyStatusHandle`]: that status specifically answers "is the
/// policy layer healthy", not "did every downstream action succeed".
enum StepOutcome {
    Healthy,
    PolicyError(String),
}

/// Runs the autoscale + placement control loop forever (until the process
/// shuts down). See the module doc comment for why this is driven
/// directly on the caller's task, never spawned.
///
/// Reads `control`'s effective config fresh every tick (console round,
/// D23: `PUT /admin/autoscaler` takes effect on the very next tick, live,
/// no restart) and rebuilds its ticker whenever the effective tick
/// interval has changed. When `control` reports paused, the tick is
/// skipped entirely (no autoscale step, no placement step) but the loop
/// still records a healthy tick: a paused policy layer is not an
/// unhealthy one.
pub async fn run(
    supervisor: Arc<Mutex<Supervisor>>,
    data_root: PathBuf,
    engine: PolicyEngine,
    control: Arc<AutoscalerControl>,
    status: Arc<PolicyStatusHandle>,
    gateway_metrics: Arc<dyn Fn() -> GatewayMetrics + Send + Sync>,
) -> ! {
    let mut current_config = control.effective().await;
    let mut ticker = tokio::time::interval(current_config.tick_interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut tick_number: u64 = 0;
    let mut last_action_at: Option<Instant> = None;
    let mut last_action: Option<LastAction> = None;
    let mut previous_cell_counts: HashMap<String, (u64, Instant)> = HashMap::new();

    loop {
        control.wait_for_tick(&mut ticker).await;
        tick_number += 1;

        current_config = control.effective().await;
        if ticker.period() != current_config.tick_interval {
            ticker = tokio::time::interval(current_config.tick_interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        }

        if control.is_paused().await {
            status.record_ok(tick_number);
            continue;
        }

        run_tick(
            tick_number,
            &supervisor,
            &data_root,
            &engine,
            &current_config,
            &status,
            &mut last_action_at,
            &mut last_action,
            &mut previous_cell_counts,
            gateway_metrics.as_ref(),
        )
        .await;
    }
}

/// Derives each cell's current requests/sec from the delta between this
/// tick's and the previous tick's cumulative request count, divided by
/// the elapsed wall time between the two samples. `burner_gateway`'s
/// per-cell counters are lifetime cumulative totals (never reset), so a
/// rate has to be derived this way, not read directly; the shipped
/// `autoscale-default` policy's threshold check operates on exactly this
/// per-cell `qps` signal (see [`snapshot::CellSnapshot::qps`]'s doc
/// comment for why it exists at all). A cell seen for the first time (no
/// previous sample yet) reports `0.0`, honestly, not a fabricated rate.
fn compute_qps(
    mut metrics: GatewayMetrics,
    previous: &mut HashMap<String, (u64, Instant)>,
    now: Instant,
) -> GatewayMetrics {
    for cell in &mut metrics.cell_requests {
        cell.qps = match previous.get(&cell.cell_id) {
            Some((prev_count, prev_at)) => {
                let elapsed = now.saturating_duration_since(*prev_at).as_secs_f64();
                if elapsed > 0.0 {
                    cell.count.saturating_sub(*prev_count) as f64 / elapsed
                } else {
                    0.0
                }
            }
            None => 0.0,
        };
        previous.insert(cell.cell_id.clone(), (cell.count, now));
    }
    metrics
}

#[allow(clippy::too_many_arguments)]
async fn run_tick(
    tick: u64,
    supervisor: &Arc<Mutex<Supervisor>>,
    data_root: &Path,
    engine: &PolicyEngine,
    config: &AutoscalerConfig,
    status: &PolicyStatusHandle,
    last_action_at: &mut Option<Instant>,
    last_action: &mut Option<LastAction>,
    previous_cell_counts: &mut HashMap<String, (u64, Instant)>,
    gateway_metrics: &(dyn Fn() -> GatewayMetrics + Send + Sync),
) {
    let autoscale_outcome = run_autoscale_step(
        tick,
        supervisor,
        data_root,
        engine,
        config,
        last_action_at,
        last_action,
        previous_cell_counts,
        gateway_metrics,
    )
    .await;
    let mut healthy = matches!(autoscale_outcome, StepOutcome::Healthy);
    if let StepOutcome::PolicyError(message) = &autoscale_outcome {
        tracing::error!(tick, package = AUTOSCALE_DEFAULT_NAME, error = %message, "policy error");
        status.record_error(message.clone());
    }

    let placement_outcome = run_placement_step(tick, supervisor, data_root, engine, config).await;
    if let StepOutcome::PolicyError(message) = &placement_outcome {
        healthy = false;
        tracing::error!(tick, package = PLACEMENT_DEFAULT_NAME, error = %message, "policy error");
        status.record_error(message.clone());
    }

    if healthy {
        status.record_ok(tick);
    }
}

#[allow(clippy::too_many_arguments)]
async fn append_entry(
    data_root: &Path,
    tick: u64,
    package: &str,
    input_sha256: &str,
    raw_decision: Value,
    plan: ClampedPlan,
    executed: bool,
    error: Option<String>,
) {
    let entry = DecisionLogEntry {
        ts_ms: now_ms(),
        tick,
        package: package.to_string(),
        input_sha256: input_sha256.to_string(),
        raw_decision,
        clamped: plan.actions,
        clamps_applied: plan.clamps_applied,
        executed,
        error,
    };
    if let Err(error) = log::append(data_root, &entry).await {
        tracing::error!(tick, package, error = %error, "failed to append decision log entry");
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[allow(clippy::too_many_arguments)]
async fn run_autoscale_step(
    tick: u64,
    supervisor: &Arc<Mutex<Supervisor>>,
    data_root: &Path,
    engine: &PolicyEngine,
    config: &AutoscalerConfig,
    last_action_at: &mut Option<Instant>,
    last_action: &mut Option<LastAction>,
    previous_cell_counts: &mut HashMap<String, (u64, Instant)>,
    gateway_metrics: &(dyn Fn() -> GatewayMetrics + Send + Sync),
) -> StepOutcome {
    let metrics = compute_qps(gateway_metrics(), previous_cell_counts, Instant::now());
    let snapshot_result = MetricsSnapshot::build(
        supervisor,
        data_root,
        SnapshotInputs {
            tick,
            min_cells: config.min_cells,
            max_cells: config.max_cells,
            max_actions_per_tick: MAX_ACTIONS_PER_TICK,
            cooldown_secs: config.cooldown_secs,
            last_action: last_action.clone(),
            gateway_metrics: &metrics,
        },
    )
    .await;

    let snapshot = match snapshot_result {
        Ok(snapshot) => snapshot,
        Err(error) => {
            let message = format!("building metrics snapshot: {error}");
            append_entry(
                data_root,
                tick,
                AUTOSCALE_DEFAULT_NAME,
                "",
                Value::Null,
                ClampedPlan::default(),
                false,
                Some(message.clone()),
            )
            .await;
            return StepOutcome::PolicyError(message);
        }
    };

    let input = match serde_json::to_value(&snapshot).context("serializing metrics snapshot") {
        Ok(input) => input,
        Err(error) => return StepOutcome::PolicyError(error.to_string()),
    };
    let input_sha256 = sha256_hex(serde_json::to_vec(&input).unwrap_or_default().as_slice());

    let raw = match engine.run(AUTOSCALE_DEFAULT_NAME, &input) {
        Ok(raw) => raw,
        Err(error) => {
            append_entry(
                data_root,
                tick,
                AUTOSCALE_DEFAULT_NAME,
                &input_sha256,
                Value::Null,
                ClampedPlan::default(),
                false,
                Some(error.to_string()),
            )
            .await;
            return StepOutcome::PolicyError(error.to_string());
        }
    };

    let decision = match AutoscaleDecision::parse(&raw) {
        Ok(decision) => decision,
        Err(error) => {
            append_entry(
                data_root,
                tick,
                AUTOSCALE_DEFAULT_NAME,
                &input_sha256,
                raw,
                ClampedPlan::default(),
                false,
                Some(error.to_string()),
            )
            .await;
            return StepOutcome::PolicyError(error.to_string());
        }
    };

    let free_cells_oldest_first = {
        let manifest = match ClusterManifest::load(data_root).await {
            Ok(manifest) => manifest,
            Err(error) => {
                let message =
                    format!("loading cluster manifest to clamp autoscale decision: {error}");
                append_entry(
                    data_root,
                    tick,
                    AUTOSCALE_DEFAULT_NAME,
                    &input_sha256,
                    raw,
                    ClampedPlan::default(),
                    false,
                    Some(message.clone()),
                )
                .await;
                return StepOutcome::PolicyError(message);
            }
        };
        free_cells_oldest_first(&manifest)
    };
    let current_cell_count = {
        let guard = supervisor.lock().await;
        guard.cell_ids().len()
    };

    let clamp_ctx = AutoscaleClampContext {
        current_cell_count,
        min_cells: config.min_cells,
        max_cells: config.max_cells,
        free_cells_oldest_first,
        seconds_since_last_action: last_action_at.map(|at| at.elapsed().as_secs()),
        cooldown_secs: config.cooldown_secs,
    };
    let plan = clamp::clamp_autoscale(&decision, &clamp_ctx);

    let execute_result = execute_plan(&plan, supervisor, data_root, config).await;
    let executed = execute_result.is_ok();
    if executed && !plan.actions.is_empty() {
        *last_action_at = Some(Instant::now());
        *last_action = plan.actions.first().map(|action| LastAction {
            tick,
            action: action_label(action).to_string(),
        });
    }

    let error = execute_result.as_ref().err().map(|error| error.to_string());
    append_entry(
        data_root,
        tick,
        AUTOSCALE_DEFAULT_NAME,
        &input_sha256,
        raw,
        plan,
        executed,
        error,
    )
    .await;

    StepOutcome::Healthy
}

async fn run_placement_step(
    tick: u64,
    supervisor: &Arc<Mutex<Supervisor>>,
    data_root: &Path,
    engine: &PolicyEngine,
    config: &AutoscalerConfig,
) -> StepOutcome {
    let manifest = match ClusterManifest::load(data_root).await {
        Ok(manifest) => manifest,
        Err(error) => {
            return StepOutcome::PolicyError(format!("loading cluster manifest: {error}"));
        }
    };

    let input = snapshot::placement_input(&manifest);
    if input.pending_tenants.is_empty() {
        // Nothing to place this tick: not a decision worth logging.
        return StepOutcome::Healthy;
    }

    let input_value = match serde_json::to_value(&input).context("serializing placement input") {
        Ok(value) => value,
        Err(error) => return StepOutcome::PolicyError(error.to_string()),
    };
    let input_sha256 = sha256_hex(
        serde_json::to_vec(&input_value)
            .unwrap_or_default()
            .as_slice(),
    );

    let raw = match engine.run(PLACEMENT_DEFAULT_NAME, &input_value) {
        Ok(raw) => raw,
        Err(error) => {
            append_entry(
                data_root,
                tick,
                PLACEMENT_DEFAULT_NAME,
                &input_sha256,
                Value::Null,
                ClampedPlan::default(),
                false,
                Some(error.to_string()),
            )
            .await;
            return StepOutcome::PolicyError(error.to_string());
        }
    };

    let decision = match PlacementDecision::parse(&raw) {
        Ok(decision) => decision,
        Err(error) => {
            append_entry(
                data_root,
                tick,
                PLACEMENT_DEFAULT_NAME,
                &input_sha256,
                raw,
                ClampedPlan::default(),
                false,
                Some(error.to_string()),
            )
            .await;
            return StepOutcome::PolicyError(error.to_string());
        }
    };

    let clamp_ctx = PlacementClampContext {
        free_cells: input.free_cells,
        required_replicas: manifest
            .tenants
            .iter()
            .map(|tenant| (tenant.name.clone(), tenant.replicas))
            .collect(),
    };
    let plan = clamp::clamp_placement(&decision, &clamp_ctx);

    let execute_result = execute_plan(&plan, supervisor, data_root, config).await;
    let executed = execute_result.is_ok();
    let error = execute_result.as_ref().err().map(|error| error.to_string());
    append_entry(
        data_root,
        tick,
        PLACEMENT_DEFAULT_NAME,
        &input_sha256,
        raw,
        plan,
        executed,
        error,
    )
    .await;

    StepOutcome::Healthy
}

fn action_label(action: &ClampedAction) -> &'static str {
    match action {
        ClampedAction::ScaleUp => "scale_up",
        ClampedAction::ScaleDown { .. } => "scale_down",
        ClampedAction::Place { .. } => "place",
    }
}

/// Cell ids assigned to no tenant, in manifest (provisioning) order, so
/// `.last()` is the newest -- the ordering [`clamp::clamp_autoscale`]'s
/// "newest first" scale-down rule relies on.
fn free_cells_oldest_first(manifest: &ClusterManifest) -> Vec<String> {
    let assigned: std::collections::HashSet<&str> = manifest
        .tenants
        .iter()
        .flat_map(|tenant| tenant.cells.iter().map(String::as_str))
        .collect();
    manifest
        .cells
        .iter()
        .map(|cell| cell.id.clone())
        .filter(|id| !assigned.contains(id.as_str()))
        .collect()
}

/// Executes every action in `plan` against the live cluster. Autoscale
/// plans and placement plans both flow through here (they share
/// [`ClampedPlan`]'s single action type): a `Place` action only records
/// the manifest's `cells` assignment; the actual schema+wire pass runs
/// once, after every action in the plan is processed, via
/// `burner_mesh::reconcile` (D14/D17: "reuse burner-mesh") -- so N
/// placements in one tick cost one reconcile pass, not N.
async fn execute_plan(
    plan: &ClampedPlan,
    supervisor: &Arc<Mutex<Supervisor>>,
    data_root: &Path,
    config: &AutoscalerConfig,
) -> Result<()> {
    if plan.actions.is_empty() {
        return Ok(());
    }

    let placements: Vec<(&str, &[String])> = plan
        .actions
        .iter()
        .filter_map(|action| match action {
            ClampedAction::Place { tenant, cells } => Some((tenant.as_str(), cells.as_slice())),
            _ => None,
        })
        .collect();
    if !placements.is_empty() {
        let mut manifest = ClusterManifest::load(data_root)
            .await
            .context("loading manifest to record policy placements")?;
        for (tenant, cells) in &placements {
            if let Some(spec) = manifest.tenants.iter_mut().find(|t| t.name == *tenant) {
                spec.cells = cells.to_vec();
            }
        }
        manifest
            .save(data_root)
            .await
            .context("saving manifest with policy-chosen placements")?;
    }

    for action in &plan.actions {
        match action {
            ClampedAction::ScaleUp => {
                execute_scale_up(supervisor, data_root, config).await?;
            }
            ClampedAction::ScaleDown { cell_id } => {
                let mut guard = supervisor.lock().await;
                guard
                    .remove_cell(cell_id, false)
                    .await
                    .with_context(|| format!("removing scale-down cell '{cell_id}'"))?;
            }
            ClampedAction::Place { .. } => {}
        }
    }

    if !placements.is_empty() {
        let mut guard = supervisor.lock().await;
        let outcomes = burner_mesh::reconcile(&mut guard, data_root)
            .await
            .context("reconciling policy-placed tenants")?;
        // Per-tenant isolation (bug-fix round, D25 addendum): only the
        // tenant(s) *this tick* just placed can fail this tick's own
        // action. A different, unrelated tenant coming back degraded in
        // the same reconcile pass is real, visible state (already
        // persisted into the manifest by `reconcile` itself) but must
        // not mark this tick's placement as failed.
        let mut failed = Vec::new();
        for (tenant, _) in &placements {
            match outcomes.iter().find(|outcome| outcome.name() == *tenant) {
                Some(burner_mesh::TenantOutcome::Degraded { reason, .. }) => {
                    failed.push(format!("'{tenant}': {reason}"));
                }
                Some(burner_mesh::TenantOutcome::Ready(_)) | None => {}
            }
        }
        if !failed.is_empty() {
            anyhow::bail!(
                "reconciling policy-placed tenant(s) failed: {}",
                failed.join("; ")
            );
        }
    }
    Ok(())
}

/// Provisions one fresh cell for a scale-up, driven either by an
/// autoscale decision (`execute_plan`) or directly by the admin
/// `ProvisionCells` command (console round, D25). `pub` for that second
/// caller.
///
/// REQUIRED correctness (D25): the supervisor lock is taken *before*
/// `next_cell_index`, not after, and held across both the index scan and
/// the `provision` call. `ProvisionCells` makes this function genuinely
/// multi-caller for the first time (the autoscaler tick and an admin
/// request can now race to scale up); computing the next id before
/// locking would let two concurrent callers both read the same
/// not-yet-created index and collide on the same cell id.
pub async fn execute_scale_up(
    supervisor: &Arc<Mutex<Supervisor>>,
    data_root: &Path,
    config: &AutoscalerConfig,
) -> Result<CellSpec> {
    let mut guard = supervisor.lock().await;

    let index = next_cell_index(data_root).await?;
    let id = format!("cell-{index}");
    let offset = u16::try_from(index)
        .with_context(|| format!("cell index {index} does not fit in a u16 port offset"))?;
    let port = config.base_port.checked_add(offset).with_context(|| {
        format!(
            "base_port {} + cell index {index} overflows a u16 port",
            config.base_port
        )
    })?;

    let spec = CellSpec {
        signing_key_file: burner_cell::identity::key_path(data_root, &id),
        id: id.clone(),
        group: "default".to_string(),
        backend: BackendKind::Lark,
        p2p_port: port,
        bind_addr: config.bind_addr,
        mem_budget_bytes: DEFAULT_MEM_BUDGET_BYTES,
    };
    guard
        .provision(spec.clone())
        .await
        .with_context(|| format!("provisioning scale-up cell '{id}'"))?;
    Ok(spec)
}

/// The next never-before-used cell index for a scale-up: one past the
/// highest `cell-<N>` directory found under `data_root/cells/`, live or
/// previously drained. `Supervisor::remove_cell` deliberately leaves a
/// drained cell's data directory in place specifically so this scan can
/// never recycle an id: a fresh store `open` on a directory that still
/// holds another cell's old files would silently resume that old data
/// instead of starting empty.
async fn next_cell_index(data_root: &Path) -> Result<u64> {
    let cells_dir = data_root.join("cells");
    let mut max_seen: Option<u64> = None;
    let mut entries = match tokio::fs::read_dir(&cells_dir).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => {
            return Err(error).with_context(|| format!("reading {}", cells_dir.display()));
        }
    };
    while let Some(entry) = entries
        .next_entry()
        .await
        .with_context(|| format!("listing {}", cells_dir.display()))?
    {
        if let Some(name) = entry.file_name().to_str()
            && let Some(index) = name
                .strip_prefix("cell-")
                .and_then(|s| s.parse::<u64>().ok())
        {
            max_seen = Some(max_seen.map_or(index, |m| m.max(index)));
        }
    }
    Ok(max_seen.map_or(0, |m| m + 1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use burner_cell::{BackendKind, CellSpec, TenantSpec, TenantStatus};

    fn cell(id: &str) -> CellSpec {
        CellSpec {
            id: id.to_string(),
            group: "default".to_string(),
            backend: BackendKind::Lark,
            p2p_port: 9171,
            bind_addr: "127.0.0.1".parse().unwrap(),
            mem_budget_bytes: DEFAULT_MEM_BUDGET_BYTES,
            signing_key_file: PathBuf::from(format!("/data/keys/{id}.ed25519")),
        }
    }

    #[test]
    fn policy_status_handle_starts_honestly_empty() {
        let status = PolicyStatusHandle::new();
        let snapshot = status.snapshot();
        assert_eq!(snapshot.last_ok_tick, None);
        assert_eq!(snapshot.consecutive_errors, 0);
        assert_eq!(snapshot.last_error, None);
    }

    #[test]
    fn policy_status_handle_tracks_ok_and_error_transitions() {
        let status = PolicyStatusHandle::new();
        status.record_error("boom".to_string());
        status.record_error("boom again".to_string());
        let snapshot = status.snapshot();
        assert_eq!(snapshot.consecutive_errors, 2);
        assert_eq!(snapshot.last_error.as_deref(), Some("boom again"));
        assert_eq!(snapshot.last_ok_tick, None);

        status.record_ok(7);
        let snapshot = status.snapshot();
        assert_eq!(snapshot.consecutive_errors, 0);
        assert_eq!(snapshot.last_ok_tick, Some(7));
    }

    #[test]
    fn free_cells_oldest_first_excludes_assigned_cells_in_manifest_order() {
        let manifest = ClusterManifest {
            version: 1,
            cells: vec![cell("cell-0"), cell("cell-1"), cell("cell-2")],
            tenants: vec![TenantSpec {
                name: "acme-co".to_string(),
                replicas: 1,
                cells: vec!["cell-1".to_string()],
                token_sha256: String::new(),
                status: TenantStatus::Placed,
                admission: None,
                health: Default::default(),
            }],
            autoscaler: AutoscalerSpec::default(),
        };
        assert_eq!(
            free_cells_oldest_first(&manifest),
            vec!["cell-0".to_string(), "cell-2".to_string()]
        );
    }

    #[tokio::test]
    async fn next_cell_index_is_zero_when_no_cells_dir_exists() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(next_cell_index(dir.path()).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn next_cell_index_is_one_past_the_highest_seen_directory() {
        let dir = tempfile::tempdir().unwrap();
        let cells_dir = dir.path().join("cells");
        tokio::fs::create_dir_all(cells_dir.join("cell-0"))
            .await
            .unwrap();
        tokio::fs::create_dir_all(cells_dir.join("cell-3"))
            .await
            .unwrap();
        tokio::fs::create_dir_all(cells_dir.join("cell-1"))
            .await
            .unwrap();
        assert_eq!(next_cell_index(dir.path()).await.unwrap(), 4);
    }

    #[tokio::test]
    async fn next_cell_index_ignores_non_matching_directory_names() {
        let dir = tempfile::tempdir().unwrap();
        let cells_dir = dir.path().join("cells");
        tokio::fs::create_dir_all(cells_dir.join("cell-2"))
            .await
            .unwrap();
        tokio::fs::create_dir_all(cells_dir.join("not-a-cell-dir"))
            .await
            .unwrap();
        assert_eq!(next_cell_index(dir.path()).await.unwrap(), 3);
    }

    #[test]
    fn compute_qps_is_zero_on_the_first_observation() {
        let mut previous = HashMap::new();
        let metrics = GatewayMetrics {
            cell_requests: vec![crate::snapshot::CellRequestCounters {
                cell_id: "cell-0".to_string(),
                count: 500,
                ..Default::default()
            }],
            tenant_admission: Vec::new(),
        };
        let out = compute_qps(metrics, &mut previous, Instant::now());
        assert_eq!(out.cell_requests[0].qps, 0.0);
    }

    #[test]
    fn compute_qps_derives_the_rate_from_the_delta_since_the_last_sample() {
        let mut previous = HashMap::new();
        let t0 = Instant::now();
        previous.insert("cell-0".to_string(), (100u64, t0));

        let metrics = GatewayMetrics {
            cell_requests: vec![crate::snapshot::CellRequestCounters {
                cell_id: "cell-0".to_string(),
                count: 600,
                ..Default::default()
            }],
            tenant_admission: Vec::new(),
        };
        let t1 = t0 + Duration::from_secs(2);
        let out = compute_qps(metrics, &mut previous, t1);
        // (600 - 100) requests over 2 seconds = 250 req/s.
        assert!((out.cell_requests[0].qps - 250.0).abs() < f64::EPSILON);
        assert_eq!(previous.get("cell-0"), Some(&(600u64, t1)));
    }

    #[test]
    fn compute_qps_of_a_quiet_cell_after_load_stops_is_near_zero() {
        let mut previous = HashMap::new();
        let t0 = Instant::now();
        previous.insert("cell-0".to_string(), (1000u64, t0));

        // No new requests since the previous sample.
        let metrics = GatewayMetrics {
            cell_requests: vec![crate::snapshot::CellRequestCounters {
                cell_id: "cell-0".to_string(),
                count: 1000,
                ..Default::default()
            }],
            tenant_admission: Vec::new(),
        };
        let t1 = t0 + Duration::from_secs(1);
        let out = compute_qps(metrics, &mut previous, t1);
        assert_eq!(out.cell_requests[0].qps, 0.0);
    }

    #[test]
    fn action_label_matches_the_clamped_action() {
        assert_eq!(action_label(&ClampedAction::ScaleUp), "scale_up");
        assert_eq!(
            action_label(&ClampedAction::ScaleDown {
                cell_id: "cell-0".to_string()
            }),
            "scale_down"
        );
        assert_eq!(
            action_label(&ClampedAction::Place {
                tenant: "acme-co".to_string(),
                cells: vec![]
            }),
            "place"
        );
    }
}
