# burner-gateway

The single listener that fronts every cell: tenant routing, admission,
and the whole admin/console control surface.

## Mechanics

One axum listener (default `127.0.0.1:9181`) builds one HTTP router per
cell from `embedded::EmbeddedNode`'s public parts, mirroring the wiring
`defra-node` uses for its own embedded server: full parity with upstream's
HTTP surface (GraphQL at `/api/v0` and `/api/v1`, health, `/p2p/*`,
transactions), not a reduced fallback. Route families the embedded node
never assembles (`rest`, `manage`, `acp`, ...) still mount, they just 503
at request time via upstream's own guards. Routers are built once per cell
and cached, never rebuilt per request.

Request flow: bearer token -> sha256 digest lookup -> tenant -> per-tenant
GCRA admission check (429 with `Retry-After` on reject) -> a sticky pick
within the tenant's cell group by `hash(token) % group_size`, with failover
to the next cell if the sticky pick isn't currently running -> proxied
into that cell's router. The tenant's own `Authorization` header is
stripped before proxying: forwarded verbatim, it hits the cell's unrelated,
cell-local JWT identity system and gets a 403, since a non-JWT bearer is an
invalid identity there, not an unauthenticated request.

Admission is a lock-free per-tenant GCRA (Generic Cell Rate Algorithm)
token bucket over `kovan_map::HopscotchMap`, one atomic bucket per tenant.
It's reimplemented here, not depended on, even though the shape mirrors
afterburner's own `thrust` admission pattern: `thrust` is BSL-licensed and
crate-private, so the pattern is borrowed, not the crate.

Every mutating admin handler (`admin_cells`, `admin_tenants`,
`admin_autoscaler`) shares one path: build a `SupervisorCommand`, send it
down a channel, await a reply with a 30-second timeout that degrades to a
503 on any failure mode (executor gone, reply dropped, timeout). Handlers
never touch the supervisor, manifest, or autoscaler control directly:
see the `defraburner` README for why. The dashboard's live feed is a
bounded-per-client SSE hub (`tokio::sync::broadcast` under the hood, 8
simultaneous clients, a 64-entry ring): a client that falls behind gets an
explicit `dropped` event carrying the exact count it missed, never a silent
gap, and a full hub returns 503 rather than accepting an unbounded number
of streams.

Tokens (admin and tenant alike) are stored only as a sha256 hex digest; the
raw token is shown exactly once, at issue time. The final digest
comparison uses a vetted constant-time compare (`subtle`) on top of an
ordinary hash-map lookup for the initial resolve - closing the narrow
window where a routing-table rebuild races a token rotation, not a general
side-channel defense on the lookup itself.

Consistency is stated, not oversold: replication across a group is
eventual (DefraDB's own CRDT merge), and sticky routing is a read-your-writes
*optimization*, not a guarantee - it breaks on failover and on
re-placement. See `docs/consistency.md` for the full statement.

## Layout

- `gateway.rs`: `GatewayState`, `build`/`serve`, the tenant request
  pipeline, `/admin/status`, `send_supervisor_command`, per-cell/per-tenant
  latency metrics.
- `routing.rs`: `RoutingTable` (token/tenant/cell-group/router caches),
  sticky pick, and `rebuild` - a genuine rebuild each reconcile, so a
  dropped tenant's token stops resolving immediately, not just once its
  entry is overwritten by something newer.
- `admission.rs`: the GCRA bucket and per-tenant overrides.
- `auth.rs`: token issue, digest, constant-time compare.
- `router_build.rs`: `build_cell_router`, upstream's server wiring
  reproduced from `EmbeddedNode`'s public fields.
- `sse.rs`: `SseHub`, the bounded broadcast-based event stream.
- `admin_cells.rs` / `admin_tenants.rs` / `admin_autoscaler.rs`: the admin
  control surface - provision/drain/inspect/dial cells; create, drop,
  drop-and-retire, rotate-token, and admission-override tenants; live
  min/max/cooldown/tick-interval, pause, and force-tick for the autoscaler.

## Gotchas / invariants

- **Every mutating admin handler goes through the command channel, never
  a direct call.** Axum runs handlers on spawned per-connection tasks, but
  `cell::ignite`'s returned future is not `Send` whenever libp2p is
  configured, so a handler that could reach ignition (provisioning a cell,
  say) can't call into that path itself: it enqueues a command instead.
- The token-digest map lookup itself is an ordinary hash lookup, not
  constant-time; only the final confirmation against the resolved tenant's
  live digest is.
- The dashboard's latency figure is a mean (count/sum/max are what's
  tracked); it is never labeled a percentile it doesn't have.

## Related

Routes to `burner-cell`'s cells; calls `burner-mesh::reconcile` directly
from live tenant creation; reads `burner-policy`'s `PolicyStatusHandle` and
`AutoscalerControl`; mounts `burner-dashboard`'s static router. See
`docs/consistency.md` for the consistency semantics; the reasoning behind
the rules above is recorded in `docs/decs/defraburner_DECS.md`.
