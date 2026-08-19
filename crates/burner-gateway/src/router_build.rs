//! Builds one axum [`Router`] per cell, mirroring defra-node's own
//! reference wiring of `defra_http::Server`
//! (`defradb.rs/crates/defra-node/src/lib.rs`, the embedded-HTTP build
//! path around its `NodeBuilder::build`) onto what
//! `embedded::EmbeddedNode<embedded::EmbeddedStore>` publicly exposes.
//!
//! This achieves **full parity with upstream's own embedded-HTTP
//! surface** (GraphQL execution, health-check, the whole `/p2p/*`
//! family, transactions, etc, all mounted under both `/api/v0` and
//! `/api/v1`), not a reduced fallback. Every builder call defra-node's
//! own reference wiring chains onto `Server::from_arc_with_config`
//! (`with_event_bus_arc`, an optional `with_node_identity_did`, an
//! optional `with_p2p_arc`) has a direct, zero-adaptation counterpart on
//! `EmbeddedNode`: `query_runner` is exactly the
//! `Arc<dyn query::QueryExecutor>` `from_arc_with_config` wants,
//! `event_bus` is exactly the `Arc<dyn events::Bus>` `with_event_bus_arc`
//! wants, `node_identity_did` is the same `Option<String>`, and
//! `p2p().ops()` is exactly the `Arc<dyn defra_http::P2POperations>`
//! `with_p2p_arc` wants (`embedded::ManagedP2PSystem::ops(&self) ->
//! &Arc<dyn defra_http::P2POperations>`).
//!
//! Route families that need components `embedded::build_with_store`
//! never constructs at all (D8): `rest`, `manage`, `acp`, `index`,
//! `encrypted_index`, `backup`, `block`, `browser_sync`, `schema`,
//! `lens`, `collection_mgmt`, `doc_acp`, `view`, `dump`, `txn_ops` --
//! are still *mounted* by `Server::router()` (it builds every route
//! family unconditionally), but their handlers 503 ("service
//! unavailable") at request time via `AppState::require_X()`
//! (`defradb.rs/crates/http/src/router/state.rs`), never a build-time
//! error and never an absent route. defraburner never calls those
//! routes: schema is applied directly via `node.add_schema()`
//! (`burner_mesh::reconcile`), never through this router.

use std::sync::Arc;

use anyhow::{Context, Result};
use axum::Router;

/// Builds the full upstream HTTP router for one cell's node.
pub fn build_cell_router(
    node: &Arc<embedded::EmbeddedNode<embedded::EmbeddedStore>>,
) -> Result<Router> {
    let config = defra_http::ServerConfig {
        query_limits: node.query_limits,
        ..Default::default()
    };
    let mut server = defra_http::Server::from_arc_with_config(node.query_runner.clone(), config)
        .with_event_bus_arc(node.event_bus.clone());
    if let Some(did) = node.node_identity_did.clone() {
        server = server.with_node_identity_did(did);
    }
    if let Some(p2p) = node.p2p() {
        server = server.with_p2p_arc(p2p.ops().clone());
    }
    server.router().context("building the per-cell HTTP router")
}
