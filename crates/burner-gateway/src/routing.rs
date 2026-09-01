//! Token -> tenant -> cell-group resolution, and sticky-by-token routing
//! within a tenant's group with failover to the next cell if the sticky
//! pick is not currently running. See `docs/consistency.md` for what
//! "sticky" does and does not guarantee.

use std::collections::HashSet;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use axum::Router;
use burner_cell::{ClusterManifest, Supervisor};
use kovan_map::HopscotchMap;

use crate::router_build::build_cell_router;

/// One tenant's resolved routing entry: its current token digest (so a
/// stale cached mapping from before a token rotation can be detected and
/// rejected) and its group's cell ids in placement order: the order
/// `sticky_index` indexes into.
#[derive(Debug, Clone)]
struct TenantRoute {
    token_sha256: String,
    cells: Vec<String>,
}

/// Token/tenant/cell-group/router cache, rebuilt wholesale on every
/// `reconcile` (placement can change which cells serve a tenant; a token
/// rotation changes which digest a tenant answers to). Every table is a
/// lock-free concurrent map (`kovan_map`), so `rebuild` takes `&self`, not
/// `&mut self`: it can run concurrently with in-flight route lookups
/// without a global lock.
pub struct RoutingTable {
    /// tenant name -> route.
    tenants: HopscotchMap<String, Arc<TenantRoute>>,
    /// token sha256 digest -> tenant name.
    tokens: HopscotchMap<String, Arc<String>>,
    /// cell id -> its built router, cached so a hot request path never
    /// rebuilds one.
    routers: HopscotchMap<String, Router>,
}

impl RoutingTable {
    pub fn new() -> Self {
        Self {
            tenants: HopscotchMap::new(),
            tokens: HopscotchMap::new(),
            routers: HopscotchMap::new(),
        }
    }

    /// Reloads the manifest at `data_root` and rebuilds the token/tenant
    /// and cell-group tables from it, building (and caching) a router for
    /// any newly-referenced, currently-running cell that doesn't have one
    /// cached yet. A tenant with no token issued yet, or no cells placed
    /// yet, is skipped (not routable until both are true).
    ///
    /// A genuine rebuild, not a monotonic union (D25/D23): any `tenants`/
    /// `tokens` entry left over from a *previous* rebuild but absent (or
    /// no longer token+cells-routable) from this one is dropped. Without
    /// this, a dropped tenant's already-cached `TenantRoute`: whose
    /// `token_sha256` still matches its now-revoked token: would keep
    /// satisfying `resolve_tenant`'s own "re-check against the tenant's
    /// current digest" defense forever, since there would be no fresher
    /// manifest entry to ever overwrite it with. `routers` (cell id ->
    /// built `Router`) is left untouched on purpose: a cell that is still
    /// running keeps its cached router regardless of which tenant (if
    /// any) currently references it, and `route()` already refuses to use
    /// a cached router for a cell the supervisor no longer reports as
    /// running.
    pub async fn rebuild(&self, data_root: &Path, supervisor: &Supervisor) -> Result<()> {
        let manifest = ClusterManifest::load(data_root)
            .await
            .context("loading cluster manifest to rebuild the routing table")?;

        let mut live_names: HashSet<String> = HashSet::new();
        let mut live_tokens: HashSet<String> = HashSet::new();

        for tenant in &manifest.tenants {
            if tenant.token_sha256.is_empty() || tenant.cells.is_empty() {
                continue;
            }
            live_names.insert(tenant.name.clone());
            live_tokens.insert(tenant.token_sha256.clone());

            let route = Arc::new(TenantRoute {
                token_sha256: tenant.token_sha256.clone(),
                cells: tenant.cells.clone(),
            });
            self.tenants.insert(tenant.name.clone(), route);
            self.tokens
                .insert(tenant.token_sha256.clone(), Arc::new(tenant.name.clone()));

            for cell_id in &tenant.cells {
                if self.routers.get(cell_id).is_some() {
                    continue;
                }
                let Some(cell) = supervisor.running_cell(cell_id) else {
                    // Not running (yet); picked up on a later rebuild once
                    // it is.
                    continue;
                };
                let router = build_cell_router(&cell.node)
                    .with_context(|| format!("building router for cell '{cell_id}'"))?;
                self.routers.insert(cell_id.clone(), router);
            }
        }

        for stale_name in self
            .tenants
            .keys()
            .filter(|name| !live_names.contains(name))
            .collect::<Vec<_>>()
        {
            self.tenants.remove(&stale_name);
        }
        for stale_token in self
            .tokens
            .keys()
            .filter(|token| !live_tokens.contains(token))
            .collect::<Vec<_>>()
        {
            self.tokens.remove(&stale_token);
        }
        Ok(())
    }

    /// Resolves a presented bearer token to its tenant name.
    ///
    /// The token-digest map lookup itself is an ordinary hash lookup (not
    /// constant-time; defending that is a much larger data-structure
    /// change than this gateway's threat model calls for), but the final
    /// confirmation against the resolved tenant's *current* stored digest
    /// uses `auth::digests_match`'s vetted constant-time comparison. That
    /// confirmation is not just theater: during the narrow window of a
    /// `rebuild` racing a token rotation, the `tokens` map can briefly
    /// hold a stale digest -> tenant-name entry for a token that
    /// `tenants` has already moved past; re-checking against `tenants`'
    /// current `token_sha256` rejects that stale mapping instead of
    /// granting access on it.
    pub fn resolve_tenant(&self, token: &str) -> Option<String> {
        let digest = crate::auth::digest_hex(token);
        let tenant_name = self.tokens.get(&digest)?;
        let route = self.tenants.get(tenant_name.as_str())?;
        crate::auth::digests_match(&route.token_sha256, &digest).then(|| (*tenant_name).clone())
    }

