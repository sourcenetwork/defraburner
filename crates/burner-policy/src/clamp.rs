//! Pure, unit-tested hard guardrails applied to every raw policy decision
//! before it is allowed to touch the cluster. No I/O, no engine calls: a
//! plain function of its inputs, gathered by the caller (`autoscaler.rs`)
//! from the live manifest/supervisor, never re-derived here. This is the
//! Rust-side trust boundary the plan describes: a policy proposes, this
//! module (and only this module) decides what is actually authorized.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::decision::{AutoscaleDecision, PlacementDecision, ScaleAction};

/// One action a [`ClampedPlan`] actually authorizes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ClampedAction {
    ScaleUp,
    ScaleDown { cell_id: String },
    Place { tenant: String, cells: Vec<String> },
}

/// The Rust-validated plan for one policy call: what is actually
/// authorized to execute, plus a human-readable note for every place a
/// raw decision was adjusted or rejected. An empty `actions` with a
/// non-empty `clamps_applied` is a full hold, not a partial success.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClampedPlan {
    pub actions: Vec<ClampedAction>,
    pub clamps_applied: Vec<String>,
}

/// Everything [`clamp_autoscale`] needs about the live cluster to validate
/// a raw [`AutoscaleDecision`].
#[derive(Debug, Clone)]
pub struct AutoscaleClampContext {
    pub current_cell_count: usize,
    pub min_cells: usize,
    pub max_cells: usize,
    /// Cell ids assigned to no tenant, in manifest (provisioning) order,
    /// so `.last()` is the newest. Only these are eligible for
    /// `scale_down`.
    pub free_cells_oldest_first: Vec<String>,
    /// Seconds since the last *executed* scale action, or `None` if none
    /// has executed yet this process (never blocked by cooldown).
    pub seconds_since_last_action: Option<u64>,
    pub cooldown_secs: u64,
}

/// Validates and clamps a raw [`AutoscaleDecision`] into a [`ClampedPlan`]
/// of at most one [`ClampedAction::ScaleUp`] or
/// [`ClampedAction::ScaleDown`] -- never both, and never more than one of
/// either: `AutoscaleDecision` carries a single `action`, not a list, so
/// "at most one scale action per tick" is enforced by the decision type
/// itself, not a runtime truncation here (see this module's tests for the
/// invariant asserted directly).
pub fn clamp_autoscale(decision: &AutoscaleDecision, ctx: &AutoscaleClampContext) -> ClampedPlan {
    let mut clamps = Vec::new();

    let min = ctx.min_cells as u64;
    let max = ctx.max_cells as u64;
    let clamped_target = decision.target_cells.clamp(min, max);
    if clamped_target != decision.target_cells {
        clamps.push(format!(
            "target_cells {} clamped to [{min}, {max}] -> {clamped_target}",
            decision.target_cells
        ));
    }
    let clamped_target = clamped_target as usize;

    if let Some(elapsed) = ctx.seconds_since_last_action
        && elapsed < ctx.cooldown_secs
    {
        clamps.push(format!(
            "cooldown active ({elapsed}s since last action, need {}s): holding",
            ctx.cooldown_secs
        ));
        return ClampedPlan {
            actions: Vec::new(),
            clamps_applied: clamps,
        };
    }

    let action = match decision.action {
        ScaleAction::Hold => None,
        ScaleAction::ScaleUp => {
            if clamped_target <= ctx.current_cell_count {
                clamps.push(format!(
                    "scale_up requested but clamped target {clamped_target} <= current cell count {}: holding",
                    ctx.current_cell_count
                ));
                None
            } else {
                Some(ClampedAction::ScaleUp)
            }
        }
        ScaleAction::ScaleDown => {
            if clamped_target >= ctx.current_cell_count {
                clamps.push(format!(
                    "scale_down requested but clamped target {clamped_target} >= current cell count {}: holding",
                    ctx.current_cell_count
                ));
                None
            } else {
                match ctx.free_cells_oldest_first.last() {
                    Some(cell_id) => Some(ClampedAction::ScaleDown {
                        cell_id: cell_id.clone(),
                    }),
                    None => {
                        clamps.push(
                            "scale_down requested but no free cell is available to remove: holding"
                                .to_string(),
                        );
                        None
                    }
                }
            }
        }
    };

    ClampedPlan {
        actions: action.into_iter().collect(),
        clamps_applied: clamps,
    }
}

/// Everything [`clamp_placement`] needs about the live cluster to validate
/// a raw [`PlacementDecision`].
#[derive(Debug, Clone)]
pub struct PlacementClampContext {
    /// Cell ids the host still considers free at the moment of clamping.
    pub free_cells: Vec<String>,
    /// tenant name -> required replica count, for every tenant a
    /// placement is allowed to target.
    pub required_replicas: HashMap<String, u8>,
}

