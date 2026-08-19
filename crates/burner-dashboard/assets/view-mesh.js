// defraburner dashboard -- Mesh view: cells x listen addrs, static peer
// dial outcomes from startup, and a dial form (delegates to the same
// per-cell dial endpoint the Cells view's inspect panel uses).
"use strict";

(function () {
  const B = window.Burner;

  function renderCells(overview) {
    const cells = overview.cells || [];
    if (cells.length === 0) { B.$("#mesh-cells").innerHTML = '<div class="ui-stat-value muted">no data yet</div>'; return; }
    const rows = cells
      .map((cell) => {
        const addrs = (cell.listen_addrs || []).map((a) => `<div class="mono">${B.escapeHtml(a)}/p2p/${B.escapeHtml(cell.peer_id)}</div>`).join("") || '<span class="fg-3">none</span>';
        return (
          `<tr><td class="mono">${B.markerSvg("cell", cell.id)} ${B.escapeHtml(cell.id)}</td><td class="mono" style="font-size:11px">${B.escapeHtml(cell.peer_id)}</td>` +
          `<td>${addrs}</td><td class="mono">${(cell.connected_peers || []).length}</td></tr>`
        );
      })
      .join("");
    B.$("#mesh-cells").innerHTML =
      `<table class="ui-data-table"><thead><tr><th>cell</th><th>peer id</th><th>listen addrs</th><th>connected</th></tr></thead><tbody>${rows}</tbody></table>`;
  }

  function renderStaticPeers(overview) {
    const outcomes = overview.static_peer_outcomes || [];
    if (outcomes.length === 0) {
      B.$("#mesh-static-peers").innerHTML = '<div class="ui-stat-hint">no static --peers configured for this cluster</div>';
      return;
    }
    const rows = outcomes
      .map(
        (o) =>
          `<tr><td class="mono">${B.markerSvg("cell", o.cell_id)} ${B.escapeHtml(o.cell_id)}</td><td class="mono" style="font-size:11px">${B.markerSvg("peer", o.peer_addr)} ${B.escapeHtml(o.peer_addr)}</td>` +
          `<td>${B.dot(o.confirmed ? "success" : o.ok ? "warning" : "error")} ${o.confirmed ? "confirmed" : o.ok ? "dialed" : "failed"}</td>` +
          `<td class="fg-3">${B.escapeHtml(o.error || o.note || "")}</td></tr>`
      )
      .join("");
    B.$("#mesh-static-peers").innerHTML =
      `<table class="ui-data-table"><thead><tr><th>cell</th><th>peer addr</th><th>status</th><th>note</th></tr></thead><tbody>${rows}</tbody></table>`;
  }

  function initDialForm() {
    const form = B.$("#mesh-dial-form");
    if (!form) return;
    form.addEventListener("submit", (event) => {
      event.preventDefault();
      const submitBtn = B.$("#mesh-dial-submit");
      const cellId = B.$("#mesh-dial-cell").value.trim();
      const addr = B.$("#mesh-dial-addr").value.trim();
      const result = B.$("#mesh-dial-result");
      if (!cellId || !addr) return;
      B.withBusy(submitBtn, "dialing...", async () => {
        try {
          const response = await B.adminFetch(`/admin/cells/${encodeURIComponent(cellId)}/dial`, {
            method: "POST",
            body: JSON.stringify({ addr }),
          });
          const body = await response.json();
          B.showResult(result, response.ok && body.ok, response.ok ? (body.ok ? "dialed successfully" : `failed: ${body.error || "unknown error"}`) : await B.describeFailure(response));
        } catch (err) {
          B.showResult(result, false, "request failed: " + err.message);
        }
      });
    });
  }

  B.onOverview((data) => {
    renderCells(data.overview);
    renderStaticPeers(data.overview);
  });
  document.addEventListener("DOMContentLoaded", initDialForm);
})();
