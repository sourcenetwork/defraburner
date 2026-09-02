//! `defraburner start`/`up`: provision a fresh cluster (or recover an
//! existing one), dial any configured static peers, reconcile tenants
//! (Phase 2, D14), write the ready-file once everything is up, then serve
//! until SIGINT/SIGTERM and shut down cleanly. Phase 4/D17 adds the policy
//! engine and the autoscale + placement control loop; the console round
//! (D21/D23/D25) adds the admin command channel/executor and, when
//! `announce` is `Some` (the `up` command), a post-readiness banner and a
//! best-effort browser open.

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use burner_cell::{
    BackendKind, CellSpec, CellStatus, ClusterManifest, DEFAULT_MEM_BUDGET_BYTES, RunningCell,
    Supervisor, Watchdog,
};
use burner_mesh::{PeerDialOutcome, TenantReady};
use burner_policy::autoscaler::{AutoscalerConfig, AutoscalerControl};
use burner_policy::snapshot::{CellRequestCounters, GatewayMetrics, TenantAdmissionCounters};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{Mutex, mpsc};

use crate::commands;
use crate::runtime::{self, RuntimeLimits};

/// Announce-mode options, present only for `up` (D21/D25); `start` passes
/// `None` and behaves exactly as before. `open_browser` is the *final*
/// computed decision (`!no_open && a display server is present`), worked
/// out by `up.rs`'s CLI handler before this module ever sees it, so this
/// module stays free of environment-sniffing.
pub struct AnnounceOptions {
    pub open_browser: bool,
}

