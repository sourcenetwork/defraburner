//! Growing a placed tenant's schema: adding collections to a tenant that
//! is already placed and wired.
//!
//! Tenant creation ([`crate::reconcile::reconcile`]'s `Pending` branch) applies a
//! tenant's whole SDL at placement time. This module covers the other
//! direction, adding a collection to a tenant that is already serving
//! traffic, without draining it, re-placing it, or touching the data it
//! already holds.

use std::path::Path;

use anyhow::{Context, Result, anyhow};
use burner_cell::{ClusterManifest, Supervisor, TenantStatus};

use crate::reconcile::{collection_names, resolve_running_cells, schema_already_registered};
use crate::wiring::wire_group;

/// Applies `sdl_fragment` to every cell holding `tenant`, wires the
/// collections it declares for replication across that group, and appends
/// the fragment to the tenant's stored SDL. Returns the names of the
/// collections that were added.
///
/// The caller is expected to have already rejected the request-shaped
/// failures (a fragment that does not parse, a collection name the tenant
/// already declares, a tenant that is not `Placed`); everything this
/// function itself returns `Err` for is a genuine execution failure.
///
/// # Ordering
///
/// Cells first, stored SDL last, deliberately. A stored SDL naming a
/// collection the cells do not carry would make every later reconcile try
/// to wire a collection that cannot be resolved locally, degrading the
/// tenant on that restart and every one after it. The opposite leftover,
/// a collection registered on a cell but absent from the stored SDL, is
/// inert: nothing wires it, nothing routes to it, and nothing writes to
/// it.
///
/// # Retry
///
/// Idempotent per cell: a cell already carrying every collection in the
/// fragment is skipped rather than re-schema'd, so retrying after a
/// partial failure finishes the job instead of failing on the cells that
/// already succeeded.
pub async fn add_collections(
    supervisor: &mut Supervisor,
    data_root: &Path,
    tenant: &str,
    sdl_fragment: &str,
) -> Result<Vec<String>> {
    let new_collections = collection_names(sdl_fragment)
        .with_context(|| format!("parsing the new schema for tenant '{tenant}'"))?;
    if new_collections.is_empty() {
        return Err(anyhow!("the new schema declares no collections"));
    }

    let manifest = ClusterManifest::load(data_root)
        .await
        .context("loading cluster manifest to add collections")?;
    let spec = manifest
        .tenants
        .iter()
        .find(|t| t.name == tenant)
        .ok_or_else(|| anyhow!("tenant '{tenant}' is not in the cluster manifest"))?;
    if spec.status != TenantStatus::Placed {
        return Err(anyhow!(
            "tenant '{tenant}' is not placed yet; it has no cells to add a collection to"
        ));
    }
    let cell_ids = spec.cells.clone();

    let cells = resolve_running_cells(supervisor, &cell_ids)
        .with_context(|| format!("resolving the cells holding tenant '{tenant}'"))?;
    for cell in &cells {
        if schema_already_registered(&cell.node, &new_collections) {
            continue;
        }
        cell.node.add_schema(sdl_fragment).await.with_context(|| {
            format!(
                "adding collections {new_collections:?} for tenant '{tenant}' on cell '{}'",
                cell.spec.id
            )
        })?;
    }

    // `wire_group`, not `ensure_group_connected`: these subscriptions are
    // genuinely new in this process, so the edge-triggered topic-join
    // event this waits on really does still fire (see
    // `wiring::wire_group`'s doc comment for why an already-`Placed`
    // tenant's *existing* collections cannot be confirmed that way).
    let mut confirmed = supervisor.confirmed_topic_joins_snapshot();
    let wire_result = wire_group(&cells, &new_collections, &mut confirmed)
        .await
        .with_context(|| format!("wiring new collections for tenant '{tenant}'"));
    drop(cells); // release the borrow of `supervisor` before merging back
    supervisor.merge_confirmed_topic_joins(confirmed);
    wire_result?;

    append_tenant_sdl(data_root, tenant, sdl_fragment)
        .await
        .with_context(|| format!("appending the new schema to tenant '{tenant}'s stored SDL"))?;

    Ok(new_collections)
}

/// Appends `fragment` to a tenant's stored SDL, keeping the file a valid
/// concatenation of type declarations (the shape `query::parse_sdl` reads
/// and `reconcile` re-applies to a late-joining cell).
async fn append_tenant_sdl(data_root: &Path, tenant: &str, fragment: &str) -> Result<()> {
    let path = crate::reconcile::tenant_sdl_path(data_root, tenant);
    let existing = tokio::fs::read_to_string(&path)
        .await
        .with_context(|| format!("reading {}", path.display()))?;
    let mut combined = existing.trim_end().to_string();
    combined.push('\n');
    combined.push_str(fragment.trim());
    combined.push('\n');
    // Parsed before it is written: a fragment that is valid alone but
    // invalid appended (a duplicate type across the seam) must not be
    // persisted, since every later reconcile reads this file.
    collection_names(&combined)
        .context("the combined schema does not parse; leaving the stored SDL unchanged")?;
    tokio::fs::write(&path, combined)
        .await
        .with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn write_sdl(root: &Path, tenant: &str, sdl: &str) {
        let path = crate::reconcile::tenant_sdl_path(root, tenant);
        tokio::fs::create_dir_all(path.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(&path, sdl).await.unwrap();
    }

    #[tokio::test]
    async fn append_joins_the_fragment_onto_the_stored_sdl() {
        let dir = tempfile::tempdir().unwrap();
        write_sdl(dir.path(), "acme", "type Widget { name: String }\n").await;

        append_tenant_sdl(dir.path(), "acme", "type Gadget { size: Int }")
            .await
            .unwrap();

        let stored =
            tokio::fs::read_to_string(crate::reconcile::tenant_sdl_path(dir.path(), "acme"))
                .await
                .unwrap();
        assert_eq!(
            collection_names(&stored).unwrap(),
            vec!["Widget".to_string(), "Gadget".to_string()]
        );
    }

    #[tokio::test]
    async fn append_leaves_the_stored_sdl_alone_when_the_result_would_not_parse() {
        let dir = tempfile::tempdir().unwrap();
        let original = "type Widget { name: String }\n";
        write_sdl(dir.path(), "acme", original).await;

        let error = append_tenant_sdl(dir.path(), "acme", "type Broken {{{")
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("does not parse"));

        let stored =
            tokio::fs::read_to_string(crate::reconcile::tenant_sdl_path(dir.path(), "acme"))
                .await
                .unwrap();
        assert_eq!(
            stored, original,
            "a rejected append must not touch the file"
        );
    }

    #[tokio::test]
    async fn add_collections_rejects_a_tenant_that_is_not_in_the_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let mut supervisor = Supervisor::new(dir.path());
        ClusterManifest::default().save(dir.path()).await.unwrap();

        let error = add_collections(
            &mut supervisor,
            dir.path(),
            "ghost",
            "type Widget { name: String }",
        )
        .await
        .unwrap_err();
        assert!(format!("{error:#}").contains("not in the cluster manifest"));
    }

    #[tokio::test]
    async fn add_collections_rejects_a_fragment_that_declares_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mut supervisor = Supervisor::new(dir.path());
        ClusterManifest::default().save(dir.path()).await.unwrap();

        let error = add_collections(&mut supervisor, dir.path(), "acme", "   ")
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("declares no collections"));
    }
}
