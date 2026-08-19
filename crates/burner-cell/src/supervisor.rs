//! Supervisor: owns every live cell in this process and drives provision,
//! ignition, drain, and crash recovery.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use futures::stream::{self, StreamExt};
use serde::{Deserialize, Serialize};

use crate::cell::{self, RunningCell};
use crate::command::DrainCellError;
use crate::identity;
use crate::manifest::ClusterManifest;
use crate::spec::{BackendKind, CellSpec};

/// Bounded concurrency for recovery ignition and shutdown drains: at most
/// this many cells in flight at once.
const RECOVERY_CONCURRENCY: usize = 4;

/// Schema and query for the `BurnerMarker` recovery marker: proof that a
/// cell's data survived a restart (including SIGKILL). Written once at
/// provision, read back at recovery and by the watchdog's liveness probe.
const MARKER_SDL: &str = "type BurnerMarker { cell_id: String }";
const MARKER_QUERY: &str = "query { BurnerMarker { cell_id } }";

/// Owns every cell this process has ignited.
pub struct Supervisor {
    data_root: PathBuf,
    cells: HashMap<String, CellEntry>,
    /// `(cell_id, collection, peer_id)` triples this process has already
    /// confirmed joined the collection's gossipsub topic (bug-fix round,
    /// D25 addendum): `burner_mesh::wire_group` only *waits* on the
    /// upstream edge-triggered `TopicPeerEvent` for a triple not yet in
    /// this set; a triple already here is an idempotent ENSURE (skip the
    /// wait), since upstream exposes no synchronous "current topic
    /// peers" snapshot to check instead (verified against
    /// `defra_http::P2POperations`: no such method exists) and the event
    /// that already proved the join once will not be replayed to a late
    /// subscriber. Starts empty on every fresh `Supervisor` (`new` and
    /// `recover` both): a restart re-ignites every cell with empty
    /// in-memory gossipsub state, so a freshly recovered process
    /// correctly has nothing confirmed yet and must genuinely re-wait.
    confirmed_topic_joins: std::collections::HashSet<(String, String, String)>,
}

struct CellEntry {
    running: RunningCell,
    marker_ok: bool,
}

/// A point-in-time, offline-safe snapshot of one cell for status reporting.
///
/// `Deserialize` is derived alongside `Serialize` because this is also the
/// shape of `defraburner start --ready-file`'s output, which test harnesses
/// (the golden recovery test) parse back.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CellStatus {
    pub id: String,
    pub group: String,
    pub backend: BackendKind,
    pub peer_id: String,
    pub listen_addrs: Vec<String>,
    pub marker_ok: bool,
    /// Currently connected P2P peers, as reported by this cell's own
    /// `P2POperations::connected_peers`. Always empty from plain
    /// [`Supervisor::status`] (a synchronous snapshot); populated by
    /// [`Supervisor::status_with_connected_peers`], which a live P2P query
    /// makes async. `#[serde(default)]` so a ready-file predating this
    /// field still parses.
    #[serde(default)]
    pub connected_peers: Vec<String>,
}

impl Supervisor {
    /// A fresh, empty supervisor over `data_root`. Cells are added via
    /// [`Supervisor::provision`]; an existing cluster is brought back with
    /// [`Supervisor::recover`].
    pub fn new(data_root: impl Into<PathBuf>) -> Self {
        Self {
            data_root: data_root.into(),
            cells: HashMap::new(),
            confirmed_topic_joins: std::collections::HashSet::new(),
        }
    }

    pub fn data_root(&self) -> &Path {
        &self.data_root
    }

    /// Provisions a brand-new cell: validates it against the cluster
    /// manifest, generates its signing identity, creates its data
    /// directory, appends it to the manifest (saved durably), ignites it,
    /// and writes its recovery marker.
    pub async fn provision(&mut self, spec: CellSpec) -> Result<()> {
        let mut manifest = load_or_new_manifest(&self.data_root).await?;
        if manifest.cells.iter().any(|existing| existing.id == spec.id) {
            bail!("cell id '{}' is already provisioned", spec.id);
        }
        if manifest
            .cells
            .iter()
            .any(|existing| existing.p2p_port == spec.p2p_port)
        {
            bail!(
                "p2p port {} is already used by another provisioned cell",
                spec.p2p_port
            );
        }

        identity::provision(&spec.signing_key_file)
            .await
            .with_context(|| format!("provisioning identity for cell '{}'", spec.id))?;

        let dir = cell::cell_data_dir(&self.data_root, &spec.id);
        tokio::fs::create_dir_all(&dir)
            .await
            .with_context(|| format!("creating cell dir {}", dir.display()))?;

        manifest.cells.push(spec.clone());
        manifest
            .save(&self.data_root)
            .await
            .with_context(|| format!("saving cluster manifest after provisioning '{}'", spec.id))?;

        let running = cell::ignite(&self.data_root, spec.clone())
            .await
            .with_context(|| format!("igniting newly provisioned cell '{}'", spec.id))?;

        write_marker(&running.node, &spec.id)
            .await
            .with_context(|| format!("writing recovery marker for cell '{}'", spec.id))?;

        self.cells.insert(
            spec.id.clone(),
            CellEntry {
                running,
                marker_ok: true,
            },
        );
        Ok(())
    }

