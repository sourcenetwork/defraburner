# defraburner/placement-default

The default tenant placement policy: a pure function from pending tenants
and free cells to a proposed set of placements.

## What a policy is, here

Like `autoscale-default`, this package is sealed: no filesystem, network,
or clock access, nothing observable outside its input and return value.
`manifold.json` carries the same all-off grant:

```json
{
  "fs": "None",
  "net": "None",
  "crypto": false,
  "child_process": false,
  "env": "None",
  "allow_exit": false,
  "http_timeout_ms": null,
  "listen": "None"
}
```

It proposes placements; the host decides which ones actually happen - see
Clamp contract below.

## Input shape

Built by `burner_policy::snapshot::placement_input` from the cluster
manifest alone (no live cell queries): every genuinely unplaced tenant,
every cell no tenant currently claims, and how many tenants each cell
currently serves.

```json
{
  "pending_tenants": [{ "name": "acme-co", "replicas": 2 }],
  "free_cells": ["cell-1", "cell-2", "cell-3"],
  "assigned_counts": { "cell-0": 1, "cell-1": 0, "cell-2": 0, "cell-3": 0 }
}
```

`assigned_counts` covers every cell, free or not; v1 placement is disjoint,
so today every value is 0 or 1, but it is a real count (not a boolean) so a
later shared-cell phase can widen its meaning without changing this shape.

## Output shape

```json
{
  "placements": [{ "tenant": "acme-co", "cells": ["cell-1", "cell-2"] }],
  "reason": "placed 1/1 tenant(s)"
}
```

`reason` is a human-readable summary of what happened for every tenant,
not just the successes - see below.

## Selection rule

`source/main.js` ranks `free_cells` once, least-assigned-first by
`assigned_counts`, then walks `pending_tenants` in order: each tenant draws
`replicas` cells off the front of that same shrinking ranked pool. Because
every tenant in one call draws from the same pool as it shrinks, two
tenants placed in the same tick can never be handed the same cell.

A tenant that cannot fit - not enough free cells left in the pool, or an
invalid `replicas` - is skipped, not partially placed: it is left out of
`placements` entirely and named in `reason` (e.g. `"needs 2, only 1 free
cell(s) left"`), so a caller reading only `placements` never has to guess
whether a tenant was silently dropped.

## Lifecycle

Identical to `autoscale-default`'s: `source/main.js` -> `burn compile` ->
an `.afb` bundling `precompiled/wasm32-wasip1/main.wasm` -> `just packages`
extracts it to `.build/main.wasm` -> `crates/burner-policy/build.rs`
embeds it into the `defraburner` binary and registers it with
`register_precompiled` at startup. Called once per tick, but only when the
manifest has at least one genuinely pending tenant (`burner_policy::autoscaler::run`
skips the call, and logs nothing, on a tick with nothing to place).

## The clamp contract

The host (`burner_policy::clamp::clamp_placement`) re-validates every
proposed placement against the live cluster at the moment of clamping, not
against this policy's possibly-stale view of it: each placement must name
cells the host still considers free, exactly the tenant's required
replica count, with no cell repeated within the placement or reused by
another placement in the same plan. A placement that fails any of these is
dropped - the reason recorded in the clamp log - never partially
executed; the disjointness invariant (one tenant per cell) is enforced by
the host regardless of what this policy proposes.

## Overriding at runtime

Pass `--packages-dir <dir>` with a subdirectory named `placement-default`
containing a `burn compile`d `.afb`; it replaces the embedded default.

## If this policy errors or returns nonsense

Same safety net as `autoscale-default`: an engine error or an output that
doesn't parse as a `PlacementDecision` is logged, counted in
`PolicyStatusHandle`, and changes nothing - every still-pending tenant
stays pending until a later tick succeeds. See `packages/testonly-malformed`
for the fixture that proves this path against a real registered module.

## Related

`burner-policy` (the host); `packages/autoscale-default` (the sibling
autoscaling policy).
