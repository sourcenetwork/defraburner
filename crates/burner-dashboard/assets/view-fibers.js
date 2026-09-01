// defraburner dashboard -- Databases view: every cell's persistent wasm
// DefraDB. A fiber is not a separate thing from a cell (D40): each cell
// owns exactly one, sharing its id and its lifetime, so this view has no
// ignite or drain of its own. The Cells view owns that lifecycle; this is
// where you apply schema and run queries against a cell's database.
//
// Each database is the whole DefraDB engine compiled to wasm32-wasip1,
// loaded from the AOT-compiled packages/defradb .afb, persisting to the
// cell's own directory so it survives a restart with its data.
"use strict";

(function () {
  const B = window.Burner;

  // Cell ids come from the overview stream; a fiber is not a separate
  // thing to list (D40), it is the database belonging to a cell.
  let listing = { available: false, running: [] };
  let selected = null;
  let polling = null;

  function renderBanner() {
    const el = B.$("#fibers-banner");
    if (!el) return;
    if (listing.available) {
      const n = listing.running.length;
      el.innerHTML = B.banner(
        "success",
        `${n} cell${n === 1 ? "" : "s"} running; each owns one persistent wasm DefraDB`
      );
    } else {
      el.innerHTML = B.banner("warning", "no cells running");
    }
  }

  function renderList() {
    const el = B.$("#fibers-list");
    if (!el) return;
    if (!listing.available) {
      el.innerHTML = '<div class="fg-3">no cells running.</div>';
      return;
    }
    if (!listing.running.length) {
      el.innerHTML = '<div class="fg-3">no cells running.</div>';
      return;
    }
    B.preserveVolatile(el, () => {
      el.innerHTML = listing.running
        .map((id) => {
          const active = id === selected ? " active" : "";
          return (
            `<div class="row${active}" style="justify-content:space-between;align-items:center;padding:6px 0">` +
            `<button type="button" class="ui-link mono" data-fiber-select="${B.escapeHtml(id)}">${B.escapeHtml(id)}</button>` +
            `<span class="row" style="gap:6px">` +
            `<span class="ui-chip" data-fiber-engine="wasm">wasm</span>` +
            `</span></div>`
          );
        })
        .join("");
    });
  }

  function renderSelected() {
    const el = B.$("#fibers-selected");
    if (!el) return;
    el.textContent = selected ? selected : "pick a cell above";
  }

  async function refresh() {
    try {
      // Cells are the list; each one owns exactly one wasm database.
      const response = await B.adminFetch("/admin/api/overview");
      if (!response.ok) return;
      const overview = await response.json();
      const ids = (overview.cells || overview.cell_details || [])
        .map((c) => (typeof c === "string" ? c : c.id))
        .filter(Boolean);
      listing = { available: ids.length > 0, running: ids };
      if (selected && !listing.running.includes(selected)) selected = null;
      renderBanner();
      renderList();
      renderSelected();
    } catch (_) {
      // A failed poll is not worth a visible error: the connection state
      // indicator already reports the transport, and the next tick retries.
    }
  }

  function wire() {
    const schemaBtn = B.$("#fibers-schema-apply");
    if (schemaBtn) {
      schemaBtn.addEventListener("click", async () => {
        const result = B.$("#fibers-schema-result");
        if (!selected) {
          B.showResult(result, false, "pick a cell first");
          return;
        }
        const sdl = (B.$("#fibers-schema-sdl").value || "").trim();
        if (!sdl) {
          B.showResult(result, false, "enter some SDL");
          return;
        }
        await B.withBusy(schemaBtn, async () => {
          try {
            const response = await B.adminFetch(
              `/admin/cells/${encodeURIComponent(selected)}/db/schema`,
              { method: "POST", body: JSON.stringify({ sdl }) }
            );
            const body = await response.json().catch(() => ({}));
            if (response.ok) {
              const added = (body.data && body.data.collections_added) || [];
              B.showResult(result, true, `added ${added.join(", ") || "nothing"}`);
            } else {
              B.showResult(result, false, body.error || `HTTP ${response.status}`);
            }
          } catch (err) {
            B.showResult(result, false, "request failed: " + err.message);
          }
        });
      });
    }

    const runBtn = B.$("#fibers-query-run");
    if (runBtn) {
      runBtn.addEventListener("click", async () => {
        const result = B.$("#fibers-query-result");
        const out = B.$("#fibers-query-output");
        if (!selected) {
          B.showResult(result, false, "pick a cell first");
          return;
        }
        const graphql = (B.$("#fibers-query-graphql").value || "").trim();
        if (!graphql) {
          B.showResult(result, false, "enter a query");
          return;
        }
        const mutate = B.$("#fibers-query-mutate").checked;
        await B.withBusy(runBtn, async () => {
          try {
            const response = await B.adminFetch(
              `/admin/cells/${encodeURIComponent(selected)}/db/query`,
              { method: "POST", body: JSON.stringify({ graphql, mutate }) }
            );
            const body = await response.json().catch(() => ({}));
            if (response.ok) {
              B.showResult(result, true, "ok");
              out.textContent = JSON.stringify(body.data, null, 2);
            } else {
              // The guest's own message, not a generic failure: a bad query
              // is the operator's to fix and they need to see why.
              B.showResult(result, false, body.error || `HTTP ${response.status}`);
              out.textContent = body.error || "";
            }
          } catch (err) {
            B.showResult(result, false, "request failed: " + err.message);
          }
        });
      });
    }

    // Delegated: the list is rebuilt on every poll, so per-row listeners
    // would be re-attached (or leak) each time.
    document.addEventListener("click", async (event) => {
      const pick = event.target.closest("[data-fiber-select]");
      if (pick) {
        selected = pick.dataset.fiberSelect;
        renderList();
        renderSelected();
        return;
      }
    });
  }

  B.registerViewEntry("fibers", () => {
    refresh();
    if (!polling) polling = setInterval(refresh, 2000);
  });

  document.addEventListener("DOMContentLoaded", () => {
    wire();
  });
})();
