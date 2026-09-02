//! The admin command executor (console round, D25): the single-writer
//! task that dequeues `SupervisorCommand`s from the gateway's admin
//! handlers and carries them out, driven on `start.rs`'s `select!`
//! alongside the watchdog and autoscaler loops: never spawned (D12):
//! `ProvisionCells` reaches `Supervisor::provision` -> `cell::ignite`,
//! whose returned future is not `Send` whenever libp2p is configured, so
//! it can never run inside an axum handler's spawned task. Every
//! mutating admin HTTP handler enqueues a command and awaits its reply
//! instead of touching the supervisor, the manifest, or the autoscaler
//! control itself; `run` here is the only thing that ever dequeues one.
//!
//! Commands are processed strictly one at a time (a plain `while let`
//! loop, never a per-command spawn), so every command: and,
//! transitively, every manifest read-modify-write it performs: is
//! naturally serialized against every other admin command with zero
//! extra locking discipline beyond `supervisor`'s own async `Mutex`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use burner_cell::{
    AdmissionOverride, AutoscalerPatch, ClusterManifest, DropTenantOutcome, ProvisionOutcome,
    Supervisor, SupervisorCommand, TenantCommandError,
};
use burner_policy::autoscaler::AutoscalerControl;
use tokio::sync::{Mutex, mpsc};

// No `use defra_http::P2POperations;` needed: every call below goes
// through `p2p.ops(): &Arc<dyn defra_http::P2POperations>` (a trait
// object), and Rust resolves trait-object method calls without the
// trait itself being in scope: unlike a generic `T: Trait` bound.
// Mirrors `burner_mesh::static_peers`/`wiring`'s own P2P call sites,
// verified there via `cargo check` flagging the explicit import as
// unused.

/// Runs the admin command executor forever (until the process shuts
/// down). See the module doc comment for why this is driven directly on
/// the caller's task, never spawned.
pub async fn run(
    mut command_rx: mpsc::Receiver<SupervisorCommand>,
    supervisor: Arc<Mutex<Supervisor>>,
    data_root: PathBuf,
    autoscaler_control: Arc<AutoscalerControl>,
) -> ! {
    while let Some(command) = command_rx.recv().await {
        handle(command, &supervisor, &data_root, &autoscaler_control).await;
    }
    // Every sender (every gateway admin handler, via `GatewayState`)
    // holds a clone of the `mpsc::Sender` for the process's lifetime, so
    // `recv()` returning `None` (every sender dropped) never happens in
    // practice. If it somehow did, idling here is the honest choice: it
    // keeps this branch's `-> !` contract (never claim a normal return
    // from a loop that is supposed to run forever) rather than quietly
    // exiting.
    std::future::pending().await
}

async fn handle(
    command: SupervisorCommand,
    supervisor: &Arc<Mutex<Supervisor>>,
    data_root: &Path,
    autoscaler_control: &Arc<AutoscalerControl>,
) {
    match command {
        SupervisorCommand::ProvisionCells { count, reply } => {
            let outcomes = provision_cells(supervisor, data_root, autoscaler_control, count).await;
            let _ = reply.send(outcomes);
        }
        SupervisorCommand::DrainCell { id, reply } => {
            let mut guard = supervisor.lock().await;
            let result = guard.remove_cell(&id, false).await;
            drop(guard);
            let _ = reply.send(result);
        }
        SupervisorCommand::DropTenant {
            name,
            retire,
            reply,
        } => {
            let result = drop_tenant(supervisor, data_root, &name, retire).await;
            let _ = reply.send(result);
        }
        SupervisorCommand::RotateTenantToken { name, reply } => {
            let result = rotate_tenant_token(data_root, &name).await;
            let _ = reply.send(result);
        }
        SupervisorCommand::SetTenantAdmission {
            name,
            admission,
            reply,
        } => {
            let result = set_tenant_admission(data_root, &name, admission).await;
            let _ = reply.send(result);
        }
        SupervisorCommand::SetAutoscaler { patch, reply } => {
            let result = set_autoscaler(data_root, autoscaler_control, patch).await;
            let _ = reply.send(result);
        }
        SupervisorCommand::ForceAutoscalerTick { reply } => {
            autoscaler_control.force_tick();
            let _ = reply.send(());
        }
        SupervisorCommand::DialPeer {
            cell_id,
            addr,
            reply,
        } => {
            let result = dial_peer(supervisor, &cell_id, &addr).await;
            let _ = reply.send(result);
        }
    }
}