    /// Shuts a cell down cleanly and drops it from the running set.
    ///
    /// Does not re-register sibling cells' signing identities after the
    /// drain. `defra_core::signing`'s DID-keyed identity registry is wiped
    /// wholesale on every cell shutdown (verified:
    /// `defradb.rs crates/embedded/src/node.rs:471`, inside
    /// `ShutdownHandle::shutdown`, invoked by `EmbeddedNode::shutdown` via
    /// `p2p.shutdown()`), but a repo-wide grep of `defradb.rs/crates` for
    /// `get_identity` / `resolve_signing_config[_with_flag]` /
    /// `find_remote_signer_did` call sites found the registry read only
    /// from `cli`, `defra-node`, `ffi`, `http` (all request-time DID
    /// lookups) and `sourcehub` -- never from `db`, `query`, `p2p`,
    /// `db-merge`, or `storage`. The bare `embedded::EmbeddedNode` path this
    /// crate uses (`execute()` / `add_schema()`, no `defra_http` server, no
    /// FFI) never triggers any of those call sites; the mutation-time
    /// signing check (`db::doc_mutator` / `auto_commit_mutator`) instead
    /// reads a *different*, thread-local signing config
    /// (`defra_core::signing::get_signing_config`) that only the http/ffi
    /// layers populate. So wiping the registry has no observable effect on
    /// any query/write/replication path a still-running sibling cell
    /// exercises here, and no re-registration defense is needed.
    pub async fn drain(&mut self, id: &str) -> Result<()> {
        let entry = self
            .cells
            .remove(id)
            .with_context(|| format!("no running cell '{id}' to drain"))?;
        entry.running.node.shutdown().await;
        Ok(())
    }

