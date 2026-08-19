//! Tenant reconciliation: brings the manifest's tenant records and the
//! live cluster into agreement. Called during `defraburner start` (once,
//! after the supervisor's cells are up and any static peers are dialed)
//! and, since the console round, live and repeatedly from admin tenant
//! create/drop and the autoscaler's own placement step (D14: declarative
//! provisioning, `tenant create` only edits the manifest offline; a
//! reconcile pass is what actually places a tenant).
//!
//! Respects D12: nothing here reaches `burner_cell::cell::ignite` (tenants
//! are placed onto cells the supervisor already has running), so
//! `reconcile` is a plain `async fn`, safe to `.await` directly on the
//! caller's task.
//!
//! Bug-fix round (D25 addendum), per-tenant isolation: a single tenant's
//! placement or (re-)wiring failure never aborts the whole pass and never
//! surfaces as an unrelated caller's error. Observed live: creating tenant
//! X failed with a 500 because an already-`Placed`, unrelated tenant Y's
//! re-wiring timed out waiting for a gossipsub join event that had
//! already fired and would never re-arrive (see `wiring::wire_group`'s
//! doc comment for the mechanism). `reconcile` now returns a
//! [`TenantOutcome`] per tenant instead of bailing on the first error:
//! every tenant gets its own attempt, its own manifest-persisted
//! [`burner_cell::TenantHealth`], and its own place in the returned list,
//! so a caller that cares about one specific tenant (an admin create/drop
//! handler, the autoscaler's own placement step) can check exactly that
//! tenant's outcome without being at the mercy of every other tenant's.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use burner_cell::{ClusterManifest, RunningCell, Supervisor, TenantHealth, TenantStatus};

use crate::placement;
use crate::wiring::{ensure_group_connected, wire_group};

/// Convergence report for one successfully reconciled tenant, folded into
/// the `start --ready-file` payload.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TenantReady {
    pub name: String,
    pub cells: Vec<String>,
    pub collections: Vec<String>,
}

/// One tenant's own reconcile outcome, independent of every other
/// tenant's (see the module doc comment). `name` is always populated on
/// both variants so a caller never has to separately track which tenant
/// an outcome belongs to.
#[derive(Debug, Clone)]
pub enum TenantOutcome {
    Ready(TenantReady),
    Degraded { name: String, reason: String },
}

impl TenantOutcome {
    pub fn name(&self) -> &str {
        match self {
            TenantOutcome::Ready(ready) => &ready.name,
            TenantOutcome::Degraded { name, .. } => name,
        }
    }
}

/// Reconciles every tenant recorded in `data_root`'s cluster manifest
/// against `supervisor`'s live cells, one tenant at a time, isolated from
/// every other tenant's outcome:
///
/// - `Pending`: placed (`placement::place` picks free cells), schema'd
///   (`add_schema` from the tenant's stored SDL), wired
///   (`wiring::wire_group`), then flipped to `Placed` and saved -- but
///   only on full success; a failure at any step leaves the tenant
///   `Pending` (no cells committed) with its failure recorded in
///   `health`, rather than a half-placed tenant.
/// - `Placed`: its assigned cells are verified as currently running
///   (a missing cell degrades the tenant, naming it, rather than hanging
///   the whole pass on a dead peer), then re-wired (idempotent; see
///   `wiring::wire_group`'s doc comment). Its existing `cells`/`status`
///   are never touched by a re-wiring failure -- only `health` changes,
///   since the placement itself already succeeded in an earlier pass.
///
/// The outer `Result::Err` is reserved for setup failures before any
/// per-tenant attempt could even begin (the manifest itself failing to
/// load); once the loop starts, every tenant's own failure becomes a
/// `TenantOutcome::Degraded` entry, never an early return.
pub async fn reconcile(
    supervisor: &mut Supervisor,
    data_root: &Path,
) -> Result<Vec<TenantOutcome>> {
    let mut manifest = ClusterManifest::load(data_root)
        .await
        .context("loading cluster manifest for tenant reconciliation")?;

    let mut outcomes = Vec::with_capacity(manifest.tenants.len());
    for i in 0..manifest.tenants.len() {
        let name = manifest.tenants[i].name.clone();
        let status = manifest.tenants[i].status;

        let attempt = match status {
            TenantStatus::Pending => {
                reconcile_pending(supervisor, &mut manifest, i, &name, data_root).await
            }
            TenantStatus::Placed => reconcile_placed(supervisor, &manifest, &name, data_root).await,
        };

        match attempt {
            Ok(ready) => {
                if manifest.tenants[i].health != TenantHealth::Ok {
                    manifest.tenants[i].health = TenantHealth::Ok;
                    if let Err(error) = manifest.save(data_root).await {
                        tracing::error!(
                            tenant = %name, error = %error,
                            "failed to persist a recovered tenant's health"
                        );
                    }
                }
                outcomes.push(TenantOutcome::Ready(ready));
            }
            Err(error) => {
                let reason = format!("{error:#}");
                tracing::error!(
                    tenant = %name, error = %reason,
                    "tenant reconcile failed; marking degraded, continuing with other tenants"
                );
                manifest.tenants[i].health = TenantHealth::Degraded {
                    reason: reason.clone(),
                    since_ms: now_ms(),
                };
                if let Err(save_error) = manifest.save(data_root).await {
                    tracing::error!(
                        tenant = %name, error = %save_error,
                        "failed to persist a degraded tenant's health"
                    );
                }
                outcomes.push(TenantOutcome::Degraded { name, reason });
            }
        }
    }

    Ok(outcomes)
}