/// Provisions `count` cells one at a time via `burner_policy::autoscaler`'s
/// `execute_scale_up` (D25: the exact same lock-before-index-scan path
/// the autoscaler's own scale-up uses, now genuinely multi-caller), never
/// concurrently within one request: keeping every id/port assignment
/// strictly sequential is what makes the underlying `next_cell_index`
/// scan race-free even under a `count > 1` request. Each attempt's
/// outcome is independent: a failure partway through never rolls back
/// the cells already provisioned.
async fn provision_cells(
    supervisor: &Arc<Mutex<Supervisor>>,
    data_root: &Path,
    autoscaler_control: &Arc<AutoscalerControl>,
    count: usize,
) -> Vec<ProvisionOutcome> {
    let config = autoscaler_control.effective().await;
    let mut outcomes = Vec::with_capacity(count);
    for _ in 0..count {
        match burner_policy::autoscaler::execute_scale_up(supervisor, data_root, &config).await {
            Ok(spec) => {
                let peer_id = {
                    let guard = supervisor.lock().await;
                    guard
                        .running_cell(&spec.id)
                        .map(|cell| cell.peer_id.clone())
                };
                outcomes.push(ProvisionOutcome {
                    id: Some(spec.id),
                    peer_id,
                    error: None,
                });
            }
            Err(error) => {
                outcomes.push(ProvisionOutcome {
                    id: None,
                    peer_id: None,
                    error: Some(format!("{error:#}")),
                });
            }
        }
    }
    outcomes
}

/// The tenant's declared collection names, read from its stored SDL
/// (`burner_mesh::tenant_sdl_path`) exactly like `start.rs`'s reconcile
/// path does. Used only to unsubscribe them on drop; a read/parse
/// failure here is not fatal to the drop itself (see `drop_tenant`'s own
/// comment on that call site).
async fn load_tenant_collections(data_root: &Path, name: &str) -> Result<Vec<String>> {
    let sdl_path = burner_mesh::tenant_sdl_path(data_root, name);
    let sdl = tokio::fs::read_to_string(&sdl_path)
        .await
        .with_context(|| format!("reading tenant schema {}", sdl_path.display()))?;
    let collections = query::parse_sdl(&sdl)
        .map_err(|error| anyhow!("SDL parse error in {name}'s schema: {error}"))?;
    Ok(collections.into_iter().map(|c| c.name).collect())
}

/// Drops tenant `name` (D23): unsubscribes its collections on its cells
/// (best-effort per cell: a cell that has already gone away, or an
/// unsubscribe RPC that fails, must never block revoking the token,
/// which is the operator-facing point of this command), removes its
/// placement and the tenant record itself from the manifest, then, when
/// `retire` is set, drains and erases its cells (including their data
/// directories). The tenant is removed from the manifest *before* any
/// cell retirement: `Supervisor::remove_cell`'s own "assigned to a
/// tenant" guard reads the manifest fresh, so this ordering is what lets
/// a `retire`-requested cell actually be removed instead of being
/// refused as still tenant-owned.
async fn drop_tenant(
    supervisor: &Arc<Mutex<Supervisor>>,
    data_root: &Path,
    name: &str,
    retire: bool,
) -> Result<DropTenantOutcome, TenantCommandError> {
    let mut manifest = ClusterManifest::load(data_root).await.map_err(|error| {
        TenantCommandError::Failed(format!("loading cluster manifest: {error}"))
    })?;

    let Some(index) = manifest.tenants.iter().position(|t| t.name == name) else {
        return Err(TenantCommandError::NotFound(name.to_string()));
    };
    let tenant = manifest.tenants[index].clone();

    if !tenant.cells.is_empty() {
        let collections = load_tenant_collections(data_root, name).await;
        let guard = supervisor.lock().await;
        for cell_id in &tenant.cells {
            let Some(cell) = guard.running_cell(cell_id) else {
                continue;
            };
            let Some(p2p) = cell.node.p2p() else {
                continue;
            };
            match &collections {
                Ok(collections) => {
                    if let Err(error) = p2p.ops().remove_collections(collections.clone()).await {
                        tracing::warn!(
                            tenant = name,
                            cell_id,
                            error = %error,
                            "unsubscribing tenant collections on drop failed (continuing)"
                        );
                    }
                }
                Err(error) => {
                    tracing::warn!(
                        tenant = name,
                        error = %error,
                        "reading tenant schema to unsubscribe collections on drop failed (continuing)"
                    );
                }
            }
        }
    }

    manifest.tenants.remove(index);
    manifest
        .save(data_root)
        .await
        .map_err(|error| TenantCommandError::Failed(format!("saving cluster manifest: {error}")))?;

    let mut retired_cells = Vec::new();
    if retire {
        let mut guard = supervisor.lock().await;
        for cell_id in &tenant.cells {
            match guard.remove_cell(cell_id, true).await {
                Ok(()) => retired_cells.push(cell_id.clone()),
                Err(error) => {
                    tracing::error!(
                        tenant = name,
                        cell_id,
                        error = %error,
                        "retiring a dropped tenant's cell failed"
                    );
                }
            }
        }
    }

    let data_remains_on_cells = if retire {
        Vec::new()
    } else {
        tenant.cells.clone()
    };

    Ok(DropTenantOutcome {
        name: name.to_string(),
        data_remains_on_cells,
        retired_cells,
    })
}