    /// Drains cell `id`, removes it from the cluster manifest (so
    /// `recover` never resurrects it), and deletes its signing key file.
    /// Used by the autoscaler's scale-down execution (Phase 4, D17) and
    /// the admin `DrainCell`/`DropTenant(retire)` commands (console round,
    /// D25): `id` must already be free (in no tenant's `cells`) --
    /// verified independently here too, not merely trusted from the
    /// caller's own clamp step, since an automated or admin-triggered
    /// removal path is exactly where a defense-in-depth check earns its
    /// cost. The whole check-then-act runs under this one method's `&mut
    /// self` (the caller's held `Arc<Mutex<Supervisor>>` guard), so a
    /// concurrent tenant reconcile can never sneak an assignment in
    /// between the check and the removal (D25's second required
    /// correctness catch).
    ///
    /// The cell's data directory is deleted only when `delete_data` is
    /// set (tenant retirement, D23: the operator asked for the tenant's
    /// data gone, not just its placement). Otherwise it is deliberately
    /// left in place: a future scale-up always picks a fresh,
    /// never-before-used cell id (see `burner-policy`'s
    /// `autoscaler::next_cell_index`, which scans for exactly this), so a
    /// lingering old directory is inert, not a hazard -- and leaving it
    /// avoids ever risking a real data-loss bug on an automated path.
    pub async fn remove_cell(&mut self, id: &str, delete_data: bool) -> Result<(), DrainCellError> {
        let entry = self.cells.get(id).ok_or(DrainCellError::NotFound)?;
        let signing_key_file = entry.running.spec.signing_key_file.clone();

        let mut manifest = ClusterManifest::load(&self.data_root)
            .await
            .map_err(|error| {
                DrainCellError::Failed(format!("loading cluster manifest: {error}"))
            })?;
        if let Some(tenant) = manifest
            .tenants
            .iter()
            .find(|tenant| tenant.cells.iter().any(|cell_id| cell_id == id))
        {
            return Err(DrainCellError::AssignedToTenant(tenant.name.clone()));
        }

        self.drain(id)
            .await
            .map_err(|error| DrainCellError::Failed(error.to_string()))?;

        let before = manifest.cells.len();
        manifest.cells.retain(|cell| cell.id != id);
        if manifest.cells.len() == before {
            return Err(DrainCellError::Failed(format!(
                "cell '{id}' was not present in the cluster manifest"
            )));
        }
        manifest.save(&self.data_root).await.map_err(|error| {
            DrainCellError::Failed(format!(
                "saving cluster manifest after removing '{id}': {error}"
            ))
        })?;

        match tokio::fs::remove_file(&signing_key_file).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(DrainCellError::Failed(format!(
                    "removing signing key file {}: {error}",
                    signing_key_file.display()
                )));
            }
        }

        if delete_data {
            let dir = cell::cell_data_dir(&self.data_root, id);
            match tokio::fs::remove_dir_all(&dir).await {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(DrainCellError::Failed(format!(
                        "removing data directory {}: {error}",
                        dir.display()
                    )));
                }
            }
        }
        Ok(())
    }

    /// Brings back every cell recorded in `data_root`'s cluster manifest:
    /// loads the manifest, ignites every cell with bounded concurrency (at
    /// most `RECOVERY_CONCURRENCY` in flight), and verifies each cell's
    /// recovery marker. A cell that fails to ignite aborts the whole
    /// recovery (a partially-recovered cluster would silently be missing a
    /// cell); a cell that ignites but fails its marker check is reported
    /// honestly via `marker_ok = false` in [`Supervisor::status`], not
    /// hidden.
    pub async fn recover(data_root: impl Into<PathBuf>) -> Result<Self> {
        let data_root = data_root.into();
        let manifest = ClusterManifest::load(&data_root)
            .await
            .context("loading cluster manifest for recovery")?;

        // `embedded::build_with_store`'s returned future is not `Send` (see
        // `watchdog::Watchdog::run`'s doc comment for why), so bounded
        // ignition concurrency polls multiple in-flight builds on this one
        // task via `buffer_unordered` rather than spawning each onto its
        // own task.
        let ignition_data_root = data_root.clone();
        let mut ignitions = stream::iter(manifest.cells)
            .map(move |spec| {
                let data_root = ignition_data_root.clone();
                async move {
                    let id = spec.id.clone();
                    (id, ignite_and_verify(&data_root, spec).await)
                }
            })
            .buffer_unordered(RECOVERY_CONCURRENCY);

        let mut cells = HashMap::new();
        while let Some((id, outcome)) = ignitions.next().await {
            let entry = outcome.with_context(|| format!("recovering cell '{id}'"))?;
            cells.insert(id, entry);
        }

        Ok(Self {
            data_root,
            cells,
            confirmed_topic_joins: std::collections::HashSet::new(),
        })
    }

    /// Drains and re-ignites `id` with its existing spec, re-checking (but
    /// not requiring) its recovery marker. Used by the watchdog after
    /// repeated liveness-probe failures.
    pub async fn reignite(&mut self, id: &str) -> Result<()> {
        let spec = self
            .cells
            .get(id)
            .map(|entry| entry.running.spec.clone())
            .with_context(|| format!("no running cell '{id}' to re-ignite"))?;

        self.drain(id).await?;

        let running = cell::ignite(&self.data_root, spec.clone())
            .await
            .with_context(|| format!("re-igniting cell '{id}'"))?;
        let marker_ok = verify_marker(&running.node, &spec.id)
            .await
            .unwrap_or(false);

        self.cells
            .insert(id.to_string(), CellEntry { running, marker_ok });
        Ok(())
    }

    /// Drains every running cell, bounded to `RECOVERY_CONCURRENCY` in
    /// flight at once.
    pub async fn shutdown_all(&mut self) {
        let semaphore = Arc::new(tokio::sync::Semaphore::new(RECOVERY_CONCURRENCY));
        let mut join_set = tokio::task::JoinSet::new();
        for (_, entry) in self.cells.drain() {
            let semaphore = semaphore.clone();
            let node = entry.running.node.clone();
            join_set.spawn(async move {
                let _permit = semaphore.acquire_owned().await;
                node.shutdown().await;
            });
        }
        while join_set.join_next().await.is_some() {}
    }

    /// A point-in-time snapshot of every running cell. `connected_peers` is
    /// always empty here (a synchronous snapshot cannot make the live P2P
    /// query); use [`Supervisor::status_with_connected_peers`] for that.
    pub fn status(&self) -> Vec<CellStatus> {
        self.cells
            .values()
            .map(|entry| CellStatus {
                id: entry.running.spec.id.clone(),
                group: entry.running.spec.group.clone(),
                backend: entry.running.spec.backend,
                peer_id: entry.running.peer_id.clone(),
                listen_addrs: entry.running.listen_addrs.clone(),
                marker_ok: entry.marker_ok,
                connected_peers: Vec::new(),
            })
            .collect()
    }

    /// [`Supervisor::status`], enriched with each cell's live connected-peer
    /// list. A cell whose `connected_peers` query errors gets an empty list
    /// rather than failing the whole snapshot (the query itself, not
    /// connectivity, is what is best-effort here; a real connectivity
    /// problem still shows up as an empty list, never fabricated peers).
    pub async fn status_with_connected_peers(&self) -> Vec<CellStatus> {
        let mut statuses = self.status();
        for status in &mut statuses {
            let Some(entry) = self.cells.get(&status.id) else {
                continue;
            };
            let Some(p2p) = entry.running.node.p2p() else {
                continue;
            };
            status.connected_peers = match p2p.ops().connected_peers().await {
                Ok(peers) => peers,
                Err(error) => {
                    tracing::warn!(
                        cell_id = %status.id,
                        error = %error,
                        "querying connected_peers for status failed"
                    );
                    Vec::new()
                }
            };
        }
        statuses
    }

    /// The ids of every currently running cell.
    pub fn cell_ids(&self) -> Vec<String> {
        self.cells.keys().cloned().collect()
    }

    /// Live `sync_status` for every currently running cell (Phase 4,
    /// D17: feeds `burner-policy`'s `MetricsSnapshot`). Exposed here
    /// (rather than in `burner-policy`, which does not depend on
    /// `embedded`/`defra-http`) so the trait-object P2P call stays behind
    /// this crate's existing boundary, mirroring
    /// [`Supervisor::status_with_connected_peers`]'s honest degrade on
    /// error: a cell whose query fails or has no p2p system reports
    /// `Value::Null` rather than being omitted or given a fabricated
    /// value.
    pub async fn sync_status_snapshot(&self) -> HashMap<String, serde_json::Value> {
        let mut out = HashMap::with_capacity(self.cells.len());
        for (id, entry) in &self.cells {
            let value = match entry.running.node.p2p() {
                Some(p2p) => match p2p.ops().sync_status().await {
                    Ok(value) => value,
                    Err(error) => {
                        tracing::warn!(cell_id = %id, error = %error, "querying sync_status for snapshot failed");
                        serde_json::Value::Null
                    }
                },
                None => serde_json::Value::Null,
            };
            out.insert(id.clone(), value);
        }
        out
    }

    /// A cheap, cloned handle to a running cell's node, for callers (the
    /// watchdog) that need to probe it without holding the supervisor lock
    /// for the duration of the probe.
    pub fn node_handle(
        &self,
        id: &str,
    ) -> Option<Arc<embedded::EmbeddedNode<embedded::EmbeddedStore>>> {
        self.cells.get(id).map(|entry| entry.running.node.clone())
    }

    /// A shared reference to one running cell, for callers (tenant group
    /// wiring, `burner-mesh`) that need the full [`RunningCell`] (node
    /// handle, peer id, listen addresses), not just the [`CellStatus`]
    /// snapshot.
    pub fn running_cell(&self, id: &str) -> Option<&RunningCell> {
        self.cells.get(id).map(|entry| &entry.running)
    }

    /// A cheap owned copy of every `(cell_id, collection, peer_id)` topic
    /// join this process has confirmed so far. Cloned out (not borrowed)
    /// specifically so a caller (`burner_mesh::reconcile`) can hold it
    /// independently of `self` while it also holds cells borrowed from
    /// `self.running_cell`/`node_handle` -- those two borrows would
    /// otherwise conflict with taking `&mut self` to record new
    /// confirmations after wiring completes. Bounded by cluster size x
    /// collections x replication factor, tiny in practice; this is an
    /// occasional reconcile-time operation, never a per-request one.
    pub fn confirmed_topic_joins_snapshot(
        &self,
    ) -> std::collections::HashSet<(String, String, String)> {
        self.confirmed_topic_joins.clone()
    }

    /// Merges newly-confirmed topic joins back in (monotonic: this set
    /// only ever grows for the life of the process, mirroring the real
    /// fact it tracks -- a gossipsub subscription, once joined, stays
    /// joined until the cell is drained/re-ignited).
    pub fn merge_confirmed_topic_joins(
        &mut self,
        additional: impl IntoIterator<Item = (String, String, String)>,
    ) {
        self.confirmed_topic_joins.extend(additional);
    }
}

