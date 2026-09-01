# defraburner decision log (vertexia-loop run)

Plan: docs/plans/defraburner.md (approved 2026-08-18, loop to completion).
Newest first. Every decision the loop takes is recorded here for operator
review: what was decided, the options, why, what it affects, reversibility.

## D43 (2026-09-01): the red mesh detector was leaked test processes

`cross_process_mesh_dials_static_peers`, kept red since August as the D20
second-listener regression detector, now passes on three consecutive runs
and inside the full suite (251/251).

- What the failures actually showed: B's `connected_peers` never contained
  A's *second* cell. The ports named in two separate failure messages
  (`20270`, `32761`) were being held at that moment by leaked
  `defraburner` processes from earlier runs of the same test, which picks
  a random base port per run. The new B was connecting to a stale A from
  a previous run rather than to its own.
- So at least some of this project's "known upstream defect" evidence was
  self-inflicted. That does not retroactively disprove D20, whose original
  repro (docs/upstream/) stands on its own, but it does mean the detector
  was not measuring what it claimed on those runs.
- Recorded rather than resolved, deliberately: whether upstream's 274
  commits fixed the listener lifetime has NOT been confirmed on a clean
  host, and claiming a fix on the strength of three passes next to nine
  leftover processes would be exactly the false green this project's
  honesty fence exists to prevent. The README now states the ambiguity
  instead of either old claim.
- Operational lesson, worth more than the test result: leaked test
  processes are not untidy, they corrupt results. Two were also bound to
  9171-9179 against the operator's real data root, which is what produced
  the "Unexpected peer ID at /ip4/127.0.0.1/tcp/9172" in a live run.
- Affects: README's suite-count and detector paragraph.
- Reversible: n/a; this is a finding, not a change in behaviour.

## D42 (2026-09-01): a pre-fold data root fails loud instead of coming up empty

Found by running against the operator's real data root: 12 cells each held
a `data.lark` from before upstream's backend fold, and every one of them
started anyway with an empty regolith store created beside it. Three
tenants degraded with "collection 'Lazer' not found", which reads as data
loss but was actually an unmigrated data root plus a silent failure.

- Options: (a) migrate lark data to regolith on open; (b) refuse to ignite
  a cell whose directory holds a retired backend's store; (c) leave it and
  document the upgrade step.
- Chosen: (b).
- Why: (a) is not possible with any code that exists. Upstream deleted the
  lark and redb backends outright, so nothing in the current tree can read
  `data.lark`; a migrator would have to be built against a pre-fold
  checkout of defradb.rs, which is a project of its own and not something
  to fake. (c) is what was already happening, and it is precisely the
  false-green the honesty fence forbids: the cluster reported healthy
  cells while serving none of the data.
- The guard never touches the legacy directory. The data is still there,
  and the error exists to give an operator the chance to archive or
  migrate it before anything overwrites it.
- The error names the cell, the path, why regolith cannot read it, and
  both fixes (archive the directory, or `just reset-data`). Three tests
  cover it, including that a live regolith store's own files do not trip
  it.
- Affects: burner-cell `cell::open_store`.
- Reversible: yes, but removing it would restore a silent-data-loss path.

## D41 (2026-09-01): the autoscaler grows but does not shrink

Operator direction: "autoscaler must not scale down ok? for this poc let
it spawn more", plus a dashboard control to disable the autoscaler.

- Options: (a) edit the `autoscale-default` policy package to stop
  proposing scale_down; (b) veto scale_down in the Rust clamp; (c) leave
  the behaviour and only add a UI toggle.
- Chosen: (b), with the toggle.
- Why: a policy proposes and the clamp decides, which is the trust
  boundary this project already has. Editing the package would leave the
  guardrail still authorizing removal, so any other policy (an override, a
  future package) would shrink the cluster again. Vetoing in the clamp
  makes the property hold for every policy, present and future.
- Scale-down became data-destroying when fibers became cells (D40):
  draining a cell now destroys the wasm database it owns. An automatic
  removal is therefore a data-loss action taken with no operator in the
  loop, which is not a trade worth making for a proof of concept.
  Removal stays explicit: `DELETE /admin/cells/{id}`.
- Default is off, and off is the direction a missing value drifts:
  `#[serde(default)]` on a `bool` is `false`, so a manifest written before
  the field existed also loads with removal disabled.
- Both bools (`paused`, `scale_down_enabled`) are always written to the
  manifest rather than omitted when false, so an operator reading
  `cluster.json` sees the safety posture stated rather than inferred from
  an absent key. A serialization test pins the exact shape.
- The dashboard gained two switches: "disable the autoscaler" (the
  existing pause, relabelled to say what it does) and "allow scale down",
  which is off by default and says why in place.
- Affects: burner-cell manifest/command, burner-policy clamp/autoscaler,
  burner-gateway admin_autoscaler + overview, the Autoscaler view.
- Reversible: yes, it is a knob; turning it on restores the prior
  behaviour exactly.

## D40 (2026-09-01): fibers are cells

Operator direction, verbatim: "fibers = cells". A fiber is not a separate
resource with its own ids and lifecycle; every cell owns exactly one wasm
DefraDB, sharing the cell's id and lifetime.

- What changed from D39: `/admin/fibers` (a parallel registry with its own
  ignite/drain and arbitrary names) is gone. A cell's database is
  addressed at `/admin/cells/{id}/db`, and there is no ignite or drain
  there because `/admin/cells` already owns that lifecycle.
- The fiber is spawned inside `cell::ignite` and dropped with the cell, so
  there is no reachable state where a cell has a stale fiber or a fiber
  outlives its cell. `FiberPool` was deleted rather than left unused: the
  supervisor's own cell map is the pool, and a second ownership model for
  the same objects is exactly the divergence the doctrine forbids.
- A cell's database lives at `<data_root>/cells/<id>/fiber/`, nested
  inside the cell rather than beside it, so removing a cell's directory
  removes its database and cannot orphan one.
- Verified live: a spawned cell answers on its database immediately with
  no separate step; a drained cell's database is gone with it; and the
  autoscaler scaling an idle cell down took that cell's database with it
  without any fiber-specific code in the autoscaler, which is the whole
  point of the unification.
- Measured while building this, and corrected once observed properly
  (honesty fence): regolith **does** lock, but only on native targets. A
  native cell's store carries a `LOCK` file; the same store opened by the
  wasm guest does not, because WASI preview1 has no `flock`. A second
  fiber opened on a live directory therefore succeeds, which a second
  native store would not.
  So for fibers specifically the single-writer guarantee is structural,
  not enforced by the store: one fiber per cell, a directory derived from
  the cell id, and a manifest that already refuses a duplicate id. Two
  tests pin this, and neither claims a guarantee the stack does not
  provide on this target.
- Not done here, and gated on the mesh bridge: tenant traffic still routes
  to the cell's native embedded node, because a wasm database cannot
  replicate (no sockets in WASI preview1). Routing tenants at fibers
  before Phase 5 would silently downgrade every multi-replica tenant to a
  single-node database.
