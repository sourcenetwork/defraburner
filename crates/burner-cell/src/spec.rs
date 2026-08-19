//! `CellSpec`: the declarative shape of one governed DefraDB cell.

use std::net::IpAddr;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Default per-cell memory budget: 512 MiB.
pub const DEFAULT_MEM_BUDGET_BYTES: u64 = 512 * 1024 * 1024;

/// Declarative spec for one governed cell, persisted as part of the cluster
/// manifest (`manifest::ClusterManifest`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CellSpec {
    pub id: String,
    pub group: String,
    pub backend: BackendKind,
    pub p2p_port: u16,
    pub bind_addr: IpAddr,
    /// Memory budget for this cell, in bytes. Genuinely drives storage cache
    /// sizing as of Phase 6 (D11): `cell::open_store` derives the backend's
    /// own cache/buffer knobs from this value (`cell::lark_block_cache_bytes`
    /// / `cell::lark_write_buffer_bytes` for Lark, `cell::redb_cache_bytes`
    /// for Redb; `Memory` has no cache to size). It is still not a hard
    /// admission cap: a cell whose live working set exceeds its backend
    /// cache is not throttled or rejected, since request-level admission and
    /// full `MemoryLedger` accounting are a separate, later mechanism this
    /// field does not yet wire into.
    ///
    /// vertexia: storage cache sizing only; a hard per-cell admission cap
    /// off of this budget is a future `MemoryLedger` accounting phase, not
    /// yet built.
    pub mem_budget_bytes: u64,
    /// Path to this cell's persisted Ed25519 signing seed
    /// (`identity::key_path`).
    pub signing_key_file: PathBuf,
}

/// Storage backend selection for a cell, threaded through to upstream's
/// public `embedded::EmbeddedStore` enum (D8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BackendKind {
    #[default]
    Lark,
    Redb,
    /// Dev-only. Not safe to persist across restarts inside a cluster
    /// manifest: rejected at manifest load/save time
    /// (`manifest::ClusterManifest::validate`).
    Memory,
}

/// Highest allowed length for a `TenantSpec::name` (D14: `[a-z0-9-]{1,63}`).
pub const TENANT_NAME_MAX_LEN: usize = 63;

/// Declarative spec for one tenant: the shard unit (D14, plan Phase 2). A
/// tenant is placed on a group of `replicas` cells that replicate the
/// tenant's collections among themselves; v1 placement is disjoint, so each
/// cell id appears in at most one tenant's `cells`
/// (`manifest::ClusterManifest::validate`).
///
/// Provisioning is declarative: `tenant create` appends a `Pending` spec
/// with empty `cells`/`token_sha256`; `start`'s reconcile (`burner-mesh`)
/// places it, flips `status` to `Placed`, and fills `cells`. `token_sha256`
/// is filled in when Phase 3's gateway issues the tenant's bearer token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TenantSpec {
    /// Validated against [`is_valid_tenant_name`]: `[a-z0-9-]{1,63}`.
    pub name: String,
    /// Replication factor. Must be at least 1.
    pub replicas: u8,
    /// Assigned cell ids, in placement order. Empty until placed.
    #[serde(default)]
    pub cells: Vec<String>,
    /// Hex-encoded sha256 of the tenant's bearer token. Empty until a token
    /// is issued (`tenant create` / `tenant rotate-token`, Phase 3).
    #[serde(default)]
    pub token_sha256: String,
    pub status: TenantStatus,
    /// Per-tenant admission override (console round, D23): when set, the
    /// gateway's `Admission` consults this instead of its process-wide
    /// default rate/burst for this tenant. Absent means "use the default".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admission: Option<AdmissionOverride>,
    /// This tenant's own reconcile health (bug-fix round, D25 addendum):
    /// `burner_mesh::reconcile` sets this per tenant, isolated from every
    /// other tenant's own reconcile outcome in the same pass (one
    /// tenant's wiring failure must never abort another tenant's, or
    /// show up as an unrelated admin request's 500). Surfaced verbatim
    /// in `/admin/api/overview` and the dashboard's Tenants table and
    /// mesh cluster caption, rather than silently swallowed.
    #[serde(default)]
    pub health: TenantHealth,
}

/// A tenant's own reconcile health, independent of every other tenant's.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "lowercase")]
pub enum TenantHealth {
    #[default]
    Ok,
    /// Placement or wiring for *this* tenant failed; every other
    /// tenant's own reconcile still ran to completion regardless.
    Degraded { reason: String, since_ms: u64 },
}

/// A per-tenant GCRA admission override (D23): `PUT
/// /admin/tenants/{name}/admission` persists this in the manifest;
/// `burner_gateway::admission::Admission` consults it when it first builds
/// (or rebuilds) that tenant's bucket.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdmissionOverride {
    pub rate_per_sec: u64,
    pub burst: u64,
}

/// Lifecycle state of a [`TenantSpec`] (D14: declarative provisioning).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TenantStatus {
    /// Recorded in the manifest but not yet placed on any cell.
    #[default]
    Pending,
    /// Placed: `cells` names its replica group.
    Placed,
}

