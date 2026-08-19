//! burner-policy: hosts the cluster's policy packages (autoscaling,
//! placement), each an AOT-precompiled wasm module (D9/D17a), on a shared
//! afterburner engine handle it is given rather than one it builds itself
//! (console round, operator directive: engine lifecycle lives in
//! `defraburner::runtime`). See `docs/plans/defraburner.md` (Phase 4, "The
//! pieces") and `docs/decs/defraburner_DECS.md` (D9, D12, D17) for the
//! decisions this crate implements.

pub mod autoscaler;
pub mod clamp;
pub mod decision;
pub mod engine;
pub mod log;
pub mod snapshot;

pub use autoscaler::{
    AutoscalerConfig, AutoscalerControl, PolicyStatusHandle, PolicyStatusSnapshot,
};
pub use engine::{AUTOSCALE_DEFAULT_NAME, PLACEMENT_DEFAULT_NAME, PolicyEngine, RegisteredPackage};
