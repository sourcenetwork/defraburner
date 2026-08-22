# burner-mesh

Tenant placement, replication wiring, and reconciliation across a fleet of
`burner-cell` cells.

## Mechanics

Placement (`placement::place`) picks a tenant's replica group from cells the
manifest doesn't already assign to anyone: v1 placement is disjoint (one
tenant per cell), least-assigned first. In practice that degrades to
manifest order today, since disjoint placement means every free cell ties
at zero assignments; it stays a ranking, not a bare filter, so a later
shared-cell phase can widen it without changing callers. An already-placed
tenant gets its existing assignment back unchanged, not a second pick.

Group wiring (`wiring::wire_group`) connects every cell in a group to every
other cell, then subscribes their shared collections. The `connect_peer`
calls are sequenced, but the `add_collections` calls and the topic-ready
waits are **joined**, not sequenced. That's deliberate: upstream's
`TopicPeerEvent` is edge-triggered and never replayed to a late subscriber,
so a wait that subscribes only after every `add_collections` had already
returned lost the race to the background gossipsub delivery roughly
10-50% of the time when this was tried the naive way. `try_join_all` polls
every stored future once, in listing order, on its very first poll, so
listing every `wait_topic_peer` future ahead of every `add_collections`
future guarantees each wait's entirely synchronous prefix (event-bus
subscribe, topic lookup, drain of anything already buffered) runs to
completion before any `add_collections` future is polled at all - before
its network effects, and the events they can trigger, can even begin.

`reconcile` is the per-tenant convergence pass `defraburner start` runs
once, after cells are up: a `Pending` tenant is placed, schema'd
(`add_schema` from its stored SDL), wired, then flipped to `Placed` and
saved - one tenant at a time, so a crash mid-pass leaves already-placed
tenants durably placed. A `Placed` tenant has its assigned cells verified as
running and idempotently re-wired (upstream's own `connect_peer` and
`subscribe_collection` are no-ops on a repeat, verified in source, not
assumed).

Static cross-host peers are dialed best-effort per `(cell, peer)` pair
(`static_peers::dial_static_peers`); a bad address never aborts the rest of
startup. `confirm_dialed_peers` then deadline-polls each successful dial
into the dialing cell's own `connected_peers()`, so a caller writing a
ready-file or a status snapshot afterward reflects a settled connection,
not a dial that was merely accepted.

## Layout

- `placement.rs`: `place` - pure selection, no I/O.
- `wiring.rs`: `wire_group` - connect + joined subscribe/wait.
- `topic_ready.rs`: `wait_topic_peer`, the deterministic topic-mesh-ready
  wait `wire_group` depends on before it trusts a subscription is live.
- `reconcile.rs`: `reconcile`, `TenantReady`, `tenant_sdl_path`.
- `grow.rs`: `add_collections` - adds collections to a tenant that is
  already placed and serving, without draining or re-placing it. Applies
  the SDL on every cell in the group, wires the new collections with
  `wire_group` (the topic-join event really does fire for a subscription
  that is new in this process, unlike an already-`Placed` tenant's
  existing ones), then appends to the stored SDL. Cells first, SDL last:
  a stored SDL naming a collection the cells lack would degrade the
  tenant on every later reconcile, while the reverse leftover is inert.
- `static_peers.rs`: `dial_static_peers`, `confirm_dialed_peers`,
  `PeerDialOutcome`.

## Gotchas / invariants

- **Nothing in this crate ever needs to avoid `tokio::spawn`.** Tenants
  are placed onto cells that already exist by the time any function here
  runs, so nothing reaches `cell::ignite` (see `burner-cell`'s README for
  why that path can never be spawned); every function here is a plain
  `async fn`, safe to `.await` directly, with no non-`Send`-future
  constraint to worry about.
- `wire_group`'s idempotency leans on upstream's no-op behavior for a
  repeat `connect_peer`/`subscribe_collection`. A repeat `add_collections`
  against an already-subscribed collection sends no new SUBSCRIBE message,
  so a wait joined against that specific repeat call has nothing to catch.
  Every call site today is still correct: `reconcile` runs at most once per
  process lifetime, so even a `Placed` tenant's re-wire is a genuine first
  subscribe on that process's freshly-ignited cells.
- **Known upstream defect** (not ours, but visible here): with more than
  one in-process cell,
  every libp2p listener after the first is dead at the OS level roughly
  500ms after ignition, though the API keeps advertising it. In-process
  wiring (this crate's own connect+wire pass) completes inside that window
  and survives; a *late* inbound dial from another process to cell 2+ does
  not. Cross-host meshes should dial out from later-ignited cells toward
  earlier ones until upstream fixes the listener lifetime. Full repro:
  `docs/upstream/defradb-rs-second-listener-dies.md`.

## Related

Operates on `burner-cell`'s `Supervisor`/`RunningCell`; called by
`defraburner`'s `start`/`up` flow and by `burner-gateway`'s live tenant
creation; `burner-policy`'s autoscaler calls `reconcile` after executing a
placement plan. See `docs/consistency.md` for what this wiring actually
guarantees; the reasoning behind the rules above is recorded in
`docs/decs/defraburner_DECS.md`.
