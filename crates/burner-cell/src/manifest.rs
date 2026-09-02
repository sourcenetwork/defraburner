//! Cluster manifest: the durable record of every provisioned cell,
//! persisted at `<data_root>/cluster.json`.

use std::collections::HashSet;
use std::fs::{self, File};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::spec::{BackendKind, CellSpec, TenantSpec, is_valid_tenant_name};

/// The only manifest version this crate understands. Bumped on a breaking
/// change to the persisted shape.
pub const MANIFEST_VERSION: u32 = 1;
const MANIFEST_FILE_NAME: &str = "cluster.json";

/// The durable cluster manifest: every cell this data root knows about.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClusterManifest {
    pub version: u32,
    pub cells: Vec<CellSpec>,
    /// Tenant placements (Phase 2, D14). Defaulted so a Phase 1 manifest
    /// (written before this field existed) still loads: version stays 1,
    /// an absent `tenants` key means "no tenants yet", not a parse error.
    #[serde(default)]
    pub tenants: Vec<TenantSpec>,
    /// Live autoscaler overrides (console round, D23): `PUT
    /// /admin/autoscaler` persists here so a restart honors the last
    /// admin-configured values. Defaulted so a pre-D23 manifest (written
    /// before this field existed) still loads: version stays 1, an absent
    /// `autoscaler` key means "no overrides yet", not a parse error.
    #[serde(default)]
    pub autoscaler: AutoscalerSpec,
}

/// One field absent means "keep the CLI-configured/default value"; `paused`
/// alone is never optional ("not paused" is itself always a meaningful,
/// present state, not an absence).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutoscalerSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_cells: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_cells: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tick_interval_secs: Option<u64>,
    #[serde(default)]
    pub paused: bool,
    /// Whether the autoscaler may remove cells (D41).
    ///
    /// Defaults to `false`: the cluster grows on demand and never shrinks
    /// on its own. Draining a cell destroys the wasm database it owns
    /// (D40), so an automatic scale-down is a data-destroying action taken
    /// without an operator in the loop. For this proof of concept that
    /// trade is not worth making, and removal stays an explicit
    /// `DELETE /admin/cells/{id}`.
    ///
    /// `#[serde(default)]` on a `bool` yields `false`, so a manifest
    /// written before this field existed also loads with scale-down off,
    /// which is the safe direction for a default to drift.
    #[serde(default)]
    pub scale_down_enabled: bool,
}

impl Default for ClusterManifest {
    fn default() -> Self {
        Self::new()
    }
}

impl ClusterManifest {
    pub fn new() -> Self {
        Self {
            version: MANIFEST_VERSION,
            cells: Vec::new(),
            tenants: Vec::new(),
            autoscaler: AutoscalerSpec::default(),
        }
    }

    /// Path to the manifest file under `data_root`.
    pub fn path(data_root: &Path) -> PathBuf {
        data_root.join(MANIFEST_FILE_NAME)
    }

    /// True if a manifest file already exists under `data_root`.
    pub fn exists(data_root: &Path) -> bool {
        Self::path(data_root).exists()
    }

