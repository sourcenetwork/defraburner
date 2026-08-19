//! Pure tenant-placement selection: which free cells a tenant's group
//! lands on. No I/O; `reconcile::reconcile` is the only caller that turns
//! the chosen ids into a live, wired group.

use std::collections::HashSet;

use anyhow::{Result, anyhow, bail};
use burner_cell::ClusterManifest;

/// Picks a tenant's replica group from `manifest`: free cells (not already
/// assigned to any tenant, per D14's disjoint-placement rule), preferring
/// the least-assigned. Idempotent: a tenant that already has cells
/// assigned (already placed) gets that same assignment back unchanged,
/// rather than picking a second, different group.
///
/// "Least-assigned first" degrades to manifest order today: v1 placement
/// is disjoint (D14), so every free cell ties at zero assignments and the
/// ranking has nothing to break ties on. It is still written as a ranking,
/// not a bare filter, so a later phase that allows shared-cell density
/// (multiple tenants per cell) can widen what "assignment count" means
/// here without changing this function's contract.
pub fn place(manifest: &ClusterManifest, tenant: &str) -> Result<Vec<String>> {
    let spec = manifest
        .tenants
        .iter()
        .find(|t| t.name == tenant)
        .ok_or_else(|| anyhow!("tenant '{tenant}' not found in cluster manifest"))?;

    if !spec.cells.is_empty() {
        return Ok(spec.cells.clone());
    }

    let assigned: HashSet<&str> = manifest
        .tenants
        .iter()
        .flat_map(|t| t.cells.iter().map(String::as_str))
        .collect();

    let free_cells: Vec<&str> = manifest
        .cells
        .iter()
        .map(|c| c.id.as_str())
        .filter(|id| !assigned.contains(id))
        .collect();

    let replicas = spec.replicas as usize;
    if free_cells.len() < replicas {
        bail!(
            "tenant '{tenant}' needs {replicas} free cell(s) but only {} are free \
             ({} cells total, {} already assigned to a tenant)",
            free_cells.len(),
            manifest.cells.len(),
            assigned.len(),
        );
    }

    Ok(free_cells
        .into_iter()
        .take(replicas)
        .map(String::from)
        .collect())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use burner_cell::{BackendKind, CellSpec, DEFAULT_MEM_BUDGET_BYTES, TenantSpec, TenantStatus};

    use super::*;

    fn cell(id: &str, port: u16) -> CellSpec {
        CellSpec {
            id: id.to_string(),
            group: "default".to_string(),
            backend: BackendKind::Lark,
            p2p_port: port,
            bind_addr: "127.0.0.1".parse().unwrap(),
            mem_budget_bytes: DEFAULT_MEM_BUDGET_BYTES,
            signing_key_file: PathBuf::from(format!("/data/keys/{id}.ed25519")),
        }
    }

    fn tenant_spec(name: &str, replicas: u8, cells: &[&str]) -> TenantSpec {
        let cells: Vec<String> = cells.iter().map(|c| c.to_string()).collect();
        TenantSpec {
            name: name.to_string(),
            replicas,
            status: if cells.is_empty() {
                TenantStatus::Pending
            } else {
                TenantStatus::Placed
            },
            cells,
            token_sha256: String::new(),
            admission: None,
            health: Default::default(),
        }
    }

    fn manifest(cells: Vec<CellSpec>, tenants: Vec<TenantSpec>) -> ClusterManifest {
        ClusterManifest {
            version: 1,
            cells,
            tenants,
            autoscaler: Default::default(),
        }
    }

    #[test]
    fn found_picks_free_cells_up_to_replicas() {
        let m = manifest(
            vec![
                cell("cell-0", 9171),
                cell("cell-1", 9172),
                cell("cell-2", 9173),
            ],
            vec![tenant_spec("acme-co", 2, &[])],
        );
        let placed = place(&m, "acme-co").unwrap();
        assert_eq!(placed.len(), 2);
        assert_eq!(placed, vec!["cell-0".to_string(), "cell-1".to_string()]);
    }

    #[test]
    fn found_skips_cells_already_assigned_to_another_tenant() {
        let m = manifest(
            vec![
                cell("cell-0", 9171),
                cell("cell-1", 9172),
                cell("cell-2", 9173),
            ],
            vec![
                tenant_spec("other-co", 1, &["cell-0"]),
                tenant_spec("acme-co", 2, &[]),
            ],
        );
        let placed = place(&m, "acme-co").unwrap();
        assert_eq!(placed, vec!["cell-1".to_string(), "cell-2".to_string()]);
    }

    #[test]
    fn insufficient_free_cells_errors_with_counts() {
        let m = manifest(
            vec![cell("cell-0", 9171)],
            vec![
                tenant_spec("other-co", 1, &["cell-0"]),
                tenant_spec("acme-co", 1, &[]),
            ],
        );
        let error = place(&m, "acme-co").unwrap_err();
        let message = error.to_string();
        assert!(message.contains("needs 1 free cell"));
        assert!(message.contains("only 0 are free"));
    }

    #[test]
    fn already_placed_tenant_returns_its_existing_assignment() {
        let m = manifest(
            vec![cell("cell-0", 9171), cell("cell-1", 9172)],
            vec![tenant_spec("acme-co", 1, &["cell-1"])],
        );
        let placed = place(&m, "acme-co").unwrap();
        assert_eq!(placed, vec!["cell-1".to_string()]);
    }

    #[test]
    fn unknown_tenant_errors() {
        let m = manifest(vec![cell("cell-0", 9171)], vec![]);
        let error = place(&m, "ghost-co").unwrap_err();
        assert!(error.to_string().contains("not found"));
    }
}
