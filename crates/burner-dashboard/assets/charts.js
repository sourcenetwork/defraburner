// defraburner dashboard -- charts: the cluster multi-line req/s chart
// (fixed series color order, direct labels, legend, crosshair+tooltip on
// hover, one y-axis) and a small sparkline helper. Pure SVG, no library.
"use strict";

(function () {
  const B = window.Burner;

  /// Builds a cluster multi-line chart from `series`: an array of
  /// {id, label, color, points: [{x, y}...]} plus an optional
  /// `otherSeries` ({label, color, points}) for the folded ">4 cells"
  /// line. `points[].x` are tick indices (0..119); `.y` is req/s.
  function renderLineChart(container, series, otherSeries, opts) {
    opts = opts || {};
    const width = container.clientWidth || 640;
    const height = opts.height || 220;
    const padding = { top: 14, right: 64, bottom: 22, left: 40 };
    const innerW = Math.max(10, width - padding.left - padding.right);
    const innerH = Math.max(10, height - padding.top - padding.bottom);

    const allSeries = otherSeries ? series.concat([otherSeries]) : series;
    const allPoints = allSeries.flatMap((s) => s.points);
    const maxX = Math.max(1, ...allPoints.map((p) => p.x));
    const maxY = Math.max(1, ...allPoints.map((p) => p.y)) * 1.15;

    const xScale = (x) => padding.left + (x / maxX) * innerW;
    const yScale = (y) => padding.top + innerH - (y / maxY) * innerH;

    function pathFor(points) {
      if (!points.length) return "";
      return points
        .map((p, i) => `${i === 0 ? "M" : "L"}${xScale(p.x).toFixed(1)},${yScale(p.y).toFixed(1)}`)
        .join(" ");
    }

    const yTicks = 4;
    const gridLines = Array.from({ length: yTicks + 1 }, (_, i) => {
      const value = (maxY / yTicks) * i;
      const y = yScale(value);
      return `<line x1="${padding.left}" x2="${width - padding.right}" y1="${y}" y2="${y}" stroke="var(--border-subtle)" stroke-width="1"/>` +
        `<text class="chart-axis-label" x="${padding.left - 8}" y="${y + 3}" text-anchor="end">${value.toFixed(0)}</text>`;
    }).join("");

    const lines = allSeries
      .map((s) => {
        const last = s.points[s.points.length - 1];
        const label = last
          ? `<text class="chart-axis-label" x="${xScale(last.x) + 6}" y="${yScale(last.y) + 3}" fill="${s.color}">${B.escapeHtml(s.label)}</text>`
          : "";
        return (
          `<path d="${pathFor(s.points)}" fill="none" stroke="${s.color}" stroke-width="1.75" data-series="${B.escapeHtml(s.id)}"/>` +
          label
        );
      })
      .join("");

    const svgId = "chart-" + Math.random().toString(36).slice(2, 9);
    container.innerHTML =
      `<div class="chart-wrap">` +
      `<svg class="chart" id="${svgId}" viewBox="0 0 ${width} ${height}" preserveAspectRatio="none">` +
      gridLines + lines +
      `<line class="chart-crosshair" x1="0" x2="0" y1="${padding.top}" y2="${padding.top + innerH}" stroke="var(--border-strong)" stroke-width="1" opacity="0" />` +
      `</svg>` +
      `<div class="chart-tooltip" style="display:none"></div>` +
      `</div>`;

    const svg = document.getElementById(svgId);
    const tooltip = container.querySelector(".chart-tooltip");
    const crosshair = svg.querySelector(".chart-crosshair");

    svg.addEventListener("mousemove", (event) => {
      const rect = svg.getBoundingClientRect();
      const relX = ((event.clientX - rect.left) / rect.width) * width;
      const tickX = Math.round(((relX - padding.left) / innerW) * maxX);
      if (tickX < 0 || tickX > maxX) { tooltip.style.display = "none"; crosshair.setAttribute("opacity", "0"); return; }
      const px = xScale(tickX);
      crosshair.setAttribute("x1", px); crosshair.setAttribute("x2", px); crosshair.setAttribute("opacity", "1");

      const rows = allSeries
        .map((s) => {
          const point = s.points.find((p) => p.x === tickX) || s.points.reduce((a, b) => (Math.abs(b.x - tickX) < Math.abs(a.x - tickX) ? b : a), s.points[0]);
          if (!point) return "";
          return `<div style="color:${s.color}">${B.escapeHtml(s.label)}: <span class="mono">${point.y.toFixed(1)}</span></div>`;
        })
        .join("");
      tooltip.innerHTML = rows || "no data yet";
      tooltip.style.display = "block";
      tooltip.style.left = Math.min(px + 12, width - 160) + "px";
      tooltip.style.top = "8px";
    });
    svg.addEventListener("mouseleave", () => { tooltip.style.display = "none"; crosshair.setAttribute("opacity", "0"); });

    const legendItems = allSeries
      .map((s) => `<span class="item"><span class="sw" style="background:${s.color}"></span>${B.escapeHtml(s.label)}</span>`)
      .join("");
    return legendItems;
  }

  function sparkline(values, color) {
    const width = 100, height = 24;
    if (!values || values.length === 0) {
      return `<svg class="sparkline" viewBox="0 0 ${width} ${height}" title="no data yet"></svg>`;
    }
    const min = Math.min(...values);
    const max = Math.max(...values);
    const range = max - min || 1;
    const stepX = values.length > 1 ? width / (values.length - 1) : 0;
    const pts = values.map((v, i) => [i * stepX, height - ((v - min) / range) * (height - 4) - 2]);
    const path = pts.map((p, i) => `${i === 0 ? "M" : "L"}${p[0].toFixed(1)},${p[1].toFixed(1)}`).join(" ");
    const last = pts[pts.length - 1];
    const c = color || "var(--accent)";
    return (
      `<svg class="sparkline" viewBox="0 0 ${width} ${height}" preserveAspectRatio="none" title="${values[values.length - 1]}">` +
      `<path d="${path}" fill="none" stroke="${c}" stroke-width="1.5" stroke-linejoin="round" stroke-linecap="round"/>` +
      `<circle cx="${last[0].toFixed(1)}" cy="${last[1].toFixed(1)}" r="2" fill="${c}"/>` +
      `</svg>`
    );
  }

  B.renderLineChart = renderLineChart;
  B.sparkline = sparkline;
})();
