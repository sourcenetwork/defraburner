# Gateway consistency semantics

Plain-language statement of what the Phase 3 gateway (`burner-gateway`)
actually guarantees, so nothing here is read as stronger than it is.

## Replication: eventual, not strong

A tenant's collections replicate across its group of `replicas` cells via
DefraDB's own CRDT merge (gossipsub subscription within the group,
`burner_mesh::wiring::wire_group`). This is **eventual consistency**: a
write accepted on one cell in the group will, absent a partition, arrive
on every other cell in the group, but there is no bound on when, and no
cross-cell transaction spanning the group. Two clients reading two
different cells in the same group at the same moment can observe
different, both-valid states until convergence catches up. Nothing in
this gateway upgrades that to a stronger guarantee.

## Sticky routing: an optimization, not a guarantee

The gateway picks a cell for each request via
`index = hash(bearer_token) % group_size` (`burner_gateway::routing`), so
repeat requests from the same client tend to land on the same cell while
it stays up. This gives **session read-your-writes as an optimization**:
if every request from a session hits the same cell, that cell's own
writes are visible to that session immediately, without waiting on
cross-cell replication.

It is not a guarantee, for two reasons:

- **Failover breaks it.** If the sticky cell is not currently running,
  the gateway fails over to the next cell in the group
  (`RoutingTable::route`). A session that fails over can read a version
  of its own prior write that has not replicated to the new cell yet,
  until eventual convergence (above) catches up. This is a real,
  observable window, not a corner case to hand-wave away.
- **Placement can change.** A future re-placement of a tenant's group
  (not present in v1, but the routing table is rebuilt on every
  `reconcile`) can move which cell a token's hash lands on.

Read-after-write within a single request/response pair on one cell is
always consistent (it is one CRDT store answering its own query). What is
not guaranteed is read-after-write *across* two separate requests once a
failover or re-placement has happened in between.

## Admission: per-tenant, not global

Each tenant has its own GCRA (Generic Cell Rate Algorithm) token bucket
(`burner_gateway::admission`), independent of every other tenant's. A
noisy or overloaded tenant cannot starve another tenant's admission
budget: a reject on tenant A carries no information about tenant B's
current capacity, and does not consume it. Admission is enforced at the
gateway, before a request reaches any cell; a rejected request never
reaches DefraDB's own CRDT merge path at all, so it has no consistency
implications one way or the other.

## What this document does not claim

- Not "strong consistency", not "linearizable", not "read-your-writes"
  unqualified: every one of those words is qualified above with exactly
  the condition under which it holds.
- Not a promise that sticky routing survives a cell restart, a
  reconcile-driven re-placement, or a client that changes its bearer
  token mid-session (a token rotation, `tenant rotate-token`, changes
  which cell a client's requests hash to).
- Not a claim about cross-tenant isolation of *data* (D14's disjoint
  placement already guarantees that structurally: a cell serves at most
  one tenant); this document is only about consistency *within* one
  tenant's own group.