/// The `Pending` branch of one tenant's reconcile attempt: place, schema,
/// wire. Only commits `cells`/`status` to `manifest` on full success (see
/// [`reconcile`]'s own doc comment for why a partial failure leaves the
/// tenant `Pending`, not half-placed).
async fn reconcile_pending(
    supervisor: &mut Supervisor,
    manifest: &mut ClusterManifest,
    index: usize,
    name: &str,
    data_root: &Path,
) -> Result<TenantReady> {
    let cell_ids =
        placement::place(manifest, name).with_context(|| format!("placing tenant '{name}'"))?;
    let sdl = load_tenant_sdl(data_root, name).await?;
    let collections =
        collection_names(&sdl).with_context(|| format!("parsing schema for tenant '{name}'"))?;

    let cells = resolve_running_cells(supervisor, &cell_ids)
        .with_context(|| format!("resolving cells placed for tenant '{name}'"))?;
    for cell in &cells {
        // Idempotent ENSURE, not a bare call (bug-fix round, D25
        // addendum): a tenant recreated under the same name after a
        // plain drop (which keeps data, by design -- D23) lands back on
        // a cell whose local schema was never removed, so a repeat
        // `add_schema` for the identical collections fails loudly with
        // upstream's `CollectionAlreadyExists` -- observed live,
        // recreating "acme-co" with the same schema 500'd. Checked
        // directly against the node's own collection list (the same
        // synchronous `get_collection` lookup
        // `topic_ready::resolve_collection_topic` already relies on),
        // not by string-matching the error.
        if schema_already_registered(&cell.node, &collections) {
            continue;
        }
        cell.node.add_schema(&sdl).await.with_context(|| {
            format!(
                "adding schema for tenant '{name}' on cell '{}'",
                cell.spec.id
            )
        })?;
    }

    let mut confirmed = supervisor.confirmed_topic_joins_snapshot();
    let wire_result = wire_group(&cells, &collections, &mut confirmed)
        .await
        .with_context(|| format!("wiring group for tenant '{name}'"));
    drop(cells); // release the borrow of `supervisor` before merging back
    supervisor.merge_confirmed_topic_joins(confirmed);
    wire_result?;

    manifest.tenants[index].cells = cell_ids.clone();
    manifest.tenants[index].status = TenantStatus::Placed;
    manifest
        .save(data_root)
        .await
        .with_context(|| format!("saving manifest after placing tenant '{name}'"))?;

    Ok(TenantReady {
        name: name.to_string(),
        cells: cell_ids,
        collections,
    })
}

/// The `Placed` branch of one tenant's reconcile attempt: verify its
/// assigned cells are actually running (bug 3: a missing cell degrades
/// the tenant, naming it, instead of hanging the whole pass on a dead
/// peer), then re-wire via [`ensure_group_connected`] -- connectivity
/// confirmed, `add_collections` re-issued (idempotent), but never a wait
/// on a topic-join event (D25 "the real bug" fix: see that function's own
/// doc comment for why that event cannot be observed here -- upstream
/// already restored the subscription from disk at its own startup,
/// before this call ever runs). A cell that cannot be positively
/// confirmed connected is the real, observable problem this branch still
/// degrades the tenant for. Never mutates `cells`/`status`: those were
/// already committed by an earlier successful placement.
async fn reconcile_placed(
    supervisor: &Supervisor,
    manifest: &ClusterManifest,
    name: &str,
    data_root: &Path,
) -> Result<TenantReady> {
    let cell_ids = manifest
        .tenants
        .iter()
        .find(|t| t.name == name)
        .map(|t| t.cells.clone())
        .ok_or_else(|| anyhow!("tenant '{name}' vanished from the manifest mid-reconcile"))?;
    let sdl = load_tenant_sdl(data_root, name).await?;
    let collections =
        collection_names(&sdl).with_context(|| format!("parsing schema for tenant '{name}'"))?;

    let cells = resolve_running_cells(supervisor, &cell_ids)
        .with_context(|| format!("verifying cells assigned to tenant '{name}' are running"))?;

    ensure_group_connected(&cells, &collections)
        .await
        .with_context(|| format!("re-wiring group for tenant '{name}'"))?;

    Ok(TenantReady {
        name: name.to_string(),
        cells: cell_ids,
        collections,
    })
}