- Affects: burner-cell (cell/supervisor), burner-fiber (pool deleted),
  burner-gateway admin_fibers, the dashboard Databases view,
  console_coverage.
- Reversible: yes, but there is no reason to: the unified model is
  strictly simpler than the parallel one it replaced.

## D39 (2026-09-01, SUPERSEDED by D40): fibers as a parallel admin surface

`/admin/fibers` ignites, drives, and drains wasm DefraDB fibers. Tenant
traffic still routes to native cells.

- Superseded the same day: the operator's direction was "fibers = cells".
  Kept here as history, not as current truth; D40 is what stands.
- Options: (a) replace the cell backend outright, routing tenants at
  fibers; (b) add a parallel fiber surface and migrate later; (c) a
  per-cell engine knob threaded through the supervisor now.
- Chosen at the time: (b).
- Why: (a) is the plan's Phase 6 and is gated on the mesh bridge, which
  does not exist: a fiber cannot replicate, so making it the tenant
  backend today would silently downgrade every multi-replica tenant to a
  single-node database. (c) spends supervisor and manifest churn on a
  seam whose second implementation cannot yet pass parity. (b) ships the
  whole fiber capability, operable from the dashboard, without touching
  the path that serves real tenant traffic.
- Fibers live under `<data_root>/fibers/<id>`, never `cells/<id>`, so a
  fiber and a native cell of the same name cannot collide on one
  directory. A unit test pins that.
- Igniting is explicit, never implicit on first query: starting a
  database is an operator action with a real cost, and doing it as a side
  effect would hide it. A query to a fiber that is not running is a 404
  naming the fix.
- Affects: burner-fiber (new crate), burner-gateway admin_fibers, the
  dashboard Fibers view, console_coverage (4 new enforced rows).
- Reversible: yes; the surface is additive and nothing else depends on it.

## D38 (2026-09-01): the fiber protocol is duplicated, and a test enforces it

The wire protocol exists twice, in `crates/burner-fiber/src/protocol.rs`
and `packages/defradb/source/protocol.rs`.

- Options: (a) one shared crate both sides depend on; (b) two copies with
  a drift test; (c) two copies, reviewed by hand.
- Chosen: (b).
- Why: (a) is impossible in practice. The guest is a separate cargo tree
  built for wasm32-wasip1 with `db` in a configuration that does not
  compile for the host at all, so a shared crate would have to be
  target-generic across a boundary whose whole point is that the two
  sides differ. (c) is how two copies rot. (b) keeps them honest
  mechanically: `contract.rs` parses the guest's own source and fails if
  an operation is added, renamed, or removed on one side only, if the
  frame ceilings diverge, or if the guest's `DATA_DIR` stops matching the
  host's preopen path. The parser asserts it found operations at all, so
  a broken parser cannot make the test vacuously pass.
- This is the doctrine's "two places that must agree call one function"
  rule honored at the only level a wasm boundary allows: they cannot call
  one function, so a test proves they agree.
- Affects: crates/burner-fiber/src/contract.rs.
- Reversible: n/a; the duplication is structural.

## D37 (2026-09-01): the defradb fiber package is its own cargo tree

`packages/defradb` declares an empty `[workspace]` table, so it resolves
independently of the defraburner workspace, and it carries a copy of
defradb.rs's own `Cargo.lock`.

- Options: (a) join the defraburner workspace; (b) standalone workspace,
  fresh resolution; (c) standalone workspace pinned to upstream's lockfile.
- Chosen: (c).
- Why: (a) is impossible, the package targets wasm32-wasip1 with
  `panic = "abort"` and a size-first profile, and its `db` dependency has
  `native` off, a configuration that does not compile for the host at all.
  (b) was tried and failed: a fresh resolution picked socket2 0.6.5, which
  `db` reaches unconditionally through reqwest and which does not build for
  wasip1. Upstream's lock pins 0.5.10/0.6.3, the resolution the Phase 0
  probe validated, so inheriting it is both the fix and the reproducibility
  guarantee.
- Affects: packages/defradb/Cargo.toml, packages/defradb/Cargo.lock.
- Reversible: yes; re-resolving is a lockfile delete away, and would need
  the socket2 problem solved another way.

## D36 (2026-09-01): upstream folded every storage backend into regolith

defradb.rs `0c8597b4` deleted the lark and redb backends. defraburner did
not resolve against upstream main at all until this was ported.

- Options for `BackendKind`: (a) keep three variants and map two onto one
  engine; (b) collapse to `Regolith`/`Memory`, no compatibility; (c)
  collapse, with `lark`/`redb` kept as deserialization aliases.
- Chosen: (c).
- Why: (a) keeps names for engines that no longer exist, which is a lie in
  the manifest and on the dashboard. (b) fails a manifest that is merely
  named for a retired engine, when the rename itself is trivially
  recoverable. (c) loads the old name, writes the new one, so a manifest
  migrates itself on first save. Aliases are read-only and covered by two
  tests.
- CORRECTION (2026-09-01, found by running against a real pre-fold data
  root): the original text here claimed regolith "can in fact open that
  directory". That was wrong, and the mistake mattered. The alias makes
  the *manifest* load, but the *data* is written in lark's format, which
  no current code can read: regolith finds nothing of its own and creates
  an empty store beside the untouched `data.lark`, so the cell comes up
  with zero collections and every tenant on it degrades with "collection
  not found". The alias is still correct for what it does; it just never
  migrated data and must not be read as if it did. D42 makes that state
  fail loud instead of silent.
- The D11 memory-budget derivation is preserved exactly: regolith exposes
  the same `block_cache_size` and `write_buffer_size` knobs lark did, so a
  cell's budget still lands in the same proportions. The redb-only
  `redb_cache_bytes` derivation and its test went with the backend.
- An in-memory cell is now `RegolithStore::in_memory` rather than a
  separate engine, so it gains real transaction diagnostics, which the
  retired `Memory` backend never reported.
- Affects: burner-cell spec/cell/manifest/supervisor, every call site
  naming a backend, the attribution test's four RSS cases.
- Reversible: no; the retired backends do not exist upstream.

## D35 (2026-08-21): add collections to a live tenant

Operator, using the console: "there is no collection selection or
creation there". Two distinct gaps behind one sentence, both closed.

SELECTION was present but dead-ended: the Data view's collection list
refused to load without a tenant token, and the host stores only a
token's hash, so it genuinely cannot show an existing one. The way
forward (rotate to mint a fresh one) sat in a different card with no
pointer to it. The empty state now says why it cannot show the token and
offers the mint button in place, stating plainly that minting rotates the
token and cuts off any client still holding the old one.

CREATION did not exist: a tenant's collections were fixed at creation
time by its `schema_sdl`. Added `POST /admin/tenants/{name}/collections`
plus `burner_mesh::grow::add_collections`, which applies the SDL on every
cell in the tenant's group, wires the new collections for replication
with `wire_group`, and appends them to the tenant's stored SDL.

