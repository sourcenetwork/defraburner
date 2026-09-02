//! RSS attribution measurement (Phase 1, following up on D7's measured
//! surprise): Phase 0 measured a combined per-cell ignition RSS delta
//! (~279 MiB / ~243 MiB, on-disk store + libp2p) without separating the storage
//! backend's cost from the transport's. This test isolates them by
//! building four minimal node variants directly via
//! `embedded::build_with_store` (bypassing `burner_cell::cell`, which
//! always uses a fixed-port libp2p transport, never `TransportConfig::None`
//! or port 0):
//!
//! (i)   In-memory regolith, no P2P transport.
//! (ii)  On-disk regolith, no P2P transport (isolates the storage backend).
//! (iii) On-disk regolith, libp2p on an OS-assigned port (Phase 0's
//!       combination).
//! (iv)  On-disk regolith, no P2P transport, storage options derived from a
//!       128 MiB `mem_budget_bytes` (D11: the same derivation
//!       `burner_cell::cell::open_store` wires in, applied directly here
//!       via `RegolithStoreOptions` since this file bypasses
//!       `burner_cell::cell` entirely -- see this module's doc comment).
//!
//! Upstream folded every backend into regolith (D36), so (i) and (ii) now
//! differ by persistence mode rather than by engine: (i) is
//! `RegolithStore::in_memory`, (ii) is the same engine opened on a path.
//! That is still the attribution this test exists to make, since what it
//! separates is storage cost from transport cost, not one vendor's engine
//! from another's.
//!
//! This is a measurement, not a gate: no threshold assertions, only that
//! all four deltas were actually computed (RSS is environment-noisy; the
//! derivation's floors/ceilings behavior is enforced by
//! `burner_cell::cell`'s own unit tests, not by a threshold here). The
//! allocator does not necessarily return freed memory to the OS on
//! shutdown, so each delta is an upper bound on that step's true
//! incremental cost rather than an exact figure; all cases are still
//! sequenced with an explicit `node.shutdown()` between them so each delta
//! is attributed to one isolated build step in time, even though the
//! absolute RSS baseline may drift upward across cases.

use std::sync::Arc;

use anyhow::{Context, Result};
use burner_cell::cell::{regolith_block_cache_bytes, regolith_write_buffer_bytes};

/// The `mem_budget_bytes` case (iv) measures against: well below the
/// default 512 MiB budget, so the derivation's halving/eighth-ing (not its
/// floors) is what is being exercised here.
const BUDGET_128_MIB: u64 = 128 * 1024 * 1024;

