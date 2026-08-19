//! Decision shapes a policy package's raw JSON output parses into. Any
//! parse failure (wrong type, missing field, non-whole `target_cells`, ...)
//! is a `PolicyError` exactly like an engine call failing: the autoscaler
//! never trusts a policy's output shape any further than serde is willing
//! to validate it.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The autoscale policy's proposed action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScaleAction {
    ScaleUp,
    ScaleDown,
    Hold,
}

/// Parsed `autoscale-default` (or an override's) output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AutoscaleDecision {
    pub action: ScaleAction,
    #[serde(deserialize_with = "whole_non_negative_u64")]
    pub target_cells: u64,
    pub reason: String,
}

impl AutoscaleDecision {
    /// Parses a raw policy output `Value` into an `AutoscaleDecision`,
    /// failing loudly (with the raw output attached) on any shape
    /// mismatch.
    pub fn parse(value: &Value) -> Result<Self> {
        serde_json::from_value(value.clone())
            .with_context(|| format!("parsing autoscale policy output: {value}"))
    }
}

/// One tenant placement the placement policy proposes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Placement {
    pub tenant: String,
    pub cells: Vec<String>,
}

/// Parsed `placement-default` (or an override's) output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlacementDecision {
    pub placements: Vec<Placement>,
    pub reason: String,
}

impl PlacementDecision {
    /// Parses a raw policy output `Value` into a `PlacementDecision`,
    /// failing loudly (with the raw output attached) on any shape
    /// mismatch.
    pub fn parse(value: &Value) -> Result<Self> {
        serde_json::from_value(value.clone())
            .with_context(|| format!("parsing placement policy output: {value}"))
    }
}

/// Deserializes a JSON number into a `u64`, accepting either integer
/// (`2`) or whole-valued float (`2.0`) JSON encodings -- a JS engine's
/// `JSON.stringify` of an integer-valued `Number` is not guaranteed to
/// omit the decimal point across every implementation, so this accepts
/// both rather than rejecting a numerically-whole float. A genuine
/// fraction or a negative number still fails loudly: this is a
/// leniency about *encoding*, not about the value itself.
fn whole_non_negative_u64<'de, D>(deserializer: D) -> std::result::Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = f64::deserialize(deserializer)?;
    if value.is_finite() && value >= 0.0 && value.fract() == 0.0 {
        Ok(value as u64)
    } else {
        Err(serde::de::Error::custom(format!(
            "expected a non-negative whole number, got {value}"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn autoscale_decision_parses_a_well_formed_value() {
        let value = json!({"action": "scale_up", "target_cells": 3, "reason": "avg qps high"});
        let decision = AutoscaleDecision::parse(&value).unwrap();
        assert_eq!(decision.action, ScaleAction::ScaleUp);
        assert_eq!(decision.target_cells, 3);
        assert_eq!(decision.reason, "avg qps high");
    }

    #[test]
    fn autoscale_decision_accepts_a_whole_valued_float_target_cells() {
        let value = json!({"action": "hold", "target_cells": 3.0, "reason": "steady"});
        let decision = AutoscaleDecision::parse(&value).unwrap();
        assert_eq!(decision.target_cells, 3);
    }

    #[test]
    fn autoscale_decision_rejects_a_fractional_target_cells() {
        let value = json!({"action": "hold", "target_cells": 3.5, "reason": "steady"});
        assert!(AutoscaleDecision::parse(&value).is_err());
    }

    #[test]
    fn autoscale_decision_rejects_a_negative_target_cells() {
        let value = json!({"action": "hold", "target_cells": -1, "reason": "steady"});
        assert!(AutoscaleDecision::parse(&value).is_err());
    }

    #[test]
    fn autoscale_decision_rejects_an_unknown_action() {
        let value = json!({"action": "explode", "target_cells": 1, "reason": "?"});
        assert!(AutoscaleDecision::parse(&value).is_err());
    }

    #[test]
    fn autoscale_decision_rejects_the_nonsense_shape() {
        let value = json!({"nonsense": 1});
        let error = AutoscaleDecision::parse(&value).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("parsing autoscale policy output")
        );
    }

    #[test]
    fn placement_decision_parses_a_well_formed_value() {
        let value = json!({"placements": [{"tenant": "acme-co", "cells": ["cell-0", "cell-1"]}], "reason": "placed 1/1"});
        let decision = PlacementDecision::parse(&value).unwrap();
        assert_eq!(decision.placements.len(), 1);
        assert_eq!(decision.placements[0].tenant, "acme-co");
        assert_eq!(decision.placements[0].cells, vec!["cell-0", "cell-1"]);
    }

    #[test]
    fn placement_decision_rejects_the_nonsense_shape() {
        let value = json!({"nonsense": 1});
        let error = PlacementDecision::parse(&value).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("parsing placement policy output")
        );
    }
}