Decisions inside that:

- `wire_group`, not `ensure_group_connected`. D25 established that an
  already-`Placed` tenant's EXISTING collections cannot be confirmed via
  the edge-triggered topic-join event, because upstream restores those
  subscriptions from disk before reconcile runs. A collection being added
  now is genuinely new in this process, so that event really does still
  fire and waiting on it is meaningful again.
- Cells first, stored SDL last. A stored SDL naming a collection the
  cells lack would make every later reconcile fail to resolve it,
  degrading the tenant on that restart and every one after. The reverse
  leftover (registered on a cell, absent from the SDL) is inert: nothing
  wires it, routes to it, or writes to it. Applying per cell is
  idempotent (skips a cell that already has every collection), so a retry
  after a partial failure finishes the job.
- Add only, no remove. Dropping a collection destroys data; the two
  destructive paths that already exist say plainly what they erase.
- Every request-shaped failure is rejected in the handler before the mesh
  function runs (unparseable SDL, a name the tenant already has, unknown
  tenant, tenant not yet `Placed`), so anything the mesh function returns
  Err for is a real execution failure and a 500.

Verified live, not just unit-tested: added `Torpedo` to a serving tenant,
wrote and read a document through the tenant's own data plane, restarted
the cluster, and read the document back. The console-coverage contract
gained a row whose probe asserts the same data-plane write, so a route
that answered 200 while leaving the cells unable to serve the collection
would fail rather than pass a status-only check.

Reversible: the endpoint, the module, and the form can be removed; a
collection already added stays, as any other collection would.

## D34 (2026-08-21): three console defects found by running it

All three were found by the operator running the binary, and all three
share a shape: a per-tick redraw destroying state the operator was in the
middle of using.

