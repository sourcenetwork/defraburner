//! burner-cell: cell specs, manifold-shaped grants, lifecycle, identity
//! persistence, the cluster manifest, and crash recovery for one governed
//! DefraDB cell.
//!
//! Every cell is a real `embedded::EmbeddedNode`, built through upstream's
//! public `EmbeddedStore` enum (D8) with an explicit per-cell Ed25519
//! identity (D3: never the process-global signing registry) and a
//! fixed-port libp2p transport, so its data directory, its identity, and
//! its peer id all survive a restart, including a SIGKILL. See
//! `docs/plans/defraburner.md` (Phase 1) and `docs/decs/defraburner_DECS.md`
//! (D1-D8) for the decisions this crate implements.

pub mod cell;
pub mod command;
pub mod identity;
pub mod manifest;
pub mod spec;
pub mod supervisor;
pub mod watchdog;

pub use cell::RunningCell;
pub use command::{
    AutoscalerPatch, COMMAND_CHANNEL_CAPACITY, DrainCellError, DropTenantOutcome, ProvisionOutcome,
    SupervisorCommand, TenantCommandError,
};
pub use manifest::{AutoscalerSpec, ClusterManifest};
pub use spec::{
    AdmissionOverride, BackendKind, CellSpec, DEFAULT_MEM_BUDGET_BYTES, TENANT_NAME_MAX_LEN,
    TenantHealth, TenantSpec, TenantStatus, is_valid_tenant_name,
};
pub use supervisor::{CellStatus, Supervisor};
pub use watchdog::{CellHealth, DEFAULT_PROBE_INTERVAL, ProbeOutcome, Watchdog};
