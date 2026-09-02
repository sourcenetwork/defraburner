# Plan: defradb as an AOT-compiled burn package, one persistent wasm DefraDB per DefraCell

> Status: PLAN ONLY
> Operator direction: 2026-09-01 (recorded decisions inline, rejected forks at the bottom).
> No product code until this plan is approved.

## 0. TL;DR

- defraburner gains `packages/defradb/`: a `language = "rust"` burn package carrying the defradb.rs
  engine (db, query, storage/regolith, schema, document, crdt, acp, kms, blockstore, datastore,
  identity, crypto, events), AOT-compiled by the `burn` CLI into
  `precompiled/wasm32-wasip1/main.wasm` inside a `.afb` (the documented afterburner AOT form:
  "compile [dir] ... AOT-compiles to a WASM module under precompiled/ so the runtime loads it
  directly", afterburner.sh docs).
- defraburner loads that `.afb` and keeps one long-lived wasm instance per DefraCell: a preopened
  data directory per cell gives real disk persistence; a framed request loop drives schema
  application, document CRUD, and queries inside the sealed module.
- Verified before this plan (measured, this host, 2026-09-01): the whole engine stack compiles
  clean for `wasm32-wasip1` against defradb.rs main (`cargo check --target wasm32-wasip1
  --no-default-features -p db -p query -p storage -p schema -p document -p crdt -p defra-core
  -p events -p acp -p kms -p blockstore -p datastore`, green in 1m02s, in a scratch target dir;
  the defradb.rs checkout was not modified).
- Honest open gaps, named up front: afterburner 0.2.7 itself cannot host this package (its sealed
  path hard-codes a stdin/stdout-only WASI context at
  afterburner crates/afterburner-wasi/src/host.rs:243, and its long-lived daemon is JS-only), so
  the loader is defraburner-owned; the network layer (iroh, libp2p, p2p sync engine) is the
  operator's lane and stays native in the process; the wasm32-wasip1 size and start-time numbers
  are not measured yet and are Phase 0 deliverables, not claims.
- End state the operator locked in: wasm-only cells. Native cells retire once the fiber passes
  parity gates and the operator's network lane lands.

## 1. Problem and context

Three facts make this non-trivial.

1. defraburner no longer builds against defradb.rs main. Upstream commit `0c8597b4`
   ("feat(storage): make regolith the only backend", 2026-08-28) deleted the `lark` and `redb`
   backends; `cargo check` today does not even resolve ("depends on `storage` with feature `lark`
   but `storage` does not have that feature"). 274 upstream commits landed since defraburner last
   built. Any wasm work rides on a tree that is green first.
2. The wasm target is real for the engine but absent for the node shell. The probe above is
   green, and regolith (the only backend now) is explicitly a wasm32-wasip1 target
   (regolith src/portability.rs lists `wasm32-wasip1` supported; compaction runs on the calling
   thread when threads are absent). But defradb.rs's own embedded assembly hard-requires
   `p2p/libp2p-transport`, which cannot compile for wasm (tokio refuses: "Only features
   sync,macros,io-util,rt,time are supported on wasm", hit today via crates/p2p unconditional
   tokio `fs`; libp2p needs TCP/noise/yamux sockets). So the package assembles its node from `db`
   directly, the same pattern defradb.rs's own browser client (crates/wasm) uses, not through
   `embedded`.
3. Afterburner 0.2.7 cannot host a persistent wasm command. The sealed precompiled path builds
   `WasiCtxBuilder::new().stdin(..).stdout(..).stderr(..).build_p1()` (afterburner
   crates/afterburner-wasi/src/host.rs:243): no preopened dir, no env, no sockets, fresh `Store`
   per call. `DaemonRuntime`, the only long-lived path, takes JS source and runs the QuickJS
   plugin (afterburner crates/afterburner-wasi/src/daemon_runtime.rs:73). The operator directed
   the package be compiled by the burn CLI and loaded by defraburner; the loading is therefore
   defraburner-owned, on wasmtime directly (already in the tree via afterburner).

On iroh: it cannot compile into the wasm package, and that is a fact of the ecosystem, not a
scope choice. iroh 1.0.1's wasm gates are browser-only (`target_family = "wasm" +
target_os = "unknown"`, wasm-bindgen), and even there its transports are compiled out; on
`wasm32-wasip1` it takes the native path, which tokio forbids outright, and WASI preview1 has no
UDP for QUIC regardless. iroh keeps running natively in the defraburner process, where it works
today.

## 2. Approach

One package, one loader, one cell model.

### The package: `packages/defradb/`

- `afb.toml` with `language = "rust"`, namespace `defraburner`, name `defradb`.
- `source/main.rs`: a WASI command with a real `main` (burn's rust path requires a `[[bin]]`
  target). It opens regolith at a path inside the preopened directory, then serves a
  length-prefixed JSON request loop on stdin/stdout: apply schema, put/get/delete documents,
  execute queries, subscribe/emit local events. One request in flight at a time; the loop keeps
  the instance's database alive across requests, which is what makes the cell persistent rather
  than per-call.
- Dependencies: path deps into the sibling `../defradb.rs` checkout, maximal engine set from the
  probe (db, query, storage, schema, document, crdt, defra-core, events, acp, kms, blockstore,
  datastore, identity, crypto; each additional crate joins only if its Phase 0 probe compiles).
  Not in the package: p2p, embedded, http, cli, ffi, defra-node.
- Build: `burn compile` (the burn CLI, per operator direction) produces
  `packages/defradb/defraburner-defradb-<v>.afb` containing `precompiled/wasm32-wasip1/main.wasm`
  plus `source/` fallback. The `.afb` is committed, exactly like the existing policy packages'
  committed `.afb` files, so `just start` stays zero-flag.
- Tests: `burn test` runs `tests/` in the sandbox (engine-level: schema round-trip, CRUD,
  query correctness, persistence across simulated reopen).

### The loader: a new `burner-fiber` crate

- Extracts the wasm from the `.afb` (same zstd+tar mechanism burner-policy already uses for
  policy packages; one canonical implementation shared, not copied).
- Per DefraCell: one wasmtime `Engine`-shared, `Module`-cached instance with
  `WasiCtxBuilder::preopened_dir(cell_data_root)` (persistence: the cell's existing data root
  layout under the cluster manifest, so reset-data and recovery semantics carry over unchanged),
  `StoreLimits` memory ceiling, and the framed request loop served over pipes the host owns.
- Cells keep their existing identity, manifest entry, and dashboard presence. One fiber per
  DefraCell, serialized per fiber (regolith on wasip1 is single-threaded by design), N fibers
  across the tokio worker pool.
- Per-fiber metrics (ignite time, request latency histogram, memory ceiling usage) land in the
  existing status snapshot the dashboard already consumes.

### The cell model

Internal engine seam in burner-cell with two implementations (native EmbeddedNode, wasm fiber)
during the transition, selected per cell. Native stays the default until the fiber passes the
parity gate (Phase 3). When the fiber passes parity AND the operator's network lane lands, the
default flips and the native implementation is deleted: the wasm-only end state. The interim seam
is transition scaffolding with a written removal condition, not a permanent fork.

### The network boundary, and the bridge

The fiber serves the data plane. iroh/libp2p run natively in the defraburner process (where they
work today) and reach every fiber through a bridge, so the transport works for every running wasm
instance the same way it works for native cells today (operator direction, 2026-09-01: the
transport "must work for every running WASM persistent defra instance", with autoscaling, many
tenants, many databases, a mesh of databases).

The bridge splits the upstream p2p crate at its own seam:

- Guest side (inside the package): the p2p crate's transport-agnostic modules (sync engine,
  replicator, protocol, topics, signing, the `P2PTransport` trait at defradb.rs
  crates/p2p/src/transport.rs) compile into the package once its one wasm blocker is resolved
  (see below). The package's main.rs implements `P2PTransport` over host imports: send-to-peer,
  topic publish/subscribe, dial, inbound-message events. Implementing that trait in our package
  source modifies nothing upstream.
- Host side (burner-fiber): a native transport service running the same p2p crate's wire half
  (libp2p/iroh crates natively) and shuttling each fiber's messages across the host imports, one
  mesh identity per fiber, routed per cell. Both sides use the same p2p crate, split at the
  transport trait: no protocol reimplementation, and the wire stays byte-compatible with native
  and Go nodes.
- The blocker, as measured in Phase 0 (this supersedes the pre-probe estimate that it was one
  manifest line; it is two things, and the second is the real one):
  1. p2p's manifest enables tokio `fs` unconditionally (used only inside the iroh-gated module),
     and tokio forbids `fs` on wasm. One line.
  2. With that fixed in a scratch copy, p2p then fails with 85 errors that are all one root
     cause: `storage`'s `Store`/`Txn` traits are declared
     `#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]`
     (defradb.rs crates/storage/src/corekv/traits.rs:196), so on wasm their futures are not
     `Send`, while p2p's sync layer requires `Send` futures and `S: Send + Sync` stores.
     Blast radius is small and concentrated: `sync/collection_store.rs`,
     `sync/pending_store.rs`, and three call sites
     (`sync/coordinator/event_handler/branchable_sync.rs`, `.../doc_sync.rs`,
     `sync/replication/handlers.rs`).
  This is not novel work: upstream already applies exactly this cfg_attr pattern in every crate
  it intends for wasm (measured: db 49 sites, query 65, crdt 9, blockstore 2). p2p has 0,
  because it was never a wasm target. Phase 5 is "apply the house pattern to p2p's sync layer",
  a mechanical change following a convention with 125 existing instances upstream.
  The defradb.rs checkout is read-only to this project, so the resolution is an explicit
  operator call at the Phase 5 gate: make the change upstream in defradb.rs (preferred, it is
  upstreamable as-is and benefits every wasm consumer), or vendor a patched p2p copy into this
  repo and re-sync it on upstream bumps. Nothing proceeds past Phase 4 without that call.
  `replication-filter` is red for wasm only because it depends on p2p, so it turns green with
  the same fix. `sourcehub` stays out: its `acp-light-client` git dependency pulls `tokio/full`
  (process, signal, net), and it is already an optional upstream feature.
- Autoscaling and placement are engine-blind today (they act on cells through the supervisor's
  command channel); fibers integrate through that same path in Phase 3, so autoscaling many
  tenant databases over wasm cells is inherited behavior, verified by a gate, not new code.

## 3. Features: correctness, honesty, scale

| Feature | Math correct? | Honest? | Scales to target? | Action |
|---|---|---|---|---|
| Engine compiles to wasm32-wasip1 | ok (measured probe, 1m02s green) | ok | ok | Phase 0 widens the matrix to every candidate crate and pins the maximal set |
| AOT artifact via burn CLI | ok (measured Phase 0c: burn compile produced a precompiled wasm32-wasip1 .afb) | ok | caveat: real module size still unknown (0d measured a storage-only spike) | Phase 2 measures the real package; if the module is absurd, return to the operator before loader code |
| Per-cell disk persistence on wasip1 | ok (measured Phase 0b: clean reopen 100/100, crash-path WAL recovery 100/100) | ok | ok | Re-verified per cell in Phase 3 under the real fiber loop |
| Persistent fiber loop (one DB alive across requests) | ok (wasmtime instances are long-lived) | ok | caveat: per-fiber RSS unmeasured | Phase 0 measures; StoreLimits ceiling per fiber |
| Native defraburner upgrade to defradb.rs main (regolith) | ok (mechanical port, API surface verified present) | ok | ok | Phase 1, gate green, ships standalone |
| Schema/CRUD/query parity fiber vs native | not yet (no fiber yet) | ok (stated as untested) | ok | Phase 3 parity golden set, both engines, same inputs, same outputs |
| Crash recovery of a fiber cell | not yet | ok | ok | Phase 3: SIGKILL the process, restart, data intact (the existing recovery test pattern, re-pointed at fibers) |
| iroh/libp2p mesh over wasm cells | no (impossible in the module; the bridge runs it native, section 2) | ok | ok | Phase 5 bridge: guest P2PTransport over host imports, native transport service per fiber |
| Autoscaling / placement over fiber cells | ok (command-channel path is engine-blind; verified in Phase 3) | ok | ok | Phase 3 gate: autoscaler ignites and drains a fiber cell |
| wasm-only end state | ok | caveat: gated on the network lane | ok | Phase 4 flip, only after parity + network lane |
| Admin SDL validation through the package | ok (parse_sdl is in the compiled set) | ok | ok | Phase 4 consumer |
| Policy-brain real query semantics | ok | ok | ok | Phase 4 consumer |
| Dashboard dry-run against a sealed copy | ok | ok | ok | Phase 4 consumer |

## 4. Phases

De-risk experiments first. No phase depends on a later one. Every phase gates before the next
starts.

### Phase 0 results (run 2026-09-01, scratch only, no product code, both sibling repos untouched)

Every number below is fresh output from this host, not an estimate.

- 0a compile matrix, `wasm32-wasip1`, against defradb.rs main `305b16ed`:
  - green: db, query, storage (regolith), schema, document, crdt, defra-core, events, acp, kms,
    blockstore, datastore, identity, crypto, cursor, defra-version, zanzibar, lens.
  - red: p2p (two causes, see section 2), replication-filter (only via p2p), sourcehub
    (`acp-light-client` -> `tokio/full`; optional upstream feature, legitimately excluded).
- 0b persistence, the plan's kill criterion: PASSED, twice.
  - clean path: guest opens regolith under a WASI preopen, writes 100 keys, commits, closes;
    host filesystem shows real `MANIFEST`, `wal/wal_000002.log`, `sst/000003.sst`; a separate
    wasm process reopens and reads back `found=100/100 mismatch=0`.
  - crash path (the SIGKILLed-fiber case): commit then exit with no `close()`; only
    `MANIFEST` + `wal/wal_000001.log` on disk, no SST flush; a fresh process recovers
    `found=100/100 mismatch=0` from the WAL.
- 0c burn CLI rust path: PASSED. `burn compile` on a `language = "rust"` package produced
  `defraburner/probe@0.1.0 (precompiled wasm32-wasip1)`, 28015 bytes,
  digest `sha256:b1b0bfec...`. The operator-directed toolchain path works end to end.
- 0d measurements, *storage-only spike, not the real package* (the full defradb module will be
  substantially larger; its numbers are a Phase 2 deliverable, not claimed here):
  wasm 773.5 KiB, cwasm 1913.3 KiB, cold start JIT 14-15 ms vs AOT-precompiled 6-7 ms
  (5 runs each). AOT is ~2.4x faster to start even at this size, which is the evidence behind
  keeping the AOT artifact rather than compiling at every process start.

- Phase 0 (de-risk, scratch only, no product code):
  - 0a: maximal compile matrix: every defradb.rs crate that could join the package, probed for
    wasm32-wasip1 in a scratch target dir. Proves the maximal dependency set. Kill: if the core
    set beyond the already-green list cannot compile without upstream changes, the package scope
    shrinks to what compiles and this plan is re-presented.
  - 0b: regolith persistence spike: a scratch wasmtime harness (not defraburner code) instantiates
    a trivial wasm command with a preopened dir, opens regolith through the defradb storage crate,
    writes, drops, reopens, reads back. Also crash-mid-write behavior. Kill: if regolith cannot
    persist under wasip1, the persistent-cell goal is unmeetable and the plan halts for an
    operator decision.
  - 0c: burn CLI rust path end-to-end: scaffold the real package shape, `burn compile`, extract
    the `.afb`, run the module in the 0b harness. Proves the operator-directed toolchain path
    before any defraburner code exists. Kill: if burn's rust path cannot produce a runnable
    wasip1 command, fall back is discussed with the operator, not improvised.
  - 0d: measure: module size, cold instantiate time, per-request overhead, per-fiber RSS.
    These numbers go in this plan's status table, not into a claim.
- Phase 1: native defraburner upgrade to defradb.rs main. The regolith port (storage,
  EmbeddedStore::Regolith, Persistent roots), gate green (fmt, clippy -D warnings, doc, tests;
  the D20 two-process detector remains the known red). New upstream knobs (at-rest encryption,
  query_limits, rate limits) recorded deferred, not smuggled in. Ships standalone: defraburner
  works again against latest upstream.
- Phase 2: the package. `packages/defradb/` with source, protocol, tests; `burn test` green;
  `burn compile` artifact committed; justfile recipe to rebuild it. Gate: package tests + the
  defraburner gate (the package is a separate cargo tree; its lockfile is committed).
- Phase 3: the loader and the fiber cell. burner-fiber crate, burner-cell engine seam, fiber
  ignition through the existing supervisor path, parity golden set (schema, CRUD, query,
  persistence, SIGKILL recovery, both engines, identical inputs), dashboard per-cell engine
  metric. Gate: full defraburner gate + parity set green with fibers enabled for a test tenant.
- Phase 4: consumers. Admin SDL validation through the package (the admin boundary stops parsing
  untrusted SDL in-process), policy-brain query semantics, dashboard dry-run view, per-fiber
  metrics surfaced.
- Phase 5: the mesh bridge. p2p compiles into the package (after the operator resolves the
  one-line blocker at this gate), the guest implements `P2PTransport` over host imports, the
  native transport service routes per fiber, replication over a fiber group works end to end
  (two fiber cells, one tenant, documents replicate, both persist), and the result meshes with
  native cells and the Go interop suite unchanged. Gate: the existing two-process mesh detector
  and replication golden set pass with fiber cells.
- Phase 6: the flip. Default becomes wasm cells, the native implementation is deleted, wasm-only
  end state reached (many tenants, many databases, autoscaled, meshed).

## 5. Scale and streaming

- The bounded resource is per-fiber linear memory. Each DefraCell fiber gets an explicit
  wasmtime `StoreLimits` ceiling and a regolith mem_budget sized under it; the ceiling is a
  dashboard knob (per-cell), defaulted from measurement (Phase 0d), never unbounded. N cells are
  N fibers: linear in cells, same as today's native cells.
- The request loop is framed and one-request-at-a-time per fiber: a request payload larger than
  the configured frame ceiling is rejected at the boundary, never buffered whole. Query results
  stream page-wise through the frame protocol exactly as the gateway pushes LIMIT to the source
  today; the fiber holds a page, not a collection.
- No new dependency without a ladder check: wasmtime + wasmtime-wasi become direct deps of
  burner-fiber (already in the tree via afterburner, so no new build unit); tar+zstd reuse
  burner-policy's existing pins.
- Binary size is a measured number in Phase 0d, and the `.afb` is committed, so the repo grows by
  exactly that artifact, the same way the policy packages already do.

## 6. Verification

- Golden case, persistence: start, create a tenant with a wasm cell, write documents, SIGKILL the
  process, restart, documents are present and query-identical. This is the existing recovery test
  pattern re-pointed at a fiber cell; it is the test that proves "persistent" is not a claim.
- Golden case, parity: the same schema/CRUD/query script run against a native cell and a fiber
  cell produces identical results. The parity set lives in defraburner's test suite and runs both
  engines.
- Package-local: `burn test` in packages/defradb (sandboxed), plus a protocol framing property
  test (no partial frame ever observed at the guest).
- The gate: vertexia gate per phase on the defraburner tree; the package's own cargo tree gets
  fmt/clippy/doc/tests the same way (it is a workspace of its own under packages/).
- Every performance claim (ignite time, request RTT, RSS, size) ships with the Phase 0d/3 script
  and numbers; anything unmeasured reads "not measured yet", in this plan and in the README.

## 7. Status

See `docs/plans/defradb-wasm-STATUS.md` (created with this plan, all phases not-started).
Deferred items with reasons live there; the load-bearing one: the p2p manifest one-liner, an
explicit operator call at the Phase 5 gate (upstream fix or vendored copy), before any mesh work.

## Rejected forks and change log

- Rejected: hosting the fiber inside afterburner 0.2.7. Its sealed path cannot preopen a
  directory and its daemon is JS-only (section 1). Operator directed the burn CLI + load into
  defraburner; loader is defraburner-owned on wasmtime.
- Rejected: modifying afterburner or defradb.rs. Standing operator constraint; both repos are
  consumed, never written. The one upstream line this plan would have wanted (p2p's unconditional
  tokio `fs`) belongs to the operator's network lane and is named here for it.
- Rejected: in-memory-only wasm cells. Operator explicit: "No in memory ephemeral thing, we must
  have full persistence." Persistence is a Phase 0 kill-criterion spike, not a hope.
- Rejected: iroh in the wasm module. Impossible on wasm32-wasip1 (browser-only wasm gates in
  iroh 1.0.1, tokio's wasm feature wall, no UDP in WASI preview1). Native iroh in the process
  continues to serve the mesh.
- Rejected: vendor-patching p2p into the package now. Operator chose "engine now, p2p in your
  lane"; a vendored fork would diverge from upstream for no current payoff.
- Rejected: wasmtime `.cwasm` sidecar caching on top of the AOT artifact. Operator directed the
  documented burn CLI convention (`burn compile` -> `precompiled/wasm32-wasip1/main.wasm`). If
  Phase 0d start times justify it, it returns as a measured proposal, not a silent addition.
- 2026-09-01: plan written from operator decisions taken live in session (scope, package
  contents, burn CLI AOT, defraburner-owned loading, wasm-only end state, network lane carve-out).
- 2026-09-01: Phase 0 run. Section 2's network-bridge paragraph was rewritten: the pre-probe
  estimate that p2p's wasm blocker was "one manifest line" was wrong. It is that line plus an
  85-error Send-bound mismatch at the storage/p2p seam, with a small blast radius and an
  existing upstream pattern to follow. Section 3's persistence and AOT rows moved from caveat
  to measured. Numbers added under "Phase 0 results".
- 2026-09-01: operator approved and extended: the transport must work for every running wasm
  instance, with autoscaling, many tenants, many databases, a mesh of databases. The mesh bridge
  moved from "operator's lane, out of plan" into this plan as Phase 5, with Phase 6 the
  wasm-only flip. The one defradb.rs blocker (p2p's unconditional tokio `fs`) is now
  load-bearing and gets an explicit operator call at the Phase 5 gate: upstream one-liner or
  vendored patched copy.
