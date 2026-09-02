//! Builds one `embedded::EmbeddedNode` per `CellSpec` through upstream's
//! public `EmbeddedStore` enum (D8) and a fixed-port libp2p transport:
//! stable addresses across restart are the point, so the listen port always
//! comes from the spec, never port 0.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use tokio::time::Instant;

use crate::identity;
use crate::spec::{BackendKind, CellSpec};

/// Deadline for a freshly-built cell to bind its libp2p listen address.
const LISTEN_ADDR_DEADLINE: Duration = Duration::from_secs(10);
const LISTEN_ADDR_POLL_STEP: Duration = Duration::from_millis(100);

/// One live, ignited cell.
pub struct RunningCell {
    pub spec: CellSpec,
    pub node: Arc<embedded::EmbeddedNode<embedded::EmbeddedStore>>,
    pub peer_id: String,
    pub listen_addrs: Vec<String>,
    /// This cell's wasm DefraDB (D40). A cell and its fiber share an id
    /// and a lifetime: the fiber is spawned here during ignition and shut
    /// down when the cell is dropped, so there is no way to hold a cell
    /// whose fiber is stale, or a fiber whose cell is gone.
    ///
    /// `None` only when the process has no fiber image, which happens when
    /// `packages/defradb` has not been built. That is a legitimate state
    /// (the `.afb` is a build output), so it degrades honestly rather than
    /// refusing to ignite the cell at all.
    pub fiber: Option<Arc<tokio::sync::Mutex<burner_fiber::Fiber>>>,
}

/// The on-disk directory a cell's wasm database lives in.
///
/// Nested inside the cell's own directory rather than beside it: a cell
/// and its fiber are one unit, so removing a cell's directory removes its
/// database with it and cannot leave an orphan.
pub fn cell_fiber_dir(data_root: &Path, cell_id: &str) -> PathBuf {
    cell_data_dir(data_root, cell_id).join("fiber")
}

impl RunningCell {
    /// The dialable multiaddr other peers connect to:
    /// `<listen-addr>/p2p/<peer-id>`.
    ///
    /// Empirical fact from the Phase 0 spike: `listen_addresses()` returns
    /// bare transport multiaddrs (libp2p's `Swarm::listeners()`), while
    /// `connect_peer` requires the `/p2p/<peer-id>` suffix to know which
    /// identity to expect.
    pub fn dialable_addr(&self) -> Option<String> {
        assemble_dialable_addr(&self.listen_addrs, &self.peer_id)
    }
}

/// The collections currently registered on `node` (console round, D23:
/// `GET /admin/cells/{id}/inspect`). A free function over
/// `&EmbeddedNode<EmbeddedStore>` rather than a `RunningCell` method, so a
/// caller holding only a cloned `Arc<EmbeddedNode<EmbeddedStore>>` (e.g.
/// `Supervisor::node_handle`) can call it without needing the whole
/// `RunningCell` borrow (and, transitively, the supervisor lock) alive
/// across the call: the same shape the watchdog's own probe already
/// uses. A synchronous, process-wide cached lookup on upstream's own side
/// (`db::DB::list_collections`), so no `spawn_blocking` is needed.
pub fn cell_collections(
    node: &embedded::EmbeddedNode<embedded::EmbeddedStore>,
) -> Result<Vec<String>> {
    node.database
        .list_collections()
        .map_err(|error| anyhow!("listing collections: {error}"))
}

/// Backend-neutral transaction diagnostics for `node`'s store, when the
/// backend tracks them (console round, D23: `GET
/// /admin/cells/{id}/inspect`'s `transaction_stats`).
///
/// Honest, not fabricated: `embedded::EmbeddedStore`'s own blanket
/// `storage::Store` impl does not forward `transaction_stats_handle` (it
/// inherits the trait's `None` default), so this matches on the concrete
/// backend variant instead and calls the real per-backend implementation
/// directly (verified: `RegolithStore` implements it).
///
/// After upstream's backend fold (D36) an in-memory cell is a regolith
/// store too, so it now reports real transaction diagnostics where the
/// retired `Memory` backend had none. `Encrypted` wraps another store and
/// does not forward the handle, so it still degrades to `None` rather than
/// a fabricated snapshot; `_` also covers any future variant upstream's
/// `non_exhaustive` enum adds.
pub fn cell_transaction_stats(
    node: &embedded::EmbeddedNode<embedded::EmbeddedStore>,
) -> Option<serde_json::Value> {
    use storage::Store;

    let snapshot = match node.database.store().as_ref() {
        embedded::EmbeddedStore::Regolith(store) => store.transaction_stats_handle(),
        _ => None,
    }?
    .snapshot();
    serde_json::to_value(snapshot).ok()
}