async fn load_or_new_manifest(data_root: &Path) -> Result<ClusterManifest> {
    if ClusterManifest::exists(data_root) {
        ClusterManifest::load(data_root).await
    } else {
        tokio::fs::create_dir_all(data_root)
            .await
            .with_context(|| format!("creating data root {}", data_root.display()))?;
        Ok(ClusterManifest::new())
    }
}

async fn ignite_and_verify(data_root: &Path, spec: CellSpec) -> Result<CellEntry> {
    let id = spec.id.clone();
    let running = cell::ignite(data_root, spec)
        .await
        .with_context(|| format!("igniting cell '{id}' during recovery"))?;
    let marker_ok = verify_marker(&running.node, &id)
        .await
        .with_context(|| format!("verifying recovery marker for cell '{id}'"))?;
    if !marker_ok {
        tracing::error!(
            cell_id = %id,
            "recovery marker check failed after restart: cell_id not found in BurnerMarker"
        );
    }
    Ok(CellEntry { running, marker_ok })
}

/// Registers the `BurnerMarker` schema and writes one document tagging this
/// cell with its own id: the data-intactness proof a restart is expected to
/// preserve.
async fn write_marker(
    node: &embedded::EmbeddedNode<embedded::EmbeddedStore>,
    cell_id: &str,
) -> Result<()> {
    let txn = node
        .database
        .new_txn(false)
        .await
        .map_err(|error| anyhow!("opening marker transaction: {error}"))?;
    txn.systemstore()
        .map_err(|error| anyhow!("opening systemstore for the marker: {error}"))?
        .set(MARKER_KEY, cell_id.as_bytes())
        .await
        .map_err(|error| anyhow!("writing the cell marker: {error}"))?;
    txn.commit()
        .await
        .map_err(|error| anyhow!("committing the cell marker: {error}"))?;
    Ok(())
}

