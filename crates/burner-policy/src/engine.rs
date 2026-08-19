//! `PolicyEngine`: one afterburner engine hosting the cluster's policy
//! packages, registered exclusively from AOT-precompiled wasm (D9/D17a) --
//! never from JS source at runtime. Sources, in registration order (later
//! wins): the two embedded defaults built into this binary via
//! `include_bytes!`, then any `--packages-dir` override directories, each
//! extracted in-process from its `.afb` archive (a zstd-compressed tar; no
//! shell-out) and registered under its own directory name.
//!
//! Every call into afterburner runs through `tokio::task::block_in_place`
//! (a lesson learned early and still load-bearing): afterburner's engine
//! API is synchronous and itself blocks on wasmtime-wasi's async plumbing,
//! which panics ("cannot start a runtime from within a runtime") if
//! called directly from an already-running tokio task.

use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use afterburner::{Afterburner, ScriptId};
use anyhow::{Context, Result, anyhow, bail};

/// Wasm target string afterburner's precompiled loader expects for a
/// sealed, self-contained Javy WASI module (D9): not the
/// dynamically-linked `"wasm32-wasip1-dyn"` variant.
const PRECOMPILED_TARGET: &str = "wasm32-wasip1";

/// Registration name of the default autoscale policy.
pub const AUTOSCALE_DEFAULT_NAME: &str = "autoscale-default";
/// Registration name of the default placement policy.
pub const PLACEMENT_DEFAULT_NAME: &str = "placement-default";

/// Path within a `.afb` archive to its AOT-compiled sealed module.
const PRECOMPILED_ENTRY_PATH: &str = "precompiled/wasm32-wasip1/main.wasm";

// Paths are relative to this file (`src/`), one level deeper than
// `build.rs`'s `CARGO_MANIFEST_DIR`-relative paths; `build.rs` panics with
// an actionable message before either of these could fail to resolve.
const AUTOSCALE_DEFAULT_WASM: &[u8] =
    include_bytes!("../../../packages/autoscale-default/.build/main.wasm");
const PLACEMENT_DEFAULT_WASM: &[u8] =
    include_bytes!("../../../packages/placement-default/.build/main.wasm");

/// One registered package's name and content hash (console round,
/// operator directive: surfaced in `/admin/status`'s `runtime` block so
/// the operator can see exactly which package bytes are live).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct RegisteredPackage {
    pub name: String,
    /// Hex-encoded `ScriptId::hash`: afterburner's own content-addressed
    /// identity for the registered bytes.
    pub content_hash: String,
}

/// The name -> registered-script-id table for every policy package this
/// process knows about, hosted on a shared afterburner engine handle it
/// does not own the lifecycle of (the engine is built once, in
/// `defraburner::runtime`, and handed in here: see that module's doc
/// comment for why engine construction moved out of this crate).
#[derive(Debug)]
pub struct PolicyEngine {
    afterburner: Arc<Afterburner>,
    scripts: HashMap<String, ScriptId>,
}

impl PolicyEngine {
    /// Registers the two embedded default policies on `engine`, then
    /// applies `packages_dir`'s override archives (if given). Fails
    /// loudly on a missing, ambiguous, or corrupt override: a bad
    /// `--packages-dir` is a startup configuration error, never a silent
    /// fallback to the default.
    pub fn load(packages_dir: Option<&Path>, engine: Arc<Afterburner>) -> Result<Self> {
        tokio::task::block_in_place(|| Self::load_blocking(packages_dir, engine))
    }

    fn load_blocking(packages_dir: Option<&Path>, afterburner: Arc<Afterburner>) -> Result<Self> {
        let mut scripts = HashMap::new();
        register(
            &afterburner,
            &mut scripts,
            AUTOSCALE_DEFAULT_NAME,
            AUTOSCALE_DEFAULT_WASM,
        )
        .context("registering embedded autoscale-default policy")?;
        register(
            &afterburner,
            &mut scripts,
            PLACEMENT_DEFAULT_NAME,
            PLACEMENT_DEFAULT_WASM,
        )
        .context("registering embedded placement-default policy")?;

        if let Some(dir) = packages_dir {
            apply_overrides(&afterburner, &mut scripts, dir)
                .with_context(|| format!("applying policy overrides from {}", dir.display()))?;
        }

        Ok(Self {
            afterburner,
            scripts,
        })
    }