/// Pure formatting logic behind `RunningCell::dialable_addr`, split out so
/// it is unit-testable without standing up a real embedded node.
fn assemble_dialable_addr(listen_addrs: &[String], peer_id: &str) -> Option<String> {
    listen_addrs
        .first()
        .map(|addr| format!("{addr}/p2p/{peer_id}"))
}

/// The on-disk directory this cell's store lives under.
pub fn cell_data_dir(data_root: &Path, cell_id: &str) -> PathBuf {
    data_root.join("cells").join(cell_id)
}

/// Builds and ignites one cell from `spec`: opens its store, builds the
/// embedded node with a fixed-port libp2p transport and its persisted
/// signing identity, then waits for the libp2p listen address and resolves
/// the peer id.
pub async fn ignite(
    data_root: &Path,
    spec: CellSpec,
    fiber_image: Option<&burner_fiber::FiberImage>,
) -> Result<RunningCell> {
    // Phase 1 only builds IPv4 multiaddrs (the plan's spec fixes the
    // "/ip4/<bind_addr>/tcp/<port>" template); fail loud up front on an
    // IPv6 bind_addr rather than doing real I/O first and then only
    // surfacing a confusing libp2p parse error deep inside the swarm.
    if !spec.bind_addr.is_ipv4() {
        anyhow::bail!(
            "cell '{}' has a non-IPv4 bind_addr ({}); Phase 1 only supports IPv4 listen addresses",
            spec.id,
            spec.bind_addr
        );
    }

    let dir = cell_data_dir(data_root, &spec.id);
    tokio::fs::create_dir_all(&dir)
        .await
        .with_context(|| format!("creating cell dir {}", dir.display()))?;

    let store = open_store(&spec, &dir)
        .await
        .with_context(|| format!("opening store for cell '{}'", spec.id))?;

    // D3: cells never use the process-global signing identity registry
    // (SigningConfig::RegisteredIdentity); explicit per-cell key material
    // sidesteps it entirely.
    let key_bytes = identity::load_signing_key_bytes(&spec.signing_key_file)
        .await
        .with_context(|| format!("loading signing key for cell '{}'", spec.id))?;
    let signing = embedded::SigningConfig::Enabled {
        key: Some(embedded::SigningKey::Ed25519(key_bytes)),
    };

    let persistence = match spec.backend {
        BackendKind::Memory => embedded::Persistence::Memory,
        BackendKind::Regolith => embedded::Persistence::Persistent,
    };

    let listen_addr = format!("/ip4/{}/tcp/{}", spec.bind_addr, spec.p2p_port);
    let config = embedded::EmbeddedNodeConfig {
        persistence,
        transport: embedded::TransportConfig::Libp2p(embedded::Libp2pConfig { listen_addr }),
        signing,
        ..Default::default()
    };

    let node = embedded::build_with_store(Arc::new(store), config)
        .await
        .with_context(|| format!("building cell '{}' (p2p port {})", spec.id, spec.p2p_port))?;
    let node = Arc::new(node);

    let p2p = node
        .p2p()
        .ok_or_else(|| anyhow!("cell '{}' has no p2p system after a libp2p build", spec.id))?
        .clone();

    // Named explicitly with the port, not just the cell id: on recovery
    // this is the one path that must never silently move a persisted
    // cell's port (its peers already know that address), so a foreign
    // process squatting on it has to fail loud and specific, not a bare
    // "did not bind" a reader has to go dig for the port to diagnose.
    let listen_addrs = wait_for_listen_addrs(&p2p).await.with_context(|| {
        format!(
            "cell '{}' did not bind its libp2p listen address on port {} (a foreign process may already be using it)",
            spec.id, spec.p2p_port
        )
    })?;
    let peer_id = p2p
        .ops()
        .local_peer_id()
        .await
        .map_err(|error| anyhow!(error))
        .with_context(|| format!("resolving peer id for cell '{}'", spec.id))?;

    // The cell's wasm database, spawned last: everything above must be
    // live before this cell claims to exist at all, and a fiber that fails
    // to open its store is an ignition failure for the whole cell, not a
    // quietly half-ignited one.
    let fiber = match fiber_image {
        None => None,
        Some(image) => {
            let dir = cell_fiber_dir(data_root, &spec.id);
            let image = image.clone();
            let cell_id = spec.id.clone();
            let spawned = tokio::task::spawn_blocking(move || {
                burner_fiber::Fiber::spawn(&image, &cell_id, &dir)
            })
            .await
            .context("the fiber spawn task panicked")?
            .with_context(|| format!("spawning the wasm database for cell '{}'", spec.id))?;
            Some(Arc::new(tokio::sync::Mutex::new(spawned)))
        }
    };

    Ok(RunningCell {
        spec,
        node,
        peer_id,
        listen_addrs,
        fiber,
    })
}

