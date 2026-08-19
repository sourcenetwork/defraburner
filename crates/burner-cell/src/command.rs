//! Admin control-surface command shapes (console round, D25): the message
//! types an axum handler sends down one channel to the single-writer
//! executor running on `defraburner`'s main (never-spawned) task, and
//! awaits a reply from.
//!
//! D12 still holds here exactly as it did for the watchdog and the
//! autoscaler: `Supervisor::provision` reaches `cell::ignite`, whose
//! returned future is not `Send` whenever libp2p is configured, so it can
//! never run inside a `tokio::spawn`ed axum handler task. Every admin
//! handler that mutates the cluster therefore enqueues a
//! [`SupervisorCommand`] and awaits its `reply` channel instead of
//! mutating anything itself; the executor that actually calls into
//! `burner-cell`/`burner-mesh`/`burner-policy` lives in
//! `defraburner::commands` (a higher layer that can depend on all three
//! without creating a cycle), driven from `start.rs`'s `select!` alongside
//! the watchdog and autoscaler loops.
//!
//! This module holds only the message shapes and their outcome/error
//! types, deliberately free of business logic: the executor composes
//! `Supervisor`, `burner_mesh::reconcile`, and `burner_policy` primitives
//! to actually carry each command out.

use serde::Serialize;
use tokio::sync::oneshot;

use crate::spec::AdmissionOverride;

/// Bounded capacity for the admin command channel: a burst of admin
/// requests queues up to this many in flight; nothing about it is
/// unbounded.
pub const COMMAND_CHANNEL_CAPACITY: usize = 32;

/// Outcome of provisioning one cell via
/// [`SupervisorCommand::ProvisionCells`].
#[derive(Debug, Clone, Serialize)]
pub struct ProvisionOutcome {
    /// The cell id actually provisioned. `None` only when the failure
    /// happened before any id was chosen (an honest "unknown", never a
    /// guessed id): the error text itself still names the attempted id in
    /// the far more common case (any failure once provisioning is
    /// underway), since it is threaded through `with_context` there.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peer_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Why [`SupervisorCommand::DrainCell`] could not remove a cell.
#[derive(Debug, Clone, thiserror::Error)]
pub enum DrainCellError {
    #[error("no running cell to drain")]
    NotFound,
    #[error("assigned to a tenant '{0}'; refusing to remove an in-use cell")]
    AssignedToTenant(String),
    #[error("{0}")]
    Failed(String),
}

/// Outcome of [`SupervisorCommand::DropTenant`].
#[derive(Debug, Clone, Serialize)]
pub struct DropTenantOutcome {
    pub name: String,
    /// Data remains on these cells unless `retire` was requested; empty
    /// when it was not.
    pub data_remains_on_cells: Vec<String>,
    /// Cell ids drained and erased (including their data directories).
    /// Empty unless `retire` was requested.
    pub retired_cells: Vec<String>,
}

/// Why a command targeting one existing tenant by name -- `DropTenant`,
/// `RotateTenantToken`, `SetTenantAdmission` -- could not complete. Shared
/// across all three: every tenant-targeting command needs to name "not
/// found" distinctly (404) from any other failure (500), and nothing else
/// separates their error shapes.
#[derive(Debug, Clone, thiserror::Error)]
pub enum TenantCommandError {
    #[error("tenant '{0}' not found")]
    NotFound(String),
    #[error("{0}")]
    Failed(String),
}

/// A live autoscaler-config change (console round, D23): every field
/// absent means "leave this knob as it is"; `paused` is the one exception
/// -- when present it always takes effect (there is no partial-pause).
/// Deserialized directly from a `PUT /admin/autoscaler` body, which is
/// expected to name only the fields it wants to change: `#[serde(default)]`
/// at the container level (not just on each field) is required here, not
/// a nicety -- serde does not default a missing `Option<T>` field to
/// `None` on its own; without this, `{"paused": true}` alone would fail
/// deserialization with "missing field `min_cells`".
#[derive(Debug, Clone, Copy, Default, serde::Deserialize)]
#[serde(default)]
pub struct AutoscalerPatch {
    pub min_cells: Option<usize>,
    pub max_cells: Option<usize>,
    pub cooldown_secs: Option<u64>,
    pub tick_interval_secs: Option<u64>,
    pub paused: Option<bool>,
}

/// One admin mutation, enqueued by a gateway handler and carried out by
/// `defraburner::commands`'s executor on the main task.
pub enum SupervisorCommand {
    /// Provisions `count` fresh cells, one at a time, reporting each
    /// attempt's outcome independently (a later failure never rolls back
    /// an earlier success).
    ProvisionCells {
        count: usize,
        reply: oneshot::Sender<Vec<ProvisionOutcome>>,
    },
    /// Drains and erases one free cell.
    DrainCell {
        id: String,
        reply: oneshot::Sender<Result<(), DrainCellError>>,
    },
    /// Drops a tenant: unsubscribes its collections, removes its
    /// placement and the tenant itself from the manifest (revoking its
    /// token). When `retire` is set, also drains and erases the tenant's
    /// cells and deletes their data directories.
    DropTenant {
        name: String,
        retire: bool,
        reply: oneshot::Sender<Result<DropTenantOutcome, TenantCommandError>>,
    },
    /// Issues a fresh bearer token for `name`, replacing its previous one.
    RotateTenantToken {
        name: String,
        reply: oneshot::Sender<Result<String, TenantCommandError>>,
    },
    /// Sets (or clears, when `admission` is `None`) a tenant's per-tenant
    /// GCRA admission override.
    SetTenantAdmission {
        name: String,
        admission: Option<AdmissionOverride>,
        reply: oneshot::Sender<Result<(), TenantCommandError>>,
    },
    /// Applies a live autoscaler config patch, persisting it in the
    /// manifest.
    SetAutoscaler {
        patch: AutoscalerPatch,
        reply: oneshot::Sender<Result<(), String>>,
    },
    /// Forces one autoscaler tick outside its normal cadence.
    ForceAutoscalerTick { reply: oneshot::Sender<()> },
    /// Dials a peer multiaddr from one specific cell.
    DialPeer {
        cell_id: String,
        addr: String,
        reply: oneshot::Sender<Result<(), String>>,
    },
}
