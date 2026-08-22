# defraburner: an afterburner-governed DefraDB cluster in one binary

Status: awaiting operator approval. No product code exists yet.
Date: 2026-08-18. Operator decisions recorded inline; change log at the bottom.

## TL;DR

defraburner is a single Rust binary that ignites N unmodified defradb.rs nodes
("cells") inside one process, meshes them over P2P (libp2p by default, iroh by
knob), shards tenants across cell groups with replication factor R, autoscales
the cell fleet with policy brains that run as sealed afterburner (.afb)
packages, and serves an embedded dashboard in the afterburner design language.
Everything persists to disk; a restart recovers every cell and the mesh.

Headline claims, each grounded in verified fact:

- N full DefraDB nodes in one process is a proven upstream pattern, not a bet:
  defradb.rs's own defra-node test suite builds multiple complete nodes in one
  process on multi-threaded tokio, meshes them on loopback, replicates, and
  reopens stores after shutdown (defradb.rs crates/defra-node/src/p2p_tests.rs,
  crates/p2p/tests/host_tests.rs, crates/embedded/tests/iroh_smoke.rs).
- The embedded assembly we consume (defradb.rs crates/embedded) already has
  transport as a knob (TransportConfig::Libp2p | Iroh) and storage as a
  generic (build_with_store), and the per-node axum Router is public, so one
  gateway listener can front all cells. DefraDB stays byte-for-byte unmodified.
- Afterburner's verified sweet spot is exactly what the policy layer needs:
  one-shot, sealed, deterministic JSON-in/JSON-out execution of registered
  packages. The cluster's decision logic (autoscaling, placement, admission
  tuning) ships as .afb packages under packages/ and is called each control
  tick; the Rust mechanism clamps every decision inside hard guardrails.

Honest open gaps (tracked, not hidden; updated after Phase 0):

- Measured (final, this host; see DECS D16/D19/D20 for context): dist
  binary 91.8 MiB (fat LTO; thin-LTO dev build 139.7 MiB); ignite
  ~70 ms/cell; explicit sync 109 ms; subscription sync 808 ms; 2-cell
  SIGKILL recovery 250 ms on a quiet host (a loaded-host re-run gave
  13.3 s / 10.5 s and is flagged non-representative); gateway adds
  ~1.0 ms p50 over direct execution, and admitted requests through the
  full gateway path hold p50 1.53-1.56 ms at 1 and 3 cells; snapshot
  assembly for 64 cells 0.37 ms; GCRA admission 368 ns/check; per-cell
  RSS: lark defaults ~260 MiB (mostly fixed engine overhead: the
  mem_budget knob now drives cache sizes, but shrinking the fixed part
  needs upstream lark tuning), libp2p ~28-30 MiB, base ~30 MiB.
- Known upstream defect, discovered and independently reproduced by this
  project (D20): with N>1 in-process nodes, every libp2p TCP listener
  after the first is advertised by the API but dead at the OS level
  shortly after ignition. In-process groups are unaffected; late inbound
  dials to cells 2..N fail, so cross-host meshes should dial out from
  later cells toward first cells until defradb.rs fixes the listener
  lifetime. Full repro: docs/upstream/defradb-rs-second-listener-dies.md.
  The two-process mesh test stays in the suite as the regression detector
  and is the one non-green test (223/224).
- Verified in Phase 0: Libp2pConfig carries only listen_addr, but upstream
  persists the libp2p keypair in the cell's own peerstore, so stable peer IDs
  across restart come free (golden-test-asserted in Phase 1). The signing
  registry is written by every Enabled cell and wiped whole by any cell
  shutdown; Phase 1 verifies whether any runtime path reads it.
- Not tested yet: live interop against a Go DefraDB node (local checkout at
  ~/projects/defradb-go exists for this). Phase 2 gate covers it.
- Build prerequisite discovered: libclang is required (rquickjs bindgen is
  unconditional in afterburner-node-compat even for wasm-only builds).