/// Validates and clamps a raw [`PlacementDecision`] into a [`ClampedPlan`]
/// of [`ClampedAction::Place`] actions: each placement must name cells
/// still free at clamp time, exactly `required_replicas` many, with no
/// repeated cell within the placement and no cell reused across two
/// placements in the same plan. A placement that fails any of these is
/// dropped (noted in `clamps_applied`), never partially executed.
pub fn clamp_placement(decision: &PlacementDecision, ctx: &PlacementClampContext) -> ClampedPlan {
    let mut clamps = Vec::new();
    let mut actions = Vec::new();
    let mut still_free: HashSet<&str> = ctx.free_cells.iter().map(String::as_str).collect();

    for placement in &decision.placements {
        let Some(&required) = ctx.required_replicas.get(&placement.tenant) else {
            clamps.push(format!(
                "placement for unknown or already-placed tenant '{}' dropped",
                placement.tenant
            ));
            continue;
        };
        if placement.cells.len() != required as usize {
            clamps.push(format!(
                "placement for '{}' names {} cell(s), needs exactly {required}: dropped",
                placement.tenant,
                placement.cells.len()
            ));
            continue;
        }

        let mut seen_in_this_placement: HashSet<&str> =
            HashSet::with_capacity(placement.cells.len());
        let all_free_and_disjoint = placement.cells.iter().all(|cell| {
            still_free.contains(cell.as_str()) && seen_in_this_placement.insert(cell.as_str())
        });
        if !all_free_and_disjoint {
            clamps.push(format!(
                "placement for '{}' names a cell that is not free or repeats a cell: dropped",
                placement.tenant
            ));
            continue;
        }

        for cell in &placement.cells {
            still_free.remove(cell.as_str());
        }
        actions.push(ClampedAction::Place {
            tenant: placement.tenant.clone(),
            cells: placement.cells.clone(),
        });
    }

    ClampedPlan {
        actions,
        clamps_applied: clamps,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decision::Placement;

    fn autoscale_ctx() -> AutoscaleClampContext {
        AutoscaleClampContext {
            current_cell_count: 2,
            min_cells: 1,
            max_cells: 8,
            free_cells_oldest_first: vec!["cell-0".to_string(), "cell-1".to_string()],
            seconds_since_last_action: None,
            cooldown_secs: 60,
        }
    }

    fn decision(action: ScaleAction, target_cells: u64) -> AutoscaleDecision {
        AutoscaleDecision {
            action,
            target_cells,
            reason: "test".to_string(),
        }
    }

    #[test]
    fn scale_up_within_bounds_is_authorized() {
        let plan = clamp_autoscale(&decision(ScaleAction::ScaleUp, 3), &autoscale_ctx());
        assert_eq!(plan.actions, vec![ClampedAction::ScaleUp]);
        assert!(plan.clamps_applied.is_empty());
    }

    #[test]
    fn target_above_max_cells_is_clamped_down() {
        let plan = clamp_autoscale(&decision(ScaleAction::ScaleUp, 99), &autoscale_ctx());
        assert_eq!(plan.actions, vec![ClampedAction::ScaleUp]);
        assert!(
            plan.clamps_applied
                .iter()
                .any(|c| c.contains("clamped to [1, 8]"))
        );
    }

    #[test]
    fn target_below_min_cells_is_clamped_up() {
        let mut ctx = autoscale_ctx();
        ctx.current_cell_count = 1;
        let plan = clamp_autoscale(&decision(ScaleAction::ScaleDown, 0), &ctx);
        // Clamped target (1) equals current (1): scale_down holds.
        assert!(plan.actions.is_empty());
        assert!(
            plan.clamps_applied
                .iter()
                .any(|c| c.contains("clamped to [1, 8]"))
        );
    }

    #[test]
    fn scale_down_picks_the_newest_free_cell() {
        let plan = clamp_autoscale(&decision(ScaleAction::ScaleDown, 1), &autoscale_ctx());
        assert_eq!(
            plan.actions,
            vec![ClampedAction::ScaleDown {
                cell_id: "cell-1".to_string()
            }]
        );
    }

    #[test]
    fn scale_down_with_no_free_cells_holds() {
        let mut ctx = autoscale_ctx();
        ctx.free_cells_oldest_first = Vec::new();
        let plan = clamp_autoscale(&decision(ScaleAction::ScaleDown, 1), &ctx);
        assert!(plan.actions.is_empty());
        assert!(
            plan.clamps_applied
                .iter()
                .any(|c| c.contains("no free cell"))
        );
    }

    #[test]
    fn cooldown_active_holds_regardless_of_action() {
        let mut ctx = autoscale_ctx();
        ctx.seconds_since_last_action = Some(5);
        ctx.cooldown_secs = 60;
        let plan = clamp_autoscale(&decision(ScaleAction::ScaleUp, 5), &ctx);
        assert!(plan.actions.is_empty());
        assert!(
            plan.clamps_applied
                .iter()
                .any(|c| c.contains("cooldown active"))
        );
    }

    #[test]
    fn cooldown_elapsed_allows_the_action() {
        let mut ctx = autoscale_ctx();
        ctx.seconds_since_last_action = Some(120);
        ctx.cooldown_secs = 60;
        let plan = clamp_autoscale(&decision(ScaleAction::ScaleUp, 3), &ctx);
        assert_eq!(plan.actions, vec![ClampedAction::ScaleUp]);
    }

    #[test]
    fn hold_action_never_produces_an_action() {
        let plan = clamp_autoscale(&decision(ScaleAction::Hold, 2), &autoscale_ctx());
        assert!(plan.actions.is_empty());
        assert!(plan.clamps_applied.is_empty());
    }

    #[test]
    fn scale_up_at_max_cells_holds_rather_than_exceeding_it() {
        let mut ctx = autoscale_ctx();
        ctx.current_cell_count = 8;
        ctx.max_cells = 8;
        let plan = clamp_autoscale(&decision(ScaleAction::ScaleUp, 9), &ctx);
        assert!(plan.actions.is_empty());
    }

    #[test]
    fn at_most_one_scale_action_is_ever_produced_by_construction() {
        // AutoscaleDecision::action is a single ScaleAction, not a list,
        // so this holds for every possible decision -- exercised across
        // the full variant space rather than asserted once.
        for action in [
            ScaleAction::ScaleUp,
            ScaleAction::ScaleDown,
            ScaleAction::Hold,
        ] {
            let plan = clamp_autoscale(&decision(action, 5), &autoscale_ctx());
            assert!(plan.actions.len() <= 1);
        }
    }

    fn placement_ctx() -> PlacementClampContext {
        PlacementClampContext {
            free_cells: vec![
                "cell-0".to_string(),
                "cell-1".to_string(),
                "cell-2".to_string(),
            ],
            required_replicas: HashMap::from([("acme-co".to_string(), 2u8)]),
        }
    }

    fn placement_decision(placements: Vec<Placement>) -> PlacementDecision {
        PlacementDecision {
            placements,
            reason: "test".to_string(),
        }
    }

    #[test]
    fn a_valid_placement_onto_free_cells_is_authorized() {
        let decision = placement_decision(vec![Placement {
            tenant: "acme-co".to_string(),
            cells: vec!["cell-0".to_string(), "cell-1".to_string()],
        }]);
        let plan = clamp_placement(&decision, &placement_ctx());
        assert_eq!(
            plan.actions,
            vec![ClampedAction::Place {
                tenant: "acme-co".to_string(),
                cells: vec!["cell-0".to_string(), "cell-1".to_string()]
            }]
        );
        assert!(plan.clamps_applied.is_empty());
    }

    #[test]
    fn a_placement_naming_a_non_free_cell_is_dropped() {
        let decision = placement_decision(vec![Placement {
            tenant: "acme-co".to_string(),
            cells: vec!["cell-0".to_string(), "cell-99".to_string()],
        }]);
        let plan = clamp_placement(&decision, &placement_ctx());
        assert!(plan.actions.is_empty());
        assert!(plan.clamps_applied.iter().any(|c| c.contains("not free")));
    }

    #[test]
    fn a_placement_with_the_wrong_replica_count_is_dropped() {
        let decision = placement_decision(vec![Placement {
            tenant: "acme-co".to_string(),
            cells: vec!["cell-0".to_string()],
        }]);
        let plan = clamp_placement(&decision, &placement_ctx());
        assert!(plan.actions.is_empty());
        assert!(
            plan.clamps_applied
                .iter()
                .any(|c| c.contains("needs exactly 2"))
        );
    }

    #[test]
    fn a_placement_for_an_unknown_tenant_is_dropped() {
        let decision = placement_decision(vec![Placement {
            tenant: "ghost-co".to_string(),
            cells: vec!["cell-0".to_string(), "cell-1".to_string()],
        }]);
        let plan = clamp_placement(&decision, &placement_ctx());
        assert!(plan.actions.is_empty());
        assert!(plan.clamps_applied.iter().any(|c| c.contains("unknown")));
    }

    #[test]
    fn a_placement_repeating_a_cell_within_itself_is_dropped() {
        let mut ctx = placement_ctx();
        ctx.required_replicas.insert("acme-co".to_string(), 2);
        let decision = placement_decision(vec![Placement {
            tenant: "acme-co".to_string(),
            cells: vec!["cell-0".to_string(), "cell-0".to_string()],
        }]);
        let plan = clamp_placement(&decision, &ctx);
        assert!(plan.actions.is_empty());
    }

    #[test]
    fn two_placements_claiming_the_same_cell_the_second_is_dropped() {
        let mut ctx = placement_ctx();
        ctx.required_replicas.insert("other-co".to_string(), 1);
        let decision = placement_decision(vec![
            Placement {
                tenant: "acme-co".to_string(),
                cells: vec!["cell-0".to_string(), "cell-1".to_string()],
            },
            Placement {
                tenant: "other-co".to_string(),
                cells: vec!["cell-1".to_string()],
            },
        ]);
        let plan = clamp_placement(&decision, &ctx);
        assert_eq!(plan.actions.len(), 1);
        assert!(
            matches!(&plan.actions[0], ClampedAction::Place { tenant, .. } if tenant == "acme-co")
        );
        assert!(plan.clamps_applied.iter().any(|c| c.contains("other-co")));
    }
}