    /// Validates structural invariants: known version, unique cell ids,
    /// unique p2p ports, and no `Memory`-backend cells (dev-only; unsafe to
    /// persist since a fresh process can never recover its data).
    pub fn validate(&self) -> Result<()> {
        if self.version != MANIFEST_VERSION {
            bail!(
                "unsupported cluster manifest version {} (this build understands version {})",
                self.version,
                MANIFEST_VERSION
            );
        }

        let mut ids = HashSet::with_capacity(self.cells.len());
        let mut ports = HashSet::with_capacity(self.cells.len());
        for cell in &self.cells {
            if !ids.insert(cell.id.as_str()) {
                bail!("duplicate cell id in cluster manifest: '{}'", cell.id);
            }
            if !ports.insert(cell.p2p_port) {
                bail!(
                    "duplicate p2p_port {} in cluster manifest (cell '{}')",
                    cell.p2p_port,
                    cell.id
                );
            }
            if cell.backend == BackendKind::Memory {
                bail!(
                    "cell '{}' uses the Memory backend, which cannot be persisted in a cluster \
                     manifest (dev-only: its data does not survive a restart)",
                    cell.id
                );
            }
        }

        let mut tenant_names = HashSet::with_capacity(self.tenants.len());
        // Every cell id assigned to a tenant across the whole manifest, so a
        // second tenant (or a repeat within the same tenant) claiming an
        // already-assigned cell id is caught (D14: v1 placement is
        // disjoint, one tenant per cell).
        let mut assigned_cells: HashSet<&str> = HashSet::new();
        for tenant in &self.tenants {
            if !is_valid_tenant_name(&tenant.name) {
                bail!(
                    "invalid tenant name '{}': must match [a-z0-9-]{{1,63}}",
                    tenant.name
                );
            }
            if !tenant_names.insert(tenant.name.as_str()) {
                bail!(
                    "duplicate tenant name in cluster manifest: '{}'",
                    tenant.name
                );
            }
            if tenant.replicas == 0 {
                bail!(
                    "tenant '{}' has replicas=0; replicas must be at least 1",
                    tenant.name
                );
            }
            for cell_id in &tenant.cells {
                if !ids.contains(cell_id.as_str()) {
                    bail!(
                        "tenant '{}' is assigned to unknown cell id '{}'",
                        tenant.name,
                        cell_id
                    );
                }
                if !assigned_cells.insert(cell_id.as_str()) {
                    bail!(
                        "cell id '{}' is assigned more than once across tenant placements \
                         (v1 disjoint placement, D14): each cell serves at most one tenant",
                        cell_id
                    );
                }
            }
        }
        Ok(())
    }

    /// Saves this manifest atomically: write `cluster.json.tmp`, fsync the
    /// file, rename over `cluster.json`, then fsync the containing
    /// directory so the rename itself is durable.
    pub async fn save(&self, data_root: &Path) -> Result<()> {
        self.validate()?;
        let json = serde_json::to_vec_pretty(self).context("serializing cluster manifest")?;
        let data_root = data_root.to_path_buf();
        tokio::task::spawn_blocking(move || save_blocking(&data_root, &json))
            .await
            .context("manifest save task panicked")??;
        Ok(())
    }

    /// Loads and validates the manifest at `<data_root>/cluster.json`.
    pub async fn load(data_root: &Path) -> Result<Self> {
        let path = Self::path(data_root);
        let owned_path = path.clone();
        let bytes = tokio::task::spawn_blocking(move || fs::read(&owned_path))
            .await
            .context("manifest load task panicked")?
            .with_context(|| format!("reading {}", path.display()))?;
        let manifest: ClusterManifest = serde_json::from_slice(&bytes)
            .with_context(|| format!("parsing {}", path.display()))?;
        manifest.validate()?;
        Ok(manifest)
    }
}