- WASM-hosted cells are not possible today on either side (see Deferred), and
  this plan does not pretend otherwise.

## Problem and context

The operator wants a DBaaS-shaped system: many DefraDB instances, multi-
threaded, sharded, fast, P2P-meshed, autoscaling, one binary, fancy embedded
UI, DefraDB untouched.

Verified ground truth the plan is built on (2026-08-18):

- defradb.rs (local: ~/projects/defradb.rs, workspace v0.5.0, MSRV 1.91,
  Apache-2.0 OR MIT) is the Rust implementation of DefraDB with Go v1.0.0-rc1
  network interop. Not on crates.io (verified 404); consumed as path deps.
- Go DefraDB v1.0.0 is BSL 1.1 with a managed-services exclusion; the Rust
  implementation's Apache/MIT licensing is the DBaaS-clean base.
- DefraDB has no native sharding in either implementation (verified: zero
  shard/partition concepts). Replication is whole-collection (gossipsub
  subscription) or per-peer replicators with optional per-collection filters.
  Sharding therefore lives entirely in defraburner's routing layer.
- defra-node (the "reusable embedded builder" crate) is iroh-only by design;
  the embedded crate is the assembly with libp2p (default, Go-compatible) plus
  iroh behind a feature, generic storage, and no forced HTTP listener. We use
  embedded, and mirror defra-node's public wiring of defra_http::Server to
  build one router per cell under our own single listener.
- Process-global state in defradb.rs is catalogued and each item has a clean
  answer: one tracing subscriber installed once in main; OTel via
  without_global() (or skipped per-cell); telemetry conflict counters are
  process-global and will be shown as cluster-wide, never per-cell; the
  signing identity registry is shared but keyed by DID (one DID per cell);
  each cell gets its own data dir and its own P2P port.
- afterburner (local: ~/projects/afterburner, v0.2.7, BSL 1.1, owned by the
  operator) cannot host long-running WASI guests and gives WASI guests no
  sockets; its resident-guest machinery is QuickJS/JS-only; it has no
  autoscaler (worker pools are fixed at construction; io_workers and
  .shards(n) are dead knobs). What it does superbly: sealed one-shot
  JSON-in/JSON-out execution, content-addressed registration, Manifold
  capability vocabulary, thrust admission/backpressure patterns, MemoryLedger
  accounting hook, and a complete no-framework design system
  (website/design-system.css) for the dashboard.
- defradb.rs has no GraphQL playground anywhere (verified). The dashboard's
  query console fills a real gap.

## Chosen approach: afterburner-governed native cells, afterburner-sandboxed policy brains

One binary, `defraburner`. Operator decisions that define it:

1. Cells are native in v1 (operator choice, 2026-08-18): each cell is one
   embedded::EmbeddedNode built in-process, governed like an afterburner
   package: a manifold-shaped grant (its own data dir, its own P2P port, a
   memory budget, admission tokens), an explicit lifecycle, supervision, and
   full observability. Governance is enforced by configuration, accounting,
   and admission, not hardware isolation; the dashboard and docs say
   "governed", never "isolated". True WASM cells are deferred (blocked
   upstream on both repos; see Deferred).
2. Policy brains are real .afb packages, shipped AOT-compiled (operator
   choice, sharpened 2026-08-19): defraburner embeds the afterburner engine.
   Autoscaling, placement, and admission-tuning policies live under packages/
   as sealed JS-authored burn packages whose shipped form is the
   `burn compile` output: an .afb bundling a self-contained AOT-compiled
   wasm32-wasip1 module (verified locally: precompiled/wasm32-wasip1/main.wasm,
   javy 8.1.1 at build time only). The release binary embeds the default
   packages' precompiled wasm at build time (include_bytes) and registers
   them via register_precompiled(bytes, "wasm32-wasip1"); it performs no JS
   compilation at startup. A packages/ directory on disk is the override and
   extension point (precompiled .afb preferred; raw source registration
   remains a dev-mode convenience only). Policies run each control tick as
   pure functions MetricsSnapshot -> Decision; Rust validates and clamps
   every decision inside hard guardrails; a failing policy never wedges the
   cluster (last-known-good plan holds, the failure surfaces loudly).

   Deliverables are exactly two build outputs (operator directive
   2026-08-19): the defraburner release binary (dashboard and default policy
   wasm embedded) and the AOT-compiled packages under packages/. Nothing
   else ships. Build prerequisites: rustc (1.91+), libclang, burn, javy
   8.1.1; the justfile's package recipe runs burn compile per package and
   orders it before the release build.
