// defraburner/testonly-malformed: an Afterburner package.
//
// Test fixture only (crates/defraburner/tests/policy_safety.rs): a
// syntactically valid, sealed policy package that answers every call with
// a shape neither AutoscaleDecision nor PlacementDecision can parse. Used
// to prove burner-policy's broken-policy safety net (last-known-good plan
// held, the failure surfaced loudly, the cluster never wedged) against a
// real registered wasm module, not a mocked failure. Never embedded as a
// default; only ever loaded via an explicit --packages-dir override.
"use strict";

module.exports = function (_input) {
  return { nonsense: 1 };
};
