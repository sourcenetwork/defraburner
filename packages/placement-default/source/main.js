// defraburner/placement-default: an Afterburner package.
"use strict";

// Least-assigned-first placement policy. Pure function:
// PlacementSnapshot -> PlacementDecision. The Rust host clamps and
// validates the decision again before acting on it (docs/plans/
// defraburner.md, burner-policy): placements land only on cells the host
// still considers free, exactly `replicas` many, disjoint. This package
// only proposes.
module.exports = function (input) {
  const pendingTenants =
    input && Array.isArray(input.pending_tenants) ? input.pending_tenants : [];
  const assignedCounts = (input && input.assigned_counts) || {};
  const freeCells = input && Array.isArray(input.free_cells) ? input.free_cells.slice() : [];

  if (pendingTenants.length === 0) {
    return { placements: [], reason: "no pending tenants" };
  }

  const countOf = (cellId) => {
    const count = assignedCounts[cellId];
    return typeof count === "number" ? count : 0;
  };

  // Ranked once up front, least-assigned-first; every tenant below draws
  // from (and shrinks) this same ranked pool, so two tenants placed in one
  // call never claim the same cell.
  let pool = freeCells.sort((a, b) => countOf(a) - countOf(b));

  const placements = [];
  const skipped = [];
  for (const tenant of pendingTenants) {
    const name = tenant && typeof tenant.name === "string" ? tenant.name : "";
    const replicas = tenant && typeof tenant.replicas === "number" ? tenant.replicas : 0;

    if (!name || replicas <= 0) {
      skipped.push(`${name || "<unnamed>"} (invalid replicas)`);
      continue;
    }
    if (pool.length < replicas) {
      skipped.push(`${name} (needs ${replicas}, only ${pool.length} free cell(s) left)`);
      continue;
    }

    placements.push({ tenant: name, cells: pool.slice(0, replicas) });
    pool = pool.slice(replicas);
  }

  const reason =
    `placed ${placements.length}/${pendingTenants.length} tenant(s)` +
    (skipped.length > 0 ? `; skipped: ${skipped.join(", ")}` : "");

  return { placements, reason };
};