/// True if every one of `collections` already exists on `node`'s local
/// database, so a caller can skip a redundant `add_schema` call entirely
/// rather than have it fail on an already-registered collection. `false`
/// (never skip) unless *every* collection is confirmed present: a
/// partially-applied schema (unusual, but not impossible after an
/// interrupted previous attempt) still needs the real `add_schema` call
/// to finish registering whatever is missing.
fn schema_already_registered(
    node: &embedded::EmbeddedNode<embedded::EmbeddedStore>,
    collections: &[String],
) -> bool {
    collections
        .iter()
        .all(|name| node.database.get_collection(name).ok().flatten().is_some())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Path to a tenant's stored SDL file: `data_root/tenants/<name>.graphql`,
/// written by `tenant create` (D14) so a late-joining cell can be schema'd
/// without a live admin connection.
pub fn tenant_sdl_path(data_root: &Path, name: &str) -> PathBuf {
    data_root.join("tenants").join(format!("{name}.graphql"))
}

async fn load_tenant_sdl(data_root: &Path, name: &str) -> Result<String> {
    let path = tenant_sdl_path(data_root, name);
    tokio::fs::read_to_string(&path).await.with_context(|| {
        format!(
            "reading tenant schema {} (missing: run `tenant create` before `start`)",
            path.display()
        )
    })
}

/// Collection names declared in `sdl`, via upstream's own SDL parser: the
/// source of truth for what `wire_group`'s `add_collections` call needs to
/// subscribe.
fn collection_names(sdl: &str) -> Result<Vec<String>> {
    let collections = query::parse_sdl(sdl).map_err(|error| anyhow!("SDL parse error: {error}"))?;
    Ok(collections.into_iter().map(|c| c.name).collect())
}

/// Looks up every id in `cell_ids` as a live [`RunningCell`], failing
/// loudly (naming the missing id) rather than silently wiring a partial
/// group.
fn resolve_running_cells<'a>(
    supervisor: &'a Supervisor,
    cell_ids: &[String],
) -> Result<Vec<&'a RunningCell>> {
    cell_ids
        .iter()
        .map(|id| {
            supervisor
                .running_cell(id)
                .ok_or_else(|| anyhow!("cell '{id}' is not running in this process"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tenant_sdl_path_nests_under_tenants() {
        let root = Path::new("/data");
        assert_eq!(
            tenant_sdl_path(root, "acme-co"),
            PathBuf::from("/data/tenants/acme-co.graphql")
        );
    }

    #[test]
    fn collection_names_reads_every_declared_type() {
        let sdl = "type Spike { name: String }\ntype Other { count: Int }";
        let names = collection_names(sdl).unwrap();
        assert_eq!(names, vec!["Spike".to_string(), "Other".to_string()]);
    }

    #[test]
    fn collection_names_reports_a_parse_error() {
        let error = collection_names("not valid graphql sdl {{{").unwrap_err();
        assert!(error.to_string().contains("SDL parse error"));
    }

    #[tokio::test]
    async fn resolve_running_cells_errors_on_a_missing_id() {
        let dir = tempfile::tempdir().unwrap();
        let supervisor = Supervisor::new(dir.path());
        // `Vec<&RunningCell>` (the Ok side) is not `Debug` (it holds a real
        // `EmbeddedNode`), so this matches instead of `unwrap_err()`
        // (mirrors the same pattern in burner-cell's `cell.rs` tests).
        match resolve_running_cells(&supervisor, &["cell-0".to_string()]) {
            Ok(_) => panic!("expected an error for a cell id the supervisor never ran"),
            Err(error) => {
                assert!(error.to_string().contains("cell-0"));
                assert!(error.to_string().contains("not running"));
            }
        }
    }
}
