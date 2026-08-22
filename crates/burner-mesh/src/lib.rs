//! burner-mesh: tenant placement, group replication wiring, static
//! cross-host peer dialing, and tenant reconciliation over a cluster of
//! governed cells (`burner-cell`). See `docs/plans/defraburner.md`
//! (Phase 2, "The pieces") and `docs/decs/defraburner_DECS.md` (D13
//! topic-ready wiring, D14 disjoint placement and declarative
//! provisioning) for the decisions this crate implements.
//!
//! D12 (standing constraint, carried from Phase 1): nothing in this crate
//! reaches `burner_cell::cell::ignite` (tenants are placed onto cells that
//! already exist by the time any function here runs), so every async fn is
//! plain and safe to `.await` directly on the caller's task; none of it may
//! be wrapped in `tokio::spawn`.

pub mod grow;
pub mod placement;
pub mod reconcile;
pub mod static_peers;
pub mod topic_ready;
pub mod wiring;

/// Extracts the peer id from one `connected_peers()` entry.
///
/// Upstream does not return bare peer ids there: the libp2p adapter
/// resolves each connected peer to an address string
/// (`crates/p2p-adapter/src/libp2p.rs`, `connected_peers` ->
/// `handle.resolve_peer_addresses`), so entries look like
/// `/ip4/127.0.0.1/tcp/9172/p2p/12D3Koo...`. Upstream itself parses ids
/// back out the same way (`crates/p2p/src/host/handle.rs`, via
/// `rsplit("/p2p/")`). Comparing an entry to a bare peer id with `==`
/// therefore never matches, which silently degraded healthy tenants and
/// drew every replication link as missing until this was normalized.
///
/// A bare id (no `/p2p/` segment) is returned trimmed, so this keeps
/// working if upstream ever changes what it lists.
pub fn peer_id_of(entry: &str) -> &str {
    match entry.rsplit_once("/p2p/") {
        Some((_, id)) => id.trim_end_matches('/'),
        None => entry.trim(),
    }
}

pub use grow::add_collections;
pub use placement::place;
pub use reconcile::{TenantOutcome, TenantReady, reconcile, tenant_sdl_path};
pub use static_peers::{PeerDialOutcome, confirm_dialed_peers, dial_static_peers};
pub use topic_ready::wait_topic_peer;
pub use wiring::{ensure_group_connected, wire_group};
