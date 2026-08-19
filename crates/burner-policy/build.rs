//! Pre-flight check for the default policy wasm `src/engine.rs` embeds
//! with `include_bytes!` (D9/D17a). Its only job is to fail with an
//! actionable message before that would otherwise be an opaque compile
//! error, and to tell cargo to re-run whenever either file changes.

use std::path::Path;

/// Paths relative to `packages/`, matching `src/engine.rs`'s
/// `include_bytes!` paths (relative to `src/`, one level deeper).
const DEFAULT_PACKAGE_WASM: &[&str] = &[
    "autoscale-default/.build/main.wasm",
    "placement-default/.build/main.wasm",
];

fn main() {
    let packages_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages");
    for relative in DEFAULT_PACKAGE_WASM {
        let path = packages_dir.join(relative);
        println!("cargo:rerun-if-changed={}", path.display());
        if !path.exists() {
            panic!("policy wasm not built: run `just packages` first");
        }
    }
}
