// defraburner dashboard -- Cells view: searchable DataTable, per-row
// expander with a live introspection panel (GET /admin/cells/{id}/inspect),
// dial-peer, two-click drain, and a spawn-cell toolbar button.
"use strict";

(function () {
  const B = window.Burner;
  const openRows = new Set();
  const inspectCache = new Map(); // cell id -> last fetched inspect response

  function tenantChip(tenant) {
    return tenant ? B.chip("neutral", tenant, { dot: false, sm: true }) : '<span class="fg-3">-</span>';
  }

  function renderTable(overview) {
    const cells = overview.cells || [];
    const detailsById = new Map((overview.cell_details || []).map((d) => [d.id, d]));
    const query = (B.$("#cells-search") && B.$("#cells-search").value.trim().toLowerCase()) || "";

    const filtered = query
      ? cells.filter((c) => c.id.toLowerCase().includes(query) || (detailsById.get(c.id) || {}).tenant?.toLowerCase().includes(query))
      : cells;

    B.$("#cells-count").textContent = `${filtered.length} of ${cells.length}`;

    if (filtered.length === 0) {
      B.$("#cells-tbody").innerHTML = `<tr><td colspan="7" class="ui-data-table-empty">${cells.length === 0 ? "no data yet" : "no cells match your search"}</td></tr>`;
      return;
    }

    const rows = filtered
      .map((cell) => {
        const detail = detailsById.get(cell.id) || {};
        const history = (B.state.cellRequestHistory.get(cell.id) || []).map((p) => p.y);
        const seriesColor = B.markerFor("cell", cell.id).color;
        const isOpen = openRows.has(cell.id);
        return (
          `<tr class="cell-row" data-cell-id="${B.escapeHtml(cell.id)}">` +
          `<td class="ui-data-table-expander"><button type="button" class="ui-data-table-toggle" data-toggle="${B.escapeHtml(cell.id)}">${isOpen ? B.ICONS.chevronDown : B.ICONS.chevronRight}</button></td>` +
          `<td class="mono">${B.markerSvg("cell", cell.id)} ${B.escapeHtml(cell.id)}</td>` +
          `<td>${tenantChip(detail.tenant)}</td>` +
          `<td>${B.dot(cell.marker_ok ? "success" : "error")}</td>` +
          `<td>${detail.storage_bytes !== undefined ? B.humanizeBytes(detail.storage_bytes) : "no data yet"}</td>` +
          `<td>${B.sparkline(history, seriesColor)}</td>` +
          `<td class="mono">${cell.connected_peers ? cell.connected_peers.length : 0}</td>` +
          `</tr>` +
          `<tr class="ui-data-table-detail" data-detail-for="${B.escapeHtml(cell.id)}" ${isOpen ? "" : "hidden"}>` +
          `<td colspan="7">${isOpen ? renderInspectPanel(cell.id, inspectCache.get(cell.id)) : ""}</td>` +
          `</tr>`
        );
      })
      .join("");
    // The tick rebuild would otherwise erase a peer multiaddr mid-typing
    // and the result line of an action that just finished, both of which
    // live inside an expanded row's markup.
    const tbody = B.$("#cells-tbody");
    B.preserveVolatile(tbody, () => {
      tbody.innerHTML = rows;
      B.$all("[data-toggle]", tbody).forEach((btn) => {
        btn.addEventListener("click", () => toggleRow(btn.dataset.toggle));
      });
      wireRowActions();
    });
  }

  function toggleRow(cellId) {
    if (openRows.has(cellId)) {
      openRows.delete(cellId);
    } else {
      openRows.add(cellId);
      loadInspect(cellId);
    }
    if (B.state.overview) renderTable(B.state.overview);
  }

  async function loadInspect(cellId) {
    try {
      const response = await B.adminFetch(`/admin/cells/${encodeURIComponent(cellId)}/inspect`);
      if (response.ok) {
        inspectCache.set(cellId, await response.json());
      } else {
        inspectCache.set(cellId, { error: `HTTP ${response.status}` });
      }
    } catch (err) {
      inspectCache.set(cellId, { error: err.message });
    }
    if (B.state.overview) renderTable(B.state.overview);
  }

  function renderInspectPanel(cellId, inspect) {
    if (!inspect) return '<div class="ui-stat-value muted">loading...</div>';
    if (inspect.error) return B.banner("error", `inspect failed: ${B.escapeHtml(inspect.error)}`);

    const peerChips = (inspect.connected_peers || []).length
      ? inspect.connected_peers.map((p) => B.chip("success", p, { sm: true, dot: false })).join(" ")
      : '<span class="fg-3">no connected peers</span>';
    const listenAddrs = (inspect.listen_addrs || []).map((a) => `<div class="mono">${B.escapeHtml(a)}</div>`).join("") || '<span class="fg-3">none</span>';
    const collections = (inspect.collections || []).map((c) => B.chip("neutral", c, { sm: true, dot: false, mono: true })).join(" ") || '<span class="fg-3">none</span>';

    const storagePct = inspect.mem_budget_bytes > 0 ? Math.min(100, (inspect.storage_bytes / inspect.mem_budget_bytes) * 100) : 0;
    const storageGauge = B.progressBar({
      variant: "gradient",
      value: storagePct,
      label: `storage vs configured memory budget (${B.humanizeBytes(inspect.storage_bytes)} / ${B.humanizeBytes(inspect.mem_budget_bytes)})`,
    });

    const pending = inspect.sync_status && typeof inspect.sync_status.pending_dags === "number" ? inspect.sync_status.pending_dags : null;
    const capacity = inspect.sync_status && typeof inspect.sync_status.pending_dag_capacity === "number" ? inspect.sync_status.pending_dag_capacity : null;
    const backlogGauge = pending !== null && capacity !== null
      ? B.segmentedProgress({
          done: pending, total: capacity,
          variant: capacity > 0 && pending / capacity > 0.7 ? "warning" : "accent",
          label: "pending-DAG backlog",
        })
      : '<div class="ui-stat-hint">pending-DAG backlog: no data yet</div>';

    const txStats = inspect.transaction_stats
      ? `<pre class="code">${B.escapeHtml(JSON.stringify(inspect.transaction_stats, null, 2))}</pre>`
      : '<div class="ui-stat-hint">transaction stats: not tracked by this backend</div>';

    return (
      `<div class="col" style="padding:16px 8px">` +
      `<div class="row" style="gap:8px">${B.markerSvg("cell", cellId)}<span class="mono fg-3">${B.escapeHtml(cellId)}</span></div>` +
      `<div class="ui-toolbar"><strong>collections</strong></div><div class="row" style="flex-wrap:wrap">${collections}</div>` +
      `<div class="ui-toolbar" style="margin-top:12px"><strong>listen addresses</strong></div>${listenAddrs}` +
      `<div class="ui-toolbar" style="margin-top:12px"><strong>connected peers</strong></div><div class="row" style="flex-wrap:wrap">${peerChips}</div>` +
      `<div style="margin-top:12px">${storageGauge}</div>` +
      `<div style="margin-top:12px">${backlogGauge}</div>` +
      `<div class="ui-toolbar" style="margin-top:12px"><strong>sync status</strong></div>` +
      `<pre class="code">${B.escapeHtml(JSON.stringify(inspect.sync_status, null, 2))}</pre>` +
      `<div class="ui-toolbar" style="margin-top:12px"><strong>transaction stats</strong></div>${txStats}` +
      `<div class="ui-toolbar" style="margin-top:16px"><strong>dial a peer</strong></div>` +
      `<div class="input-group" style="max-width:520px">` +
      `<input type="text" class="input" placeholder="/ip4/host/tcp/port/p2p/peer-id" data-dial-input="${B.escapeHtml(cellId)}" />` +
      `<button type="button" class="btn btn-secondary" data-dial-btn="${B.escapeHtml(cellId)}">dial</button>` +
      `</div><div class="ui-stat-hint" data-dial-result="${B.escapeHtml(cellId)}"></div>` +
      (inspect.marker_ok === false || (B.state.overview.tenants || []).every((t) => !t.cells.includes(cellId))
        ? `<div class="ui-toolbar" style="margin-top:16px"><button type="button" class="btn btn-danger" data-drain-btn="${B.escapeHtml(cellId)}">drain this cell</button>` +
          `<span class="ui-stat-hint" data-drain-result="${B.escapeHtml(cellId)}"></span></div>`
        : "") +
      `</div>`
    );
  }

  function wireRowActions() {
    B.$all("[data-dial-btn]").forEach((btn) => {
      btn.addEventListener("click", () => {
        const cellId = btn.dataset.dialBtn;
        const input = B.$(`[data-dial-input="${cssEscape(cellId)}"]`);
        const result = B.$(`[data-dial-result="${cssEscape(cellId)}"]`);
        const addr = input.value.trim();
        if (!addr) return;
        B.withBusy(btn, "dialing...", async () => {
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
    });
    B.$all("[data-drain-btn]").forEach((btn) => {
      const result = B.$(`[data-drain-result="${cssEscape(btn.dataset.drainBtn)}"]`);
      B.wireTwoClickArm(btn, "confirm drain", () => {
        B.withBusy(btn, "draining...", async () => {
          const cellId = btn.dataset.drainBtn;
          try {
            const response = await B.adminFetch(`/admin/cells/${encodeURIComponent(cellId)}`, { method: "DELETE" });
            if (!response.ok) B.showResult(result, false, await B.describeFailure(response));
            // success: the DELETE's own `publish_cell_change` SSE event
            // re-renders this table with the cell gone; no extra refetch.
          } catch (err) {
            B.showResult(result, false, "request failed: " + err.message);
          }
        });
      });
    });
  }

  function cssEscape(value) {
    return window.CSS && window.CSS.escape ? window.CSS.escape(value) : value.replace(/"/g, '\\"');
  }

  function initToolbar() {
    B.$("#cells-search").addEventListener("input", () => { if (B.state.overview) renderTable(B.state.overview); });
    B.$("#cells-spawn").addEventListener("click", () => {
      const btn = B.$("#cells-spawn");
      const result = B.$("#cells-spawn-result");
      B.withBusy(btn, "spawning...", async () => {
        try {
          const response = await B.adminFetch("/admin/cells", { method: "POST", body: JSON.stringify({ count: 1 }) });
          if (!response.ok) B.showResult(result, false, await B.describeFailure(response));
          else B.showResult(result, true, ""); // success: publish_cell_change re-renders the table
        } catch (err) {
          B.showResult(result, false, "request failed: " + err.message);
        }
      });
    });
  }

  B.onOverview((data) => renderTable(data.overview));
  B.onCellChange(() => { if (B.state.overview) renderTable(B.state.overview); });

  // Cross-view navigation entry point (the mesh panel's node click):
  // expands the named cell's row and loads its inspect panel, exactly
  // as clicking the row's own toggle would.
  B.registerViewEntry("cells", (cellId) => {
    if (!cellId) return;
    if (B.$("#cells-search")) B.$("#cells-search").value = "";
    openRows.add(cellId);
    loadInspect(cellId);
    if (B.state.overview) renderTable(B.state.overview);
  });

  document.addEventListener("DOMContentLoaded", initToolbar);
})();
