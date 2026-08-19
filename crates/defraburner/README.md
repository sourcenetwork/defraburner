# defraburner

The binary: the composition root that builds every other crate into one
running cluster.

## Mechanics

Running it resolves a data root (`--data-root`, else `$DEFRABURNER_DATA`,
else `~/.local/share/defraburner` for `up`), recovers an existing cluster
or provisions a fresh one, dials and confirms any configured static peers,
reconciles tenants (place, schema, wire), builds and binds the gateway,
starts the watchdog and the autoscale/placement loop, prints a banner and
best-effort opens the console in a browser (`up` only), then serves until
SIGINT/SIGTERM and drains every cell on the way out.

`start.rs`'s `select!` loop races five things: the watchdog, the
autoscale/placement loop, the admin command executor, the shutdown-signal
wait, and the (spawned) gateway server task. The first three are
deliberately never `tokio::spawn`ed, because each can reach `cell::ignite`
(re-ignition, scale-up, admin-triggered provisioning), and that path's
returned future is not `Send` whenever libp2p is configured: a future
that isn't `Send` can't be handed to `tokio::spawn` at all, so anything
that might reach it has to run on the task that already owns it. Only
serving already-bound HTTP connections is safe to spawn.

That constraint is exactly why the admin command channel exists: an axum
handler runs on its own spawned per-connection task, so it can never call
into ignition directly. Instead every mutating admin request (provision a
cell, drop a tenant, change an autoscaler knob) enqueues a
`SupervisorCommand` and awaits a reply; `commands::run`, driven on the same
never-spawned task as the watchdog and the autoscaler, is the single
writer that actually carries each one out - which also means every admin
mutation is naturally serialized against the autoscaler's own actions, for
free, with no extra locking discipline.

The `runtime` module owns the one afterburner engine this process ever
builds: `Mode::Wasm`, `Manifold::sealed()` (no filesystem, network, or
process access for any policy package), with fuel, memory, and timeout
ceilings from `--policy-fuel`/`--policy-memory-bytes`/`--policy-timeout-ms`
(each optional, defaulting to afterburner's own unlimited). `burner-policy`
is handed the built `Arc<Afterburner>` rather than building its own, so
this module is the one place the engine's lifecycle and resource
ceilings live: one engine, one owner, for the whole process.

Bare `defraburner` - or any invocation whose first argument isn't a known
subcommand - is spliced into `defraburner up` before clap ever parses it:
the shipped binary's front door is the console, not a bare CLI surface.

## Layout

- `main.rs`: the clap CLI surface, the bare-invocation-to-`up` splice,
  dispatch.
- `up.rs`: `up`-only decisions (data-root resolution, a free-port scan for
  a fresh single-cell provision, the browser-open decision), kept apart
  from the shared `run()` both `up` and `start` call into.
- `start.rs`: the composition root itself - provision/recover, dial and
  confirm peers, reconcile, build the gateway, the `select!` loop, the
  ready-file, the post-readiness banner.
- `runtime.rs`: `build_engine` - the one afterburner engine construction
  site in the whole workspace.
- `commands.rs`: the admin command executor.
- `tenant.rs`: the offline `tenant create`/`list`/`rotate-token`
  subcommands.
- `tests/`: golden SIGKILL recovery, tenant convergence, gateway,
  autoscaler, policy safety, two-process mesh (kept red on purpose, as the
  regression detector for the upstream libp2p listener defect described in
  the `burner-mesh` README), Go interop, dashboard, and RSS attribution.
- `examples/loadgen.rs`: the load generator `just perf` drives.

## Gotchas / invariants

- `start`'s recover-vs-provision-fresh decision reads the manifest's cell
  *count*, not merely whether a manifest file exists, because `tenant
  create` legitimately writes a cell-less manifest (a tenant with no cells
  yet) before any `start` ever runs, and checking file existence alone would
  wrongly treat that as an existing cluster and skip fresh provisioning.
- A gateway bind failure is awaited directly, before the ready-file is
  written, so it fails `start`/`up` loudly with a real exit code rather
  than looking like a clean shutdown to anything polling for readiness.
- An early de-risk spike (a hidden `spike` subcommand, `spike.rs`,
  `tests/spike.rs`) has been removed now that its proofs are superseded by
  `tests/tenants.rs`, `tests/go_interop.rs`, and the policy engine's own
  tests; its RSS-measurement helper moved into `tests/attribution.rs`
  rather than being deleted, since the measurement itself is still useful.

## Related

Composes every crate under `crates/`: `burner-cell` (the `Supervisor` this
binary owns), `burner-mesh` (reconcile), `burner-gateway` (the listener),
`burner-policy` (the control loop, run against the engine this crate
builds), `burner-dashboard` (mounted by the gateway). See the repository
root README for the full knob table and quickstart; the decision log at
`docs/decs/defraburner_DECS.md` records the reasoning behind the choices
above.