/// Issues a fresh bearer token for `name`, replacing (and immediately
/// invalidating, once the caller rebuilds the routing table) its
/// previous one.
async fn rotate_tenant_token(data_root: &Path, name: &str) -> Result<String, TenantCommandError> {
    let mut manifest = ClusterManifest::load(data_root).await.map_err(|error| {
        TenantCommandError::Failed(format!("loading cluster manifest: {error}"))
    })?;
    let Some(tenant) = manifest.tenants.iter_mut().find(|t| t.name == name) else {
        return Err(TenantCommandError::NotFound(name.to_string()));
    };

    let issued = burner_gateway::auth::issue()
        .map_err(|error| TenantCommandError::Failed(format!("issuing tenant token: {error}")))?;
    tenant.token_sha256 = issued.digest_hex;

    manifest
        .save(data_root)
        .await
        .map_err(|error| TenantCommandError::Failed(format!("saving cluster manifest: {error}")))?;
    Ok(issued.token_hex)
}

/// Sets (or, when `admission` is `None`, clears) tenant `name`'s
/// per-tenant GCRA admission override, persisted in the manifest. The
/// caller applies it to the live `Admission` bucket (that is gateway
/// state this executor has no handle to by design; see
/// `admin_tenants::admin_set_tenant_admission`'s own call site).
async fn set_tenant_admission(
    data_root: &Path,
    name: &str,
    admission: Option<AdmissionOverride>,
) -> Result<(), TenantCommandError> {
    let mut manifest = ClusterManifest::load(data_root).await.map_err(|error| {
        TenantCommandError::Failed(format!("loading cluster manifest: {error}"))
    })?;
    let Some(tenant) = manifest.tenants.iter_mut().find(|t| t.name == name) else {
        return Err(TenantCommandError::NotFound(name.to_string()));
    };
    tenant.admission = admission;

    manifest
        .save(data_root)
        .await
        .map_err(|error| TenantCommandError::Failed(format!("saving cluster manifest: {error}")))?;
    Ok(())
}

/// Applies `patch` to the live autoscaler control, then persists the
/// resulting override layer in the manifest. If persistence fails, the
/// live control is reverted to what it was before this call: a config
/// change that "took" live but did not survive a restart would be a
/// silent inconsistency between what the dashboard shows and what a
/// restart actually restores, which is worse than simply rejecting the
/// request.
async fn set_autoscaler(
    data_root: &Path,
    control: &Arc<AutoscalerControl>,
    patch: AutoscalerPatch,
) -> Result<(), String> {
    let previous = control.spec_snapshot().await;
    control.apply_patch(patch).await?;
    let spec = control.spec_snapshot().await;

    let save_result = save_autoscaler_spec(data_root, spec).await;
    if let Err(error) = &save_result {
        let revert = AutoscalerPatch {
            min_cells: previous.min_cells,
            max_cells: previous.max_cells,
            cooldown_secs: previous.cooldown_secs,
            tick_interval_secs: previous.tick_interval_secs,
            paused: Some(previous.paused),
            scale_down_enabled: Some(previous.scale_down_enabled),
        };
        // Best-effort: `revert` is built from a snapshot this same
        // control already validated once (`previous`), so re-applying it
        // cannot fail its own validation; only a defensive `let _` in
        // case that ever changes.
        let _ = control.apply_patch(revert).await;
        tracing::error!(error = %error, "persisting autoscaler override failed; reverted the live control");
    }
    save_result
}

async fn save_autoscaler_spec(
    data_root: &Path,
    spec: burner_cell::AutoscalerSpec,
) -> Result<(), String> {
    let mut manifest = ClusterManifest::load(data_root)
        .await
        .map_err(|error| format!("loading cluster manifest: {error}"))?;
    manifest.autoscaler = spec;
    manifest
        .save(data_root)
        .await
        .map_err(|error| format!("saving cluster manifest: {error}"))?;
    Ok(())
}

/// Dials `addr` (a multiaddr carrying a `/p2p/<peer-id>` suffix --
/// `connect_peer` itself validates and rejects a malformed address, so
/// that grammar is not re-checked here, mirroring
/// `burner_mesh::static_peers::dial_static_peers`) from cell `cell_id`.
async fn dial_peer(
    supervisor: &Arc<Mutex<Supervisor>>,
    cell_id: &str,
    addr: &str,
) -> Result<(), String> {
    let guard = supervisor.lock().await;
    let Some(cell) = guard.running_cell(cell_id) else {
        return Err(format!("cell '{cell_id}' not found"));
    };
    let Some(p2p) = cell.node.p2p() else {
        return Err(format!("cell '{cell_id}' has no p2p system"));
    };
    p2p.ops()
        .connect_peer(addr)
        .await
        .map_err(|error| anyhow!(error).to_string())
}
