# defraburner

A single Rust binary that runs a cluster of embedded
[defradb.rs](https://github.com/sourcenetwork/defradb.rs) nodes ("cells")
meshed over libp2p inside one process, places and replicates tenants
across cell groups, fronts every cell through one gateway with per-tenant
admission, autoscales the fleet using sandboxed wasm policy packages, and
serves an embedded realtime console for the whole cluster. The build
produces exactly two artifacts: the `defraburner` release binary (console
and default policies compiled in) and the AOT-compiled policy packages
under `packages/*/`.

![The defraburner console: live cluster stats, the traffic generator, and
the cluster replication map with one node group per tenant](art/dashboard-overview.png)

The replication map above is drawn from real `connected_peers` data, never
an idealized full mesh: a solid line is a confirmed live replication link,
a dashed one is a link that should exist and provably does not, and a
dotted one is a link that has never been positively confirmed either way.

## Quickstart

```bash
just start
```

That's the front door: it builds (thin-LTO, fast to rebuild, fast enough
to actually run) and runs `defraburner up`, which recovers the cluster
already at the default data root or, on a fresh clone, provisions a single
cell, then prints a banner and best-effort opens the console in a browser
with the admin token already in the URL:

```text
defraburner up
  data:      /home/you/.local/share/defraburner
  gateway:   http://127.0.0.1:9181
  dashboard: http://127.0.0.1:9181/dashboard?token=<admin-token>
  cells:     1 running
```

The manual equivalents: `defraburner up` (the same interactive flow,
banner and browser included) or `defraburner start` (the scripted form:
explicit `--data-root`/`--cells`/`--base-port`, no banner, no browser:
what a CI job or a supervised service should run). Either way, the admin
token lives at `<data-root>/admin.token` (`just token` prints it for the
default data root), and the gateway listens on `127.0.0.1:9181` by
default.

Everything past this point - creating a tenant, running a query, watching
the cluster - is normally done from the console in a browser (see
"What the console gives you" below); the same operations are also a plain
HTTP surface, which is what makes them scriptable:

```bash
# The scripted equivalent of the console's "create tenant" action.
curl -s -X POST http://127.0.0.1:9181/admin/tenants \
  -H "Authorization: Bearer $(just token)" \
  -H "Content-Type: application/json" \
  -d '{"name": "acme", "schema_sdl": "type Greeting { message: String }", "replicas": 1}'
# {"name":"acme","token":"<TENANT_TOKEN>"}

# Query through the gateway with the tenant's own token, same path any client uses.
curl -s -X POST http://127.0.0.1:9181/api/v1/graphql \
  -H "Authorization: Bearer <TENANT_TOKEN>" \
  -H "Content-Type: application/json" \
  -d '{"query": "mutation { add_Greeting(input: {message: \"hello\"}) { _docID message } }"}'
```

The dashboard at `http://127.0.0.1:9181/dashboard` is entirely embedded in
the binary - HTML, CSS, JS, fonts, icons - so it renders with zero
external requests, offline included.

## Prerequisites

Run `just setup`. It installs the build tooling this repo needs into
`$HOME/.local/bin` without root, verifies what is already there, and
reports anything it cannot install itself with the exact command to fix
it. It is idempotent, so running it again costs nothing.

```sh
just setup
```

What it installs: the `burn` CLI (via `https://afterburner.sh`) and `javy`
8.1.1, pinned and checked against its published SHA-256 before use. Both
are build-time only, used by `just packages` to compile the policy
packages ahead of time to wasm; the shipped binary never invokes either.
`burn compile` requires `javy` on `PATH` and does not fetch it itself,
which is why setup does.

What it checks and reports rather than installs, since these need a
package manager: rustc 1.91 or newer (the workspace's pinned
`rust-version`), `libclang` (required transitively and not optional:
`afterburner`'s QuickJS bindgen needs it even for this workspace's
wasm-only build), plus `zstd` and `tar`.

One thing setup cannot do for you: a sibling checkout at `../defradb.rs`,
consumed as an unmodified path dependency. It is not published to
crates.io (verified 404), so there is no registry version to depend on
instead. `afterburner` and `kovan-map` *are* published, and are consumed
from crates.io at their released versions.

## What the console gives you

The console is a full control surface: everything below is doable from
the browser, no CLI or curl required.

- **Cluster**: spawn and drain cells, inspect any cell (its collections,
  listen addresses, connected peers, sync status, transaction stats,
  storage, watchdog counters), dial a peer multiaddr from a chosen cell,
  and watch live per-cell and cluster-wide metrics.