3. Sharding is tenant-first (operator choice): the shard unit is the tenant.
   Each tenant is placed on a group of R cells that replicate the tenant's
   collections among themselves. No cross-shard query surface exists in v1,
   so no scatter-gather correctness risk. Document-hash sharding within a
   collection is a designed later phase, not dropped.
4. Scope is host-local autoscaling plus static mesh (operator choice): the
   autoscaler grows and shrinks cells on this host. Multiple defraburner
   hosts mesh over P2P via config-declared peers. Disk persistence
   everywhere; `defraburner start` after a crash recovers every cell, its
   identity, and the mesh. Cross-host placement orchestration is deferred.
5. Transport is a knob, libp2p default (operator choice): Go-wire-compatible
   TCP/QUIC/WebSocket/DNS with Kademlia discovery; iroh selectable per
   cluster. Mirrors upstream's --p2p-transport vocabulary.
6. Storage default is lark (operator choice): Source Network's own pure-Rust
   lark-kv, the upstream CLI default. redb, rocksdb, and fjall stay available
   as per-cluster/per-tenant knobs via build_with_store; memory is an explicit
   dev-only flag because the operator requires disk persistence everywhere.
7. DefraDB and afterburner are consumed as path dependencies of the sibling
   checkouts (~/projects/defradb.rs, ~/projects/afterburner), both unmodified.
   afterburner is BSL 1.1 and operator-owned, so embedding it here is the
   owner's own grant; a note travels with any future distribution plan.

### The pieces

Workspace: bin crate `defraburner` plus lib crates `burner-cell`,
`burner-mesh`, `burner-gateway`, `burner-policy`, `burner-dashboard`.
Policies live in `packages/` (autoscale-default, placement-default). Every
source file stays under ~1000 lines; modules split along these same seams.

- burner-cell: CellSpec (id, group, manifold grant, backend, transport,
  identity DID), lifecycle Provision -> Ignite -> Ready -> Drain ->
  Extinguish, plus Recover. Builds embedded::NodeBuilder / build_with_store,
  persists each cell's P2P secret key so peer identity survives restart,
  registers one DID per cell in the signing registry, owns per-cell event-bus
  subscriptions. Panic containment: defraburner builds with panic=unwind and
  cell tasks run under supervised join handles; a panicked cell is reported
  and re-ignited. An abort still takes the process; that blast radius is
  documented, and a process-isolation knob is deferred.
- burner-mesh: the cluster manifest (data_root/cluster.json, atomic
  write-rename plus fsync, human-readable), tenant -> cell-group placement
  table, replication wiring (within a group every cell subscribes the
  tenant's collections via P2POperations::add_collections; cross-host links
  use add_replicator), static peer dialing from config, restart recovery
  (read manifest, re-ignite all cells, re-dial peers).