/// Parses `VmRSS` (kB) out of `/proc/self/status`. Previously lived on the
/// now-removed Phase 0 spike (`defraburner::spike::read_rss_kb`); this is
/// its only caller, so it moved here rather than into a shared module.
fn read_rss_kb() -> Result<u64> {
    let status =
        std::fs::read_to_string("/proc/self/status").context("reading /proc/self/status")?;
    status
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|kb| kb.parse::<u64>().ok())
        .context("VmRSS not present in /proc/self/status")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn attributes_rss_delta_by_backend_and_transport() {
    let mut deltas: Vec<(&str, i64)> = Vec::new();

    // (i) In-memory regolith, no P2P transport at all.
    {
        let before = read_rss_kb().expect("read RSS before regolith-memory/no-p2p build");
        let store = Arc::new(embedded::EmbeddedStore::Regolith(
            storage::RegolithStore::in_memory().expect("open in-memory regolith store"),
        ));
        let config = embedded::EmbeddedNodeConfig {
            persistence: embedded::Persistence::Memory,
            transport: embedded::TransportConfig::None,
            ..Default::default()
        };
        let node = embedded::build_with_store(store, config)
            .await
            .expect("build regolith-memory/no-p2p cell");
        let after = read_rss_kb().expect("read RSS after regolith-memory/no-p2p build");
        deltas.push(("regolith_memory_no_p2p", after as i64 - before as i64));

        // The idempotent unified teardown (never the manual p2p+database
        // pair): embedded::EmbeddedNode::shutdown() at
        // defradb.rs crates/embedded/src/node.rs:200.
        node.shutdown().await;
        drop(node);
    }
    println!("ATTR {}_rss_delta_kb={}", deltas[0].0, deltas[0].1);

    // (ii) On-disk regolith, no P2P transport: isolates the storage backend's
    // own cost from the transport's.
    {
        let dir = tempfile::tempdir().expect("tempdir for regolith/no-p2p");
        let before = read_rss_kb().expect("read RSS before regolith/no-p2p build");
        let disk_store = storage::RegolithStore::open(dir.path()).expect("open regolith store");
        let store = Arc::new(embedded::EmbeddedStore::Regolith(disk_store));
        let config = embedded::EmbeddedNodeConfig {
            persistence: embedded::Persistence::Persistent,
            transport: embedded::TransportConfig::None,
            ..Default::default()
        };
        let node = embedded::build_with_store(store, config)
            .await
            .expect("build regolith/no-p2p cell");
        let after = read_rss_kb().expect("read RSS after regolith/no-p2p build");
        deltas.push(("regolith_no_p2p", after as i64 - before as i64));

        node.shutdown().await;
        drop(node);
    }
    println!("ATTR {}_rss_delta_kb={}", deltas[1].0, deltas[1].1);

    // (iii) On-disk regolith with libp2p on an OS-assigned port: the Phase 0
    // spike's combination, now measured on its own.
    {
        let dir = tempfile::tempdir().expect("tempdir for regolith/libp2p");
        let before = read_rss_kb().expect("read RSS before regolith/libp2p build");
        let disk_store = storage::RegolithStore::open(dir.path()).expect("open regolith store");
        let store = Arc::new(embedded::EmbeddedStore::Regolith(disk_store));
        let config = embedded::EmbeddedNodeConfig {
            persistence: embedded::Persistence::Persistent,
            transport: embedded::TransportConfig::Libp2p(embedded::Libp2pConfig {
                listen_addr: "/ip4/127.0.0.1/tcp/0".to_string(),
            }),
            ..Default::default()
        };
        let node = embedded::build_with_store(store, config)
            .await
            .expect("build regolith/libp2p cell");
        let after = read_rss_kb().expect("read RSS after regolith/libp2p build");
        deltas.push(("regolith_libp2p", after as i64 - before as i64));

        node.shutdown().await;
        drop(node);
    }
    println!("ATTR {}_rss_delta_kb={}", deltas[2].0, deltas[2].1);

    // (iv) On-disk regolith, no P2P transport, cache/buffer sizes derived from
    // a 128 MiB mem_budget_bytes (D11): a smaller, explicitly-budgeted
    // cache/buffer pair than (ii)'s implicit RegolithStoreOptions::default().
    {
        let dir = tempfile::tempdir().expect("tempdir for regolith/budgeted-128mb");
        let before = read_rss_kb().expect("read RSS before regolith/budgeted-128mb build");
        let mut options = storage::RegolithStoreOptions::new();
        options.engine.block_cache_size = regolith_block_cache_bytes(BUDGET_128_MIB) as usize;
        options.engine.write_buffer_size = regolith_write_buffer_bytes(BUDGET_128_MIB) as usize;
        let disk_store = storage::RegolithStore::open_with_options(dir.path(), options)
            .expect("open budgeted regolith store");
        let store = Arc::new(embedded::EmbeddedStore::Regolith(disk_store));
        let config = embedded::EmbeddedNodeConfig {
            persistence: embedded::Persistence::Persistent,
            transport: embedded::TransportConfig::None,
            ..Default::default()
        };
        let node = embedded::build_with_store(store, config)
            .await
            .expect("build regolith/budgeted-128mb cell");
        let after = read_rss_kb().expect("read RSS after regolith/budgeted-128mb build");
        deltas.push(("regolith_budgeted_128mb", after as i64 - before as i64));

        node.shutdown().await;
        drop(node);
    }
    println!("ATTR {}_rss_delta_kb={}", deltas[3].0, deltas[3].1);

    assert_eq!(
        deltas.len(),
        4,
        "all four RSS deltas should have been computed"
    );
}
