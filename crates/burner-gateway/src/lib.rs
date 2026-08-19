//! burner-gateway: one axum listener fronting every cell, routing by
//! tenant (bearer token -> tenant -> cell group -> sticky pick), with
//! per-tenant GCRA admission and live tenant provisioning. See
//! `docs/plans/defraburner.md` (Phase 3, "The pieces") and
//! `docs/consistency.md` for the consistency semantics this gateway
//! documents rather than overclaims.
//!
//! ## HTTP surface
//!
//! Each cell's per-request surface is the **full upstream router**
//! (`router_build::build_cell_router`): the same wiring defra-node uses
//! internally for its own embedded HTTP server
//! (`defradb.rs/crates/defra-node/src/lib.rs`, its `NodeBuilder::build`
//! path), reproduced here from `embedded::EmbeddedNode`'s public fields.
//! This is full parity, not a reduced fallback: see that module's doc
//! comment for the exact mapping and why every field defra-node's own
//! reference wiring uses has a direct counterpart on `EmbeddedNode`.
//!
//! D12 (standing constraint, carried from Phase 1/2): nothing in this
//! crate reaches `burner_cell::cell::ignite` (the gateway only routes to
//! and reconciles tenants onto cells that already exist), so every async
//! fn here, including admin handlers that run on axum's per-connection
//! spawned tasks, is `Send`-safe and never needs to avoid `tokio::spawn`.

mod admin_autoscaler;
mod admin_cells;
mod admin_tenants;
pub mod admission;
pub mod auth;
pub mod gateway;
pub mod router_build;
pub mod routing;
pub mod sse;
