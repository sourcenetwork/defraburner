// defraburner/autoscale-default: an Afterburner package.
"use strict";

// Threshold autoscaling policy. Pure function: MetricsSnapshot -> Decision.
// The Rust host clamps and validates the decision again before acting on
// it (docs/plans/defraburner.md, burner-policy); this package only
// proposes.
const SCALE_UP_QPS_THRESHOLD = 100;
const SCALE_DOWN_QPS_THRESHOLD = 10;

module.exports = function (input) {
  const cells = input && Array.isArray(input.cells) ? input.cells : [];
  const limits = (input && input.limits) || {};
  const minCells = typeof limits.min_cells === "number" ? limits.min_cells : 1;
  const maxCells =
    typeof limits.max_cells === "number" ? limits.max_cells : Math.max(cells.length, minCells);

  const clamp = (n) => Math.min(Math.max(n, minCells), maxCells);

  if (cells.length === 0) {
    return { action: "hold", target_cells: clamp(minCells), reason: "no cells in snapshot" };
  }

  const count = cells.length;
  const avgQps = cells.reduce((sum, cell) => sum + (Number(cell.qps) || 0), 0) / count;

  if (avgQps > SCALE_UP_QPS_THRESHOLD && count < maxCells) {
    return {
      action: "scale_up",
      target_cells: clamp(count + 1),
      reason: `avg qps ${avgQps} exceeds scale_up threshold ${SCALE_UP_QPS_THRESHOLD}`,
    };
  }

  if (avgQps < SCALE_DOWN_QPS_THRESHOLD && count > minCells) {
    return {
      action: "scale_down",
      target_cells: clamp(count - 1),
      reason: `avg qps ${avgQps} below scale_down threshold ${SCALE_DOWN_QPS_THRESHOLD}`,
    };
  }

  return {
    action: "hold",
    target_cells: clamp(count),
    reason: `avg qps ${avgQps} within [${SCALE_DOWN_QPS_THRESHOLD}, ${SCALE_UP_QPS_THRESHOLD}] hold band`,
  };
};