/// Runs the `start`/`up` subcommand end to end.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    data_root: PathBuf,
    cells: usize,
    bind: IpAddr,
    base_port: u16,
    peers: Vec<String>,
    gateway_addr: SocketAddr,
    ready_file: Option<PathBuf>,
    min_cells: usize,
    max_cells: usize,
    cooldown_secs: u64,
    tick_interval: Duration,
    packages_dir: Option<PathBuf>,
    policy_limits: RuntimeLimits,
    announce: Option<AnnounceOptions>,
) -> Result<()> {
    tokio::fs::create_dir_all(&data_root)
        .await
        .with_context(|| format!("creating data root {}", data_root.display()))?;

    // Built first, before touching any cell: a corrupt or ambiguous
    // --packages-dir override, or a bad policy resource knob, is a
    // startup configuration error, not a runtime PolicyError, so it fails
    // `start`/`up` loudly and fast rather than after cells are already
    // up. Engine construction is hoisted out of burner-policy (console
    // round, operator directive): `runtime::build_engine` is the one
    // place this process builds its afterburner engine; burner-policy
    // only registers and runs packages against the handle it is given.
    let policy_runtime = runtime::build_engine(policy_limits).context("building policy runtime")?;
    let policy_engine = burner_policy::PolicyEngine::load(packages_dir.as_deref(), policy_runtime)
        .context("loading policy packages")?;
    let registered_packages = policy_engine.registered_packages();

    // Recover only if the manifest already records at least one cell, not
    // merely if the manifest *file* exists: `tenant create` (D14, offline
    // provisioning) writes a manifest with a Pending tenant and zero
    // cells before `start` ever runs, and that must still take the
    // provision-fresh path (which itself loads and preserves that
    // already-written manifest: see `Supervisor::provision`'s
    // `load_or_new_manifest`: so the pending tenant is not lost).
    let existing_cells = if ClusterManifest::exists(&data_root) {
        ClusterManifest::load(&data_root)
            .await
            .context("loading cluster manifest")?
            .cells
            .len()
    } else {
        0
    };

    // The wasm database image every cell's fiber instantiates from (D40).
    // Loaded before any cell ignites, because a cell without it comes up
    // with no database at all.
    let (fiber_image, fiber_unavailable_reason) = load_fiber_runtime();
    match fiber_unavailable_reason.as_deref() {
        Some(reason) => tracing::warn!(reason, "cells will ignite without a wasm database"),
        None => tracing::info!("wasm database image loaded; every cell gets one"),
    }

    let supervisor = if existing_cells > 0 {
        tracing::info!(data_root = %data_root.display(), existing_cells, "cluster manifest found, recovering");
        Supervisor::recover_with_fiber_image(&data_root, fiber_image.clone())
            .await
            .context("recovering cluster")?
    } else {
        tracing::info!(
            data_root = %data_root.display(),
            cells,
            "no existing cells recorded, provisioning fresh cells"
        );
        provision_fresh(&data_root, cells, bind, base_port, fiber_image.clone()).await?
    };

    for status in &supervisor.status() {
        tracing::info!(
            cell_id = %status.id,
            peer_id = %status.peer_id,
            marker_ok = status.marker_ok,
            "cell up"
        );
    }

    // Wrapped here, ahead of dial/reconcile/gateway-build, so all three
    // (and the gateway's own ongoing routing, and the admin command
    // executor) share one supervisor.
    let supervisor = Arc::new(Mutex::new(supervisor));

    // Dial, then deadline-poll every successful dial into the dialing
    // cell's own `connected_peers()` before this function ever writes the
    // ready-file (D19): `connect_peer` returning `Ok` only means the dial
    // was accepted, not that the swarm task has registered the connection
    // yet, so a bare unpolled `status_with_connected_peers()` snapshot
    // taken right after `dial_static_peers` would race the swarm. Both
    // passes run inside one `supervisor` lock scope so `running_cells`
    // (borrowed from the guard) stays valid across both.
    let static_peer_outcomes: Vec<PeerDialOutcome> = {
        let guard = supervisor.lock().await;
        let running_cells: Vec<&RunningCell> = guard
            .cell_ids()
            .into_iter()
            .filter_map(|id| guard.running_cell(&id))
            .collect();
        let mut outcomes = burner_mesh::dial_static_peers(&running_cells, &peers)
            .await
            .context("dialing static peers")?;
        burner_mesh::confirm_dialed_peers(&running_cells, &mut outcomes).await;
        outcomes
    };

    // Per-tenant isolation (bug-fix round, D25 addendum): a degraded
    // tenant is logged loudly and excluded from the ready-file, never an
    // abort of the whole `start` over one tenant's problem. Its health is
    // already persisted in the manifest by `reconcile` itself, so it is
    // visible via `/admin/api/overview` the moment the gateway comes up.
    let tenants_ready: Vec<TenantReady> = {
        let mut guard = supervisor.lock().await;
        let outcomes = burner_mesh::reconcile(&mut guard, &data_root)
            .await
            .context("reconciling tenants")?;
        let mut ready = Vec::with_capacity(outcomes.len());
        for outcome in outcomes {
            match outcome {
                burner_mesh::TenantOutcome::Ready(tenant_ready) => ready.push(tenant_ready),
                burner_mesh::TenantOutcome::Degraded { name, reason } => {
                    tracing::error!(
                        tenant = %name, reason = %reason,
                        "tenant degraded during startup reconcile; omitted from the ready-file"
                    );
                }
            }
        }
        ready
    };

    // Shared with the autoscaler loop below (writer) and the gateway's
    // /admin/status (reader): constructed here, before either exists, so
    // a fresh "no tick yet" status is always available.
    let policy_status = Arc::new(burner_policy::PolicyStatusHandle::new());

    // Autoscaler live control (console round, D23): the CLI-derived base
    // config layered with any admin overrides already persisted in the
    // manifest (`PUT /admin/autoscaler`), so a restart honors the last
    // admin-configured values rather than silently reverting to the CLI
    // defaults.
    let autoscaler_spec = ClusterManifest::load(&data_root)
        .await
        .context("loading cluster manifest for autoscaler overrides")?
        .autoscaler;
    let autoscaler_config = AutoscalerConfig {
        min_cells,
        max_cells,
        cooldown_secs,
        tick_interval,
        bind_addr: bind,
        base_port,
        // The base config never authorizes removal; only the manifest
        // override (settable from the dashboard) can turn it on. D41.
        scale_down_enabled: false,
    };
    let autoscaler_control = Arc::new(AutoscalerControl::new(autoscaler_config, autoscaler_spec));

    // The admin command channel (console round, D25): every mutating
    // admin HTTP handler enqueues a `SupervisorCommand` here; the
    // executor (`commands::run`, below) is the only thing that ever
    // dequeues and carries one out, driven on this same never-spawned
    // task alongside the watchdog and autoscaler loops (D12: a
    // `ProvisionCells` command reaches `cell::ignite`, whose returned
    // future is not `Send`).
    let (command_tx, command_rx) = mpsc::channel(burner_cell::COMMAND_CHANNEL_CAPACITY);

    // Shared with the gateway (read-only, for `GET /admin/cells/{id}/inspect`)
    // and the watchdog loop below (the only writer).
    let watchdog = Arc::new(Watchdog::new());

    let runtime_info = burner_gateway::gateway::RuntimeInfo {
        mode: "wasm".to_string(),
        fuel: policy_limits.fuel,
        memory_bytes: policy_limits.memory_bytes,
        timeout_ms: policy_limits.timeout_ms,
        registered_packages,
    };

    // Awaited directly, never spawned: a bind failure (e.g. the gateway
    // port is already in use) must fail `start` loudly, with a real exit
    // code, before the ready-file claims the cluster is up (see
    // `burner_gateway::gateway::build`'s doc comment for why this is
    // split from `serve`, which *is* spawned, below).

    let (gateway_listener, gateway_router, gateway_metrics, admin_token) =
        burner_gateway::gateway::build(
            gateway_addr,
            data_root.clone(),
            supervisor.clone(),
            policy_status.clone(),
            watchdog.clone(),
            autoscaler_control.clone(),
            command_tx,
            runtime_info,
            static_peer_outcomes.clone(),
            fiber_unavailable_reason,
        )
        .await
        .context("starting gateway")?;
    let bound_gateway_addr = gateway_listener
        .local_addr()
        .context("reading the gateway's bound local address")?;

    if let Some(ready_file) = &ready_file {
        let statuses = supervisor.lock().await.status_with_connected_peers().await;
        write_ready_file(ready_file, &statuses, &tenants_ready, &static_peer_outcomes)
            .await
            .with_context(|| format!("writing ready-file {}", ready_file.display()))?;
    }

    if let Some(announce) = &announce {
        let cell_count = supervisor.lock().await.cell_ids().len();
        print_banner(&data_root, bound_gateway_addr, &admin_token, cell_count);
        if announce.open_browser {
            let dashboard_url =
                format!("http://{bound_gateway_addr}/dashboard?token={admin_token}");
            spawn_browser(&dashboard_url);
        }
    }

    let watchdog_supervisor = supervisor.clone();

    // Adapts burner-gateway's own counter shapes into burner-policy's
    // (burner-policy does not depend on burner-gateway; see
    // `burner_policy::snapshot`'s doc comment).
    let gateway_metrics_fn: Arc<dyn Fn() -> GatewayMetrics + Send + Sync> = {
        let gateway_metrics = gateway_metrics.clone();
        Arc::new(move || GatewayMetrics {
            cell_requests: gateway_metrics
                .cell_requests()
                .into_iter()
                .map(|c| CellRequestCounters {
                    cell_id: c.cell_id,
                    count: c.count,
                    sum_ms: c.sum_micros as f64 / 1000.0,
                    max_ms: c.max_micros as f64 / 1000.0,
                    // Filled in by `burner_policy::autoscaler`'s
                    // `compute_qps` (tick-to-tick delta), which needs
                    // memory of the previous tick this stateless glue
                    // closure does not hold; see `CellSnapshot::qps`'s
                    // doc comment.
                    qps: 0.0,
                })
                .collect(),
            tenant_admission: gateway_metrics
                .tenant_admission()
                .into_iter()
                .map(|t| TenantAdmissionCounters {
                    tenant: t.tenant,
                    allowed: t.allowed,
                    rejected: t.rejected,
                })
                .collect(),
        })
    };

    // Safe to `tokio::spawn` (D12): serving already-bound HTTP connections
    // never calls `cell::ignite`, so nothing on this path is the non-`Send`
    // future D12 guards against (unlike `watchdog.run`, which still cannot
    // be spawned: see its doc comment -- and stays a plain `select!`
    // branch on this task for the same reason as before).
    let mut gateway_handle = tokio::spawn(burner_gateway::gateway::serve(
        gateway_listener,
        gateway_router,
    ));

    tokio::select! {
        _ = watchdog.run(watchdog_supervisor, burner_cell::DEFAULT_PROBE_INTERVAL) => {}
        // NOT spawned (D12): scale-up execution reaches
        // `Supervisor::provision` -> `cell::ignite`, whose returned
        // future is not `Send` whenever libp2p is configured, exactly the
        // constraint `watchdog.run` above already carries.
        _ = burner_policy::autoscaler::run(
            supervisor.clone(),
            data_root.clone(),
            policy_engine,
            autoscaler_control.clone(),
            policy_status,
            gateway_metrics_fn,
        ) => {}
        // NOT spawned (D12, D25): `ProvisionCells` reaches
        // `execute_scale_up` -> `Supervisor::provision` -> `cell::ignite`,
        // the same non-`Send` path as the autoscaler's own scale-up.
        _ = commands::run(
            command_rx,
            supervisor.clone(),
            data_root.clone(),
            autoscaler_control,
        ) => {}
        signal = wait_for_shutdown_signal() => {
            let signal = signal.context("waiting for a shutdown signal")?;
            tracing::info!(signal, "shutdown signal received, draining cells");
        }
        gateway_result = &mut gateway_handle => {
            match gateway_result {
                Ok(Ok(())) => tracing::warn!("gateway server exited normally (unexpected: it runs until shutdown)"),
                Ok(Err(error)) => tracing::error!(error = %error, "gateway server exited with an error"),
                Err(join_error) => tracing::error!(error = %join_error, "gateway server task panicked"),
            }
        }
    }
    gateway_handle.abort();

    supervisor.lock().await.shutdown_all().await;
    Ok(())
}