fn save_blocking(data_root: &Path, json: &[u8]) -> Result<()> {
    use std::io::Write;

    let final_path = ClusterManifest::path(data_root);
    let tmp_path = data_root.join(format!("{MANIFEST_FILE_NAME}.tmp"));

    let mut file =
        File::create(&tmp_path).with_context(|| format!("creating {}", tmp_path.display()))?;
    file.write_all(json)
        .with_context(|| format!("writing {}", tmp_path.display()))?;
    file.sync_all()
        .with_context(|| format!("fsyncing {}", tmp_path.display()))?;
    drop(file);

    fs::rename(&tmp_path, &final_path).with_context(|| {
        format!(
            "renaming {} to {}",
            tmp_path.display(),
            final_path.display()
        )
    })?;

    let dir = File::open(data_root)
        .with_context(|| format!("opening directory {} for fsync", data_root.display()))?;
    dir.sync_all()
        .with_context(|| format!("fsyncing directory {}", data_root.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::DEFAULT_MEM_BUDGET_BYTES;
    use proptest::prelude::*;

    fn cell(id: &str, port: u16) -> CellSpec {
        CellSpec {
            id: id.to_string(),
            group: "default".to_string(),
            backend: BackendKind::Regolith,
            p2p_port: port,
            bind_addr: "127.0.0.1".parse().unwrap(),
            mem_budget_bytes: DEFAULT_MEM_BUDGET_BYTES,
            signing_key_file: PathBuf::from(format!("/data/keys/{id}.ed25519")),
        }
    }

    fn tenant(name: &str, replicas: u8, cells: &[&str]) -> TenantSpec {
        let cells: Vec<String> = cells.iter().map(|c| c.to_string()).collect();
        TenantSpec {
            name: name.to_string(),
            replicas,
            status: if cells.is_empty() {
                crate::spec::TenantStatus::Pending
            } else {
                crate::spec::TenantStatus::Placed
            },
            cells,
            token_sha256: String::new(),
            admission: None,
            health: Default::default(),
        }
    }

    #[test]
    fn validate_rejects_unknown_version() {
        let manifest = ClusterManifest {
            version: 2,
            cells: vec![],
            tenants: vec![],
            autoscaler: AutoscalerSpec::default(),
        };
        let error = manifest.validate().unwrap_err();
        assert!(
            error
                .to_string()
                .contains("unsupported cluster manifest version")
        );
    }

    #[test]
    fn validate_rejects_duplicate_ids() {
        let manifest = ClusterManifest {
            version: MANIFEST_VERSION,
            cells: vec![cell("cell-0", 9171), cell("cell-0", 9172)],
            tenants: vec![],
            autoscaler: AutoscalerSpec::default(),
        };
        let error = manifest.validate().unwrap_err();
        assert!(error.to_string().contains("duplicate cell id"));
    }

    #[test]
    fn validate_rejects_duplicate_ports() {
        let manifest = ClusterManifest {
            version: MANIFEST_VERSION,
            cells: vec![cell("cell-0", 9171), cell("cell-1", 9171)],
            tenants: vec![],
            autoscaler: AutoscalerSpec::default(),
        };
        let error = manifest.validate().unwrap_err();
        assert!(error.to_string().contains("duplicate p2p_port"));
    }

    #[test]
    fn validate_rejects_memory_backend() {
        let mut memory_cell = cell("cell-0", 9171);
        memory_cell.backend = BackendKind::Memory;
        let manifest = ClusterManifest {
            version: MANIFEST_VERSION,
            cells: vec![memory_cell],
            tenants: vec![],
            autoscaler: AutoscalerSpec::default(),
        };
        let error = manifest.validate().unwrap_err();
        assert!(error.to_string().contains("Memory backend"));
    }

    #[test]
    fn validate_accepts_a_well_formed_manifest() {
        let manifest = ClusterManifest {
            version: MANIFEST_VERSION,
            cells: vec![cell("cell-0", 9171), cell("cell-1", 9172)],
            tenants: vec![],
            autoscaler: AutoscalerSpec::default(),
        };
        manifest.validate().unwrap();
    }

    #[test]
    fn validate_accepts_a_well_formed_manifest_with_tenants() {
        let manifest = ClusterManifest {
            version: MANIFEST_VERSION,
            cells: vec![cell("cell-0", 9171), cell("cell-1", 9172)],
            tenants: vec![
                tenant("acme-co", 1, &["cell-0"]),
                tenant("other-co", 1, &["cell-1"]),
                tenant("pending-co", 2, &[]),
            ],
            autoscaler: AutoscalerSpec::default(),
        };
        manifest.validate().unwrap();
    }

    #[test]
    fn validate_rejects_invalid_tenant_name() {
        let manifest = ClusterManifest {
            version: MANIFEST_VERSION,
            cells: vec![cell("cell-0", 9171)],
            tenants: vec![tenant("Acme_Co", 1, &[])],
            autoscaler: AutoscalerSpec::default(),
        };
        let error = manifest.validate().unwrap_err();
        assert!(error.to_string().contains("invalid tenant name"));
    }

    #[test]
    fn validate_rejects_duplicate_tenant_names() {
        let manifest = ClusterManifest {
            version: MANIFEST_VERSION,
            cells: vec![cell("cell-0", 9171), cell("cell-1", 9172)],
            tenants: vec![
                tenant("acme-co", 1, &["cell-0"]),
                tenant("acme-co", 1, &["cell-1"]),
            ],
            autoscaler: AutoscalerSpec::default(),
        };
        let error = manifest.validate().unwrap_err();
        assert!(error.to_string().contains("duplicate tenant name"));
    }

    #[test]
    fn validate_rejects_zero_replicas() {
        let manifest = ClusterManifest {
            version: MANIFEST_VERSION,
            cells: vec![],
            tenants: vec![tenant("acme-co", 0, &[])],
            autoscaler: AutoscalerSpec::default(),
        };
        let error = manifest.validate().unwrap_err();
        assert!(error.to_string().contains("replicas=0"));
    }

    #[test]
    fn validate_rejects_tenant_assigned_to_unknown_cell() {
        let manifest = ClusterManifest {
            version: MANIFEST_VERSION,
            cells: vec![cell("cell-0", 9171)],
            tenants: vec![tenant("acme-co", 1, &["cell-99"])],
            autoscaler: AutoscalerSpec::default(),
        };
        let error = manifest.validate().unwrap_err();
        assert!(error.to_string().contains("unknown cell id"));
    }

    #[test]
    fn validate_rejects_a_cell_assigned_to_two_tenants() {
        let manifest = ClusterManifest {
            version: MANIFEST_VERSION,
            cells: vec![cell("cell-0", 9171)],
            tenants: vec![
                tenant("acme-co", 1, &["cell-0"]),
                tenant("other-co", 1, &["cell-0"]),
            ],
            autoscaler: AutoscalerSpec::default(),
        };
        let error = manifest.validate().unwrap_err();
        assert!(
            error
                .to_string()
                .contains("assigned more than once across tenant placements")
        );
    }

    #[tokio::test]
    async fn save_then_load_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = ClusterManifest {
            version: MANIFEST_VERSION,
            cells: vec![cell("cell-0", 9171), cell("cell-1", 9172)],
            tenants: vec![tenant("acme-co", 1, &["cell-0"])],
            autoscaler: AutoscalerSpec::default(),
        };

        manifest.save(dir.path()).await.unwrap();
        assert!(ClusterManifest::exists(dir.path()));
        let loaded = ClusterManifest::load(dir.path()).await.unwrap();

        assert_eq!(manifest, loaded);
        assert!(!dir.path().join("cluster.json.tmp").exists());
    }

    #[tokio::test]
    async fn save_rejects_invalid_manifest_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = ClusterManifest {
            version: MANIFEST_VERSION,
            cells: vec![cell("cell-0", 9171), cell("cell-0", 9172)],
            tenants: vec![],
            autoscaler: AutoscalerSpec::default(),
        };

        assert!(manifest.save(dir.path()).await.is_err());
        assert!(!ClusterManifest::exists(dir.path()));
    }

    #[test]
    fn autoscaler_spec_round_trips_through_json() {
        let spec = AutoscalerSpec {
            scale_down_enabled: false,
            min_cells: Some(2),
            max_cells: Some(6),
            cooldown_secs: Some(90),
            tick_interval_secs: Some(10),
            paused: true,
        };
        let json = serde_json::to_string(&spec).unwrap();
        let parsed: AutoscalerSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(spec, parsed);
    }

    #[test]
    fn autoscaler_spec_defaults_every_field_when_absent() {
        // A pre-D23 manifest has no `autoscaler` key at all; a manifest
        // written after D23 but before any `PUT /admin/autoscaler` call
        // has `"autoscaler":{}` (every field its own default). Both must
        // parse to the same all-absent, not-paused value.
        let manifest_json = format!(r#"{{"version":{MANIFEST_VERSION},"cells":[],"tenants":[]}}"#);
        let parsed: ClusterManifest = serde_json::from_str(&manifest_json).unwrap();
        assert_eq!(parsed.autoscaler, AutoscalerSpec::default());
        assert_eq!(parsed.autoscaler.min_cells, None);
        assert!(!parsed.autoscaler.paused);

        let spec: AutoscalerSpec = serde_json::from_str("{}").unwrap();
        assert_eq!(spec, AutoscalerSpec::default());
    }

    #[test]
    fn autoscaler_spec_omits_absent_fields_from_json() {
        let json = serde_json::to_string(&AutoscalerSpec::default()).unwrap();
        // Option fields are omitted when absent; the bools are always
        // written, so an operator reading the manifest sees the safety
        // posture (D41: scale-down off) stated rather than implied by
        // absence.
        assert_eq!(json, r#"{"paused":false,"scale_down_enabled":false}"#);
    }

    fn backend_strategy() -> impl Strategy<Value = BackendKind> {
        Just(BackendKind::Regolith)
    }

    /// Arbitrary (console round, D23): every field independently present or
    /// absent, `paused` either value, so the manifest round-trip proptest
    /// below covers the whole autoscaler-section shape, not just its
    /// all-default case.
    fn autoscaler_spec_strategy() -> impl Strategy<Value = AutoscalerSpec> {
        (
            proptest::option::of(1usize..64),
            proptest::option::of(1usize..64),
            proptest::option::of(any::<u64>()),
            proptest::option::of(any::<u64>()),
            any::<bool>(),
            any::<bool>(),
        )
            .prop_map(
                |(
                    min_cells,
                    max_cells,
                    cooldown_secs,
                    tick_interval_secs,
                    paused,
                    scale_down_enabled,
                )| {
                    AutoscalerSpec {
                        scale_down_enabled,
                        min_cells,
                        max_cells,
                        cooldown_secs,
                        tick_interval_secs,
                        paused,
                    }
                },
            )
    }

    /// Generates a manifest whose cells are valid by construction: id and
    /// p2p_port are derived from the cell's index, so uniqueness holds for
    /// every generated value and round-trip failures point at
    /// serialization, never at the generator producing an invalid fixture.
    fn valid_manifest_strategy() -> impl Strategy<Value = ClusterManifest> {
        (
            prop::collection::vec((backend_strategy(), any::<u64>()), 0..8),
            autoscaler_spec_strategy(),
        )
            .prop_map(|(entries, autoscaler)| {
                let cells = entries
                    .into_iter()
                    .enumerate()
                    .map(|(i, (backend, mem_budget_bytes))| CellSpec {
                        id: format!("cell-{i}"),
                        group: "default".to_string(),
                        backend,
                        p2p_port: 9171 + i as u16,
                        bind_addr: "127.0.0.1".parse().unwrap(),
                        mem_budget_bytes,
                        signing_key_file: PathBuf::from(format!("/data/keys/cell-{i}.ed25519")),
                    })
                    .collect();
                ClusterManifest {
                    version: MANIFEST_VERSION,
                    cells,
                    tenants: Vec::new(),
                    autoscaler,
                }
            })
    }

    /// Layers valid-by-construction tenants onto `valid_manifest_strategy`'s
    /// cells: each generated tenant claims a disjoint prefix slice of the
    /// remaining cell ids (so D14's one-cell-one-tenant invariant holds by
    /// construction, and round-trip failures point at serialization, never
    /// at the generator producing an invalid fixture), named uniquely by
    /// index, with `replicas` matching its assigned cell count (at least 1).
    fn valid_manifest_with_tenants_strategy() -> impl Strategy<Value = ClusterManifest> {
        (
            valid_manifest_strategy(),
            prop::collection::vec(0usize..3, 0..4),
        )
            .prop_map(|(manifest, tenant_cell_wants)| {
                let mut remaining: Vec<String> =
                    manifest.cells.iter().map(|c| c.id.clone()).collect();
                let tenants = tenant_cell_wants
                    .into_iter()
                    .enumerate()
                    .map(|(i, wanted)| {
                        let take = wanted.min(remaining.len());
                        let cells: Vec<String> = remaining.drain(0..take).collect();
                        let status = if cells.is_empty() {
                            crate::spec::TenantStatus::Pending
                        } else {
                            crate::spec::TenantStatus::Placed
                        };
                        TenantSpec {
                            name: format!("tenant-{i}"),
                            replicas: take.max(1) as u8,
                            cells,
                            token_sha256: String::new(),
                            status,
                            admission: None,
                            health: Default::default(),
                        }
                    })
                    .collect();
                ClusterManifest {
                    tenants,
                    ..manifest
                }
            })
    }

    proptest! {
        #[test]
        fn manifest_survives_save_and_load(manifest in valid_manifest_strategy()) {
            let dir = tempfile::tempdir().unwrap();
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let loaded = rt.block_on(async {
                manifest.save(dir.path()).await.unwrap();
                ClusterManifest::load(dir.path()).await.unwrap()
            });
            prop_assert_eq!(&manifest, &loaded);
        }

        #[test]
        fn manifest_with_tenants_survives_save_and_load(
            manifest in valid_manifest_with_tenants_strategy()
        ) {
            let dir = tempfile::tempdir().unwrap();
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let loaded = rt.block_on(async {
                manifest.save(dir.path()).await.unwrap();
                ClusterManifest::load(dir.path()).await.unwrap()
            });
            prop_assert_eq!(&manifest, &loaded);
        }
    }
}
