# burner-dashboard

The embedded, offline-capable operational console served by the gateway.

## Mechanics

Everything the console needs - HTML, CSS, JS, fonts, icons - is compiled
into the binary (`include_str!`/`include_bytes!`) and served by this
crate's router, which `burner-gateway` mounts into its own. There is no
build step for these assets and no external request the browser ever
makes: the console works fully offline. The shell itself carries no
cluster data and needs no admin-token auth; every data-bearing endpoint it
calls (`/admin/api/overview`, `/admin/api/stream`, and every admin
mutation) is token-gated on the gateway side, not here.

The realtime model is server-pushed, not polled: the gateway's SSE hub
delivers a one-second overview tick plus event-driven pushes (a new
decision-log entry, a cell-lifecycle change) over one stream per client.
The client reads it by hand over `fetch()` + `ReadableStream` rather than
the native `EventSource`, specifically so the admin token can travel as a
real `Authorization` header and never as a URL query string that could end
up in browser history or a server log. A client that falls behind sees an
explicit "dropped N" marker, never a silent gap; the connection pill shows
`connected`/`reconnecting` honestly and reconnects automatically; recent
ticks are held in a bounded client-side ring, never the whole history. The
same honesty rule governs rendering: a metric with no sample yet shows "no
data yet", never a fabricated zero - matching the host's own `None`
(not `0`) for "no tick has succeeded yet."

The console covers the cluster's full lifecycle without leaving the
browser: spawning and draining cells, per-cell inspection and peer dialing
on the Cells side; creating a tenant with its schema, rotating its token,
setting a per-tenant admission override, and dropping (or dropping and
retiring) a tenant on the Tenants side; live min/max cells, cooldown, and
tick interval, pause/resume, and force-tick on the Autoscaler side; and a
data plane where an operator picks a tenant, browses its collections and
documents, and creates, edits, and deletes documents through the gateway
exactly as any API client would (document filtering here is a deliberately
simple field-equality match, not the full GraphQL filter grammar), plus a
raw GraphQL tab with a copy-as-curl button for anything more expressive.
Every data operation goes through the same gateway path as a scripted
client: per-tenant admission applies to it too, and a rate-limited request
surfaces its real `Retry-After`. Because tokens are stored only as hashes,
the console can never display an existing tenant's token back to an
operator - only mint a fresh one via rotate.

The visual design is the DNA theme: a dark, void-paper surface by default,
indigo and orange as the primary/accent pair, a serif display face over a
sans body and a mono for code, cornered edges (no rounded corners
anywhere), and a brand gradient reserved for display text, accent
numerals, and progress fills. `design/dna/` holds the imported design
reference this theme implements verbatim in provenance but not in
delivery: it is read material, never served. Charts follow fixed rules
too: a validated categorical palette in a fixed series order, gradients
only on single-measure gauges (never to distinguish series), and every
status indicator paired with an icon and a label, never color alone.

## Layout

- `src/lib.rs`: `router()` - the shell and one wildcard asset route
  (`/dashboard/assets/{*path}`, matched against a fixed table), mounted by
  `burner_gateway::gateway::build`.
- `assets/dashboard.html`: the served shell (token gate, app shell,
  sidebar, one `<section>` per view).
- `assets/tokens.css`, `app.css`: design tokens and the component library.
- `assets/core.js`: state, pure helpers, theme, token gate, realtime SSE,
  navigation - every other script depends on the `window.Burner` it builds.
- `assets/charts.js`, `main.js`: the shared line-chart/sparkline
  renderers, and the Console section's Data/Raw-GraphQL tab switch.
- `assets/view-overview.js`, `view-cells.js`, `view-tenants.js`,
  `view-autoscaler.js`, `view-mesh.js`, `view-console.js`: one module per
  sidebar view, each self-registering its own `Burner.onOverview`/
  `onDecision`/`onCellChange` handlers and form wiring.
- `assets/fonts/`: embedded woff2 fonts plus their license note.
- `design/dna/`: the imported design reference (app.css, tokens.css,
  dna.css, primitives.jsx) - not served, translation source only.

## Gotchas / invariants

- `design/dna/` is reference material, full stop. Anything the served
  dashboard needs from it is translated into `assets/`, never imported
  live (no CDN, no React/Babel, no remote font request), because the
  console has to render with zero external requests, offline included.
- The design import's mock data file was deliberately never stored here:
  the console renders real cluster data only.

## Related

Mounted by `burner-gateway`, which owns every endpoint this console calls
and the SSE hub that drives it; `burner-policy`'s decision log and policy
status are what the Autoscaler view renders. See `docs/consistency.md`
for the consistency semantics; the reasoning behind the console's design
is recorded in `docs/decs/defraburner_DECS.md`.