async fn provision_fresh(
    data_root: &Path,
    cells: usize,
    bind: IpAddr,
    base_port: u16,
    fiber_image: Option<burner_fiber::FiberImage>,
) -> Result<Supervisor> {
    let mut supervisor = Supervisor::new(data_root);
    if let Some(image) = fiber_image {
        supervisor = supervisor.with_fiber_image(image);
    }
    for i in 0..cells {
        let id = format!("cell-{i}");
        let offset = u16::try_from(i)
            .with_context(|| format!("cell index {i} does not fit in a u16 port offset"))?;
        let port = base_port.checked_add(offset).with_context(|| {
            format!("base_port {base_port} + cell index {i} overflows a u16 port")
        })?;

        let spec = CellSpec {
            signing_key_file: burner_cell::identity::key_path(data_root, &id),
            id: id.clone(),
            group: "default".to_string(),
            backend: BackendKind::Regolith,
            p2p_port: port,
            bind_addr: bind,
            mem_budget_bytes: DEFAULT_MEM_BUDGET_BYTES,
        };
        supervisor
            .provision(spec)
            .await
            .with_context(|| format!("provisioning cell '{id}'"))?;
    }
    Ok(supervisor)
}

#[derive(serde::Serialize)]
struct ReadyFilePayload<'a> {
    cells: &'a [CellStatus],
    tenants: &'a [TenantReady],
    static_peer_outcomes: &'a [PeerDialOutcome],
}