    /// Runs the registered policy package `name` against `input`, returning
    /// its raw JSON output. An unknown `name` is a defraburner
    /// configuration bug (never a policy-authoring one, since callers only
    /// ever pass the fixed autoscale/placement names) and fails loudly
    /// rather than silently no-op-ing.
    pub fn run(&self, name: &str, input: &serde_json::Value) -> Result<serde_json::Value> {
        tokio::task::block_in_place(|| {
            let id = self
                .scripts
                .get(name)
                .ok_or_else(|| anyhow!("no policy package registered under '{name}'"))?;
            self.afterburner
                .run(id, input)
                .map_err(|error| anyhow!("running policy package '{name}': {error}"))
        })
    }

    /// True if a package is registered under `name` (default or override).
    pub fn has_package(&self, name: &str) -> bool {
        self.scripts.contains_key(name)
    }

    /// Every currently-registered package, name-sorted, with its content
    /// hash (console round, operator directive): feeds `/admin/status`'s
    /// `runtime` block.
    pub fn registered_packages(&self) -> Vec<RegisteredPackage> {
        let mut packages: Vec<RegisteredPackage> = self
            .scripts
            .iter()
            .map(|(name, id)| RegisteredPackage {
                name: name.clone(),
                content_hash: hex_encode(&id.hash),
            })
            .collect();
        packages.sort_by(|a, b| a.name.cmp(&b.name));
        packages
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn register(
    afterburner: &Afterburner,
    scripts: &mut HashMap<String, ScriptId>,
    name: &str,
    wasm: &[u8],
) -> Result<()> {
    let id = afterburner
        .register_precompiled(wasm, PRECOMPILED_TARGET)
        .map_err(|error| anyhow!("{error}"))?;
    scripts.insert(name.to_string(), id);
    Ok(())
}

/// Scans `packages_dir` for per-package override directories: each direct
/// subdirectory containing exactly one `*.afb` file is extracted and
/// registered under that subdirectory's own name: the override key is
/// the directory name, never the name recorded inside the archive's own
/// `afb.toml`. This is what lets `tests/policy_safety.rs` swap in a
/// differently-named test fixture by laying its `.afb` out under a
/// directory literally named `autoscale-default`.
fn apply_overrides(
    afterburner: &Afterburner,
    scripts: &mut HashMap<String, ScriptId>,
    packages_dir: &Path,
) -> Result<()> {
    let entries = fs::read_dir(packages_dir)
        .with_context(|| format!("reading packages dir {}", packages_dir.display()))?;
    for entry in entries {
        let entry = entry.with_context(|| format!("listing {}", packages_dir.display()))?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| {
                anyhow!(
                    "non-UTF8 package directory name under {}",
                    packages_dir.display()
                )
            })?
            .to_string();

        let Some(afb_path) = find_afb(&path)? else {
            continue;
        };
        let wasm = extract_precompiled_wasm(&afb_path)
            .with_context(|| format!("extracting precompiled wasm from {}", afb_path.display()))?;
        register(afterburner, scripts, &name, &wasm).with_context(|| {
            format!(
                "registering override package '{name}' from {}",
                afb_path.display()
            )
        })?;
        tracing::info!(package = %name, afb = %afb_path.display(), "policy package overridden from packages-dir");
    }
    Ok(())
}

/// The single `*.afb` file directly under `dir`, or `None` if there isn't
/// one. Fails loudly if there is more than one: an ambiguous override is a
/// configuration error worth surfacing, not a silent pick of either.
fn find_afb(dir: &Path) -> Result<Option<PathBuf>> {
    let mut found = None;
    for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry.with_context(|| format!("listing {}", dir.display()))?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) == Some("afb") {
            if found.is_some() {
                bail!("more than one .afb file under {}", dir.display());
            }
            found = Some(path);
        }
    }
    Ok(found)
}