/// Floor for regolith's `block_cache_size` (D11): half the cell's memory
/// budget, never below this, so a small budget still gets a workable cache.
const REGOLITH_MIN_BLOCK_CACHE_BYTES: u64 = 16 * 1024 * 1024;
/// Floor for regolith's `write_buffer_size` (D11).
const REGOLITH_MIN_WRITE_BUFFER_BYTES: u64 = 8 * 1024 * 1024;
/// Ceiling for regolith's `write_buffer_size` (D11): upstream's own default,
/// so a generous budget does not balloon the memtable machinery
/// disproportionately to the cache.
const REGOLITH_MAX_WRITE_BUFFER_BYTES: u64 = 64 * 1024 * 1024;

/// Regolith's `block_cache_size`, derived from a cell's memory budget (D11):
/// half the budget, floored at 16 MiB (`REGOLITH_MIN_BLOCK_CACHE_BYTES`).
/// Pure arithmetic (no I/O), unit-tested directly below; `open_store` is the
/// only caller that turns this into a real `RegolithStoreOptions`.
///
/// The derivation is unchanged across upstream's backend fold (D36):
/// regolith exposes the same two knobs lark did (`block_cache_size`,
/// `write_buffer_size`), so a cell's budget still lands on the cache and
/// the memtable in exactly the proportions D11 chose.
pub fn regolith_block_cache_bytes(mem_budget_bytes: u64) -> u64 {
    (mem_budget_bytes / 2).max(REGOLITH_MIN_BLOCK_CACHE_BYTES)
}

/// Regolith's `write_buffer_size`, derived from a cell's memory budget
/// (D11): an eighth of the budget, floored at 8 MiB
/// (`REGOLITH_MIN_WRITE_BUFFER_BYTES`) and capped at 64 MiB
/// (`REGOLITH_MAX_WRITE_BUFFER_BYTES`).
pub fn regolith_write_buffer_bytes(mem_budget_bytes: u64) -> u64 {
    (mem_budget_bytes / 8).clamp(
        REGOLITH_MIN_WRITE_BUFFER_BYTES,
        REGOLITH_MAX_WRITE_BUFFER_BYTES,
    )
}

/// Clamps a derived byte count into `usize` (the backend option structs'
/// own field type), saturating rather than panicking on the practically
/// unreachable 32-bit-`usize` overflow case.
fn clamp_to_usize(bytes: u64) -> usize {
    usize::try_from(bytes).unwrap_or(usize::MAX)
}

/// Store directories written by backends upstream has since deleted.
///
/// Upstream folded every backend into regolith (D36). Regolith cannot read
/// what lark or redb wrote, and, worse, it does not notice: opening a cell
/// directory that holds `data.lark` simply creates an empty regolith store
/// beside it. The cell then comes up with zero collections and every
/// tenant on it degrades with "collection not found", which reads as data
/// loss rather than as the unmigrated data root it actually is.
const LEGACY_STORE_DIRS: &[&str] = &["data.lark", "data.redb"];

