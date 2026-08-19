// defraburner dashboard -- Overview view: stat cards (req/s uses the
// accent gradient variant), cluster multi-line req/s chart (fixed series
// color order, >4 cells fold into "other"), legend, crosshair+tooltip.
"use strict";

(function () {
  const B = window.Burner;

  // Which cells get their own line vs fold into "other": a stable
  // lexicographic sort of ids, not first-seen-in-a-render order (which
  // depended on the backend's HashMap iteration order and was not
  // actually stable tick to tick) and not table position. Color itself
  // comes from markerFor's hash, entirely independent of this ordering,
  // so a cell's color never changes for as long as it lives regardless
  // of which other cells come and go.
  function primaryAndOverflowIds(cells) {
    const sorted = cells.map((c) => c.id).slice().sort();
    return { primary: sorted.slice(0, B.SERIES_COLORS.length), overflow: sorted.slice(B.SERIES_COLORS.length) };
  }

  function recordRates(overview, nowMs) {
    const previous = B.state.lastOverviewSample;
    const elapsedSecs = previous ? (nowMs - previous.ts) / 1000 : 0;
    const requestsByCell = new Map((overview.cell_requests || []).map((c) => [c.cell_id, c.count]));

    let totalCount = 0;
    let totalRejected = overview.admission ? overview.admission.rejected : 0;
    for (const count of requestsByCell.values()) totalCount += count;

    const seen = new Set();
    for (const cell of overview.cells || []) {
      seen.add(cell.id);
      const countNow = requestsByCell.get(cell.id) || 0;
      const prevCount = previous && previous.perCell.get(cell.id);
      const rate = previous && elapsedSecs > 0 && prevCount !== undefined
        ? Math.max(0, countNow - prevCount) / elapsedSecs
        : 0;
      const history = B.state.cellRequestHistory.get(cell.id) || [];
      history.push({ x: history.length ? history[history.length - 1].x + 1 : 0, y: rate });
      while (history.length > B.RING_TICKS) history.shift();
      B.state.cellRequestHistory.set(cell.id, history);
    }
    for (const id of Array.from(B.state.cellRequestHistory.keys())) {
      if (!seen.has(id)) B.state.cellRequestHistory.delete(id);
    }

    const totalRate = previous && elapsedSecs > 0 ? Math.max(0, totalCount - previous.totalCount) / elapsedSecs : null;
    const rejectedRate = previous && elapsedSecs > 0 ? Math.max(0, totalRejected - previous.totalRejected) / elapsedSecs : null;

    B.state.lastOverviewSample = {
      ts: nowMs,
      totalCount,
      totalRejected,
      perCell: requestsByCell,
    };
    return { totalRate, rejectedRate };
  }

  function meanLatencyMs(latency) {
    if (!latency || !latency.length) return null;
    const totalCount = latency.reduce((sum, l) => sum + l.count, 0);
    if (totalCount === 0) return null;
    const weighted = latency.reduce((sum, l) => sum + l.mean_micros * l.count, 0);
    return weighted / totalCount / 1000;
  }

  function renderStats(overview, rates) {
    const cellsUp = (overview.cells || []).length;
    const tenants = (overview.tenants || []).length;
    const meanMs = meanLatencyMs(overview.latency);
    const policy = overview.policy || {};
    const healthy = (policy.consecutive_errors || 0) === 0;

    const cards = [
      B.statCard({ label: "cells running", value: cellsUp }),
      B.statCard({ label: "tenants", value: tenants }),
      B.statCard({ label: "req/s", variant: "accent", value: rates.totalRate === null ? undefined : rates.totalRate.toFixed(1), noData: rates.totalRate === null }),
      B.statCard({ label: "mean ms", value: meanMs === null ? undefined : meanMs.toFixed(2), noData: meanMs === null }),
      B.statCard({ label: "rejected/s", value: rates.rejectedRate === null ? undefined : rates.rejectedRate.toFixed(2), noData: rates.rejectedRate === null }),
      B.statCard({
        variant: healthy ? "success" : "warning",
        label: "policy health",
        value: healthy ? "healthy" : "degraded",
      }),
    ];
    B.$("#overview-stats").innerHTML = cards.join("");
  }

  function renderChart(overview) {
    const { primary: primaryIds, overflow: overflowIds } = primaryAndOverflowIds(overview.cells || []);

    const series = primaryIds.map((id) => ({
      id,
      label: id,
      color: B.markerFor("cell", id).color,
      points: B.state.cellRequestHistory.get(id) || [],
    }));

    let otherSeries = null;
    if (overflowIds.length > 0) {
      const byTick = new Map();
      for (const id of overflowIds) {
        const history = B.state.cellRequestHistory.get(id) || [];
        for (const point of history) {
          byTick.set(point.x, (byTick.get(point.x) || 0) + point.y);
        }
      }
      const points = Array.from(byTick.entries()).sort((a, b) => a[0] - b[0]).map(([x, y]) => ({ x, y }));
      otherSeries = { id: "other", label: `other (${overflowIds.length} cells, summed)`, color: B.SERIES_OTHER_COLOR, points };
    }

    const container = B.$("#overview-chart");
    if (!container) return;
    if (series.every((s) => s.points.length === 0) && !otherSeries) {
      container.innerHTML = '<div class="ui-stat-value muted">no data yet</div>';
      B.$("#overview-legend").innerHTML = "";
      return;
    }
    const legend = B.renderLineChart(container, series, otherSeries);
    B.$("#overview-legend").innerHTML = legend;
  }

  function renderClusterPill(overview) {
    const pill = B.$("#cluster-pill");
    if (!pill) return;
    const cells = (overview.cells || []).length;
    const tenants = (overview.tenants || []).length;
    pill.textContent = `${cells} cell${cells === 1 ? "" : "s"} / ${tenants} tenant${tenants === 1 ? "" : "s"}`;
  }

  B.onOverview((data) => {
    const overview = data.overview;
    const rates = recordRates(overview, Date.now());
    renderStats(overview, rates);
    renderChart(overview);
    renderClusterPill(overview);
  });
})();