    /// Sticky-by-token pick within `tenant`'s group
    /// (`index = hash(token) % cells.len()`), with failover to the next
    /// cell (wrapping) if the sticky pick is not currently running.
    /// Returns the chosen cell's id and a clone of its cached router
    /// (`axum::Router` clones are cheap: an `Arc` handle internally).
    pub fn route(
        &self,
        tenant: &str,
        token: &str,
        supervisor: &Supervisor,
    ) -> Result<(String, Router)> {
        let route = self
            .tenants
            .get(tenant)
            .ok_or_else(|| anyhow!("tenant '{tenant}' has no routing entry"))?;
        if route.cells.is_empty() {
            bail!("tenant '{tenant}' has no assigned cells");
        }

        let start = sticky_index(token, route.cells.len());
        for offset in 0..route.cells.len() {
            let index = (start + offset) % route.cells.len();
            let cell_id = &route.cells[index];
            if supervisor.running_cell(cell_id).is_none() {
                continue;
            }
            if let Some(router) = self.routers.get(cell_id) {
                return Ok((cell_id.clone(), router));
            }
        }
        Err(anyhow!(
            "no running, routable cell for tenant '{tenant}' ({} assigned cells, none available)",
            route.cells.len()
        ))
    }
}

impl Default for RoutingTable {
    fn default() -> Self {
        Self::new()
    }
}

/// `hash(token) % cell_count`: a stable, deterministic pick so repeat
/// requests bearing the same token land on the same cell while it stays
/// up (session read-your-writes as an optimization, not a guarantee --
/// see docs/consistency.md).
fn sticky_index(token: &str, cell_count: usize) -> usize {
    let mut hasher = DefaultHasher::new();
    token.hash(&mut hasher);
    (hasher.finish() % cell_count as u64) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sticky_index_is_deterministic_and_in_range() {
        let a = sticky_index("token-a", 5);
        let b = sticky_index("token-a", 5);
        assert_eq!(a, b);
        assert!(a < 5);
    }

    #[test]
    fn sticky_index_a_single_cell_group_always_picks_zero() {
        assert_eq!(sticky_index("any-token", 1), 0);
        assert_eq!(sticky_index("another-token", 1), 0);
    }

    #[test]
    fn sticky_index_varies_with_the_token() {
        // Not a strict requirement (a hash collision is legal), but with
        // a reasonable cell count and a handful of distinct tokens, at
        // least one pair should land on different cells: proving this
        // is not silently constant-zero for every input.
        let picks: std::collections::HashSet<usize> = (0..16)
            .map(|i| sticky_index(&format!("token-{i}"), 4))
            .collect();
        assert!(
            picks.len() > 1,
            "expected some spread across 4 cells, got {picks:?}"
        );
    }

    /// D25/D23: `resolve_tenant` must stop honoring a tenant's token the
    /// moment `rebuild` sees that tenant is gone from the manifest --
    /// dropping a tenant (and the routes rebuild that follows) is exactly
    /// how token revocation takes effect. No real cells are needed here:
    /// with zero cells provisioned, `rebuild`'s `routers` step never
    /// finds a running cell, so this exercises the `tenants`/`tokens`
    /// cleanup in isolation.
    #[tokio::test]
    async fn rebuild_revokes_a_dropped_tenants_token() {
        let dir = tempfile::tempdir().unwrap();
        let data_root = dir.path();
        let supervisor = Supervisor::new(data_root);

        let token = "acme-co-token";
        let digest = crate::auth::digest_hex(token);
        let mut manifest = burner_cell::ClusterManifest::new();
        manifest.tenants.push(burner_cell::TenantSpec {
            name: "acme-co".to_string(),
            replicas: 1,
            cells: vec!["cell-0".to_string()],
            token_sha256: digest,
            status: burner_cell::TenantStatus::Placed,
            admission: None,
            health: Default::default(),
        });
        manifest.cells.push(burner_cell::CellSpec {
            id: "cell-0".to_string(),
            group: "default".to_string(),
            backend: burner_cell::BackendKind::Regolith,
            p2p_port: 9171,
            bind_addr: "127.0.0.1".parse().unwrap(),
            mem_budget_bytes: burner_cell::DEFAULT_MEM_BUDGET_BYTES,
            signing_key_file: std::path::PathBuf::from("/does/not/matter.ed25519"),
        });
        manifest.save(data_root).await.unwrap();

        let table = RoutingTable::new();
        table.rebuild(data_root, &supervisor).await.unwrap();
        assert_eq!(
            table.resolve_tenant(token).as_deref(),
            Some("acme-co"),
            "token should resolve while the tenant is still in the manifest"
        );

        // Drop the tenant (mirrors admin_tenants::admin_drop_tenant) and
        // rebuild again.
        let mut manifest = burner_cell::ClusterManifest::load(data_root).await.unwrap();
        manifest.tenants.clear();
        manifest.save(data_root).await.unwrap();
        table.rebuild(data_root, &supervisor).await.unwrap();

        assert_eq!(
            table.resolve_tenant(token),
            None,
            "a dropped tenant's token must stop resolving once rebuild sees it gone"
        );
    }
}