/// Refuses to open a cell whose directory still holds a pre-fold store.
///
/// Fails loud with the fix rather than silently starting empty (D42). The
/// legacy directory is never touched: the data is still there, and this
/// error is what gives an operator the chance to migrate or archive it
/// before anything overwrites it.
fn reject_legacy_store(dir: &Path, cell_id: &str) -> Result<()> {
    for legacy in LEGACY_STORE_DIRS {
        let path = dir.join(legacy);
        if path.exists() {
            anyhow::bail!(
                "cell '{cell_id}' holds a pre-fold store at {}. Upstream replaced every \
                 storage backend with regolith, which cannot read it, and starting anyway \
                 would bring the cell up empty and degrade every tenant on it with \
                 'collection not found'.\n\
                 \n\
                 The old data is untouched. To move every pre-fold store aside and start \
                 fresh, keeping the old data on disk:\n\
                 \n\
                     just archive-legacy-stores\n\
                     just start\n\
                 \n\
                 To discard the whole data root instead: `just reset-data`.",
                path.display()
            );
        }
    }
    Ok(())
}

async fn open_store(spec: &CellSpec, dir: &Path) -> Result<embedded::EmbeddedStore> {
    match spec.backend {
        // D11: cache/buffer sizes are genuinely derived from
        // mem_budget_bytes, not left at RegolithStoreOptions::default();
        // every other option (compaction, bloom filter, durability, ...)
        // stays default, per D11's ledger-phase scope of "cache sizing only".
        BackendKind::Regolith => {
            reject_legacy_store(dir, &spec.id)?;
            let dir = dir.to_path_buf();
            let block_cache_size =
                clamp_to_usize(regolith_block_cache_bytes(spec.mem_budget_bytes));
            let write_buffer_size =
                clamp_to_usize(regolith_write_buffer_bytes(spec.mem_budget_bytes));
            let store = tokio::task::spawn_blocking(move || {
                let mut options = storage::RegolithStoreOptions::new();
                options.engine.block_cache_size = block_cache_size;
                options.engine.write_buffer_size = write_buffer_size;
                storage::RegolithStore::open_with_options(&dir, options)
            })
            .await
            .context("regolith open task panicked")??;
            Ok(embedded::EmbeddedStore::Regolith(store))
        }
        // `in_memory` rather than a separate backend type: upstream's fold
        // left regolith the only engine, and its memory preset starts no
        // threads and skips fsync, which is what an ephemeral cell wants.
        BackendKind::Memory => {
            let store = tokio::task::spawn_blocking(storage::RegolithStore::in_memory)
                .await
                .context("regolith in-memory open task panicked")??;
            Ok(embedded::EmbeddedStore::Regolith(store))
        }
    }
}

