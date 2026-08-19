# defraburner/testonly-malformed

A test fixture, not a product package. It exists to prove the host's
broken-policy safety net against a real registered wasm module, not a
mocked failure.

`source/main.js` deliberately answers every call with a shape neither
`AutoscaleDecision` nor `PlacementDecision` can parse:

```js
module.exports = function (_input) {
  return { nonsense: 1 };
};
```

That is the whole point: `{"nonsense": 1}` has no `action`, no
`target_cells`, no `placements` - nothing either real decision type's
strict parser accepts. Calling it always produces the same `PolicyError`
a corrupt engine call or a genuinely broken policy would.

It is built and packaged exactly like the real policies (`afb.toml`,
`manifold.json` with the same sealed, all-off capability grant, `burn
compile` to an `.afb`), but it is **never embedded as a default**:
`crates/burner-policy/build.rs` only ever embeds `autoscale-default` and
`placement-default` - and it is **never a shipped product artifact**. The
only way it is ever loaded is by explicitly pointing `--packages-dir` at a
directory containing it, which is exactly what the test below does.

## What uses it

`crates/defraburner/tests/policy_safety.rs` lays this package's real,
`burn compile`d `.afb` out under a `--packages-dir` override directory
named `autoscale-default` (and separately, `placement-default`), starts a
real cluster against it, and asserts that a tick calling it: logs the
failure, increments `PolicyStatusHandle`'s error counter, and leaves the
cluster's cell count and tenant placements exactly where they were:
never a wedge, never a silent scale or placement change, never a crash.

## Related

`burner-policy` (the host whose safety net this proves); `packages/autoscale-default`
and `packages/placement-default` (the real policies it stands in for).