- **Tenants**: create a tenant with its schema, rotate its API token, set
  a per-tenant admission override, drop a tenant, or drop and retire it
  (also draining and erasing its cells).
- **Databases**: every cell owns one persistent wasm DefraDB (the whole
  engine compiled to `wasm32-wasip1`, loaded from the AOT-compiled
  package). Apply schema to a cell's database and run queries or
  mutations against it. Cells and their databases share a lifetime, so
  they are ignited and drained in the Cluster view.
- **Autoscaler**: change min/max cells, cooldown, and tick interval,
  pause or resume it, force an extra tick, and read the decision timeline.
- **Data**: pick a tenant, add a collection to it while it is serving
  (the SDL is applied on every cell in its group, wired for replication,
  and folded into its stored schema so a restart recovers it), browse its
  collections and documents, bulk-seed generated documents, and create,
  edit, and delete documents through the gateway exactly as an API client
  would (document filtering here is a simple field-equality match, not the
  full GraphQL filter grammar), plus a raw GraphQL tab with a
  copy-as-curl button for anything more expressive or scripted.

Every data operation goes through the same gateway path as a scripted
client: per-tenant admission applies to it too, and a rate-limited request
shows its real `Retry-After`. Because tokens are stored only as hashes,
the console can never display an existing tenant's token back to you:
only mint a fresh one via rotate, which the Data view offers in place the
moment you pick a tenant it has no token for. The whole console updates live: a
one-second SSE tick plus event-driven pushes for cell and decision
changes, with bounded client-side history and a visible, auto-reconnecting
connection state.

## How it works

- **`crates/burner-cell`** ([README](crates/burner-cell/README.md)): one
  governed DefraDB cell - spec, ignition, identity, the cluster manifest,
  crash recovery, and the wasm database each cell owns.
- **`crates/burner-fiber`** ([README](crates/burner-fiber/README.md)):
  loads the AOT-compiled `packages/defradb` `.afb` and runs one persistent
  wasm DefraDB per cell.
- **`crates/burner-mesh`** ([README](crates/burner-mesh/README.md)):
  tenant placement and replication wiring across cells.
- **`crates/burner-gateway`** ([README](crates/burner-gateway/README.md)):
  the single listener - tenant routing, admission, and the admin/console
  API.
- **`crates/burner-policy`** ([README](crates/burner-policy/README.md)):
  the control loop - snapshot, call the policy, clamp, execute, log.
- **`crates/burner-dashboard`** ([README](crates/burner-dashboard/README.md)):
  the embedded console's assets, mounted by the gateway.
- **`crates/defraburner`** ([README](crates/defraburner/README.md)): the
  binary - composition root, CLI, the never-spawned control loops.

Autoscaling and placement are ordinary JS (`packages/*/source/main.js`),
ahead-of-time compiled by `burn compile` into a self-contained
`wasm32-wasip1` module bundled in a `.afb` archive, embedded into the
binary at build time and registered as a precompiled module - no
JavaScript is ever compiled while the binary runs. Each policy is called
once per control tick as a pure JSON-in/JSON-out function; the host's
clamp module then decides, independent of what was proposed, what is
actually authorized to happen to the cluster. See
`packages/autoscale-default/README.md` for the full input/output shapes
and the clamp contract.

## Knobs (`defraburner start`)

| Flag | Default | Meaning |
|---|---|---|
| `--data-root` | `./data` | Root directory for cluster and cell data; created if missing. |
| `--cells` | `2` | Cells to provision on a *fresh* cluster only; an existing cluster recovers exactly what its manifest records. |
| `--bind` | `127.0.0.1` | Bind address for freshly-provisioned cells' libp2p transport. |
| `--base-port` | `9171` | First libp2p port for freshly-provisioned cells; cell `N` binds `base_port + N`. |
| `--peers` | (none) | Comma-separated static cross-host peer multiaddrs to dial at startup; each needs a `/p2p/<peer-id>` suffix. |
| `--gateway-addr` | `127.0.0.1:9181` | Listen address for the gateway (tenant routing, admission, `/admin/*`, the dashboard). |
| `--ready-file` | (none) | If given, a JSON cluster-status snapshot is atomically written here once every cell is up. |
| `--min-cells` | `1` | Floor the autoscaler will never shrink the fleet below. |
| `--max-cells` | `8` | Ceiling the autoscaler will never grow the fleet past. |
| `--cooldown-secs` | `60` | Minimum seconds between two autoscaler-executed scale actions. |
| `--tick-interval` | `5` | Seconds between autoscaler control-loop ticks. |
| `--packages-dir` | (none) | Directory of policy package overrides; each subdirectory with a `*.afb` overrides the embedded default of the same name. |
| `--policy-fuel` | (none, unlimited) | Instruction-fuel ceiling per policy call. |
| `--policy-memory-bytes` | (none, unlimited) | Linear memory ceiling for the shared policy engine. |
| `--policy-timeout-ms` | (none, unlimited) | Wall-clock timeout ceiling per policy call. |