/// Atomically writes the ready-file:
/// `{"cells": [...], "tenants": [...], "static_peer_outcomes": [...]}`,
/// written to `<path>.tmp` then renamed over `path`.
async fn write_ready_file(
    path: &Path,
    statuses: &[CellStatus],
    tenants: &[TenantReady],
    static_peer_outcomes: &[PeerDialOutcome],
) -> Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("creating ready-file directory {}", parent.display()))?;
    }

    let payload = ReadyFilePayload {
        cells: statuses,
        tenants,
        static_peer_outcomes,
    };
    let json = serde_json::to_vec_pretty(&payload).context("serializing ready-file")?;

    let tmp_path = PathBuf::from(format!("{}.tmp", path.display()));
    tokio::fs::write(&tmp_path, &json)
        .await
        .with_context(|| format!("writing {}", tmp_path.display()))?;
    tokio::fs::rename(&tmp_path, path)
        .await
        .with_context(|| format!("renaming {} to {}", tmp_path.display(), path.display()))?;
    Ok(())
}

/// Waits for SIGINT or SIGTERM, returning which one fired (for logging).
async fn wait_for_shutdown_signal() -> Result<&'static str> {
    let mut sigint = signal(SignalKind::interrupt()).context("installing a SIGINT handler")?;
    let mut sigterm = signal(SignalKind::terminate()).context("installing a SIGTERM handler")?;

    Ok(tokio::select! {
        _ = sigint.recv() => "SIGINT",
        _ = sigterm.recv() => "SIGTERM",
    })
}