/// Extracts `precompiled/wasm32-wasip1/main.wasm` from a `.afb` archive
/// in-process: a `.afb` is a zstd-compressed tar (D9/D17a), so this is a
/// streaming zstd decode straight into `tar::Archive`, never a shell-out.
fn extract_precompiled_wasm(afb_path: &Path) -> Result<Vec<u8>> {
    let file =
        fs::File::open(afb_path).with_context(|| format!("opening {}", afb_path.display()))?;
    let decoder = zstd::Decoder::new(file)
        .with_context(|| format!("zstd-decoding {}", afb_path.display()))?;
    let mut archive = tar::Archive::new(decoder);
    let entries = archive
        .entries()
        .with_context(|| format!("reading tar entries in {}", afb_path.display()))?;
    for entry in entries {
        let mut entry =
            entry.with_context(|| format!("reading a tar entry in {}", afb_path.display()))?;
        let entry_path = entry
            .path()
            .with_context(|| format!("reading a tar entry path in {}", afb_path.display()))?
            .to_path_buf();
        if entry_path == Path::new(PRECOMPILED_ENTRY_PATH) {
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes).with_context(|| {
                format!(
                    "reading {PRECOMPILED_ENTRY_PATH} from {}",
                    afb_path.display()
                )
            })?;
            return Ok(bytes);
        }
    }
    bail!(
        "{} has no {PRECOMPILED_ENTRY_PATH} entry (not an AOT-compiled sealed package? run `burn compile`, not `burn package`)",
        afb_path.display()
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A minimal wasm-mode engine for this module's own tests. Production
    /// callers build their one shared engine via `defraburner::runtime`
    /// (which this crate cannot depend on: that would be the dependency
    /// cycle the other direction); this mirrors that same
    /// `Afterburner::builder().mode(Mode::Wasm).build()` call for
    /// test-only purposes.
    fn test_engine() -> Arc<Afterburner> {
        Arc::new(
            Afterburner::builder()
                .mode(afterburner::Mode::Wasm)
                .build()
                .expect("building a test afterburner engine"),
        )
    }

    /// EARLY VERIFICATION (Phase 4): registers the real, embedded,
    /// AOT-precompiled `autoscale-default` wasm through
    /// `register_precompiled` and runs it against the same two snapshots
    /// the Phase 0 spike (since removed; its proofs now live here and in
    /// `tests/tenants.rs`/`go_interop.rs`) used against source
    /// registration, asserting the same scale_up/scale_down outcomes.
    /// Proves the facade's documented contract end to end: a sealed
    /// precompiled module registered via `register_precompiled` drives
    /// stdin -> stdout identically to a source-registered one.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn precompiled_autoscale_default_matches_phase_0_source_registration() {
        let engine =
            PolicyEngine::load(None, test_engine()).expect("loading the embedded default policies");
        assert!(engine.has_package(AUTOSCALE_DEFAULT_NAME));
        assert!(engine.has_package(PLACEMENT_DEFAULT_NAME));

        let scale_up_input = json!({
            "cells": [
                {"id": "a", "qps": 250.0, "p99_ms": 12.0, "mem_bytes": 1000, "mem_budget_bytes": 4000}
            ],
            "limits": {"min_cells": 1, "max_cells": 8}
        });
        let scale_up_output = engine
            .run(AUTOSCALE_DEFAULT_NAME, &scale_up_input)
            .expect("running autoscale-default (scale_up case)");
        assert_eq!(
            scale_up_output.get("action").and_then(|v| v.as_str()),
            Some("scale_up"),
            "unexpected scale_up output: {scale_up_output}"
        );

        let scale_down_input = json!({
            "cells": [
                {"id": "a", "qps": 1.0, "p99_ms": 5.0, "mem_bytes": 1000, "mem_budget_bytes": 4000},
                {"id": "b", "qps": 1.0, "p99_ms": 5.0, "mem_bytes": 1000, "mem_budget_bytes": 4000},
                {"id": "c", "qps": 1.0, "p99_ms": 5.0, "mem_bytes": 1000, "mem_budget_bytes": 4000}
            ],
            "limits": {"min_cells": 1, "max_cells": 8}
        });
        let scale_down_output = engine
            .run(AUTOSCALE_DEFAULT_NAME, &scale_down_input)
            .expect("running autoscale-default (scale_down case)");
        assert_eq!(
            scale_down_output.get("action").and_then(|v| v.as_str()),
            Some("scale_down"),
            "unexpected scale_down output: {scale_down_output}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn placement_default_places_a_pending_tenant() {
        let engine =
            PolicyEngine::load(None, test_engine()).expect("loading the embedded default policies");
        let input = json!({
            "pending_tenants": [{"name": "acme-co", "replicas": 2}],
            "free_cells": ["cell-0", "cell-1", "cell-2"],
            "assigned_counts": {}
        });
        let output = engine
            .run(PLACEMENT_DEFAULT_NAME, &input)
            .expect("running placement-default");
        let placements = output
            .get("placements")
            .and_then(|v| v.as_array())
            .expect("placements array");
        assert_eq!(placements.len(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn run_fails_loudly_for_an_unregistered_package_name() {
        let engine =
            PolicyEngine::load(None, test_engine()).expect("loading the embedded default policies");
        let error = engine
            .run("does-not-exist", &json!({}))
            .expect_err("running an unregistered package name should fail");
        assert!(error.to_string().contains("does-not-exist"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn load_overrides_a_default_package_by_directory_name() {
        let dir = tempfile::tempdir().unwrap();
        let override_dir = dir.path().join(AUTOSCALE_DEFAULT_NAME);
        std::fs::create_dir_all(&override_dir).unwrap();
        // Reuse the real, already-compiled placement-default .afb as the
        // "override" payload: its content doesn't matter here, only that a
        // *different* package's wasm lands under the autoscale-default
        // name and answers calls (proving the override, not the default,
        // now serves that name).
        let afb_source = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../packages/placement-default")
            .read_dir()
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .find(|p| p.extension().and_then(|e| e.to_str()) == Some("afb"))
            .expect("placement-default must already be `burn compile`d (run `just packages`)");
        std::fs::copy(&afb_source, override_dir.join("override.afb")).unwrap();

        let engine = PolicyEngine::load(Some(dir.path()), test_engine())
            .expect("loading with an override dir");
        let output = engine
            .run(
                AUTOSCALE_DEFAULT_NAME,
                &json!({"pending_tenants": [], "free_cells": [], "assigned_counts": {}}),
            )
            .expect("running the overridden autoscale-default package");
        // placement-default's shape ("placements"/"reason"), not
        // autoscale-default's ("action"/"target_cells"/"reason"): proof the
        // override actually replaced the embedded default.
        assert!(
            output.get("placements").is_some(),
            "expected the override's shape: {output}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn load_rejects_a_corrupt_override_archive() {
        let dir = tempfile::tempdir().unwrap();
        let override_dir = dir.path().join(AUTOSCALE_DEFAULT_NAME);
        std::fs::create_dir_all(&override_dir).unwrap();
        std::fs::write(
            override_dir.join("truncated.afb"),
            b"not a real afb archive",
        )
        .unwrap();

        let error = PolicyEngine::load(Some(dir.path()), test_engine())
            .expect_err("a corrupt override must fail loudly");
        // anyhow's `Display` shows only the outermost context; the useful
        // detail (which stage failed, and why) is further down the chain,
        // so assert against the full `{:#}` chain rendering instead.
        let chain = format!("{error:#}").to_lowercase();
        assert!(
            chain.contains("zstd") || chain.contains("truncated.afb"),
            "expected the error chain to mention zstd or the file name, got: {chain}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn load_rejects_an_ambiguous_override_directory() {
        let dir = tempfile::tempdir().unwrap();
        let override_dir = dir.path().join(AUTOSCALE_DEFAULT_NAME);
        std::fs::create_dir_all(&override_dir).unwrap();
        std::fs::write(override_dir.join("a.afb"), b"one").unwrap();
        std::fs::write(override_dir.join("b.afb"), b"two").unwrap();

        let error = PolicyEngine::load(Some(dir.path()), test_engine())
            .expect_err("two .afb files must be ambiguous");
        let chain = format!("{error:#}");
        assert!(
            chain.contains("more than one"),
            "expected the error chain to mention the ambiguity, got: {chain}"
        );
    }
}
