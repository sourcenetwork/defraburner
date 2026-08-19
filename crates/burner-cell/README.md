# burner-cell

One governed DefraDB cell: its spec, lifecycle, identity, and the cluster
manifest that makes a fleet of them recoverable after a crash.

## Mechanics

A cell is one embedded `defradb.rs` node (`embedded::EmbeddedNode`) with its
own data directory, its own fixed libp2p port, its own Ed25519 signing
identity, and a memory budget. `CellSpec` is the declarative shape (id,
group, `BackendKind`, port, bind address, `mem_budget_bytes`, signing key
path); `cell::ignite` turns one into a live `RunningCell` by opening the
chosen backend (`BackendKind::Lark | Redb | Memory`, matching upstream's
public `EmbeddedStore` enum) and calling `embedded::build_with_store` so
the backend is selected at runtime. Ports are fixed, not ephemeral: a cell
always binds `spec.p2p_port`, so its dialable address is stable across
restarts, and its peer id comes stable for free too - upstream persists
the libp2p keypair in the cell's own peerstore, so reopening the same data
directory reproduces the same peer id with no extra bookkeeping here.

Identity is a raw 32-byte Ed25519 seed on disk (0600), expanded to the
64-byte `(seed || public key)` form `embedded::SigningKey::Ed25519`
requires. Cells never use the process-global signing identity registry
(explicit per-cell keys instead): that registry is wiped wholesale by
*any* cell's shutdown, which would otherwise take every other in-process
cell's identity down with it.

The cluster manifest (`<data_root>/cluster.json`) is the durable record of
every cell and tenant, written atomically (temp file, `fsync`, rename,
`fsync` the directory) and validated on every load and save: unique cell
ids and ports, no `Memory`-backend cell ever persisted (it can't survive a
restart, so it's rejected rather than lying about recovery), and tenant
placement disjointness. A `BurnerMarker` document, written once at
provision and read back at recovery and by the watchdog, is the actual
proof that a cell's data survived a restart, including a SIGKILL.

`Supervisor` owns every live cell. `recover` re-ignites every cell the
manifest records with bounded concurrency; `Watchdog::run` probes each
cell's marker on an interval and, after three consecutive failures, drains
and re-ignites it. Every mutating admin action is a `SupervisorCommand`
sent down a bounded channel (`command.rs`) rather than called directly,
because the executor that carries these out has to run somewhere that can
reach ignition (see Gotchas).

`mem_budget_bytes` genuinely drives storage cache sizing (Lark's block
cache and write buffer, Redb's cache size, all floored and capped), but it
does not yet enforce a hard per-cell memory ceiling; that is a later
`MemoryLedger` accounting phase, marked `vertexia:` in `cell.rs`.

## Layout

- `spec.rs`: `CellSpec`, `BackendKind`, `TenantSpec`/`TenantStatus`/
  `AdmissionOverride`, tenant-name validation.
- `identity.rs`: seed generation, persistence (0600), and expansion to the
  64-byte signing key form.
- `cell.rs`: `ignite`, per-backend store opening with budget-derived cache
  sizing, and the admin-inspect helpers (`cell_collections`,
  `cell_transaction_stats`).
- `manifest.rs`: `ClusterManifest`, `AutoscalerSpec`, atomic save/load,
  structural validation.
- `supervisor.rs`: `Supervisor` (provision, drain, `remove_cell`, recover,
  reignite, shutdown), the `BurnerMarker` write/verify pair.
- `watchdog.rs`: `CellHealth`'s pure failure-counting state machine and
  `Watchdog::run`'s probe loop.
- `command.rs`: `SupervisorCommand` and every admin outcome/error shape.

## Gotchas / invariants

- **Never `tokio::spawn` anything that reaches `cell::ignite`.** Upstream's
  node builder returns a future that is not `Send` whenever libp2p is
  configured, so ignition must run on the task that owns it (the binary's
  main `select!` loop), never a spawned one. Concurrency still happens:
  `recover` ignites multiple cells at once via `buffer_unordered` on that
  same task, which is why every mutating admin action goes through the
  command channel instead of igniting inline in a spawned handler.
- A drained cell's data directory is deliberately left on disk unless a
  tenant retirement explicitly asks for deletion, because a future
  scale-up always picks a never-before-used cell id: reusing an id whose
  old directory still has files would silently resume that cell's stale
  data instead of starting empty.
- The Memory backend is dev-only and cannot appear in a saved manifest,
  because its data does not survive a restart: persisting it would let a
  cluster manifest promise a recovery it can't actually deliver.
- `ignite` only supports IPv4 bind addresses today and rejects anything
  else immediately, before any I/O, so a misconfigured IPv6 address fails
  with a clear message instead of surfacing later as a confusing libp2p
  parse error deep inside the swarm.
- Known upstream defect (not ours, but visible here): with more than one
  in-process cell, every libp2p listener after the first goes dead at the
  OS level shortly after ignition, though the API keeps advertising it.
  In-process wiring is unaffected; a late inbound dial from another
  process to cell 2+ is not. See
  `docs/upstream/defradb-rs-second-listener-dies.md`.

## Related

`burner-mesh` places tenants and wires replication on top of these cells;
`burner-gateway` builds one HTTP router per cell and routes to them;
`burner-policy` drives `Supervisor::provision`/`remove_cell` from the
autoscaler; `defraburner` is the composition root that owns the
`Supervisor` and runs the never-spawned loops. See
`docs/plans/defraburner.md` for the design and phased plan; the reasoning
behind every rule above is recorded in `docs/decs/defraburner_DECS.md`.
