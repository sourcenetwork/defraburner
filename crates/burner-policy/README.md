# burner-policy

The control loop: assembles a snapshot of the cluster, calls sealed wasm
policy packages against it each tick, and clamps and executes whatever
they propose.

## Mechanics

Once per tick, `snapshot::MetricsSnapshot::build` gathers the live
manifest, the supervisor's running cells (with a live `sync_status` query
per cell), and gateway-owned request/admission counters the caller
(`defraburner`'s `start.rs`) already gathered that tick - this crate does
not depend on `burner-gateway`, so those arrive as plain data. Cells are
ranked by request count and capped at `MAX_CELLS_IN_SNAPSHOT` (64); the cap
is reported *inside* the snapshot itself, never silently applied.
`snapshot::placement_input` builds the smaller placement-policy input the
same way, from the manifest alone.

The policy call (`PolicyEngine::run`) is JSON in, JSON out, against a
package registered from AOT-precompiled wasm only, never raw JS at
runtime, under `tokio::task::block_in_place` (afterburner's engine API is
synchronous and itself blocks on wasmtime-wasi's async plumbing, which
panics if called directly from a tokio task). The raw output is parsed
into `AutoscaleDecision`/`PlacementDecision` (`decision.rs`); any shape
mismatch is a `PolicyError`, exactly like the engine call itself failing.

The clamp module (`clamp.rs`) is the actual trust boundary: **a policy
proposes, this module decides.** Rules: `target_cells` clamps into
`[min_cells, max_cells]`; at most one action executes per tick (enforced
by `AutoscaleDecision` carrying a single `action` field, not a runtime
truncation); a cooldown-window action is held, not executed; `scale_down`
only ever picks a currently-free cell, newest first; a placement must name
cells still free at clamp time, exactly the tenant's required replica
count, with no repeated cell within or across placements in the same plan.
A rule failure drops the action and records why in `clamps_applied`,
never a partial execution.

Every tick's call (input hash, raw decision, clamped actions, clamps
applied, whether it executed, any error) is appended to an on-disk,
rotating decision log (`decisions.jsonl`, rotated to `.1` at 8 MiB, at most
two files kept) that the dashboard renders as a timeline.

A broken or nonsense policy never wedges or silently freezes the cluster:
the last-known-good plan holds for that step (no action taken), the
failure is logged loudly and counted in `PolicyStatusHandle`
(`consecutive_errors`, `last_error`), and that status is what `/admin/status`
and the dashboard's policy-health indicator read - an honest `None` for
"no tick has succeeded yet," never a fabricated zero.

The afterburner engine handle is provided by the binary, not built here:
`PolicyEngine::load` takes an already-constructed `Arc<Afterburner>`, so
this crate only registers packages against it and runs them: it never
decides the engine's own fuel/memory/timeout ceilings, keeping engine
lifecycle in exactly one place in the whole workspace.

## Layout

- `engine.rs`: `PolicyEngine` - registers the two embedded defaults plus
  any `--packages-dir` overrides, runs by name, reports registered
  packages with content hashes.
- `snapshot.rs`: `MetricsSnapshot::build`/`assemble`, `placement_input`,
  bounded storage-size walks, `/proc` host metrics.
- `decision.rs`: strict parsing of a policy's raw JSON output.
- `clamp.rs`: `clamp_autoscale`/`clamp_placement` - the trust boundary.
- `autoscaler.rs`: `AutoscalerConfig`/`AutoscalerControl` (live overrides,
  pause, force-tick), `PolicyStatusHandle`, the tick loop (`run`), and
  execution (`execute_plan`, `execute_scale_up`, `next_cell_index`).
- `log.rs`: the append-only, rotating decision log and its bounded `tail`.
- `build.rs`: fails the build with an actionable message (not an opaque
  `include_bytes!` error) if `just packages` hasn't run yet.

## Gotchas / invariants

- **Never `tokio::spawn` the tick loop (`autoscaler::run`).** A scale-up
  call chain reaches `execute_scale_up` -> `Supervisor::provision` ->
  `cell::ignite`, not `Send` whenever libp2p is configured, so the whole
  loop runs directly on `defraburner`'s own `select!` instead.
- `execute_scale_up` locks the supervisor **before** scanning for the next
  free cell index, not after: admin-triggered provisioning made this
  function genuinely multi-caller, and locking after the scan would let
  two concurrent callers read the same index and collide on one cell id.
- `next_cell_index` scans `data_root/cells/` for the highest `cell-<N>`
  ever used, live or drained: it depends on `burner-cell` never deleting a
  drained cell's directory (unless retirement asks for it), so an id is
  never recycled onto another cell's old data.
- `CellSnapshot::qps` is a derived, tick-to-tick rate, not a field the
  original plan enumerated; the shipped `autoscale-default` policy reads
  exactly this field, so renaming or dropping it would silently blind the
  default policy with no compiler error to catch the mistake.

## Related

Drives `burner-cell`'s `Supervisor`; calls `burner-mesh::reconcile` after
executing a placement plan; `burner-gateway` reads this crate's
`PolicyStatusHandle` and `AutoscalerControl`; `defraburner` builds and owns
the shared engine and hands it in. The shipped policies live at
`packages/autoscale-default` and `packages/placement-default`. The
reasoning behind the rules above is recorded in
`docs/decs/defraburner_DECS.md`.
