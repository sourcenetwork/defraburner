# defraburner/autoscale-default

The default autoscaling policy: a pure function from a cluster metrics
snapshot to a proposed scale action.

## What a policy is, here

A policy package is `MetricsSnapshot -> AutoscaleDecision`, nothing else.
It runs sealed in the wasm sandbox with no filesystem, network, or clock
access, and no way to observe or influence anything outside its own input
and return value. `manifold.json` is the literal capability grant the host
registers this package under:

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

Every field is off. This package proposes; it never acts, and it is never
trusted at face value - see Clamp contract below.

## Input shape

The host (`burner-policy::snapshot::MetricsSnapshot`) builds and passes the
full cluster snapshot each tick; this policy reads only `cells[].qps` and
`limits.min_cells`/`limits.max_cells`, but receives the whole shape:

```json
{
  "schema_version": 1,
  "tick": 42,
  "cap": { "cells_included": 3, "cells_total": 3 },
  "host": { "mem_total_kb": 32000000, "mem_avail_kb": 16000000, "load1": 0.8 },
  "cells": [
    {
      "id": "cell-0",
      "group": "default",
      "tenant": "acme-co",
      "running": true,
      "marker_ok": true,
      "requests": { "count": 15234, "sum_ms": 4200.5, "max_ms": 88.2 },
      "qps": 128.4,
      "storage_bytes": 10485760,
      "sync_status": { "synced": true }
    }
  ],
  "tenants": [
    { "name": "acme-co", "replicas": 1, "cells": ["cell-0"], "status": "placed",
      "admission": { "allowed": 15200, "rejected": 34 } }
  ],
  "limits": { "min_cells": 1, "max_cells": 8, "max_actions_per_tick": 1, "cooldown_secs": 60 },
  "last_action": { "tick": 40, "action": "scale_up" }
}
```

`cap.cells_included`/`cap.cells_total` tell you honestly when the fleet
exceeds the snapshot's 64-cell cap (see the `burner-policy` README);
`last_action` is `null` until something has actually executed.

## Output shape

```json
{ "action": "scale_up", "target_cells": 4, "reason": "avg qps 128.4 exceeds scale_up threshold 100" }
```

`action` is one of `scale_up` / `scale_down` / `hold`; `target_cells` is
the whole-numbered cell count the policy is asking for (a whole-valued
float like `4.0` is accepted, a fraction or negative number is not);
`reason` is free text, surfaced verbatim in the decision log and the
dashboard's timeline.

## Thresholds

`source/main.js` computes the average `qps` across every cell in the
snapshot and compares it against two constants at the top of the file:

- `SCALE_UP_QPS_THRESHOLD = 100`: propose `scale_up` (current cell count
  + 1) when the average exceeds this and the fleet is below `max_cells`.
- `SCALE_DOWN_QPS_THRESHOLD = 10`: propose `scale_down` (current cell
  count - 1) when the average is below this and the fleet is above
  `min_cells`.
- Otherwise: `hold` at the current count.

To change the thresholds, edit the two constants and rebuild (below); there
is no runtime knob for them.

## Lifecycle

`source/main.js` is JS source. `burn compile` ahead-of-time compiles it
(via Javy) into a self-contained `wasm32-wasip1` module and bundles it into
this directory's `.afb` archive alongside `precompiled/wasm32-wasip1/main.wasm`.
`just packages` extracts that module to `.build/main.wasm`, which
`crates/burner-policy/build.rs` embeds into the `defraburner` binary via
`include_bytes!` and registers with `register_precompiled` at startup:
no JavaScript is ever compiled when the binary runs. The registered module
is called once per autoscaler tick (`burner_policy::autoscaler::run`).

## The clamp contract

Whatever this package proposes, the host (`burner_policy::clamp::clamp_autoscale`)
still: clamps `target_cells` into the configured `[min_cells, max_cells]`;
allows at most one action per tick; holds any action inside the configured
cooldown window regardless of what was proposed; and, for `scale_down`,
picks the actual cell to remove itself (the newest currently-free one):
if no cell is free, the action holds even though this policy proposed it.
A policy cannot force the cluster past a guardrail; it can only ask.

## Overriding at runtime

Pass `--packages-dir <dir>` to `defraburner start`/`up` with a subdirectory
named `autoscale-default` containing a `.afb` built by `burn compile`; it
replaces the embedded default for that name. An ambiguous (more than one
`.afb`) or corrupt override directory fails startup loudly rather than
silently falling back to the default.

## If this policy errors or returns nonsense

The host never trusts a policy's raw output further than it can validate.
An engine error, a wasm trap, or an output that doesn't parse as an
`AutoscaleDecision` all fail the same way: the tick's decision-log entry
records the failure, `PolicyStatusHandle`'s error counter increments
(surfaced in `/admin/status` and the dashboard), and the cluster's last
known-good plan holds - no cell is added or removed that tick. See
`packages/testonly-malformed` for the fixture that proves this path.

## Related

`burner-policy` (the host that calls this package and owns the clamp);
`packages/placement-default` (the sibling policy for tenant placement).