- EVERY GENERATED READ FAILED (438 errors against 169 writes in the
  traffic generator, and the Data browser's document list too). The field
  list came from the collection's generated GraphQL object type, which
  carries DefraDB's synthetic members beside the schema's own: `_docID`,
  `_deleted`, `_version` and the aggregates COUNT, SUM, AVG, MIN, MAX,
  SIMILARITY, GROUP. The aggregates are scalar-typed, so they survived a
  "keep the scalars" filter, and selecting one without its target
  argument is a parse error. Writes survived only because DefraDB's
  mutation input parser ignores keys it does not know. Fixed by taking
  the field list from the generated mutation input type
  (`<Collection>MutationInputArg`), which contains exactly the schema's
  own fields: the schema's own answer to the question, so it cannot drift
  when upstream adds another aggregate. The input type's name is read off
  `add_<Collection>` rather than string-built, so an upstream rename
  surfaces as "no fields" instead of a wrong guess. The Data view's
  duplicate copy of this discovery was deleted; there is now one.
- THE MESH ACTION POPOVER VANISHED ONCE A SECOND. The panel rebuilds its
  SVG every tick, which re-creates the node under the pointer and makes
  the browser fire a fresh `mouseenter`; the tooltip handler called
  `openPopoverAt`, whose first act is `closePopover()`. The separation
  D25 introduced (a node's `mouseleave` must not close a clicked action
  popover) now runs both ways: a tooltip never supersedes an open action
  popover either.
- KNOBS INSIDE TICK-REBUILT TABLES WERE UNUSABLE. The Cells dial input
  and the Tenants admission rate/burst inputs live in markup rebuilt
  every second, so a value could not survive long enough to be submitted,
  and an action's result line was erased within a second of being
  written. Added `preserveVolatile`, which carries the operator's
  in-progress state across a rebuild keyed by each element's own
  data-attribute. Only fields the operator actually typed into are
  carried, tracked by a delegated `input` listener: restoring every
  captured value instead would let a stale capture mask a change the
  server made to the same setting. The deliberate consequence, recorded
  because it is a real trade: for a field you have edited, your value
  outranks a later server value until you change it again.

Also fixed in passing: `mesh-panel.js` contained three raw NUL bytes used
as a composite-key separator. Valid JavaScript, but it made the file
binary to every text tool (`grep` went silently empty on it, which is how
it was found). Same value, written as `\u0000`.

## D32 (2026-08-19): live-run bugs: tenant reconcile must be isolated

Operator ran the binary and hit real failures. Diagnosis from their log:

- Creating tenant 'gargarismo' failed with "re-wiring group for tenant
  'bombasticsystems': ... timed out after 15s waiting for peer ... to join
  the gossipsub topic". Two distinct defects:
  (1) ISOLATION: reconcile iterates every tenant and aborts the pass on any
  failure, so an unrelated unhealthy tenant blocks creating a new one.
  Fixed by making reconcile per-tenant isolated with per-tenant outcomes;
  admin handlers report only their own tenant, and other tenants surface a
  health field (ok | degraded + reason) in the overview, the Tenants table,
  and the mesh cluster caption.
  (2) STRUCTURALLY UNWINNABLE WAIT: the topic-join wait was written for
  fresh wiring, but reconcile re-invokes it for already-placed tenants, and
  the underlying event is edge-triggered and never replayed, so an already
  correctly-subscribed group can only ever time out. Re-wire becomes an
  idempotent ensure that probes current state (or, if upstream exposes no
  subscription-state API, tracks confirmed subscriptions in-process) and
  only waits on a subscription that can still fire an event.
- Stale cells: a tenant assigned to a no-longer-running cell waited on a
  dead peer. Reconcile now checks liveness first and marks the tenant
  degraded naming the missing cell; it never silently re-places
  data-bearing tenants.
- Log noise: a healthy ONE-cell cluster legitimately has no peers, so
  upstream logs ERROR/WARN per write (InsufficientPeers, kad bootstrap)
  and looks broken. Quieted precisely in the default filter for the
  no-peers case only, never for genuine multi-cell replication failures,
  with the console stating plainly that a single-cell cluster does not
  replicate.
- Also logged: upstream's per-ignition WARN about decoding
  "__local_p2p_identity__" as replicator info (it stores our keypair in the
  peerstore's replicator slot), recorded in docs/upstream/.

## D33 (2026-08-19): Overview traffic generator

- Decided (operator): an Overview toggle generates schema-driven synthetic
  traffic across placed tenants (mixed reads and writes, tenant tokens,
  through the gateway like any client, admission-aware with honest 429
  backoff) so every realtime surface can be seen working. A visible marker
  stays on screen while it runs: the measurements are real, but the load is
  generated, and an operator must never read it as organic usage. The
  round verifies by running it which surfaces actually move, and treats
  anything that does not move as a wiring bug.

## D31 (2026-08-19): blueprint mesh panel and consistent entity markers

- Decided (operator: visual panel markers for cells and tenants, the mesh
  as blueprint connected nodes per tenant on Overview, more visual
  appeal): a shared markerFor(kind, id) gives every entity a stable
  identity (hashed color from the validated dark series palette, shape by
  kind: cell circle, tenant hexagon, peer diamond) reused in tables,
  charts, sparklines, the mesh, and decision entries, so color follows the
  entity and never its rank; the mono id always renders beside it, so
  cycling colors past four entities never makes color carry identity
  alone.
- The Overview gains a deterministic SVG blueprint panel: one dot-grid
  cluster per tenant with its cells on a circle, a free-pool cluster, and
  cross-host peers as diamonds. Edges come from real connected_peers
  data: a live link is a solid accent hairline, an expected-but-missing
  link inside a tenant group is dashed in the warning color and counted in
  the cluster caption, and unknown connectivity is dotted rather than
  guessed. Drawing an idealized full mesh is explicitly forbidden: the gap
  between expected and actual is the panel's diagnostic value.
- Motion is small, honest, and reduced-motion gated: node pulse on events
  naming that cell, travelling dash only on edges of tenants with recent
  writes (static if that cannot be derived honestly).
- Reversible: yes.

## D30 (2026-08-19): completeness is mechanically enforced, not claimed

- Decided (operator: "nothing must be half wired, I want to control
  everything perfectly from the dashboard"): the console's acceptance bar
  is a bijection between control capabilities and UI controls, enforced by
  a coverage test (tests/console_coverage.rs) whose single const table is
  the source of truth. Each row asserts the route exists in the gateway
  source, its marker exists in the embedded dashboard.js, and the mounted
  endpoint answers a live binary with the admin token; the inverse scan
  fails a UI fetch to a path not in the table. Behavioral tests then prove
  each capability changes observable state (spawn, drain, dial, rotate,
  admission override actually rate-limits, pause suppresses a forced tick,
  drop vs retire, and the full document lifecycle).
- UI honesty rules per control: in-flight state, success reflecting real
  server state (never optimistic-only), failure showing the server's error
  body verbatim, 429 showing Retry-After, 409 naming the blocking tenant,
  503 naming the command-channel timeout; destructive actions armed and
  explicit about what they destroy.
- Orphans are resolved, not tolerated: a backend capability the UI cannot
  reach is wired or deleted, and the choice is reported.
- Reversible: no reason to be.

## D29 (2026-08-19): the console owns the data plane too

- Decided (operator directive: tenant creation, talking to the gateway,
  and creating/changing/deleting data must all be doable in the
  interface): the console gains a first-class Data view alongside the raw
  GraphQL tab: tenant + token selection (tokens are hash-stored, so the
  UI mints via rotate rather than pretending to recover one), collection
  browser from the tenant's schema, paged document table with a real
  limit/offset and a simple field filter, generated create/update forms
  by field kind, armed delete, and a copy-as-curl escape hatch. All data
  traffic goes through the gateway with the tenant bearer exactly like an
  external client, so per-tenant admission applies and a 429 renders its
  Retry-After honestly. Convergence is stated, not implied: the sticky
  cell answers immediately while the tenant's other cells converge.
- Proof: an end-to-end HTTP test performs the full document lifecycle
  (create, read, update, read, delete, confirm gone) through the gateway,
  so the data plane is verified independently of the browser.
- The generated mutation and query shapes are read from defradb.rs source
  rather than assumed, and reported.
- Reversible: yes (additive view + tests).

## D28 (2026-08-19): `just start` is the headline command; docs completed

- Decided (operator directive): `just start` is the one command a newcomer
  runs: it depends on the `packages` recipe (so policy wasm always exists
  on a fresh clone), builds with release-fast, and executes the `up` flow
  (banner, dashboard opened with the token). Args pass through; `just up`
  stays as an alias; `just token` prints the admin token. The default
  recipe listing puts start first.
- Also: every crate gets a short README (mechanics, invariants, gotchas),
  the three policy packages get real mechanics documentation (JSON in/out
  contract, the AOT lifecycle, and the host clamp contract a policy author
  must know), and the root README is rewritten as a front door leading
  with `just start`.
- Why: the repo's entry cost was too high for what the binary actually
  does; docs are part of done, not a follow-up.
- Reversible: yes.

## D27 (2026-08-19): spike deleted; afterburner engine hoisted to the core

- Decided (operator directives): (a) the Phase 0 spike (spike.rs, the
  hidden subcommand, tests/spike.rs) is deleted: its de-risk proofs are
  superseded by tenants.rs, go_interop.rs, and the policy engine tests;
  the RSS helper moves to the attribution test that uses it. (b) The
  afterburner engine stops being a burner-policy implementation detail:
  one engine is built at binary startup (wasm mode, sealed manifold) with
  fuel/memory/timeout surfaced as CLI knobs, burner-policy consumes the
  shared handle, and the engine appears in /admin/status and the console
  as a runtime block (mode, knobs, registered packages + hashes). Why:
  the engine is the binary's runtime service and future package workloads
  (UDFs per the deferred table) attach to it; owning it at the core makes
  that architecture visible instead of incidental.
- Reversible: yes.

## D26 (2026-08-19): published house crates ride registry versions

- Decided (operator directive, supersedes D18's path-dep for kovan and the
  path dep for afterburner): afterburner 0.2.7 and kovan-map 0.1.19 are
  consumed from crates.io (the published default versions); defradb.rs
  crates remain path deps because defradb.rs is not published (verified
  404). If a registry version lacks a needed API, the round reports it
  and keeps that one crate on path with a note, never silently.
- Reversible: yes.

## D25 (2026-08-19): the console round: D21+D23+D24 unified into one
   implementation dispatch

- Context: the Phase 7 coder round ended design-only (the wrap-up order
  landed before its first write): zero code, honest report, and a precise
  backend design handoff including two correctness catches that are now
  spec requirements: (1) execute_scale_up must take the supervisor lock
  BEFORE next_cell_index because the admin ProvisionCells path makes it
  multi-caller for the first time; (2) drain-cell holds the lock across
  its whole check-then-act, closing the race against admin tenant
  reconcile.
- Decided: implement D21 (one-command up, hidden spike, command channel,
  admin cell endpoints), D23 (full control surface + computed dataviz),
  and D24 (DNA theme, dark default, gradient DEFRABURNER wordmark) as ONE
  round with two internal gated milestones: M1 backend (Rust: up flow,
  channel, every admin endpoint, tests), M2 console UI (DNA theme,
  realtime stream, every button, charts/gauges, tests). Realtime contract
  sharpened per the operator ("non stop"): 1s SSE ticks plus event-driven
  pushes, client auto-reconnect with a visible connection pill, bounded
  ring buffers, no dead states. Sora/Inter legacy fonts are deleted when
  the DNA CSS lands (D24). The round's final sweep asserts zero external
  product names and zero em-dashes repo-wide.
- Reversible: yes.

## D24 (2026-08-19): dashboard design = the imported DNA theme
   (supersedes D22's afterburner-site styling; D23 palette recomputed)

- Decided (operator directive with a Claude Design import): the dashboard
  implements the operator's design
  project's DNA theme: void near-black paper surfaces, indigo #818cf8
  primary + orange #f97316 warm accent, Newsreader serif display + IBM
  Plex Sans body + JetBrains Mono, CORNERED edges (radius 0 everywhere),
  panels over shadows with indigo hairline hovers, and the brand gradient
  (135deg #6366f1 -> #a855f7 55% -> #f97316) on display text, accent stat
  numerals, and gradient progress fills. DARK is the default theme
  (operator confirmed); the DNA-tinted light theme rides the existing
  toggle. The DEFRABURNER wordmark gets the gradient text treatment
  (operator directive).
- Import provenance: project 4a3ce4de-d00b-4376-a370-b9f4e6145488 read via
  the claude.ai design MCP; reference set stored at
  crates/burner-dashboard/design/dna/ (app.css verbatim, tokens.css,
  dna.css extracted from the gallery page, primitives.jsx
  verbatim as the vanilla-JS markup contract; mock data.js reviewed but
  deliberately not stored). Served assets remain self-contained: no React/
  Babel/CDN/Google-Fonts (the design's app.css @imports are stripped);
  Newsreader + IBM Plex Sans variable woff2 fetched and embedded beside
  JetBrains Mono (OFL, license note updated); lucide icons inlined as
  static SVGs for the icons actually used; real cluster data only.
- Chart series palette recomputed for the dark surface (validator, sealed
  run): #6366f1, #ea580c, #059669, #a855f7: all six checks pass (lightness
  band 0.48-0.67, chroma, CVD worst adjacent deutan/protan >= 10, normal
  vision, contrast >= 3:1). UI accents stay the DNA's #818cf8/#f97316;
  status colors from the DNA slots (success oklch(60% .15 160), warning =
  scram-warm, error/witness oklch(58% .2 20)) with icon + label always.
  D23's light-theme afterburner palette is superseded.
- Sora/Inter woff2 remain on disk as legacy until the implementation round
  swaps the CSS, then they are removed.
- Reversible: yes.

## D23 (2026-08-19): Phase 8: full-control console with computed dataviz
   (operator directives: "full control in everywhere", "tenant addition
   removal and everything possible", gradient bars, metric graphs)

- Decided scope, dispatched after Phase 7 verifies: (a) tenant lifecycle
  completes: drop tenant (unsubscribe + remove placement + revoke token;
  data stays on its cells, stated in the UI) and drop-and-retire (also
  drain + remove the tenant's cells and delete their data dirs), token
  rotation endpoint, per-tenant admission overrides persisted in the
  manifest; (b) autoscaler live controls (min/max/cooldown/tick, pause /
  resume, force-tick) through the supervisor command channel, persisted;
  (c) cell introspection panel per cell (collections, listen addrs,
  connected peers, sync_status, tx stats, storage, watchdog counters) via
  GET /admin/cells/{id}/inspect; (d) mesh control: dial a peer multiaddr
  from a chosen cell in the UI; (e) metric graphs from the SSE tick ring
  (120 ticks client-side): per-cell req/s and latency area charts, cluster
  multi-line qps, storage growth; continuous per-cell status strips;
  gradient gauges for storage-vs-budget, pending-DAG backlog, admission
  utilization; (f) justfile `up` (run the binary via its banner flow) and
  `token` (print the admin token) recipes.
- Dataviz method applied (skill-validated, not eyeballed): categorical
  order fixed at #81b81a, #533afd, #e8350a, #8087ff: passes lightness,
  chroma, CVD separation (worst adjacent deutan ΔE 30.6), and
  normal-vision checks on the light surface; the contrast WARN on green
  and soft violet is discharged by mandatory direct labels and table
  views. More than 4 series folds into "Other". Sequential = violet
  light->dark. Status colors reserved (good #81b81a, warning #ffcf5e,
  serious #ff6118, critical #e8350a) and always paired with icon + label.
  Gradients (sunburst and violet) are permitted ONLY on single-measure
  gauges and decorative accents, never to distinguish series. One axis
  per chart, crosshair+tooltip hover on all time charts, legends for >=2
  series, texture available for CVD relief.
- Reversible: yes (additive endpoints + UI).

## D22 (2026-08-19): dashboard ships the real afterburner theme with
   embedded fonts (supersedes D17b's system-font compromise)

- Decided (operator: "it must use afterburner theme"): the D17b trade-off
  (offline over typography) was the wrong resolution; the operator wants
  both. Sora, Inter, and JetBrains Mono ship INSIDE the binary as
  latin-subset variable woff2 files (113 KB total, OFL 1.1, license note
  alongside), served from /dashboard/assets/fonts/ with @font-face weight
  ranges: zero external requests, full type system. The Phase 7 round also
  runs a fidelity pass so the dashboard matches the site's actual idiom
  (glass nav, gradient wordmark, kickers, pills, midnight code surfaces,
  button metrics), not merely its variables.
- Reversible: yes.

## D21 (2026-08-19): Phase 7: one-command console UX (operator directive)

- Decided (operator: the binary must work immediately, opening the admin
  console to spawn nodes): bare `defraburner` defaults to bringing the
  cluster up (recover if data exists, else provision 1 cell) under
  ~/.local/share/defraburner (DEFRABURNER_DATA / --data-root override),
  prints a banner with the authenticated dashboard URL, and best-effort
  xdg-opens it (--no-open to suppress). The dashboard gains admin actions:
  spawn cell, drain free cell, create tenant (token shown once), with the
  admin token bootstrapped via a one-time ?token= URL parameter that the
  shell strips from history after storing. New admin endpoints POST
  /admin/cells and DELETE /admin/cells/{id} run through a supervisor
  command channel processed in the start select! loop because ignition is
  not Send (D12): axum handlers enqueue and await a reply, never ignite
  in-handler. `spike` is hidden from help (kept for tests). README
  quickstart leads with the one-command flow.
- Why: the shipped binary's front door was developer plumbing; a DBaaS
  single binary must land the operator in a working console.
- Reversible: yes (all additive).

## D20 (2026-08-19): Phase 6 verified; upstream listener defect discovered,
   reproduced, and documented

Phase 6 complete: readiness now reports only confirmed peer connections
(deadline-polled into connected_peers before the ready-file, honest
confirmed:false on timeout); mem_budget_bytes genuinely drives lark/redb
cache sizing (unit-tested derivations; ATTR measurement shows most of
lark's ~260 MiB delta is fixed engine overhead, not cache: the knob is
real, the big win needs upstream lark tuning); dist profile ships at
91.8 MiB (fat LTO, stripped, panic=unwind for supervision resilience;
34% under the 139.7 MiB thin-LTO build); perf harness landed with
numbers: SNAP_MS 0.366 for a 64-cell snapshot, GCRA 368 ns/check,
gateway-admitted p50 1.53-1.56 ms at 1 and 3 cells (err count in loadgen
output is dominated by intended 429 admission rejects at the default
200 rps ceiling); README rewritten; sweep clean; final gate 159/160.

The 160th: two_process_mesh went 0/10, and the investigation found a REAL
pre-existing upstream defect, not a test race: with N>1 in-process cells,
every TCP listener after the first is announced at the libp2p API level
(listen_addresses non-empty, "listening on <addr>" logged) but is dead at
the OS level shortly after ignition (~500 ms per the coder's port-reuse
unregister trace). Independently reproduced by the loop: a single
`defraburner start --cells 2` process logs both listeners; ss -tln shows
only the first port bound. Consequences, stated precisely: in-process
tenant groups are unaffected (wiring completes inside the window and
established connections survive); outbound dialing from any cell works;
inbound dials to cells 2..N from other processes/hosts fail after the
window, which is exactly what the two-process test exercises. The fix
belongs in defradb.rs's p2p host / libp2p-tcp listener lifetime, which
this project is forbidden to modify (operator rule): a full repro document
ships at docs/upstream/defradb-rs-second-listener-dies.md for the operator
to take upstream. The test is left in place and failing-with-cause
documented, not weakened, not deleted: it is the regression detector for
the upstream fix.

Also measured but flagged non-representative: recovery timings this round
(13.3 s / 10.5 s) ran on a heavily contended host straight after a 20 min
fat-LTO build; the Phase 1 quiet-host figure (250 ms for 2 cells) stands
as the representative number until a quiet-host re-measurement.

## D19 (2026-08-19): Phases 4+5 verified; findings

Round complete: D17c rollback fix tested; AOT pipeline live (packages
recipe, build.rs embeds, register_precompiled early-verification PASSED:
the compiled autoscale-default produces identical outcomes to Phase 0's
source registration); autoscaler proven up AND down under real load with
manifest removal and decision-log evidence; malformed policy never wedges
or scales the cluster; corrupt wasm override fails start loudly; dashboard
(shell + overview + SSE) green against the real binary. 134 unit + 10/11
integration tests; final gates green.

- Honest deviations accepted: cells carry a derived tick-to-tick qps (the
  shipped policy reads cell.qps; the plan's field list lacked it); the
  dashboard labels its latency tile "mean ms" not "p50 ms" (gateway tracks
  count/sum/max; calling a mean a p50 would fabricate precision); additive
  admin-status fields for the dashboard views; SSE reads via
  fetch+ReadableStream so the admin token travels as a header, never a URL.
- kovan-map path swap done and tree-verified; a second registry copy of the
  same version remains via afterburner-node-compat's own dependency (cannot
  be removed without touching afterburner; expected).
- Known failing item carried into Phase 6 (not hidden): the Phase 2 test
  two_process_mesh::cross_process_mesh_dials_static_peers races under
  extreme host load (100+ loadavg on this shared 36-core box): root cause
  is production-adjacent: connect_peer Ok does not imply connected_peers
  reflects the connection, and start writes the ready-file from a single
  unpolled snapshot. Phase 6 fixes readiness itself (deadline-poll dialed
  peers into connected_peers before the ready-file), making the test
  deterministic rather than papering over it.

## D18 (2026-08-19): kovan consumed as a sibling path dep

- Decided (operator: "kovan is under projects/kovan"): kovan-map switches
  from the crates.io release to the local checkout
  (/home/vcq/projects/kovan/kovan-map, 0.1.19, currently version-identical),
  matching the sibling-path-dep pattern used for defradb.rs and afterburner.
  kovan stays unmodified like the other upstream repos. Executed inside the
  Phase 4+5 round to avoid parallel edits to the workspace.
- Reversible: yes (flip back to the registry version).

## D17 (2026-08-19, Phase 4+5 design): AOT loading mechanics, offline
   dashboard fonts, and the admin-tenants rollback fix

- Decided: (a) the justfile `packages` recipe runs burn compile per package
  and extracts precompiled/wasm32-wasip1/main.wasm into
  packages/<name>/.build/; build.rs embeds those bytes into the binary for
  default packages, failing with an actionable "run just packages" message
  when absent; at runtime --packages-dir loads .afb archives directly
  (tar + zstd direct deps: both already compiled in the wider graph, and
  the .afb IS the shipped artifact per D9). (b) The dashboard imports no
  remote fonts (the afterburner website uses Google Fonts; an embedded
  operational dashboard must render offline): Sora/Inter/JetBrains Mono
  font stacks with system fallbacks, all other design tokens copied from
  website/design-system.css. (c) The Phase 3 known gap (POST /admin/tenants
  leaves a Pending tenant + unreturned token when reconcile fails, no
  manifest rollback) is fixed in the Phase 4+5 round, not deferred.
- Affects: burner-policy, burner-dashboard, justfile, build.rs, gateway.
- Reversible: yes.

## D16 (2026-08-19): Phases 2+3 verified; findings

Combined round complete; coder gate green (73 unit + 8 integration tests)
and loop re-verification green. Findings worth the trail:

- Go interop smoke PASSED live: Go DefraDB (develop, built with the
  vendored go1.25.12 toolchain from defradb.rs/.tooling) served schema and
  a document over its real HTTP API; a defraburner cell connect_peer'd,
  subscribed, and sync_documents'd the doc across the actual libp2p wire.
  The honesty-table caveat on Go interop is now closed as verified.
- The gateway achieved the FULL upstream router (defra-node's reference
  wiring reproduced from embedded::EmbeddedNode's public fields); routes
  whose components embedded does not assemble 503 via upstream's own
  require_X guards. GraphQL lives at /api/v0/graphql and /api/v1/graphql
  (the bare /graphql path in the plan text did not exist upstream).
- D13's fix needed a second iteration: TopicPeerEvent is edge-triggered and
  unreplayed, so subscribe-then-check still raced the swarm task 10-50%.
  The deterministic fix relies on join! poll ordering (wait futures listed
  before add_collections futures, so every subscription's synchronous
  prefix runs before any trigger is polled). Empirical: 50/50 spike, 20/20
  tenants runs.
- A false-green was caught and killed: a gateway bind failure was logged
  and the process exited 0, indistinguishable from a clean shutdown (one
  test passed by accident because the ready-file predated gateway startup).
  Startup errors now propagate and exit 1 loudly.
- Two more real fixes: start now decides recover-vs-fresh on
  manifest.cells.len() (tenant create legitimately writes a cell-less
  manifest first), and the gateway strips the tenant bearer token before
  proxying (upstream's identity auth 403s on non-JWT bearers).
- Measured: gateway overhead ~1.0 ms p50 (direct 0.878 ms vs via-gateway
  1.874 ms, 50 sequential queries, stable across 5 runs).
- Known gap carried to the next round (D17c): admin tenant-create rollback.

## D15 (2026-08-19): remaining phases dispatched combined, gates preserved

- Decided (operator directive "carry on fast code everything"): remaining
  work runs as combined coder rounds: Phase 2+3 (mesh/tenants + gateway),
  then Phase 4+5 (policy/autoscaler + dashboard), then Phase 6. Each phase's
  gate condition is still run and reported inside the round, and the loop
  re-verifies the full gate between rounds. Nothing about scope or the
  honesty fence changes; only dispatch granularity does.
- Why: speed at the operator's explicit push; sequential rounds over a
  shared workspace avoid merge races that parallel coders would create.
- Reversible: yes (fall back to per-phase dispatch on the first bad round).

## D14 (2026-08-19, Phase 2 design): disjoint tenant placement, declarative
   provisioning, manifest extension

- Decided: (a) v1 placement is disjoint: a cell serves exactly one tenant's
  group; tenants get R fresh-or-free cells. Shared-cell density (multiple
  tenants per cell) is deferred: it requires ACP wiring plus a collection
  namespacing design (two tenants with the same collection name on one cell
  collide in one GraphQL namespace). (b) Tenant provisioning is declarative
  in Phase 2: `tenant create/list` edit and validate the manifest offline;
  `start` reconciles (places, applies schema, wires replication, reports
  Ready). Live provisioning arrives with the gateway's admin surface in
  Phase 3. (c) ClusterManifest gains a `tenants` field with serde default
  (version stays 1; nothing released yet). Tenant SDL is stored under
  data_root/tenants/<name>.graphql so late-joining cells can be schema'd.
- Options considered: shared cells with name-mangling (rewrites SDL and
  queries: heavy, semantics-touching); a live admin socket in Phase 2
  (duplicates Phase 3's gateway); separate tenants.json (second atomic
  writer for no gain).
- Affects: burner-mesh, burner-cell manifest, CLI, Phase 3 routing.
- Reversible: (a) is a placement-policy widening later; (b)(c) additive.

## D13 (2026-08-19): spike gossipsub flake gets a deterministic fix in
   Phase 2's shared wiring

- Observed: one gate run failed tests/spike.rs with a libp2p gossipsub
  InsufficientPeers publish error (write raced topic-mesh formation after
  add_collections); 3/3 green in isolation and clean on bracketing full
  gates. A flaky test is a failing test.
- Decided: Phase 2's group-wiring module ships a topic-mesh-ready wait
  built on the per-node event bus (EventName::TopicPeerEvent JOINED),
  used by tenant wiring before declaring Ready, and by the spike before
  its post-subscription write. No sleep-based papering over.
- Affects: burner-mesh wiring, spike.rs.

## D12 (2026-08-19, standing constraint): cell ignition futures are not Send

- Verified by the compiler in Phase 1: embedded::build_with_store's future
  is not Send whenever libp2p is configured (a Box<dyn FnOnce> field without
  +Send held across an await in setup_libp2p). Standing rule for every later
  phase: never tokio::spawn a path that reaches cell::ignite; drive ignition
  on the current task (futures buffer_unordered for concurrency) and
  structure long-lived loops (watchdog, autoscaler actions) as select!-driven
  async fns, not spawned tasks, wherever they re-ignite cells.
- Affects: burner-mesh reconcile, autoscaler action executor, gateway
  startup ordering.

## D11 (2026-08-19): Phase 0's RSS surprise resolved: lark defaults are the
   consumer; budget work targets LarkStoreOptions

- Measured (attribution test): memory+no-p2p cell ~29 MiB; lark+no-p2p
  ~135 MiB; lark+libp2p ~162 MiB. The lark backend's defaults (512 MiB
  block cache, 64 MiB write buffer machinery) dominate; libp2p adds
  ~28 MiB.
- Decided: per-cell memory budgeting (ledger phase) tunes
  storage::LarkStoreOptions (open_with_options; ~25 LARK_* env vars exist
  upstream) with mem_budget_bytes driving cache sizing. Marked in code as a
  vertexia: comment in burner-cell/src/cell.rs.
- Affects: Phase 6 budgets, CellSpec.mem_budget_bytes semantics.

## D10 (2026-08-19, V4 resolved): no signing-registry re-register defense

- Verified: zero runtime reads of defra_core::signing's identity registry on
  embedded-node paths (grep across db, query, p2p, db-merge, p2p-adapter,
  storage: no hits; registry reads exist only in cli/defra-node/ffi/http/
  sourcehub layers we do not use, and in the RegisteredIdentity build-time
  branch we never take; the doc-mutation path reads a different thread-local
  populated only by http/ffi). ShutdownHandle's clear_identity_store() wipe
  is real but nothing on our path reads the registry afterward.
- Decided: drain() stays plain node.shutdown(); no re-registration code.
  Citation trail lives as a doc comment in burner-cell/src/supervisor.rs.
- Reversible: yes (add the defense if a future phase adopts a registry
  reader).

## D9 (2026-08-19): deliverables are two build outputs; policies ship as
   burn-compile AOT wasm (supersedes D1's packing deferral)

- Decided (operator directive): the build produces exactly (1) AOT-compiled
  wasm policy packages under packages/ and (2) the defraburner release
  binary. Policy shipping is the `burn compile` .afb: verified locally that
  it bundles a self-contained precompiled/wasm32-wasip1/main.wasm (javy
  8.1.1, build-time only; sealed packages only, which all policies are).
  The release binary embeds default package wasm via include_bytes and
  registers with register_precompiled(bytes, "wasm32-wasip1"); zero JS
  compilation at startup. packages/ on disk overrides with precompiled
  .afbs; raw-source registration stays a dev-mode path only. Built .afb
  files are build outputs (gitignored), regenerated by the justfile package
  recipe which runs before the release build.
- Also verified: a plain `burn package` (no compile) bundles source only,
  which is why AOT requires the burn compile step; and register_precompiled
  is unsupported on afterburner's threaded engine, so the policy host stays
  on the plain engine (it already does).
- Why: single-binary operational story with deterministic, sealed,
  ahead-of-time-compiled policy brains; no runtime toolchain dependencies.
- Affects: burner-policy loading (Phase 4), justfile, .gitignore, build
  prerequisites (burn + javy join rustc + libclang).
- Reversible: yes.

## D8 (2026-08-18, Phase 0 verified): v1 backend set is EmbeddedStore's
   (lark default, redb, memory dev-only, at-rest encryption knob)

- Decided: burner-cell v1 selects backends through upstream's public
  EmbeddedStore enum (Memory | Lark | Redb | Encrypted wrapper). rocksdb and
  fjall stay deferred knobs (build_with_store accepts any Store, so adding
  them later is additive plumbing, not a redesign).
- Options: generic-over-every-backend now; EmbeddedStore now, rest later.
- Why: the ladder. EmbeddedStore exists, covers the operator's chosen default
  (lark) plus the two useful alternates, and carries at-rest encryption for
  free. Speculative generality over rocksdb/fjall serves no v1 feature.
- Affects: burner-cell store construction, CellSpec.backend vocabulary.
- Reversible: yes (additive).

## D7 (2026-08-18): Phase 0 verified; three decision corrections and one
   measured surprise

Phase 0 gate and spike verified green by the loop's own re-run (fmt, clippy
-D warnings, doc -D warnings, integration test pass in 2.27s). Corrections
against earlier entries, from source-cited findings:

- D3 correction: SigningConfig::Enabled { key: Some(..) } DOES write the
  process-global signing registry (node_identity.rs:49-63 calls
  store_identity unconditionally for every Enabled cell). The hazard is
  therefore broader than D3 stated: any cell shutdown wipes every cell's
  registry entry (clear_identity_store is store-wide). Phase 1 verifies
  whether anything reads the registry after build time; the re-registration
  defense is implemented only if a runtime read path exists (no speculative
  code).
- D4 resolved better than its fallback: the libp2p keypair is persisted by
  upstream in the cell's own peerstore (key "__local_p2p_identity__",
  node_p2p.rs:77-99), so reopening the same data dir reproduces the same
  peer ID with no config knob and no upstream change. Phase 1's golden test
  converts this source reading into verified behavior.
- D6 correction: the wasm-only feature selection does NOT drop the libclang
  build dependency; rquickjs (bindgen) is unconditional via
  afterburner-node-compat (afterburner/Cargo.toml:26, afterburner-wasi's
  unconditional node-compat dep). The decision stands on its real rationale
  (determinism and the seal); libclang is recorded as a build prerequisite.
- Measured surprise: per-cell RSS delta at ignition is ~279 MiB (cell A) /
  ~243 MiB (cell B) on lark + libp2p; binary 146.5 MB release (thin LTO,
  two wasmtime majors). Phase 1 includes an attribution measurement (memory
  vs lark vs no-p2p cells) so the Phase 6 budget targets the real consumer.
  Also measured: ignite ~70 ms/cell, explicit sync 109 ms, subscription
  sync 808 ms, spike wall 2.25 s.
- Two empirical API facts now encoded in code comments where they bite:
  listen_addresses() returns bare transport multiaddrs so the dialable
  address is assembled as <addr>/p2p/<peer_id>; afterburner's sync engine
  API must run under tokio::task::block_in_place on a multi-thread runtime.

## D6 (2026-08-18, Phase 0): policies execute in the WASM sandbox only

- Decided: defraburner embeds afterburner with default-features = false,
  features = ["wasm"]; policy packages always run in the Wasmtime sandbox,
  never the native QuickJS engine.
- Options: (a) default features (adaptive native-then-wasm), (b) wasm-only.
- Why: determinism and the seal are the point of policy brains; wasm-only also
  drops the rquickjs/libclang build dependency and shrinks the binary. The
  adaptive tier exists for latency-sensitive hot paths; a 5s control tick is
  not one.
- Affects: burner-policy, build requirements, Phase 0 binary-size measurement.
- Reversible: yes (feature flip).

## D5 (2026-08-18, Phase 0): spike proves both replication paths

- Decided: the Phase 0 spike asserts convergence via the pubsub subscription
  path (add_collections on both cells + connect, then a post-subscription
  write converges) and via the explicit path (sync_documents for a
  pre-subscription doc), mirroring upstream's own two mechanisms.
- Options: subscription only; sync only; both.
- Why: Phase 2 group replication rides subscriptions and recovery re-wiring
  rides explicit sync; both must be proven on libp2p in-process before Phase 1
  builds on them.
- Affects: spike scope, Phase 2 design confidence.
- Reversible: n/a (test scope).

## D4 (2026-08-18, Phase 0): libp2p peer-identity persistence is an open
   verification item, with a no-upstream-change fallback

- Decided: embedded's Libp2pConfig exposes only listen_addr (verified read,
  lib.rs:46-49); whether a stable libp2p keypair is reachable through public
  APIs is verified during the spike. If it is not: v1 documents libp2p peer
  IDs as ephemeral across restart, recovery re-dials static addresses and
  re-wires subscriptions/replicators; iroh offers stable identity via
  secret_key_path (upstream feature) for clusters that need it.
- Options: (a) upstream PR adding key persistence (rejected: operator ruled
  both upstream repos untouchable this run), (b) ephemeral IDs + re-wiring,
  (c) iroh for stable identity.
- Why: honesty over convenience; the fallback is operationally sound because
  group membership is defraburner's manifest, not the peer ID.
- Affects: burner-cell identity handling, Phase 1 recovery golden test
  assertions (data + addresses stable; peer-ID stability per verified outcome).
- Reversible: yes (upstream adds the knob later, we adopt it).

## D3 (2026-08-18, Phase 0): cells never use the process-global signing
   identity registry

- Decided: cell signing uses SigningConfig::Enabled with explicit per-cell key
  material (persisted under the cell dir by burner-cell), never
  SigningConfig::RegisteredIdentity.
- Options: registry DIDs per cell; explicit keys per cell; signing disabled.
- Why: verified in source (embedded node.rs:471): ShutdownHandle::shutdown()
  calls defra_core::signing::clear_identity_store(), a process-wide wipe. With
  N cells in one process, draining one cell must not clear the others'
  identities. Explicit key material sidesteps the registry entirely (spike
  verifies create_node_identity takes the explicit-key path).
- Affects: burner-cell identity module, Phase 1.
- Reversible: yes.

## D2 (2026-08-18, Phase 0): lark default via build_with_store, not the
   convenience NodeBuilder

- Decided: cells are built with embedded::build_with_store(Arc<LarkStore>,
  EmbeddedNodeConfig) (public API). The convenience NodeBuilder is not used
  for persistent cells.
- Options: NodeBuilder with feature gymnastics; build_with_store.
- Why: verified in source (embedded node.rs:338-368): NodeBuilder.data_path
  opens redb whenever the redb feature is enabled and lark only when redb is
  compiled out; the operator chose lark as default while keeping redb (and
  rocksdb/fjall through the storage crate) as knobs, which requires selecting
  the store at runtime: exactly what build_with_store exists for.
- Affects: burner-cell store construction, storage knob plumbing.
- Reversible: yes.

## D1 (2026-08-18, Phase 0): policy packages are burn package directories;
   .afb binary packing deferred

- Decided: packages/ holds real burn package directories (afb.toml,
  manifold.json, source/main.js), scaffolded with the installed burn
  toolchain; the embedded engine registers the entry source via the library
  API. Packing/distributing .afb archives is deferred until distribution
  needs it.
- Options: (a) full .afb archive loading in v1, (b) package dirs + library
  registration, (c) bare .js files.
- Why: (b) is the least code that works while keeping the real package
  format, manifest, and `burn test`-ability; (c) would drop the manifest and
  capability declaration; (a) adds archive plumbing no v1 feature needs.
- Affects: burner-policy loading, packages/ layout.
- Reversible: yes ((a) is additive later).