/// Queries `BurnerMarker` and reports whether `cell_id` is present. Shared
/// by recovery and the watchdog's liveness probe so both agree on exactly
/// one definition of "this cell's data is intact".
pub(crate) async fn verify_marker(
    node: &embedded::EmbeddedNode<embedded::EmbeddedStore>,
    cell_id: &str,
) -> Result<bool> {
    let response = node.execute(MARKER_QUERY).await;
    if response.has_errors() {
        bail!("BurnerMarker query returned errors: {:?}", response.errors);
    }
    let found = response
        .data
        .as_ref()
        .and_then(|data| data.get("BurnerMarker"))
        .and_then(|docs| docs.as_array())
        .map(|docs| {
            docs.iter()
                .any(|doc| doc.get("cell_id").and_then(|v| v.as_str()) == Some(cell_id))
        })
        .unwrap_or(false);
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::DEFAULT_MEM_BUDGET_BYTES;

    fn lark_spec(data_root: &Path, id: &str, port: u16) -> CellSpec {
        CellSpec {
            id: id.to_string(),
            group: "default".to_string(),
            backend: BackendKind::Lark,
            p2p_port: port,
            bind_addr: "127.0.0.1".parse().unwrap(),
            mem_budget_bytes: DEFAULT_MEM_BUDGET_BYTES,
            signing_key_file: identity::key_path(data_root, id),
        }
    }

    /// In-process round trip: provision one cell, confirm its marker and
    /// status, drain it, then recover the whole supervisor from disk and
    /// confirm the same peer id and an intact marker come back. The golden
    /// test (crates/defraburner/tests/recovery.rs) is the SIGKILL-verified
    /// superset of this against the real binary; this is the fast,
    /// in-process floor for the same behavior.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn provision_drain_and_recover_preserve_identity_and_data() {
        let dir = tempfile::tempdir().unwrap();
        let data_root = dir.path().to_path_buf();
        let port = free_tcp_port();

        let mut supervisor = Supervisor::new(&data_root);
        supervisor
            .provision(lark_spec(&data_root, "cell-0", port))
            .await
            .expect("provision should succeed");

        let status = supervisor.status();
        assert_eq!(status.len(), 1);
        assert_eq!(status[0].id, "cell-0");
        assert!(
            status[0].marker_ok,
            "marker should be intact right after provisioning"
        );
        let peer_id_before = status[0].peer_id.clone();
        assert!(!status[0].listen_addrs.is_empty());

        supervisor
            .drain("cell-0")
            .await
            .expect("drain should succeed");
        assert!(supervisor.status().is_empty());

        let mut recovered = Supervisor::recover(&data_root)
            .await
            .expect("recover should succeed");
        let status = recovered.status();
        assert_eq!(status.len(), 1);
        assert_eq!(status[0].id, "cell-0");
        assert!(status[0].marker_ok, "marker should survive recovery");
        assert_eq!(
            status[0].peer_id, peer_id_before,
            "peer id must be stable across drain+recover"
        );

        recovered.shutdown_all().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn provision_rejects_duplicate_cell_id() {
        let dir = tempfile::tempdir().unwrap();
        let data_root = dir.path().to_path_buf();
        let port_a = free_tcp_port();
        let port_b = free_tcp_port();

        let mut supervisor = Supervisor::new(&data_root);
        supervisor
            .provision(lark_spec(&data_root, "cell-0", port_a))
            .await
            .unwrap();

        let error = supervisor
            .provision(lark_spec(&data_root, "cell-0", port_b))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("already provisioned"));

        supervisor.shutdown_all().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn remove_cell_drains_and_erases_a_free_cell() {
        let dir = tempfile::tempdir().unwrap();
        let data_root = dir.path().to_path_buf();
        let port = free_tcp_port();

        let mut supervisor = Supervisor::new(&data_root);
        supervisor
            .provision(lark_spec(&data_root, "cell-0", port))
            .await
            .expect("provision should succeed");
        let key_file = crate::identity::key_path(&data_root, "cell-0");
        assert!(key_file.exists());

        supervisor
            .remove_cell("cell-0", false)
            .await
            .expect("removing a free cell should succeed");

        assert!(
            supervisor.status().is_empty(),
            "cell should no longer be running"
        );
        assert!(!key_file.exists(), "signing key file should be cleaned up");
        let manifest = ClusterManifest::load(&data_root).await.unwrap();
        assert!(
            manifest.cells.is_empty(),
            "removed cell should no longer be recorded in the manifest"
        );
        // The data directory is deliberately left in place.
        assert!(cell::cell_data_dir(&data_root, "cell-0").exists());
    }

    /// D23/D25: tenant retirement asks for the data gone too, unlike a
    /// plain scale-down/admin drain.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn remove_cell_deletes_the_data_directory_when_asked() {
        let dir = tempfile::tempdir().unwrap();
        let data_root = dir.path().to_path_buf();
        let port = free_tcp_port();

        let mut supervisor = Supervisor::new(&data_root);
        supervisor
            .provision(lark_spec(&data_root, "cell-0", port))
            .await
            .expect("provision should succeed");
        let data_dir = cell::cell_data_dir(&data_root, "cell-0");
        assert!(data_dir.exists());

        supervisor
            .remove_cell("cell-0", true)
            .await
            .expect("removing with delete_data=true should succeed");

        assert!(
            !data_dir.exists(),
            "data directory should be gone when delete_data is requested"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn remove_cell_refuses_a_cell_assigned_to_a_tenant() {
        let dir = tempfile::tempdir().unwrap();
        let data_root = dir.path().to_path_buf();
        let port = free_tcp_port();

        let mut supervisor = Supervisor::new(&data_root);
        supervisor
            .provision(lark_spec(&data_root, "cell-0", port))
            .await
            .unwrap();

        let mut manifest = ClusterManifest::load(&data_root).await.unwrap();
        manifest.tenants.push(crate::spec::TenantSpec {
            name: "acme-co".to_string(),
            replicas: 1,
            cells: vec!["cell-0".to_string()],
            token_sha256: String::new(),
            status: crate::spec::TenantStatus::Placed,
            admission: None,
            health: Default::default(),
        });
        manifest.save(&data_root).await.unwrap();

        let error = supervisor.remove_cell("cell-0", false).await.unwrap_err();
        assert!(error.to_string().contains("assigned to a tenant"));
        assert_eq!(
            supervisor.status().len(),
            1,
            "the cell must still be running"
        );

        supervisor.shutdown_all().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn remove_cell_rejects_an_unknown_id() {
        let dir = tempfile::tempdir().unwrap();
        let mut supervisor = Supervisor::new(dir.path());
        let error = supervisor
            .remove_cell("ghost-cell", false)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("no running cell"));
    }

    /// Binds an ephemeral OS-assigned TCP port and immediately releases it,
    /// for use as a (best-effort, not reserved) free p2p_port in tests.
    fn free_tcp_port() -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    }
}
