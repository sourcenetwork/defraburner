//! `MetricsSnapshot` v1: the per-tick input built for the autoscale
//! policy, and `placement_input`, the smaller per-tick input built for
//! the placement policy. Gateway-owned metrics (per-cell request
//! counters, per-tenant admission counters) are not read directly here --
//! `burner-policy` does not depend on `burner-gateway`: they arrive as
//! plain [`GatewayMetrics`] data the caller (`defraburner`'s `start.rs`)
//! gathers each tick and passes in.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use burner_cell::{ClusterManifest, Supervisor, TenantStatus};
use serde::Serialize;
use serde_json::Value;
use tokio::sync::Mutex;

/// Schema version of [`MetricsSnapshot`]'s wire shape.
pub const SCHEMA_VERSION: u32 = 1;
/// Cells beyond this count (ranked by request count descending) are
/// dropped from `cells`, with the drop stated honestly in `cap`. Chosen
/// generously above any `max_cells` this plan's default configuration
/// realistically reaches; a fleet larger than this is a known, named
/// limitation (see this module's doc comment on [`MetricsSnapshot::cap`]),
/// not a silent one.
pub const MAX_CELLS_IN_SNAPSHOT: usize = 64;

#[derive(Debug, Clone, Serialize)]
pub struct CapInfo {
    pub cells_included: usize,
    pub cells_total: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct HostMetrics {
    pub mem_total_kb: u64,
    pub mem_avail_kb: u64,
    pub load1: f64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct RequestCounters {
    pub count: u64,
    pub sum_ms: f64,
    pub max_ms: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct CellSnapshot {
    pub id: String,
    pub group: String,
    pub tenant: Option<String>,
    /// Always `true`: only cells the supervisor is actively running are
    /// ever iterated to build this list, so there is no "known but not
    /// running" cell to represent in v1's carried-over-manifest model.
    /// Kept as a field (not dropped) because the wire shape names it
    /// explicitly and a later phase (e.g. a cell mid-recovery) may give it
    /// a real `false` case.
    pub running: bool,
    pub marker_ok: bool,
    pub requests: RequestCounters,
    /// Instantaneous requests/sec, derived by the caller (`autoscaler.rs`)
    /// from the delta between this tick's and the previous tick's
    /// cumulative `requests.count`, divided by the elapsed wall time
    /// between the two samples (`requests.count` itself is a lifetime
    /// total, never reset, so it cannot be read as a rate directly). Not
    /// part of the plan's enumerated `MetricsSnapshot` field list, but
    /// added because the shipped `autoscale-default` policy's threshold
    /// check reads exactly this per-cell `qps` signal (unchanged from
    /// Phase 0's spike contract; the crate's `engine.rs` EARLY
    /// VERIFICATION test proves the AOT-precompiled package still honors
    /// it): omitting it would leave the default policy permanently
    /// blind. 0.0 for a cell observed for the first time (no prior
    /// sample), honestly, not a fabricated rate.
    pub qps: f64,
    pub storage_bytes: u64,
    pub sync_status: Value,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct TenantAdmission {
    pub allowed: u64,
    pub rejected: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct TenantSnapshot {
    pub name: String,
    pub replicas: u8,
    pub cells: Vec<String>,
    pub status: String,
    pub admission: TenantAdmission,
}

#[derive(Debug, Clone, Serialize)]
pub struct Limits {
    pub min_cells: usize,
    pub max_cells: usize,
    pub max_actions_per_tick: usize,
    pub cooldown_secs: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct LastAction {
    pub tick: u64,
    pub action: String,
}

/// The per-tick input built for the autoscale policy.
#[derive(Debug, Clone, Serialize)]
pub struct MetricsSnapshot {
    pub schema_version: u32,
    pub tick: u64,
    pub cap: CapInfo,
    pub host: HostMetrics,
    pub cells: Vec<CellSnapshot>,
    pub tenants: Vec<TenantSnapshot>,
    pub limits: Limits,
    pub last_action: Option<LastAction>,
}

/// Per-cell request counters as gathered from `burner-gateway`'s routing
/// layer (already converted to this crate's own shape by the caller; see
/// this module's doc comment). `count`/`sum_ms`/`max_ms` are lifetime
/// cumulative totals exactly as the gateway tracks them; `qps` is filled
/// in separately by `autoscaler.rs` (see [`CellSnapshot::qps`]) before
/// this reaches [`MetricsSnapshot::build`], since deriving a rate needs
/// memory of the previous tick that this plain data-carrier does not
/// hold.
#[derive(Debug, Clone, Default)]
pub struct CellRequestCounters {
    pub cell_id: String,
    pub count: u64,
    pub sum_ms: f64,
    pub max_ms: f64,
    pub qps: f64,
}

/// Per-tenant admission counters as gathered from `burner-gateway`.
#[derive(Debug, Clone, Default)]
pub struct TenantAdmissionCounters {
    pub tenant: String,
    pub allowed: u64,
    pub rejected: u64,
}

/// One tick's worth of gateway-owned metrics, gathered by the caller
/// (`defraburner`'s `start.rs`) and handed to [`MetricsSnapshot::build`].
#[derive(Debug, Clone, Default)]
pub struct GatewayMetrics {
    pub cell_requests: Vec<CellRequestCounters>,
    pub tenant_admission: Vec<TenantAdmissionCounters>,
}

/// The non-gateway, non-cluster-state inputs for one snapshot: values the
/// caller already has to hand (config, tick number, last action) rather
/// than re-derived here.
pub struct SnapshotInputs<'a> {
    pub tick: u64,
    pub min_cells: usize,
    pub max_cells: usize,
    pub max_actions_per_tick: usize,
    pub cooldown_secs: u64,
    pub last_action: Option<LastAction>,
    pub gateway_metrics: &'a GatewayMetrics,
}

impl MetricsSnapshot {
    /// Builds one snapshot: loads the cluster manifest, snapshots the
    /// supervisor's running cells (including a live `sync_status` query
    /// per included cell), computes each included cell's on-disk storage
    /// size (bounded, off the async executor via `spawn_blocking`,
    /// computed at most once per tick per cell), and folds in the
    /// caller-gathered gateway metrics and config.
    pub async fn build(
        supervisor: &Arc<Mutex<Supervisor>>,
        data_root: &Path,
        inputs: SnapshotInputs<'_>,
    ) -> Result<Self> {
        let manifest = ClusterManifest::load(data_root)
            .await
            .context("loading cluster manifest for snapshot")?;

        let (statuses, sync_statuses) = {
            let guard = supervisor.lock().await;
            (guard.status(), guard.sync_status_snapshot().await)
        };

        let requests_by_cell: HashMap<&str, &CellRequestCounters> = inputs
            .gateway_metrics
            .cell_requests
            .iter()
            .map(|c| (c.cell_id.as_str(), c))
            .collect();

        // Rank by request count descending, then cap: the cells the
        // autoscaler most needs to reason about (the busiest ones) are the
        // ones kept when a fleet exceeds MAX_CELLS_IN_SNAPSHOT. Ranking and
        // truncation happen here, before the storage-size walk below, so a
        // fleet larger than the cap never pays for a directory walk on the
        // cells being dropped.
        let mut ranked = statuses;
        ranked.sort_by_key(|status| {
            std::cmp::Reverse(
                requests_by_cell
                    .get(status.id.as_str())
                    .map(|c| c.count)
                    .unwrap_or(0),
            )
        });
        let cells_total = ranked.len();
        ranked.truncate(MAX_CELLS_IN_SNAPSHOT);

        let dirs: Vec<(String, PathBuf)> = ranked
            .iter()
            .map(|status| {
                (
                    status.id.clone(),
                    burner_cell::cell::cell_data_dir(data_root, &status.id),
                )
            })
            .collect();
        let storage_bytes = compute_dir_sizes(dirs).await?;
        let host = read_host_metrics()?;

        Ok(Self::assemble(
            ranked,
            cells_total,
            &sync_statuses,
            &storage_bytes,
            &manifest,
            host,
            inputs,
        ))
    }

    /// Pure assembly of a [`MetricsSnapshot`] from already-gathered,
    /// already-ranked-and-capped inputs: no I/O, no supervisor, no manifest
    /// load. [`MetricsSnapshot::build`] calls this after gathering the real
    /// data live cells, the cluster manifest, and a bounded storage-size
    /// walk; a perf test (below) calls it directly against synthetic data
    /// to time assembly cost without spinning up real cells. Both callers
    /// exercise the exact same assembly logic, so they can never drift.
    ///
    /// `ranked_cells` must already be sorted by activity and truncated to at
    /// most [`MAX_CELLS_IN_SNAPSHOT`] entries (`build`'s own ranking pass
    /// does this); `cells_total` is the pre-truncation count, carried
    /// through purely for [`CapInfo`]'s honesty.
    fn assemble(
        ranked_cells: Vec<burner_cell::CellStatus>,
        cells_total: usize,
        sync_statuses: &HashMap<String, Value>,
        storage_bytes: &HashMap<String, u64>,
        manifest: &ClusterManifest,
        host: HostMetrics,
        inputs: SnapshotInputs<'_>,
    ) -> Self {
        let mut tenant_of: HashMap<&str, &str> = HashMap::new();
        for tenant in &manifest.tenants {
            for cell_id in &tenant.cells {
                tenant_of.insert(cell_id.as_str(), tenant.name.as_str());
            }
        }

        let requests_by_cell: HashMap<&str, &CellRequestCounters> = inputs
            .gateway_metrics
            .cell_requests
            .iter()
            .map(|c| (c.cell_id.as_str(), c))
            .collect();

        let cells_included = ranked_cells.len();
        let cells = ranked_cells
            .into_iter()
            .map(|status| {
                let counters = requests_by_cell.get(status.id.as_str());
                let requests = counters
                    .map(|c| RequestCounters {
                        count: c.count,
                        sum_ms: c.sum_ms,
                        max_ms: c.max_ms,
                    })
                    .unwrap_or_default();
                CellSnapshot {
                    id: status.id.clone(),
                    group: status.group.clone(),
                    tenant: tenant_of.get(status.id.as_str()).map(|s| s.to_string()),
                    running: true,
                    marker_ok: status.marker_ok,
                    requests,
                    qps: counters.map(|c| c.qps).unwrap_or(0.0),
                    storage_bytes: storage_bytes.get(&status.id).copied().unwrap_or(0),
                    sync_status: sync_statuses
                        .get(&status.id)
                        .cloned()
                        .unwrap_or(Value::Null),
                }
            })
            .collect();

        let admission_by_tenant: HashMap<&str, &TenantAdmissionCounters> = inputs
            .gateway_metrics
            .tenant_admission
            .iter()
            .map(|a| (a.tenant.as_str(), a))
            .collect();
        let tenants = manifest
            .tenants
            .iter()
            .map(|tenant| TenantSnapshot {
                name: tenant.name.clone(),
                replicas: tenant.replicas,
                cells: tenant.cells.clone(),
                status: match tenant.status {
                    TenantStatus::Pending => "pending".to_string(),
                    TenantStatus::Placed => "placed".to_string(),
                },
                admission: admission_by_tenant
                    .get(tenant.name.as_str())
                    .map(|a| TenantAdmission {
                        allowed: a.allowed,
                        rejected: a.rejected,
                    })
                    .unwrap_or_default(),
            })
            .collect();

        Self {
            schema_version: SCHEMA_VERSION,
            tick: inputs.tick,
            cap: CapInfo {
                cells_included,
                cells_total,
            },
            host,
            cells,
            tenants,
            limits: Limits {
                min_cells: inputs.min_cells,
                max_cells: inputs.max_cells,
                max_actions_per_tick: inputs.max_actions_per_tick,
                cooldown_secs: inputs.cooldown_secs,
            },
            last_action: inputs.last_action,
        }
    }
}

/// The per-tick input built for the placement policy: every genuinely
/// unplaced (`Pending`, no cells yet assigned) tenant, the cells no
/// tenant currently claims, and how many tenants each cell currently
/// serves (v1 disjoint placement means this is always 0 or 1, but it is
/// computed as a real count, not a bool, so a later phase allowing
/// shared-cell density can widen what it means without changing this
/// function's contract: mirrors `burner-mesh`'s own
/// `placement::place` doc comment on the same point).
#[derive(Debug, Clone, Serialize)]
pub struct PlacementInput {
    pub pending_tenants: Vec<PendingTenantInput>,
    pub free_cells: Vec<String>,
    pub assigned_counts: HashMap<String, u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PendingTenantInput {
    pub name: String,
    pub replicas: u8,
}

/// Pure (no I/O): builds a [`PlacementInput`] from an already-loaded
/// manifest.
pub fn placement_input(manifest: &ClusterManifest) -> PlacementInput {
    let assigned: std::collections::HashSet<&str> = manifest
        .tenants
        .iter()
        .flat_map(|tenant| tenant.cells.iter().map(String::as_str))
        .collect();

    let free_cells = manifest
        .cells
        .iter()
        .map(|cell| cell.id.clone())
        .filter(|id| !assigned.contains(id.as_str()))
        .collect();

    let assigned_counts = manifest
        .cells
        .iter()
        .map(|cell| {
            let count = manifest
                .tenants
                .iter()
                .filter(|tenant| tenant.cells.iter().any(|id| id == &cell.id))
                .count() as u64;
            (cell.id.clone(), count)
        })
        .collect();

    // Only genuinely unplaced tenants: one already carrying a policy- or
    // reconcile-assigned `cells` list (e.g. a placement whose subsequent
    // wiring failed and is still `Pending`) is retried by `start`'s own
    // unconditional reconcile pass on the next restart, not re-proposed
    // to the policy here.
    //
    // vertexia: a tenant stuck `Pending` with `cells` already assigned
    // (a reconcile failure mid-tick) is not retried again until the next
    // `start`; a same-process retry loop would close that gap but is not
    // exercised by any gate test and adds real complexity for an
    // unobserved failure mode.
    let pending_tenants = manifest
        .tenants
        .iter()
        .filter(|tenant| tenant.status == TenantStatus::Pending && tenant.cells.is_empty())
        .map(|tenant| PendingTenantInput {
            name: tenant.name.clone(),
            replicas: tenant.replicas,
        })
        .collect();

    PlacementInput {
        pending_tenants,
        free_cells,
        assigned_counts,
    }
}

/// Public wrapper around the same bounded, off-the-executor storage-size
/// walk [`MetricsSnapshot::build`] uses internally, exposed so
/// `burner-gateway`'s `/admin/status` and `/admin/api/overview` (Phase 5)
/// can report the same honest per-cell storage figures the dashboard
/// shows, without duplicating the walk. `data_root` and `cell_ids` are
/// exactly what `burner_cell::cell::cell_data_dir` needs; the returned
/// map has one entry per id in `cell_ids` (0 for a directory that does
/// not exist), except in the practically-unreachable case of the
/// underlying blocking task itself panicking (`dir_size_bytes` has no
/// panicking call in it), where the map comes back empty rather than
/// this async fn propagating an error type callers would otherwise never
/// need to handle.
pub async fn storage_bytes_for_cells(
    data_root: &Path,
    cell_ids: &[String],
) -> HashMap<String, u64> {
    let dirs = cell_ids
        .iter()
        .map(|id| (id.clone(), burner_cell::cell::cell_data_dir(data_root, id)))
        .collect();
    compute_dir_sizes(dirs).await.unwrap_or_default()
}

async fn compute_dir_sizes(dirs: Vec<(String, PathBuf)>) -> Result<HashMap<String, u64>> {
    tokio::task::spawn_blocking(move || {
        dirs.into_iter()
            .map(|(id, path)| (id, dir_size_bytes(&path)))
            .collect()
    })
    .await
    .context("storage size computation task panicked")
}

/// Recursively sums the byte size of every regular file under `dir`.
/// Symlinks are not followed (bounds the walk against a cyclic link); a
/// directory that vanishes or errors mid-walk (e.g. a concurrent drain)
/// contributes 0 for the part that vanished rather than failing the whole
/// snapshot.
fn dir_size_bytes(dir: &Path) -> u64 {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return 0,
    };
    let mut total = 0u64;
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() {
            continue;
        } else if file_type.is_dir() {
            total += dir_size_bytes(&entry.path());
        } else if let Ok(metadata) = entry.metadata() {
            total += metadata.len();
        }
    }
    total
}

/// Parses `MemTotal`/`MemAvailable` (kB) from `/proc/meminfo` and the
/// 1-minute load average from `/proc/loadavg`. std only, same plain
/// `/proc` parsing style as `tests/attribution.rs`'s `read_rss_kb`.
fn read_host_metrics() -> Result<HostMetrics> {
    let meminfo = std::fs::read_to_string("/proc/meminfo").context("reading /proc/meminfo")?;
    let mem_total_kb = parse_meminfo_field(&meminfo, "MemTotal:")
        .context("MemTotal not present in /proc/meminfo")?;
    let mem_avail_kb = parse_meminfo_field(&meminfo, "MemAvailable:")
        .context("MemAvailable not present in /proc/meminfo")?;

    let loadavg = std::fs::read_to_string("/proc/loadavg").context("reading /proc/loadavg")?;
    let load1 = loadavg
        .split_whitespace()
        .next()
        .and_then(|field| field.parse::<f64>().ok())
        .context("load1 not present in /proc/loadavg")?;

    Ok(HostMetrics {
        mem_total_kb,
        mem_avail_kb,
        load1,
    })
}

fn parse_meminfo_field(meminfo: &str, prefix: &str) -> Option<u64> {
    meminfo.lines().find_map(|line| {
        line.strip_prefix(prefix)?
            .split_whitespace()
            .next()?
            .parse::<u64>()
            .ok()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use burner_cell::{AutoscalerSpec, BackendKind, CellSpec, CellStatus, TenantSpec};
    use std::time::{Duration, Instant};

    fn cell(id: &str) -> CellSpec {
        CellSpec {
            id: id.to_string(),
            group: "default".to_string(),
            backend: BackendKind::Regolith,
            p2p_port: 9171,
            bind_addr: "127.0.0.1".parse().unwrap(),
            mem_budget_bytes: burner_cell::DEFAULT_MEM_BUDGET_BYTES,
            signing_key_file: PathBuf::from(format!("/data/keys/{id}.ed25519")),
        }
    }

    fn tenant(name: &str, replicas: u8, cells: &[&str], status: TenantStatus) -> TenantSpec {
        TenantSpec {
            name: name.to_string(),
            replicas,
            cells: cells.iter().map(|c| c.to_string()).collect(),
            token_sha256: String::new(),
            status,
            admission: None,
            health: Default::default(),
        }
    }

    #[test]
    fn read_host_metrics_reads_real_proc_files() {
        let host = read_host_metrics().unwrap();
        assert!(host.mem_total_kb > 0);
        assert!(host.load1 >= 0.0);
    }

    #[test]
    fn dir_size_bytes_sums_files_recursively() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"12345").unwrap();
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("b.txt"), b"1234567890").unwrap();
        assert_eq!(dir_size_bytes(dir.path()), 15);
    }

    #[test]
    fn dir_size_bytes_of_a_missing_dir_is_zero() {
        assert_eq!(dir_size_bytes(Path::new("/does/not/exist")), 0);
    }

    #[test]
    fn placement_input_lists_only_genuinely_unplaced_pending_tenants() {
        let manifest = ClusterManifest {
            version: 1,
            cells: vec![cell("cell-0"), cell("cell-1"), cell("cell-2")],
            tenants: vec![
                tenant("placed-co", 1, &["cell-0"], TenantStatus::Placed),
                tenant("pending-empty-co", 2, &[], TenantStatus::Pending),
                tenant("stuck-co", 1, &["cell-1"], TenantStatus::Pending),
            ],
            autoscaler: AutoscalerSpec::default(),
        };
        let input = placement_input(&manifest);
        assert_eq!(input.pending_tenants.len(), 1);
        assert_eq!(input.pending_tenants[0].name, "pending-empty-co");
        // cell-0 (placed-co) and cell-1 (stuck-co, still Pending but
        // already claims a cell) are both excluded from free_cells.
        assert_eq!(input.free_cells, vec!["cell-2".to_string()]);
        assert_eq!(input.assigned_counts.get("cell-0"), Some(&1));
        assert_eq!(input.assigned_counts.get("cell-1"), Some(&1));
        assert_eq!(input.assigned_counts.get("cell-2"), Some(&0));
    }

    #[test]
    fn placement_input_with_no_tenants_is_all_free() {
        let manifest = ClusterManifest {
            version: 1,
            cells: vec![cell("cell-0")],
            tenants: vec![],
            autoscaler: AutoscalerSpec::default(),
        };
        let input = placement_input(&manifest);
        assert!(input.pending_tenants.is_empty());
        assert_eq!(input.free_cells, vec!["cell-0".to_string()]);
    }

    /// Perf floor (plan Phase 6, "measure or it did not happen"): times
    /// [`MetricsSnapshot::assemble`] against a synthetic 64-cell,
    /// 16-tenant, supervisor-shaped input, built directly (no real
    /// supervisor, no real cells: `assemble` is pure, exactly so this is
    /// possible). `MAX_CELLS_IN_SNAPSHOT` is 64, so this is the largest
    /// snapshot the autoscaler will ever assemble without truncation. The
    /// 1s bound is an order-of-magnitude guard against an accidental O(n^2)
    /// or a stray I/O call sneaking into `assemble`, not a tight budget:
    /// pure in-memory assembly of 64 cells is expected to take
    /// microseconds.
    #[test]
    fn snapshot_assembly_of_64_cells_is_fast() {
        const N: usize = 64;
        const TENANTS: usize = 16;
        const REPLICAS: usize = N / TENANTS;

        let ranked_cells: Vec<CellStatus> = (0..N)
            .map(|i| CellStatus {
                id: format!("cell-{i}"),
                group: "default".to_string(),
                backend: BackendKind::Regolith,
                peer_id: format!("peer-{i}"),
                listen_addrs: vec![format!("/ip4/127.0.0.1/tcp/{}", 9171 + i)],
                marker_ok: true,
                connected_peers: Vec::new(),
            })
            .collect();

        let mut sync_statuses = HashMap::with_capacity(N);
        let mut storage_bytes = HashMap::with_capacity(N);
        let mut cell_requests = Vec::with_capacity(N);
        for i in 0..N {
            let id = format!("cell-{i}");
            sync_statuses.insert(id.clone(), serde_json::json!({"synced": true, "lag_ms": i}));
            storage_bytes.insert(id.clone(), 1_000_000u64 * i as u64);
            cell_requests.push(CellRequestCounters {
                cell_id: id,
                count: 1_000 + i as u64,
                sum_ms: 500.0,
                max_ms: 12.0,
                qps: 10.0,
            });
        }

        let mut tenants = Vec::with_capacity(TENANTS);
        let mut tenant_admission = Vec::with_capacity(TENANTS);
        for t in 0..TENANTS {
            let name = format!("tenant-{t}");
            let cell_ids: Vec<String> = (0..REPLICAS)
                .map(|r| format!("cell-{}", t * REPLICAS + r))
                .collect();
            let cell_refs: Vec<&str> = cell_ids.iter().map(String::as_str).collect();
            tenants.push(tenant(
                &name,
                REPLICAS as u8,
                &cell_refs,
                TenantStatus::Placed,
            ));
            tenant_admission.push(TenantAdmissionCounters {
                tenant: name,
                allowed: 500,
                rejected: 5,
            });
        }

        let manifest = ClusterManifest {
            version: 1,
            cells: (0..N).map(|i| cell(&format!("cell-{i}"))).collect(),
            tenants,
            autoscaler: AutoscalerSpec::default(),
        };
        let gateway_metrics = GatewayMetrics {
            cell_requests,
            tenant_admission,
        };
        let inputs = SnapshotInputs {
            tick: 42,
            min_cells: 1,
            max_cells: N,
            max_actions_per_tick: 4,
            cooldown_secs: 60,
            last_action: None,
            gateway_metrics: &gateway_metrics,
        };
        let host = HostMetrics {
            mem_total_kb: 32_000_000,
            mem_avail_kb: 16_000_000,
            load1: 2.5,
        };

        let start = Instant::now();
        let snapshot = MetricsSnapshot::assemble(
            ranked_cells,
            N,
            &sync_statuses,
            &storage_bytes,
            &manifest,
            host,
            inputs,
        );
        let elapsed = start.elapsed();

        assert_eq!(snapshot.cells.len(), N);
        assert_eq!(snapshot.tenants.len(), TENANTS);
        assert_eq!(snapshot.cap.cells_included, N);
        assert_eq!(snapshot.cap.cells_total, N);

        println!(
            "SNAP_MS build_64_cells={:.3}",
            elapsed.as_secs_f64() * 1000.0
        );
        assert!(
            elapsed < Duration::from_millis(1000),
            "assembling a 64-cell snapshot should be well under 1s; took {elapsed:?}"
        );
    }
}