/// True if `name` matches the tenant-name grammar `[a-z0-9-]{1,63}`: ASCII
/// lowercase letters, digits, and hyphens only, 1 to
/// [`TENANT_NAME_MAX_LEN`] bytes. No dependency on the `regex` crate: a
/// byte-class scan is the whole job.
pub fn is_valid_tenant_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= TENANT_NAME_MAX_LEN
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_kind_serializes_lowercase() {
        assert_eq!(
            serde_json::to_string(&BackendKind::Lark).unwrap(),
            "\"lark\""
        );
        assert_eq!(
            serde_json::to_string(&BackendKind::Redb).unwrap(),
            "\"redb\""
        );
        assert_eq!(
            serde_json::to_string(&BackendKind::Memory).unwrap(),
            "\"memory\""
        );
    }

    #[test]
    fn backend_kind_default_is_lark() {
        assert_eq!(BackendKind::default(), BackendKind::Lark);
    }

    #[test]
    fn tenant_status_serializes_lowercase() {
        assert_eq!(
            serde_json::to_string(&TenantStatus::Pending).unwrap(),
            "\"pending\""
        );
        assert_eq!(
            serde_json::to_string(&TenantStatus::Placed).unwrap(),
            "\"placed\""
        );
    }

    #[test]
    fn tenant_status_default_is_pending() {
        assert_eq!(TenantStatus::default(), TenantStatus::Pending);
    }

    #[test]
    fn tenant_spec_round_trips_through_json() {
        let spec = TenantSpec {
            name: "acme-co".to_string(),
            replicas: 2,
            cells: vec!["cell-0".to_string(), "cell-1".to_string()],
            token_sha256: "ab".repeat(32),
            status: TenantStatus::Placed,
            admission: Some(AdmissionOverride {
                rate_per_sec: 50,
                burst: 10,
            }),
            health: TenantHealth::Degraded {
                reason: "cell 'cell-1' is not running in this process".to_string(),
                since_ms: 1_755_600_000_000,
            },
        };
        let json = serde_json::to_string(&spec).unwrap();
        let parsed: TenantSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(spec, parsed);
    }

    #[test]
    fn tenant_spec_defaults_cells_and_token_when_absent() {
        // A freshly `tenant create`d spec is written without `cells`/
        // `token_sha256`/`admission`/`health` populated; the
        // `#[serde(default)]` fields must still deserialize (round-tripping
        // an older, sparser manifest).
        let json = r#"{"name":"acme-co","replicas":2,"status":"pending"}"#;
        let parsed: TenantSpec = serde_json::from_str(json).unwrap();
        assert!(parsed.cells.is_empty());
        assert!(parsed.token_sha256.is_empty());
        assert_eq!(parsed.status, TenantStatus::Pending);
        assert_eq!(parsed.admission, None);
        assert_eq!(
            parsed.health,
            TenantHealth::Ok,
            "an older manifest predating health tracking must default to Ok, not a fabricated Degraded"
        );
    }

    #[test]
    fn tenant_health_round_trips_ok_and_degraded_through_json() {
        let ok = TenantHealth::Ok;
        let json = serde_json::to_string(&ok).unwrap();
        assert_eq!(json, r#"{"state":"ok"}"#);
        assert_eq!(serde_json::from_str::<TenantHealth>(&json).unwrap(), ok);

        let degraded = TenantHealth::Degraded {
            reason: "timed out waiting for peer to join topic".to_string(),
            since_ms: 42,
        };
        let json = serde_json::to_string(&degraded).unwrap();
        assert_eq!(
            serde_json::from_str::<TenantHealth>(&json).unwrap(),
            degraded
        );
    }

    #[test]
    fn tenant_health_default_is_ok() {
        assert_eq!(TenantHealth::default(), TenantHealth::Ok);
    }

    #[test]
    fn valid_tenant_names_are_accepted() {
        assert!(is_valid_tenant_name("acme-co"));
        assert!(is_valid_tenant_name("a"));
        assert!(is_valid_tenant_name("tenant-0123"));
        assert!(is_valid_tenant_name(&"a".repeat(TENANT_NAME_MAX_LEN)));
    }

    #[test]
    fn invalid_tenant_names_are_rejected() {
        assert!(!is_valid_tenant_name(""), "empty name");
        assert!(
            !is_valid_tenant_name(&"a".repeat(TENANT_NAME_MAX_LEN + 1)),
            "over length"
        );
        assert!(!is_valid_tenant_name("Acme-Co"), "uppercase");
        assert!(!is_valid_tenant_name("acme_co"), "underscore");
        assert!(!is_valid_tenant_name("acme co"), "space");
        assert!(!is_valid_tenant_name("acme.co"), "dot");
    }

    #[test]
    fn cell_spec_round_trips_through_json() {
        let spec = CellSpec {
            id: "cell-0".to_string(),
            group: "default".to_string(),
            backend: BackendKind::Lark,
            p2p_port: 9171,
            bind_addr: "127.0.0.1".parse().unwrap(),
            mem_budget_bytes: DEFAULT_MEM_BUDGET_BYTES,
            signing_key_file: PathBuf::from("/data/keys/cell-0.ed25519"),
        };
        let json = serde_json::to_string(&spec).unwrap();
        let parsed: CellSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(spec, parsed);
    }
}