- burner-gateway: one axum listener (default 9181). Builds each cell's
  defra_http router in-process (the same public wiring defra-node uses
  internally) and routes by tenant: bearer token -> tenant -> group ->
  cell. Sticky routing by token hash for read-your-writes within a session;
  failover to group peers; consistency semantics documented as CRDT eventual
  convergence, stickiness as an optimization not a guarantee. Per-tenant
  GCRA admission (thrust's pattern) before routing; rejects carry
  retry-after. Request/latency metrics recorded per cell here.
- burner-policy: embeds the afterburner engine. Loads and registers the .afb
  packages from packages/ (content-addressed), assembles the per-tick
  MetricsSnapshot (per-cell qps, p50/p99, inflight, admission rejects,
  ledger, storage bytes, sync_status gauges, event-bus lag; host cpu/mem;
  manifest), runs each policy sealed, validates the Decision against hard
  clamps (min/max cells, max actions per tick, cooldowns, never exceed host
  memory budget), executes via burner-cell/mesh, and appends every decision
  (input hash, output, clamps applied, outcome) to a bounded on-disk decision
  log the dashboard renders. Policy error => keep last plan, log loudly,
  light the dashboard red. Never a silent fallback.
- burner-dashboard: assets embedded into the binary (no build step, vanilla
  HTML/CSS/JS matching the afterburner site's approach), design tokens taken
  from ~/projects/afterburner/website/design-system.css (Sora/Inter/JetBrains
  Mono; midnight ink #061b31, deep violet #533afd, vibrant orange #ff6118,
  accent green #81b81a; light marble surfaces, midnight code blocks, inferno
  and matrix accent themes via the tweaks pattern). Views: cluster overview
  (cells, tenants, topology), cell detail (sync_status gauges, tx stats,
  storage), tenants (placement, admission, usage), autoscaler timeline (the
  decision log, with clamps shown), and a query console against the gateway
  (upstream has no playground; this fills it). Live data over SSE with
  bounded queues and visible drop counters (reusing the events crate's
  dropped_count semantics). Empty, loading, and error states are short and
  informative; the design carries the meaning (UI minimalism doctrine).
- Config and knobs: defraburner.toml plus CLI flags, mirroring upstream
  `defradb start` vocabulary 1:1 where a knob maps directly (store,
  durability, p2p-transport, peers, query-max-depth/width/filter-depth,
  query timeout, identity paths) and adding cluster knobs: data-root,
  gateway addr, replication-factor default, cells min/max, admission
  defaults, policy package paths, tick interval, telemetry.

### What "fast" means here and how it is kept honest

- No IPC on the data path: gateway -> cell is an in-process call into the
  cell's router; the only network hops are client -> gateway and P2P
  replication.
- Batch-first boundaries: metrics snapshots are assembled once per tick, not
  per request; SSE fan-out is one serialization per tick per topic, not per
  subscriber-row; decision-log writes are appended, bounded, and rotated.
- Bounded everything input-sized: admission buckets per tenant, SSE queues
  with drop counters, decision log ring, policy input capped (top-N cells by
  activity with the cap stated in the snapshot itself), replicator adds
  rate-limited, recovery re-ignition concurrency capped.
- Perf numbers ship with the phase that makes them measurable (Phase 6
  benches); until then every number in docs reads "not measured yet".

## Per-feature honesty table

| Feature | Math correct | Claim honest | Scales to target | Status |
|---|---|---|---|---|
| N in-process cells, one binary | Proven upstream pattern | Yes: "governed cells", never "isolated" | Bounded by host RAM; ledger + admission enforce budgets | ok |
| P2P mesh, libp2p default + iroh knob | Upstream transports as-is | Go interop verified live (P2 gate: real doc synced from a running Go node) | In-process groups proven; cross-host inbound to cells 2..N blocked by the upstream listener defect (D20), dial-out topology works meanwhile | caveat (upstream fix pending) |
| Tenant sharding (placement + R-replication) | Placement is a routing table, no cross-shard math | Yes: "tenant is the shard unit; no cross-shard queries in v1" | Groups scale with cells; per-doc sharding named deferred | ok |
| Autoscaler mechanism + clamps | Clamp algebra is simple and testable | Yes: decisions logged with clamps applied | Tick cost bounded by snapshot cap | ok |
| Policy brains as .afb packages | Pure JSON fn; deterministic, sealed | Yes: policy quality iterates; failure keeps last-known-good, loudly | Engine call is one-shot, ms-scale at 5s ticks | ok |
| Memory budgeting per cell | Accounting (ledger) + admission + query limits | Yes: budget is governance, not a hard wall; stated in UI | Backend cache knobs (lark/redb/rocksdb) bound the big consumers | caveat (by design, stated) |
| Restart recovery (cells, identity, mesh) | Manifest + persisted keys + reopenable stores (proven reopen upstream) | Yes: golden kill -9 test is the claim's evidence | Re-ignition concurrency capped | ok after P1 gate |
| Gateway routing + admission | GCRA per tenant (thrust pattern) | Yes: eventual consistency documented; stickiness is an optimization | One listener; per-request overhead measured in P6 | ok |
| Dashboard (embedded, live) | Renders measured values only; "no data" renders as "no data yet", never 0% | conflict_metrics shown cluster-wide with a label (process-global upstream) | SSE bounded with visible drops | ok |
| WASM-hosted cells | n/a | Truthfully impossible today (both repos verified) | n/a | not-yet (deferred, blocked upstream) |
| Cross-host placement orchestration | n/a | Named deferred | n/a | not-yet (deferred) |

## Phases and gates

De-risk first; no phase depends on a later one. Implementation is by
vertexia-implement with an ask before each phase; code is written by the
coder agent to this plan's specs; every phase ends with the gate green
(fmt, clippy -D warnings, doc build -D warnings, tests) plus its own gate
condition, run on the real artifact.

- Phase 0: scaffold + de-risk spike. git init; workspace skeleton; path deps
  on defradb.rs (embedded, defra-http, storage, p2p as needed) and
  afterburner; one spike binary: two cells ignite in-process, mesh on
  loopback, a schema and a document replicate A->B, one .afb policy package
  registers and answers a JSON call. Measures: binary size (two wasmtime
  majors), per-cell baseline RSS, spike wall time. Verifies embedded's
  Libp2pConfig field surface (listen addrs, key persistence) and records the
  fallback decision if a field is missing.
  Gate: spike test green in CI-shape (just/cargo), measurements written into
  this plan's status table.
- Phase 1: burner-cell + recovery. CellSpec/manifold, lifecycle, identity
  and key persistence, manifest, supervised panic re-ignition, recovery.
  Gate: N cells ignite; kill -9 then restart recovers every cell with the
  same peer IDs and data (golden test).
- Phase 2: burner-mesh + tenants. Placement table, group replication wiring,
  static cross-host peers, tenant provisioning (create tenant -> schema ->
  placement), teardown.
  Gate: tenant writes converge across its group (in-process); two
  defraburner processes mesh on loopback; live interop smoke against a Go
  DefraDB node from ~/projects/defradb-go.
- Phase 3: burner-gateway. Single listener, token -> tenant routing, sticky
  + failover, per-tenant admission, consistency semantics doc.
  Gate: end-to-end GraphQL through the gateway against a placed tenant;
  admission rejects observed and correct under synthetic overload.
- Phase 4: burner-policy + autoscaler. Snapshot pipeline, engine host,
  packages/autoscale-default and packages/placement-default (JS), clamps,
  decision log, execution of scale/place actions.
  Gate: synthetic load ramps scale cells up then down within guardrails; a
  deliberately broken policy package leaves the cluster on last-known-good
  with a loud surface; decision log renders every step; policies load from
  AOT-precompiled wasm (embedded defaults and packages/ .afb overrides) with
  zero JS compilation at binary startup, burn and javy being build-time
  tools only.
- Phase 5: burner-dashboard. Embedded assets in the afterburner design
  language, SSE live data, all five views including the query console and
  the autoscaler timeline.
  Gate: dashboard served from the single binary shows live cells, tenants,
  decisions, and executes a query, under the Phase 4 synthetic load.
- Phase 6: hardening + performance. Criterion benches and a load script:
  throughput/latency vs cell count, gateway overhead vs direct cell call,
  recovery time vs cell count, snapshot cost; memory ledger accuracy checks;
  numbers land in this plan and the PR; budgets set and enforced in CI shape.
  Gate: the perf script and before/after numbers exist in-repo; every
  "not measured yet" in this plan is replaced by a number or an issue.

## Scale and streaming

- The bounded resource is host RAM. Bounds: per-cell backend cache knobs
  (lark/redb/rocksdb/fjall exposures), per-cell ledger budgets, admission
  before enqueue, capped policy snapshot size (cap stated in the snapshot),
  bounded SSE queues with visible drops, bounded decision log (ring on
  disk), capped recovery concurrency. Nothing input-sized is unbounded.
- Streaming: query results stream through the cells' own HTTP/GraphQL
  streaming as upstream provides; dashboard consumes periodic snapshots and
  bounded event streams, never full-table pulls; metrics hold a page (the
  current tick and a bounded ring of history), never the universe.
- Substrate reused: defradb.rs crates for everything database-shaped; the
  afterburner engine for policy execution; thrust's GCRA admission pattern
  and MemoryLedger vocabulary reimplemented minimally in defraburner (the
  crates stay in afterburner's workspace; we borrow the shape, not a fork).

## Verification

- Golden cases: (1) kill -9 recovery with identical peer IDs and data; (2)
  tenant convergence across a group; (3) cross-process loopback mesh; (4) Go
  interop smoke; (5) admission rejection under overload with correct
  retry-after; (6) broken-policy safety (last-known-good + loud surface);
  (7) clamp enforcement (a policy demanding 10x cells gets the clamp, and
  the log shows it); (8) dashboard renders "no data yet" states honestly.
- The gate (every phase): cargo fmt --check; cargo clippy --all-targets
  -D warnings; cargo doc -D warnings; cargo test; plus the phase gate above.
  Wire as a justfile from Phase 0 so `vertexia gate` picks it up.
- End-to-end: an integration test drives the real defraburner binary (start,
  provision tenant, write via gateway, kill, restart, read via gateway).
- Tests follow doctrine: colocated units for every public behavior including
  failure paths; property tests for manifest round-trip and placement
  invariants (every tenant placed on exactly R live cells); no fixed sleeps
  (deadline + bounded backoff); float-free assertions except measured
  latencies (asserted as bounds, not equality).

## Status

| Workstream | Status | Notes |
|---|---|---|
| Phase 0 scaffold + spike | done (2026-08-18, gate green, loop-verified) | Both replication paths converge in-process over libp2p; policy package answers in the wasm sandbox; measurements in TL;DR; corrections logged as DECS D7/D8 |
| Phase 1 cells + recovery | done (2026-08-19, gate green, loop-verified) | Golden kill -9 test passes: identical peer IDs, addresses, and data across SIGKILL; recovery of 2 cells in 250 ms; RSS attributed (DECS D11); no signing-registry defense needed (D10); ignition futures are not Send (standing constraint D12) |
| Phase 2 mesh + tenants | done (2026-08-19, gate green, loop-verified) | Tenant convergence + disjointness proven in-process; two-process loopback mesh proven; Go interop smoke PASSED live over the real wire (DECS D16); deterministic topic-ready primitive replaced the flake |
| Phase 3 gateway | done (2026-08-19, gate green, loop-verified) | Full upstream router from embedded's public parts; per-tenant tokens + GCRA admission with Retry-After; overhead measured ~1.0 ms p50 (D16); consistency semantics in docs/consistency.md |
| Phase 4 policy + autoscaler | done (2026-08-19, gate green, loop-verified) | AOT precompiled packages drive the loop (D9/D17 pipeline live); scale-up and scale-down proven under real load with clamps, cooldown, and decision-log evidence; malformed/corrupt policy safety proven (D19) |
| Phase 5 dashboard | done (2026-08-19, gate green, loop-verified) | Embedded, offline, afterburner tokens; SSE with bounded queues and header-borne auth; honest "no data yet" and "mean ms" labeling (D19) |
| Console round (post-v1: D21/D23/D24/D25) | in-progress | One-command up, full admin control surface, realtime DNA-themed console; two gated milestones (backend, UI) |
| Phase 6 hardening + perf | done (2026-08-19, gate 159/160, loop-verified) | Truthful readiness (confirmed peers only); mem_budget_bytes drives lark/redb caches; dist binary 91.8 MiB; perf numbers in TL;DR; the 160th test is the deliberate regression detector for the upstream listener defect (DECS D20, docs/upstream/defradb-rs-second-listener-dies.md) |

Deferred (named, with reasons; tenant-visible wording stays honest):

| Deferred item | Reason |
|---|---|
| WASM-hosted cells | Blocked upstream on both sides: defradb.rs has no WASI target (verified zero wasip1/wasip2); afterburner has no resident WASI guests and no guest sockets (verified). Revisit when either lands; the cell abstraction is the seam where the substrate would swap. |
| Document-hash sharding | Cross-shard GraphQL merge (sort/aggregate/pagination) is the hardest correctness surface; needs its own design package. Tenant-first covers the DBaaS shape now. |
| Process-isolation knob per cell | v1 is in-process by operator choice; kernel isolation contradicts it. Revisit if blast-radius pain appears. |
| Automated live tenant migration | Data movement is risky to automate on day one; v1 places new tenants by policy and moves existing tenants via an explicit admin command. Policies may recommend moves. |
| Cross-host placement orchestration | Operator scoped v1 to host-local autoscaling + static mesh. |
| Sandboxed UDFs near data | Real afterburner fit, but speculative DBaaS scope today. |
| Per-tenant bring-your-own policy packages | Natural extension of the .afb policy layer; needs a tenancy/trust design first. |
| Postgres wire exposure (upstream pg-compat) | Upstream marks it experimental; expose as a knob once upstream stabilizes it. |

Rejected forks (for history): Go child processes supervised by Rust (superseded
by defradb.rs); WASM-first program across both repos before any product
(operator declined for v1; deferred instead); iroh-only transport via
defra-node (loses Go interop and the transport knob); kernel-sandboxed
process-per-node (contradicts in-process choice); machinery as new crates in
the afterburner workspace (operator chose in-repo packages/ + crates).

## Change log

- 2026-08-19 (final): Phases 4, 5, and 6 done and loop-verified; all
  measured numbers folded into the TL;DR; the run closes at 159/160 tests
  green with the one red test kept deliberately as the regression detector
  for the upstream defradb.rs listener defect this project discovered
  (D20, docs/upstream/defradb-rs-second-listener-dies.md). Nothing
  committed; tree awaits operator review.
- 2026-08-19: Operator sharpened the deliverables to exactly two build
  outputs: AOT-compiled wasm packages under packages/ and the defraburner
  release binary. Policy shipping switched from runtime JS registration to
  burn compile AOT artifacts (verified: .afb bundles a self-contained
  precompiled wasm32-wasip1 module); approach section 2 and the Phase 4 gate
  rewritten accordingly; DECS D9 records it and supersedes D1's packing
  deferral.
- 2026-08-18 (later): Phase 0 done and loop-verified; TL;DR open-gaps and
  status table updated to measured truth; corrections recorded in DECS D7/D8
  (signing-registry semantics, free peer-ID persistence, libclang build
  prerequisite, per-cell RSS surprise).
- 2026-08-18: Initial package. All decisions taken with the operator in
  session: substrate (governed native cells), policy home (.afb packages
  under packages/), sharding (tenant-first), scope (host-local + static
  mesh + persistence), transport (libp2p default + iroh knob), storage
  (lark default), machinery location (inside defraburner).
