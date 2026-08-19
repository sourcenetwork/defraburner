// defraburner dashboard -- Autoscaler view: policy health banner, live
// controls (min/max/cooldown/tick/pause), force-tick, and the decision
// timeline (newest first, stream-appended live).
"use strict";

(function () {
  const B = window.Burner;
  const decisionKeysSeen = new Set();
  let controlsDirty = false; // true while the operator is mid-edit; don't clobber their inputs on a tick

  function renderBanner(policy) {
    const el = B.$("#autoscaler-banner");
    if (!policy) { el.innerHTML = B.banner("info", "no data yet"); return; }
    const healthy = (policy.consecutive_errors || 0) === 0;
    const text = healthy
      ? `policy healthy${policy.last_ok_tick != null ? ` (last ok tick ${policy.last_ok_tick})` : ""}`
      : `policy degraded: ${policy.consecutive_errors} consecutive error(s) -- ${B.escapeHtml(policy.last_error || "no error message")}`;
    el.innerHTML = B.banner(healthy ? "success" : "warning", text);
  }

  function renderControls(control) {
    if (controlsDirty || !control) return;
    B.$("#autoscaler-min-cells").value = control.min_cells;
    B.$("#autoscaler-max-cells").value = control.max_cells;
    B.$("#autoscaler-cooldown").value = control.cooldown_secs;
    B.$("#autoscaler-tick-interval").value = control.tick_interval_secs;
    const toggle = B.$("#autoscaler-pause-toggle");
    toggle.classList.toggle("on", !!control.paused);
    toggle.setAttribute("aria-checked", String(!!control.paused));
  }

  function renderRuntime(runtime) {
    const el = B.$("#autoscaler-runtime");
    if (!el) return;
    if (!runtime) { el.innerHTML = '<div class="ui-stat-value muted">no data yet</div>'; return; }
    const packages = (runtime.registered_packages || [])
      .map((p) => `<div class="row" style="justify-content:space-between"><span class="mono">${B.escapeHtml(p.name)}</span><span class="mono fg-3">${B.escapeHtml(p.content_hash.slice(0, 12))}</span></div>`)
      .join("") || '<div class="fg-3">none registered</div>';
    el.innerHTML =
      `<div class="row" style="justify-content:space-between"><span class="fg-3">mode</span><span class="mono">${B.escapeHtml(runtime.mode)}</span></div>` +
      `<div class="row" style="justify-content:space-between"><span class="fg-3">fuel</span><span class="mono">${runtime.fuel ?? "unlimited"}</span></div>` +
      `<div class="row" style="justify-content:space-between"><span class="fg-3">memory</span><span class="mono">${runtime.memory_bytes ? B.humanizeBytes(runtime.memory_bytes) : "unlimited"}</span></div>` +
      `<div class="row" style="justify-content:space-between"><span class="fg-3">timeout</span><span class="mono">${runtime.timeout_ms ? runtime.timeout_ms + " ms" : "unlimited"}</span></div>` +
      `<div class="ui-section-label" style="margin-top:10px">registered packages</div>${packages}`;
  }

  // Entity markers (visual pass): a decision naming a specific cell or
  // tenant carries that entity's own marker glyph beside its mono id, the
  // same identity shown everywhere else that entity appears.
  function actionChip(action) {
    const kind = action.kind;
    if (kind === "scale_up") return B.chip("accent", "scale_up", { sm: true, dot: false });
    if (kind === "scale_down") {
      const label = action.cell_id
        ? `${B.markerSvg("cell", action.cell_id)} scale_down ${B.escapeHtml(action.cell_id)}`
        : "scale_down";
      return `<span class="chip chip-warning">${label}</span>`;
    }
    if (kind === "place") {
      const cells = (action.cells || []).map((id) => `${B.markerSvg("cell", id)} ${B.escapeHtml(id)}`).join(" ");
      const tenantMarker = action.tenant ? B.markerSvg("tenant", action.tenant) : "";
      const label = action.tenant
        ? `${tenantMarker} place ${B.escapeHtml(action.tenant)}${cells ? " -> " + cells : ""}`
        : "place";
      return `<span class="chip chip-accent">${label}</span>`;
    }
    return B.chip("neutral", "hold", { sm: true, dot: false });
  }

  function clearEmptyPlaceholder() {
    const placeholder = B.$("#decision-timeline-empty");
    if (placeholder) placeholder.remove();
  }

  function renderEntry(entry, prepend) {
    const key = entry.package + ":" + entry.tick;
    if (decisionKeysSeen.has(key)) return;
    decisionKeysSeen.add(key);
    clearEmptyPlaceholder();

    const actions = entry.clamped && entry.clamped.length ? entry.clamped : [{ kind: "hold" }];
    const badges = actions.map((a) => actionChip(a)).join(" ");
    const clampChips = (entry.clamps_applied || []).map((c) => B.chip("warning", c, { sm: true, dot: false })).join(" ");
    const reason = (entry.raw_decision && entry.raw_decision.reason) || entry.error || "-";
    const executedIcon = entry.executed
      ? `<span style="color:var(--state-success)">${B.ICONS.check}</span>`
      : `<span style="color:var(--state-error)">${B.ICONS.x}</span>`;

    const row = B.el(
      `<div class="ui-card" style="margin-bottom:8px">` +
      `<div class="ui-card-body row" style="flex-wrap:wrap;gap:10px">` +
      `<span class="mono fg-3">#${entry.tick}</span>` +
      `<span class="mono fg-3">${B.escapeHtml(entry.package)}</span>` +
      `${badges}` +
      `<span class="fg-2" style="flex:1 1 240px">${B.escapeHtml(reason)}</span>` +
      `${clampChips}` +
      `${executedIcon}` +
      `${entry.error ? `<span class="fg-3 mono">${B.escapeHtml(entry.error)}</span>` : ""}` +
      `</div></div>`
    );
    const timeline = B.$("#decision-timeline");
    if (prepend) timeline.insertBefore(row, timeline.firstChild);
    else timeline.append(row);
  }

  function initControls() {
    ["#autoscaler-min-cells", "#autoscaler-max-cells", "#autoscaler-cooldown", "#autoscaler-tick-interval"].forEach((sel) => {
      const input = B.$(sel);
      input.addEventListener("focus", () => { controlsDirty = true; });
    });

    B.$("#autoscaler-save").addEventListener("click", () => {
      const btn = B.$("#autoscaler-save");
      const result = B.$("#autoscaler-save-result");
      const patch = {
        min_cells: Number(B.$("#autoscaler-min-cells").value),
        max_cells: Number(B.$("#autoscaler-max-cells").value),
        cooldown_secs: Number(B.$("#autoscaler-cooldown").value),
        tick_interval_secs: Number(B.$("#autoscaler-tick-interval").value),
      };
      B.withBusy(btn, "saving...", async () => {
        try {
          const response = await B.adminFetch("/admin/autoscaler", { method: "PUT", body: JSON.stringify(patch) });
          B.showResult(result, response.ok, response.ok ? "saved" : await B.describeFailure(response));
          if (response.ok) controlsDirty = false; // keep edits sticky on failure so nothing typed is lost
        } catch (err) {
          B.showResult(result, false, "request failed: " + err.message);
        }
      });
    });

    B.$("#autoscaler-pause-toggle").addEventListener("click", () => {
      const toggle = B.$("#autoscaler-pause-toggle");
      const result = B.$("#autoscaler-pause-result");
      const nextPaused = !toggle.classList.contains("on");
      controlsDirty = true;
      toggle.setAttribute("aria-busy", "true");
      (async () => {
        try {
          const response = await B.adminFetch("/admin/autoscaler", { method: "PUT", body: JSON.stringify({ paused: nextPaused }) });
          if (response.ok) {
            toggle.classList.toggle("on", nextPaused);
            toggle.setAttribute("aria-checked", String(nextPaused));
            B.showResult(result, true, "");
          } else {
            B.showResult(result, false, await B.describeFailure(response));
          }
        } catch (err) {
          B.showResult(result, false, "request failed: " + err.message);
        } finally {
          toggle.removeAttribute("aria-busy");
          controlsDirty = false;
        }
      })();
    });

    B.$("#autoscaler-force-tick").addEventListener("click", () => {
      const btn = B.$("#autoscaler-force-tick");
      const result = B.$("#autoscaler-tick-result");
      B.withBusy(btn, "ticking...", async () => {
        try {
          const response = await B.adminFetch("/admin/autoscaler/tick", { method: "POST" });
          B.showResult(result, response.ok, response.ok ? "tick forced" : await B.describeFailure(response));
        } catch (err) {
          B.showResult(result, false, "request failed: " + err.message);
        }
      });
    });
  }

  B.onOverview((data) => {
    renderBanner(data.overview.policy);
    renderControls(data.overview.autoscaler_control);
    renderRuntime(data.overview.runtime);
    for (const entry of data.overview.decisions || []) renderEntry(entry, false);
  });
  B.onDecision((entry) => renderEntry(entry, true));

  document.addEventListener("DOMContentLoaded", initControls);
})();
