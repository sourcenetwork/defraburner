//! `defraburner tenant`: offline tenant provisioning (D14). `create`
//! validates and edits the cluster manifest without igniting any cell;
//! placement happens on the next `start` (`burner_mesh::reconcile`).
//! `list` is a read-only manifest dump. `rotate-token` issues a fresh
//! bearer token for an existing tenant (Phase 3).

use std::path::PathBuf;

use anyhow::{Context, Result, anyhow, bail};
use burner_cell::{ClusterManifest, TenantSpec, TenantStatus, is_valid_tenant_name};
use burner_gateway::auth;

/// Runs `tenant create`: reads and validates `schema` as GraphQL SDL,
/// copies it to `data_root/tenants/<name>.graphql`, appends a `Pending`
/// [`TenantSpec`] to the cluster manifest (with a freshly issued bearer
/// token already attached, printed once), and saves it.
pub async fn create(data_root: PathBuf, name: String, schema: PathBuf, replicas: u8) -> Result<()> {
    if !is_valid_tenant_name(&name) {
        bail!("invalid tenant name '{name}': must match [a-z0-9-]{{1,63}}");
    }
    if replicas == 0 {
        bail!("--replicas must be at least 1");
    }

    tokio::fs::create_dir_all(&data_root)
        .await
        .with_context(|| format!("creating data root {}", data_root.display()))?;

    let mut manifest = if ClusterManifest::exists(&data_root) {
        ClusterManifest::load(&data_root)
            .await
            .context("loading cluster manifest")?
    } else {
        ClusterManifest::new()
    };

    if manifest.tenants.iter().any(|t| t.name == name) {
        bail!("tenant '{name}' already exists in the cluster manifest");
    }

    let sdl = tokio::fs::read_to_string(&schema)
        .await
        .with_context(|| format!("reading schema file {}", schema.display()))?;
    let collections = query::parse_sdl(&sdl)
        .map_err(|error| anyhow!("SDL parse error in {}: {error}", schema.display()))?;
    if collections.is_empty() {
        bail!("schema file {} declares no collections", schema.display());
    }

    let sdl_path = burner_mesh::tenant_sdl_path(&data_root, &name);
    if let Some(parent) = sdl_path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("creating tenants directory {}", parent.display()))?;
    }
    tokio::fs::write(&sdl_path, &sdl)
        .await
        .with_context(|| format!("writing tenant schema {}", sdl_path.display()))?;

    let issued = auth::issue().context("issuing tenant token")?;
    manifest.tenants.push(TenantSpec {
        name: name.clone(),
        replicas,
        cells: Vec::new(),
        token_sha256: issued.digest_hex,
        status: TenantStatus::Pending,
        admission: None,
        health: Default::default(),
    });
    manifest
        .save(&data_root)
        .await
        .with_context(|| format!("saving cluster manifest after creating tenant '{name}'"))?;

    println!("tenant {name} pending; will be placed on next start");
    println!(
        "tenant {name} token (save this, shown once): {}",
        issued.token_hex
    );
    Ok(())
}

/// Runs `tenant list`: prints the cluster manifest's tenants as pretty
/// JSON, without igniting any cell.
pub async fn list(data_root: PathBuf) -> Result<()> {
    let manifest = ClusterManifest::load(&data_root)
        .await
        .with_context(|| format!("loading cluster manifest from {}", data_root.display()))?;
    println!("{}", serde_json::to_string_pretty(&manifest.tenants)?);
    Ok(())
}

/// Runs `tenant rotate-token`: issues a fresh bearer token for an
/// existing tenant, replacing `token_sha256` and saving the manifest. The
/// old token stops working immediately (the manifest holds exactly one
/// digest per tenant); the new one is printed once.
pub async fn rotate_token(data_root: PathBuf, name: String) -> Result<()> {
    let mut manifest = ClusterManifest::load(&data_root)
        .await
        .with_context(|| format!("loading cluster manifest from {}", data_root.display()))?;

    let tenant = manifest
        .tenants
        .iter_mut()
        .find(|t| t.name == name)
        .ok_or_else(|| anyhow!("tenant '{name}' not found in cluster manifest"))?;

    let issued = auth::issue().context("issuing tenant token")?;
    tenant.token_sha256 = issued.digest_hex;

    manifest
        .save(&data_root)
        .await
        .with_context(|| format!("saving cluster manifest after rotating tenant '{name}' token"))?;

    println!(
        "tenant {name} token rotated (save this, shown once): {}",
        issued.token_hex
    );
    Ok(())
}