/// Polls `listen_addresses()` on a fixed step until it returns at least one
/// address, or fails once `LISTEN_ADDR_DEADLINE` has elapsed. Deadline plus
/// bounded step: the swarm-driving task that registers the bound address
/// runs separately from `build_with_store`'s own await, so this is a real
/// (short) race, not a formality.
async fn wait_for_listen_addrs(p2p: &embedded::ManagedP2PSystem) -> Result<Vec<String>> {
    let deadline = Instant::now() + LISTEN_ADDR_DEADLINE;
    loop {
        let addrs = p2p
            .ops()
            .listen_addresses()
            .await
            .map_err(|error| anyhow!(error))
            .context("querying listen addresses")?;
        if !addrs.is_empty() {
            return Ok(addrs);
        }
        if Instant::now() >= deadline {
            anyhow::bail!("timed out waiting for a libp2p listen address");
        }
        tokio::time::sleep(LISTEN_ADDR_POLL_STEP).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dialable_addr_assembles_listen_addr_and_peer_id() {
        assert_eq!(
            assemble_dialable_addr(&["/ip4/127.0.0.1/tcp/9171".to_string()], "12D3Koo"),
            Some("/ip4/127.0.0.1/tcp/9171/p2p/12D3Koo".to_string())
        );
        assert_eq!(assemble_dialable_addr(&[], "12D3Koo"), None);
    }

    #[test]
    fn cell_data_dir_nests_under_cells() {
        let root = Path::new("/data");
        assert_eq!(
            cell_data_dir(root, "cell-0"),
            PathBuf::from("/data/cells/cell-0")
        );
    }

    #[tokio::test]
    async fn ignite_rejects_a_non_ipv4_bind_addr_before_touching_disk() {
        let spec = CellSpec {
            id: "cell-0".to_string(),
            group: "default".to_string(),
            backend: BackendKind::Memory,
            p2p_port: 9171,
            bind_addr: "::1".parse().unwrap(),
            mem_budget_bytes: crate::spec::DEFAULT_MEM_BUDGET_BYTES,
            signing_key_file: PathBuf::from("/does/not/matter"),
        };

        // A nonexistent data_root proves the IPv4 check runs before any
        // filesystem work: if it didn't, this would fail with an I/O error
        // instead of the expected validation message. `RunningCell` (the
        // Ok side) isn't `Debug` (it holds a real `EmbeddedNode`), so this
        // matches instead of using `unwrap_err()`.
        match ignite(Path::new("/nonexistent-data-root"), spec, None).await {
            Ok(_) => panic!("expected ignite to reject a non-IPv4 bind_addr"),
            Err(error) => assert!(error.to_string().contains("non-IPv4 bind_addr")),
        }
    }

    #[test]
    fn a_pre_fold_store_is_refused_with_the_fix_named() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("data.lark")).unwrap();

        let error = reject_legacy_store(dir.path(), "cell-4").unwrap_err();
        let rendered = format!("{error:#}");
        assert!(rendered.contains("cell-4"), "{rendered}");
        assert!(rendered.contains("data.lark"), "{rendered}");
        assert!(
            rendered.contains("cannot read it"),
            "the error must say why, not just that it refused: {rendered}"
        );
        assert!(
            rendered.contains("reset-data") || rendered.contains("archive"),
            "the error must name the fix: {rendered}"
        );
    }

    #[test]
    fn a_retired_redb_store_is_refused_too() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("data.redb")).unwrap();
        assert!(reject_legacy_store(dir.path(), "cell-0").is_err());
    }

    #[test]
    fn a_clean_or_regolith_directory_is_accepted() {
        let dir = tempfile::tempdir().unwrap();
        assert!(reject_legacy_store(dir.path(), "cell-0").is_ok());
        // A live regolith store's own files must not trip the guard.
        std::fs::create_dir(dir.path().join("sst")).unwrap();
        std::fs::create_dir(dir.path().join("wal")).unwrap();
        std::fs::write(dir.path().join("MANIFEST"), b"{}").unwrap();
        assert!(reject_legacy_store(dir.path(), "cell-0").is_ok());
    }

    // --- D11: memory-budget-derived storage options ------------------------

    #[test]
    fn regolith_block_cache_is_half_the_budget() {
        // Well above the floor, so half-the-budget is the effective value:
        // 512 MiB budget -> 256 MiB cache.
        assert_eq!(
            regolith_block_cache_bytes(512 * 1024 * 1024),
            256 * 1024 * 1024
        );
    }

    #[test]
    fn regolith_block_cache_floors_at_16_mib_for_a_tiny_budget() {
        assert_eq!(
            regolith_block_cache_bytes(1024 * 1024),
            REGOLITH_MIN_BLOCK_CACHE_BYTES
        );
        // Exactly at the floor boundary (32 MiB budget / 2 == 16 MiB):
        // still the floor value, not a fraction below it.
        assert_eq!(
            regolith_block_cache_bytes(2 * REGOLITH_MIN_BLOCK_CACHE_BYTES),
            REGOLITH_MIN_BLOCK_CACHE_BYTES
        );
    }

    #[test]
    fn regolith_write_buffer_is_an_eighth_of_the_budget_in_the_middle_range() {
        // 128 MiB budget / 8 == 16 MiB: above the 8 MiB floor, below the
        // 64 MiB ceiling.
        assert_eq!(
            regolith_write_buffer_bytes(128 * 1024 * 1024),
            16 * 1024 * 1024
        );
    }

    #[test]
    fn regolith_write_buffer_floors_at_8_mib_for_a_tiny_budget() {
        assert_eq!(
            regolith_write_buffer_bytes(1024 * 1024),
            REGOLITH_MIN_WRITE_BUFFER_BYTES
        );
    }

    #[test]
    fn regolith_write_buffer_ceils_at_64_mib_for_a_huge_budget() {
        assert_eq!(
            regolith_write_buffer_bytes(crate::spec::DEFAULT_MEM_BUDGET_BYTES * 100),
            REGOLITH_MAX_WRITE_BUFFER_BYTES
        );
    }

    #[test]
    fn clamp_to_usize_passes_through_in_range_values() {
        assert_eq!(clamp_to_usize(0), 0);
        assert_eq!(clamp_to_usize(512 * 1024 * 1024), 512 * 1024 * 1024);
    }
}