/// Prints the exact post-readiness banner `up` promises (D21/D25), with a
/// blank line on each side (zero-config contract, operator directive) so
/// it stands out in a terminal that just streamed compile output:
///
/// ```text
///
/// defraburner up
///   data:      <data_root>
///   gateway:   http://<gateway_addr>
///   dashboard: http://<gateway_addr>/dashboard?token=<admin-token>
///   cells:     <n> running
///
/// ```
///
/// `gateway_addr` is the *actually bound* address (from
/// `TcpListener::local_addr`), not the nominally requested one: the
/// gateway's own bind resilience (`burner_gateway::gateway::build`) may
/// have scanned to a nearby free port if the requested one was taken, and
/// the dashboard URL must be dialable, not nominal.
fn print_banner(data_root: &Path, gateway_addr: SocketAddr, admin_token: &str, cell_count: usize) {
    println!();
    println!("defraburner up");
    println!("  data:      {}", data_root.display());
    println!("  gateway:   http://{gateway_addr}");
    println!("  dashboard: http://{gateway_addr}/dashboard?token={admin_token}");
    println!("  cells:     {cell_count} running");
    println!();
}

/// Best-effort opens `url` via `xdg-open`, spawned detached (stdio nulled,
/// never awaited) so a missing or slow browser can never block or fail
/// `up` itself. Caller (`run`, above) has already applied the `!no_open`
/// and display-server gates via `AnnounceOptions::open_browser`.
fn spawn_browser(url: &str) {
    let result = std::process::Command::new("xdg-open")
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    if let Err(error) = result {
        tracing::warn!(error = %error, url, "failed to spawn xdg-open for the dashboard URL");
    }
}

/// Name of the fiber package artifact `just package-defradb` produces.
const FIBER_PACKAGE_FILE: &str = "defraburner-defradb-0.1.0.afb";

/// Environment override for the fiber package's location.
const FIBER_PACKAGE_ENV: &str = "DEFRABURNER_FIBER_PACKAGE";

/// Finds and compiles the wasm database package every cell's fiber runs.
///
/// Returns `(image, reason_it_is_absent)`. Exactly one is `Some`.
///
/// Absent is a legitimate state, not a failure: the `.afb` is a build
/// output (`.gitignore` excludes it), so a tree that has not run
/// `just package-defradb` simply has no wasm databases, and `just start`
/// must still come up. It is never silent, though: the reason travels
/// into the gateway so `/admin/cells/{id}/db` and the dashboard say *why*
/// rather than reporting a bare absence.
///
/// The package is not embedded with `include_bytes!` the way the policy
/// wasms are, deliberately: at ~1.4 MiB it would make every build depend
/// on a wasm toolchain and an extra rustup target, which would cost the
/// zero-flag front door on a fresh clone.
fn load_fiber_runtime() -> (Option<burner_fiber::FiberImage>, Option<String>) {
    let mut searched = Vec::new();

    // An explicit override wins, and a bad one is an error rather than a
    // silent fall-through to a different package than the operator named.
    if let Ok(explicit) = std::env::var(FIBER_PACKAGE_ENV) {
        let path = std::path::PathBuf::from(&explicit);
        return match burner_fiber::FiberImage::from_afb_path(&path) {
            Ok(image) => (Some(image), None),
            Err(error) => (
                None,
                Some(format!(
                    "{FIBER_PACKAGE_ENV}={explicit} could not be loaded: {error:#}"
                )),
            ),
        };
    }

    for candidate in fiber_package_candidates() {
        if candidate.is_file() {
            return match burner_fiber::FiberImage::from_afb_path(&candidate) {
                Ok(image) => (Some(image), None),
                Err(error) => (
                    None,
                    Some(format!(
                        "found {} but could not load it: {error:#}",
                        candidate.display()
                    )),
                ),
            };
        }
        searched.push(candidate.display().to_string());
    }

    (
        None,
        Some(format!(
            "the fiber package was not found (looked in: {}). \
             Build it with `just package-defradb`, or point \
             {FIBER_PACKAGE_ENV} at a .afb.",
            searched.join(", ")
        )),
    )
}

/// Where to look for the fiber package, in order: the repo layout relative
/// to the working directory (how `just start` runs), then beside the
/// executable (how a deployed binary ships).
fn fiber_package_candidates() -> Vec<std::path::PathBuf> {
    let mut candidates =
        vec![std::path::PathBuf::from("packages/defradb").join(FIBER_PACKAGE_FILE)];
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        candidates.push(dir.join(FIBER_PACKAGE_FILE));
        candidates.push(dir.join("packages/defradb").join(FIBER_PACKAGE_FILE));
    }
    candidates
}