`defraburner up` shares every knob above except three: `--data-root`
defaults to `$DEFRABURNER_DATA`, then `~/.local/share/defraburner`, not
`./data`; a fresh cluster always provisions exactly one cell on an
auto-selected free port (no `--cells`/`--base-port`); and it takes
`--no-open` to suppress the best-effort browser launch.

## Status and honesty

Core cluster operation (cells, mesh, gateway, autoscaling) is done and
gate-verified; the console's backend (every admin endpoint above, the
shared engine, the command channel) is complete too, and its DNA-themed UI
is still landing: see the status table in
[`docs/plans/defraburner.md`](docs/plans/defraburner.md) for exactly
which workstream that leaves open. That same document carries every
measured number this project claims (dist binary size, ignition and
recovery timings, gateway overhead, snapshot and admission-check cost, and
more): nothing here restates a figure that isn't there, and nothing is
quoted as "fast" without a number attached. Go interop is verified, not
assumed: a live Go DefraDB node served schema and a document over its real
HTTP API, and a defraburner cell connected, subscribed, and synced across
the actual libp2p wire.

One upstream defect is known, independently discovered and reproduced by
this project: with more than one embedded node in one process, every
libp2p TCP listener after the first goes dead at the OS level shortly
after ignition, even though the API keeps advertising it. In-process
tenant wiring is unaffected (it completes inside the window); a *late*
inbound dial from another process or host to cell 2 and beyond is not.
Cross-host meshes should dial out from later-ignited cells toward earlier
ones until this is fixed upstream. Full repro and detail:
[`docs/upstream/defradb-rs-second-listener-dies.md`](docs/upstream/defradb-rs-second-listener-dies.md).
As of 2026-09-01 the suite is 251/251 green, including that detector.
That is a change from the 223/224 this README carried through August, and
the reason is not yet fully established, so it is stated rather than
claimed: the detector failed repeatedly earlier the same day and now
passes on three consecutive runs. Every observed failure named ports that
leaked `defraburner` processes from *previous runs of the same test* were
still holding, so the new process connected to a stale peer instead of
its intended one. Whether upstream's second-listener defect is also fixed
by the 274 commits pulled in with the regolith upgrade has not been
confirmed on a clean host; treat the detector as informative, not as
proof the upstream bug is gone.

Named deferred work - WASM-hosted cells, document-hash sharding,
per-cell process isolation, automated live tenant migration, cross-host
placement orchestration, sandboxed UDFs near data, per-tenant
bring-your-own policy packages, and exposing upstream's experimental
Postgres wire - is tracked with its reason in the deferred-work table at
the bottom of [`docs/plans/defraburner.md`](docs/plans/defraburner.md#status).
This project does not claim "production ready"; it claims exactly what is
measured and verified above, and says "not measured yet" or "deferred"
for everything else.

## Development

- `just gate`: format check, `clippy -D warnings`, doc build with
  `-D warnings`, and the full test suite (depends on `just packages` first,
  since `burner-policy`'s build script embeds the compiled policy wasm at
  compile time).
- `just perf`: builds the real `dist` profile binary and runs the load
  generator against 1-cell and 3-cell clusters, then re-runs the golden
  recovery test, printing every timing it measures.
- [`docs/decs/defraburner_DECS.md`](docs/decs/defraburner_DECS.md): the
  decision log - every binding decision made along the way, newest
  first, with its reasoning and what it affects. Read it before assuming
  a design choice was arbitrary.
- The three sibling upstream checkouts (`../defradb.rs`, `../afterburner`,
  `../kovan`) are never modified by this project. A defect found in one of
  them (like the listener issue above) is documented and reported
  upstream, not patched locally.

## Further reading

- [`docs/plans/defraburner.md`](docs/plans/defraburner.md): the full
  design, phased plan, measured numbers, and status table.
- [`docs/decs/defraburner_DECS.md`](docs/decs/defraburner_DECS.md): every
  binding decision made along the way, newest first.
- [`docs/consistency.md`](docs/consistency.md): what the gateway actually
  guarantees (replication, sticky routing, admission), stated plainly.
