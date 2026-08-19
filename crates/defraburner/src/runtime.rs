//! The shared afterburner engine (console round, operator directive):
//! engine construction and lifecycle live here, in the binary, not inside
//! `burner-policy`. `burner-policy::PolicyEngine` takes the built
//! `Arc<Afterburner>` as a constructor parameter and stays purely the
//! registration/execution site for policy packages; it no longer decides
//! *how* the engine itself is built or governs its resource ceilings.
//!
//! One engine, built once, for the whole process's lifetime (D6/D9: wasm
//! sandbox only, `Manifold::sealed()`: no fs/net/crypto/child-process/env
//! access for any policy package). Its resource knobs (`fuel`,
//! `memory_bytes`, `timeout_ms`) are CLI flags on `up`/`start` so an
//! operator can tighten or loosen the sandbox without a code change;
//! `None` (the default for all three) keeps afterburner's own defaults.

use std::sync::Arc;

use afterburner::{Afterburner, Manifold, Mode};
use anyhow::{Result, anyhow};

/// CLI-plumbed resource ceilings for the shared afterburner engine. Every
/// field `None` keeps afterburner's own built-in default for that knob
/// (currently unlimited for all three).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RuntimeLimits {
    pub fuel: Option<u64>,
    pub memory_bytes: Option<usize>,
    pub timeout_ms: Option<u64>,
}

/// Builds the one afterburner engine this process uses for every policy
/// package call: `Mode::Wasm` (D6, never native), `Manifold::sealed()`
/// (no host capability grants at all: policy packages are pure
/// `MetricsSnapshot -> Decision` functions, they need none), `limits`
/// layered on top of afterburner's own defaults.
///
/// Wrapped in `block_in_place` for the same reason every other call into
/// afterburner is (see `burner_policy::engine`'s module doc comment):
/// afterburner's builder synchronously constructs the wasm backend, which
/// itself blocks on wasmtime-wasi's async plumbing and panics ("cannot
/// start a runtime from within a runtime") if called directly from an
/// already-running tokio task.
pub fn build_engine(limits: RuntimeLimits) -> Result<Arc<Afterburner>> {
    tokio::task::block_in_place(|| {
        let mut builder = Afterburner::builder()
            .mode(Mode::Wasm)
            .manifold(Manifold::sealed());
        if let Some(fuel) = limits.fuel {
            builder = builder.fuel(fuel);
        }
        if let Some(memory_bytes) = limits.memory_bytes {
            builder = builder.memory_bytes(memory_bytes);
        }
        if let Some(timeout_ms) = limits.timeout_ms {
            builder = builder.timeout_ms(timeout_ms);
        }
        builder
            .build()
            .map(Arc::new)
            .map_err(|error| anyhow!("building the shared afterburner engine: {error}"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The CLI's `--policy-fuel`/`--policy-memory-bytes`/`--policy-timeout-ms`
    /// flags plumb straight into `AfterburnerBuilder`'s own knobs: proven
    /// by actually building an engine with all three set and confirming
    /// it runs (a wrong builder call would fail construction, not merely
    /// look wrong on paper).
    #[tokio::test(flavor = "multi_thread")]
    async fn runtime_limits_plumb_into_the_builder() {
        let engine = build_engine(RuntimeLimits {
            fuel: Some(50_000_000),
            memory_bytes: Some(64 * 1024 * 1024),
            timeout_ms: Some(5_000),
        })
        .expect("building an engine with explicit limits should succeed");

        let id = tokio::task::block_in_place(|| engine.register("module.exports = (d) => d.n + 1"))
            .expect("registering a trivial script should succeed under the configured limits");
        let output = tokio::task::block_in_place(|| engine.run(&id, &serde_json::json!({"n": 41})))
            .expect("running within the configured fuel/memory/timeout ceilings should succeed");
        assert_eq!(output, serde_json::json!(42));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn default_limits_build_a_working_engine() {
        let engine = build_engine(RuntimeLimits::default()).expect("default limits should build");
        let id = tokio::task::block_in_place(|| engine.register("module.exports = (d) => d * 2"))
            .unwrap();
        let output =
            tokio::task::block_in_place(|| engine.run(&id, &serde_json::json!(21))).unwrap();
        assert_eq!(output, serde_json::json!(42));
    }
}
