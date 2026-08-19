//! RSS attribution measurement (Phase 1, following up on D7's measured
//! surprise): Phase 0 measured a combined per-cell ignition RSS delta
//! (~279 MiB / ~243 MiB, lark + libp2p) without separating the storage
//! backend's cost from the transport's. This test isolates them by
//! building three minimal node variants directly via
//! `embedded::build_with_store` (bypassing `burner_cell::cell`, which
//! always uses a fixed-port libp2p transport, never `TransportConfig::None`
//! or port 0):
//!
//! (i)   Memory backend, no P2P transport.
//! (ii)  Lark backend, no P2P transport (isolates the storage backend).
//! (iii) Lark backend, libp2p on an OS-assigned port (Phase 0's combination).
//! (iv)  Lark backend, no P2P transport, storage options derived from a
//!       128 MiB `mem_budget_bytes` (D11: the same derivation
//!       `burner_cell::cell::open_store` wires in, applied directly here
//!       via `LarkStoreOptions` since this file bypasses `burner_cell::cell`
//!       entirely -- see this module's doc comment).
//!
//! This is a measurement, not a gate: no threshold assertions, only that
//! all four deltas were actually computed (RSS is environment-noisy; the
//! derivation's floors/ceilings/both-backends behavior is enforced by
//! `burner_cell::cell`'s own unit tests, not by a threshold here). The
//! allocator does not necessarily return freed memory to the OS on
//! shutdown, so each delta is an upper bound on that step's true
//! incremental cost rather than an exact figure; all cases are still
//! sequenced with an explicit `node.shutdown()` between them so each delta
//! is attributed to one isolated build step in time, even though the
//! absolute RSS baseline may drift upward across cases.

use std::sync::Arc;

use anyhow::{Context, Result};
use burner_cell::cell::{lark_block_cache_bytes, lark_write_buffer_bytes};

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

    // (i) Memory backend, no P2P transport at all.
    {
        let before = read_rss_kb().expect("read RSS before memory/no-p2p build");
        let store = Arc::new(embedded::EmbeddedStore::Memory(storage::MemoryStore::new()));
        let config = embedded::EmbeddedNodeConfig {
            persistence: embedded::Persistence::Memory,
            transport: embedded::TransportConfig::None,
            ..Default::default()
        };
        let node = embedded::build_with_store(store, config)
            .await
            .expect("build memory/no-p2p cell");
        let after = read_rss_kb().expect("read RSS after memory/no-p2p build");
        deltas.push(("memory_no_p2p", after as i64 - before as i64));

        // The idempotent unified teardown (never the manual p2p+database
        // pair): embedded::EmbeddedNode::shutdown() at
        // defradb.rs crates/embedded/src/node.rs:200.
        node.shutdown().await;
        drop(node);
    }
    println!("ATTR {}_rss_delta_kb={}", deltas[0].0, deltas[0].1);

    // (ii) Lark backend, no P2P transport: isolates the storage backend's
    // own cost from the transport's.
    {
        let dir = tempfile::tempdir().expect("tempdir for lark/no-p2p");
        let before = read_rss_kb().expect("read RSS before lark/no-p2p build");
        let lark_store = storage::LarkStore::open(dir.path()).expect("open lark store");
        let store = Arc::new(embedded::EmbeddedStore::Lark(lark_store));
        let config = embedded::EmbeddedNodeConfig {
            persistence: embedded::Persistence::Persistent,
            transport: embedded::TransportConfig::None,
            ..Default::default()
        };
        let node = embedded::build_with_store(store, config)
            .await
            .expect("build lark/no-p2p cell");
        let after = read_rss_kb().expect("read RSS after lark/no-p2p build");
        deltas.push(("lark_no_p2p", after as i64 - before as i64));

        node.shutdown().await;
        drop(node);
    }
    println!("ATTR {}_rss_delta_kb={}", deltas[1].0, deltas[1].1);

    // (iii) Lark backend with libp2p on an OS-assigned port: the Phase 0
    // spike's combination, now measured on its own.
    {
        let dir = tempfile::tempdir().expect("tempdir for lark/libp2p");
        let before = read_rss_kb().expect("read RSS before lark/libp2p build");
        let lark_store = storage::LarkStore::open(dir.path()).expect("open lark store");
        let store = Arc::new(embedded::EmbeddedStore::Lark(lark_store));
        let config = embedded::EmbeddedNodeConfig {
            persistence: embedded::Persistence::Persistent,
            transport: embedded::TransportConfig::Libp2p(embedded::Libp2pConfig {
                listen_addr: "/ip4/127.0.0.1/tcp/0".to_string(),
            }),
            ..Default::default()
        };
        let node = embedded::build_with_store(store, config)
            .await
            .expect("build lark/libp2p cell");
        let after = read_rss_kb().expect("read RSS after lark/libp2p build");
        deltas.push(("lark_libp2p", after as i64 - before as i64));

        node.shutdown().await;
        drop(node);
    }
    println!("ATTR {}_rss_delta_kb={}", deltas[2].0, deltas[2].1);

    // (iv) Lark backend, no P2P transport, cache/buffer sizes derived from
    // a 128 MiB mem_budget_bytes (D11): a smaller, explicitly-budgeted
    // cache/buffer pair than (ii)'s implicit LarkStoreOptions::default().
    {
        let dir = tempfile::tempdir().expect("tempdir for lark/budgeted-128mb");
        let before = read_rss_kb().expect("read RSS before lark/budgeted-128mb build");
        let options = storage::LarkStoreOptions::new()
            .with_block_cache_size(lark_block_cache_bytes(BUDGET_128_MIB) as usize)
            .with_write_buffer_size(lark_write_buffer_bytes(BUDGET_128_MIB) as usize);
        let lark_store = storage::LarkStore::open_with_options(dir.path(), options)
            .expect("open budgeted lark store");
        let store = Arc::new(embedded::EmbeddedStore::Lark(lark_store));
        let config = embedded::EmbeddedNodeConfig {
            persistence: embedded::Persistence::Persistent,
            transport: embedded::TransportConfig::None,
            ..Default::default()
        };
        let node = embedded::build_with_store(store, config)
            .await
            .expect("build lark/budgeted-128mb cell");
        let after = read_rss_kb().expect("read RSS after lark/budgeted-128mb build");
        deltas.push(("lark_budgeted_128mb", after as i64 - before as i64));

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
