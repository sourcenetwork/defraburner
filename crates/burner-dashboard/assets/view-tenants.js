// defraburner dashboard -- Tenants view: DataTable with per-row rotate
// token / admission override / drop / drop-and-retire, plus a
// create-tenant card. Every mutation goes through /admin/tenants/* with
// the admin bearer (this is admin control, distinct from the Data view's
// tenant-token data plane).
"use strict";

(function () {
  const B = window.Burner;

  function statusChip(status) {
    return status === "placed" ? B.chip("success", "placed", { sm: true }) : B.chip("warning", "pending", { sm: true });
  }

  // Bug-fix round: a tenant's own health (never another tenant's -- see
  // reconcile's per-tenant isolation) plus the honest single-cell note.
  // Read via B.tenantHealthLine so the mesh panel's own cluster caption
  // can state the identical fact about the identical condition -- one
  // function, not two copies that could drift.
  B.tenantHealthLine = function tenantHealthLine(tenant) {
    const parts = [];
    const health = tenant.health || { state: "ok" };
    if (health.state === "degraded") {
      parts.push(B.banner("error", `degraded: ${B.escapeHtml(health.reason || "unknown reason")}`));
    }
    const note = B.singleCellNote((tenant.cells || []).length);
    if (note) parts.push(`<div class="ui-stat-hint">${B.escapeHtml(note)}</div>`);
    return parts.join("");
  };

  function renderTable(overview) {
    const tenants = overview.tenants || [];
    const admissionByTenant = new Map((overview.tenant_admission || []).map((a) => [a.tenant, a]));
    B.$("#tenants-count").textContent = String(tenants.length);

    if (tenants.length === 0) {
      B.$("#tenants-tbody").innerHTML =
        '<tr><td colspan="6" class="ui-data-table-empty">no tenants yet -- create one below to get started</td></tr>';
      return;
    }

    const rows = tenants
      .map((tenant) => {
        const admission = admissionByTenant.get(tenant.name);
        const cellChips = (tenant.cells || []).map((c) => B.chip("neutral", c, { sm: true, mono: true, dot: false })).join(" ") || '<span class="fg-3">-</span>';
        return (
          `<tr data-tenant-row="${B.escapeHtml(tenant.name)}">` +
          `<td class="mono">${B.markerSvg("tenant", tenant.name)} ${B.escapeHtml(tenant.name)}</td>` +
          `<td>${tenant.replicas}</td>` +
          `<td>${cellChips}</td>` +
          `<td>${statusChip(tenant.status)}${tenant.health && tenant.health.state === "degraded" ? " " + B.dot("error") : ""}</td>` +
          `<td class="mono">${admission ? admission.allowed : "no data yet"}</td>` +
          `<td class="mono">${admission ? admission.rejected : "no data yet"}</td>` +
          `</tr>` +
          `<tr class="ui-data-table-detail" data-tenant-actions="${B.escapeHtml(tenant.name)}">` +
          `<td colspan="6">${renderActions(tenant)}</td></tr>`
        );
      })
      .join("");
    // Same reason as the Cells table: the per-tenant admission rate and
    // burst inputs live in this markup, so a plain tick rebuild erased
    // an override while it was still being typed.
    const tbody = B.$("#tenants-tbody");
    B.preserveVolatile(tbody, () => {
      tbody.innerHTML = rows;
      wireActions(tenants);
    });
  }

  function renderActions(tenant) {
    const override = tenant.admission;
    return (
      B.tenantHealthLine(tenant) +
      `<div class="row" style="flex-wrap:wrap;gap:16px;padding:10px 4px">` +
      `<button type="button" class="btn btn-secondary btn-sm" data-rotate="${B.escapeHtml(tenant.name)}">rotate token</button>` +
      `<span class="row" style="gap:6px">` +
      `<input type="number" class="input input-mono" style="width:110px" placeholder="rate/sec" value="${override ? override.rate_per_sec : ""}" data-admission-rate="${B.escapeHtml(tenant.name)}" />` +
      `<input type="number" class="input input-mono" style="width:90px" placeholder="burst" value="${override ? override.burst : ""}" data-admission-burst="${B.escapeHtml(tenant.name)}" />` +
      `<button type="button" class="btn btn-secondary btn-sm" data-admission-save="${B.escapeHtml(tenant.name)}">save admission</button>` +
      `</span>` +
      `<button type="button" class="btn btn-secondary btn-sm" data-drop="${B.escapeHtml(tenant.name)}">drop (keeps data)</button>` +
      `<button type="button" class="btn btn-danger btn-sm" data-retire="${B.escapeHtml(tenant.name)}" data-cells="${B.escapeHtml((tenant.cells || []).join(","))}">drop &amp; retire cells</button>` +
      `<span class="ui-stat-hint" data-tenant-result="${B.escapeHtml(tenant.name)}"></span>` +
      `</div>`
    );
  }

  function q(attr, value) {
    return B.$(`[${attr}="${window.CSS && window.CSS.escape ? window.CSS.escape(value) : value}"]`);
  }

  function wireActions(tenants) {
    B.$all("[data-rotate]").forEach((btn) => {
      btn.addEventListener("click", () => {
        const name = btn.dataset.rotate;
        const result = q("data-tenant-result", name);
        B.withBusy(btn, "rotating...", async () => {
          try {
            const response = await B.adminFetch(`/admin/tenants/${encodeURIComponent(name)}/rotate-token`, { method: "POST" });
            if (!response.ok) { B.showResult(result, false, await B.describeFailure(response)); return; }
            const body = await response.json();
            B.showResult(result, true, "");
            showTokenModal(name, body.token);
          } catch (err) {
            B.showResult(result, false, "request failed: " + err.message);
          }
        });
      });
    });
    B.$all("[data-admission-save]").forEach((btn) => {
      btn.addEventListener("click", () => {
        const name = btn.dataset.admissionSave;
        const rate = q("data-admission-rate", name).value.trim();
        const burst = q("data-admission-burst", name).value.trim();
        const body = rate && burst ? { rate_per_sec: Number(rate), burst: Number(burst) } : {};
        const result = q("data-tenant-result", name);
        B.withBusy(btn, "saving...", async () => {
          try {
            const response = await B.adminFetch(`/admin/tenants/${encodeURIComponent(name)}/admission`, { method: "PUT", body: JSON.stringify(body) });
            B.showResult(result, response.ok, response.ok ? "admission saved" : await B.describeFailure(response));
          } catch (err) {
            B.showResult(result, false, "request failed: " + err.message);
          }
        });
      });
    });
    B.$all("[data-drop]").forEach((btn) => {
      const result = q("data-tenant-result", btn.dataset.drop);
      B.wireTwoClickArm(btn, "confirm drop", () => {
        B.withBusy(btn, "dropping...", async () => {
          const name = btn.dataset.drop;
          try {
            const response = await B.adminFetch(`/admin/tenants/${encodeURIComponent(name)}`, { method: "DELETE" });
            if (!response.ok) { B.showResult(result, false, await B.describeFailure(response)); return; }
            const body = await response.json();
            B.showResult(result, true, `dropped; data remains on: ${(body.data_remains_on_cells || []).join(", ") || "(no cells)"}`);
          } catch (err) {
            B.showResult(result, false, "request failed: " + err.message);
          }
        });
      });
    });
    B.$all("[data-retire]").forEach((btn) => {
      const cells = btn.dataset.cells ? btn.dataset.cells.split(",") : [];
      const result = q("data-tenant-result", btn.dataset.retire);
      btn.title = cells.length ? `will delete data on: ${cells.join(", ")}` : "no cells to retire";
      B.wireTwoClickArm(btn, `delete ${cells.join(", ") || "cells"}?`, () => {
        B.withBusy(btn, "retiring...", async () => {
          const name = btn.dataset.retire;
          try {
            const response = await B.adminFetch(`/admin/tenants/${encodeURIComponent(name)}?retire=true`, { method: "DELETE" });
            if (!response.ok) B.showResult(result, false, await B.describeFailure(response));
            else B.showResult(result, true, "dropped and retired");
          } catch (err) {
            B.showResult(result, false, "request failed: " + err.message);
          }
        });
      });
    });
  }

  // Exposed (mesh panel's cluster popover reuses this exact modal for a
  // rotate-token action, rather than a second copy).
  B.showTenantTokenModal = showTokenModal;
  function showTokenModal(name, token) {
    const scrim = B.el(`<div class="scrim"></div>`);
    const modal = B.el(
      `<div class="modal" role="dialog" aria-modal="true">` +
      `<div class="modal-h"><h2>Token for ${B.escapeHtml(name)}</h2><div class="sub">shown once -- copy it now</div></div>` +
      `<div class="modal-b"><pre class="code" id="token-modal-value">${B.escapeHtml(token)}</pre></div>` +
      `<div class="modal-f"><button type="button" class="btn btn-secondary" id="token-modal-copy">copy</button><button type="button" class="btn btn-primary" id="token-modal-close">close</button></div>` +
      `</div>`
    );
    document.body.append(scrim, modal);
    function close() { scrim.remove(); modal.remove(); }
    scrim.addEventListener("click", close);
    modal.querySelector("#token-modal-close").addEventListener("click", close);
    modal.querySelector("#token-modal-copy").addEventListener("click", () => {
      navigator.clipboard?.writeText(token).catch(() => {});
    });
  }

  function initCreateForm() {
    const form = B.$("#tenant-create-form");
    if (!form) return;
    form.addEventListener("submit", (event) => {
      event.preventDefault();
      const submitBtn = B.$("#tenant-create-submit");
      const result = B.$("#tenant-create-result");
      B.withBusy(submitBtn, "creating...", async () => {
        const name = B.$("#tenant-create-name").value.trim();
        const schema = B.$("#tenant-create-schema").value;
        const replicas = Number(B.$("#tenant-create-replicas").value || "1");
        try {
          const response = await B.adminFetch("/admin/tenants", {
            method: "POST",
            body: JSON.stringify({ name, schema_sdl: schema, replicas }),
          });
          if (!response.ok) { B.showResult(result, false, await B.describeFailure(response)); return; }
          const body = await response.json();
          B.showResult(result, true, "");
          showTokenModal(body.name, body.token);
          form.reset();
        } catch (err) {
          B.showResult(result, false, "request failed: " + err.message);
        }
      });
    });
  }

  B.onOverview((data) => renderTable(data.overview));
  document.addEventListener("DOMContentLoaded", initCreateForm);
})();
