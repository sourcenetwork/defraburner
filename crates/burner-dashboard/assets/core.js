// defraburner dashboard -- core: state, pure helpers (ported verbatim
// from design/dna/primitives.jsx per the console round spec), theme,
// token gate, realtime SSE, navigation, and small DOM-builder helpers
// every view module reuses. Vanilla JS, no framework, no build step, no
// external requests (CSP-clean offline). Loaded first; every other
// dashboard.assets/*.js file depends on `window.Burner`.
"use strict";

(function () {
  const TOKEN_KEY = "defraburner_admin_token";
  const THEME_KEY = "burner.theme";
  const RING_TICKS = 120;
  const RECONNECT_MIN_MS = 1000;
  const RECONNECT_MAX_MS = 10000;
  const FAILURES_UNTIL_ERROR = 3;

  // Fixed series color order (D24, validator-approved); >4 cells fold
  // into "other" in --series-other, stated in the legend.
  const SERIES_COLORS = ["#6366f1", "#ea580c", "#059669", "#a855f7"];
  const SERIES_OTHER_COLOR = "var(--fg-3)";

  const state = {
    token: null,
    overview: null,
    connection: "connecting", // connecting | streaming | reconnecting | error
    reconnectAttempts: 0,
    // cell id -> ring buffer of {ts, count} samples (raw cumulative
    // request counts; views derive rates from consecutive deltas).
    cellRequestHistory: new Map(),
    // cell id -> stable color assignment (fixed at first sight, freed
    // when a cell disappears so a later cell can reuse a freed slot).
    cellColors: new Map(),
    decisionKeysSeen: new Set(),
    lastOverviewSample: null,
    runInterval: null,
  };

  // ===== Pure helpers (ported verbatim from primitives.jsx) ===========
  function clampPct(value) {
    const n = typeof value === "number" ? value : Number(value);
    if (!Number.isFinite(n)) return n === Infinity ? 100 : 0;
    if (n < 0) return 0;
    if (n > 100) return 100;
    return n;
  }
  function pctLabel(value, opts) {
    if (value == null) return null;
    return clampPct(value).toFixed((opts && opts.digits) ?? 0) + "%";
  }
  function ringGeom(size, thickness) {
    const r = (size - thickness) / 2;
    return { r, cx: size / 2, cy: size / 2, circumference: 2 * Math.PI * r };
  }
  function ringDashOffset(value, r) {
    const c = 2 * Math.PI * r;
    return value == null ? c : c * (1 - clampPct(value) / 100);
  }
  function stackedSegments(parts, total) {
    const list = parts || [];
    const vals = list.map((p) => (Number.isFinite(p.value) && p.value > 0 ? p.value : 0));
    const sum = vals.reduce((a, b) => a + b, 0);
    const denom = Number.isFinite(total) && total > 0 ? total : sum;
    return list.map((p, i) => ({
      label: p.label,
      color: p.color,
      value: vals[i],
      pct: denom > 0 ? (vals[i] / denom) * 100 : 0,
    }));
  }
  function segmentsFilled(done, total) {
    const t = Number.isFinite(total) && total > 0 ? Math.floor(total) : 0;
    const d = Number.isFinite(done) ? Math.floor(done) : 0;
    if (d < 0) return 0;
    return d > t ? t : d;
  }

  // ===== Small generic helpers =========================================
  // Extracts the peer id from one `connected_peers()` entry. Upstream
  // resolves every connected peer to an ADDRESS before listing it, so an
  // entry is `/ip4/127.0.0.1/tcp/9172/p2p/12D3Koo...`, not a bare id;
  // matching an entry against a bare peer id by equality never succeeds.
  // Mirrors `burner_mesh::peer_id_of` on the Rust side: both sides of a
  // peer comparison must go through one of these two functions.
  function peerIdOf(entry) {
    const text = String(entry == null ? "" : entry).trim();
    const marker = text.lastIndexOf("/p2p/");
    if (marker === -1) return text;
    return text.slice(marker + 5).replace(/\/+$/, "");
  }

  function escapeHtml(value) {
    return String(value == null ? "" : value).replace(/[&<>"']/g, (ch) => {
      switch (ch) {
        case "&": return "&amp;";
        case "<": return "&lt;";
        case ">": return "&gt;";
        case '"': return "&quot;";
        default: return "&#39;";
      }
    });
  }
  function noDataYet() { return '<span class="ui-stat-value muted">no data yet</span>'; }
  function humanizeBytes(bytes) {
    if (bytes === undefined || bytes === null) return "no data yet";
    const units = ["B", "KiB", "MiB", "GiB", "TiB"];
    let value = bytes, unit = 0;
    while (value >= 1024 && unit < units.length - 1) { value /= 1024; unit += 1; }
    return value.toFixed(unit === 0 ? 0 : 1) + " " + units[unit];
  }
  function el(html) {
    const t = document.createElement("template");
    t.innerHTML = html.trim();
    return t.content.firstElementChild;
  }
  function $(selector, root) { return (root || document).querySelector(selector); }
  function $all(selector, root) { return Array.from((root || document).querySelectorAll(selector)); }

  // ===== Component builders (DOM/class contract from primitives.jsx) ===
  function chip(kind, label, opts) {
    opts = opts || {};
    const pulse = opts.pulse ? '<span class="d pulse-dot"></span>' : '<span class="d"></span>';
    const mono = opts.mono ? " chip-mono" : "";
    const sz = opts.sm ? " chip-sm" : "";
    return `<span class="chip chip-${kind}${mono}${sz}">${opts.dot === false ? "" : pulse}${escapeHtml(label)}</span>`;
  }
  function dot(kind) { return `<span class="dot dot-${kind}"></span>`; }

  function progressBar(opts) {
    opts = opts || {};
    const indeterminate = opts.value === null || opts.value === undefined;
    const pct = indeterminate ? null : clampPct(opts.value);
    const variant = opts.variant || "accent";
    const sizeClass = opts.size && opts.size !== "md" ? ` ${opts.size}` : "";
    const fillClasses = ["progress-fill", variant];
    if (indeterminate) fillClasses.push("indeterminate");
    if (opts.striped) fillClasses.push("striped");
    if (opts.animate) fillClasses.push("animate");
    if (opts.scan) fillClasses.push("scan");
    const labelRow = opts.label != null
      ? `<div class="progress-label"><span class="name">${escapeHtml(opts.label)}</span><span class="pct ${variant}">${indeterminate ? "" : pctLabel(pct)}</span></div>`
      : "";
    const buffer = !indeterminate && opts.buffer != null
      ? `<span class="progress-buffer" style="width:${clampPct(opts.buffer)}%"></span>` : "";
    const widthStyle = indeterminate ? "" : `style="width:${pct}%"`;
    return (
      `<div class="progress">${labelRow}` +
      `<div class="progress-track${sizeClass}" role="progressbar" aria-valuenow="${indeterminate ? "" : Math.round(pct)}">` +
      `${buffer}<div class="${fillClasses.join(" ")}" ${widthStyle}></div></div></div>`
    );
  }

  function segmentedProgress(opts) {
    opts = opts || {};
    const total = Number.isFinite(opts.total) && opts.total > 0 ? Math.floor(opts.total) : 0;
    const filled = segmentsFilled(opts.done, total);
    const variant = opts.variant || "accent";
    const tall = opts.size === "lg" ? " tall" : "";
    const ticks = Array.from({ length: total }, (_, i) =>
      `<span class="tick${i < filled ? ` on ${variant}` : ""}"></span>`
    ).join("");
    const labelRow = opts.label != null
      ? `<div class="progress-label"><span class="name">${escapeHtml(opts.label)}</span><span class="pct ${variant}">${filled}/${total}</span></div>`
      : "";
    return `<div class="progress">${labelRow}<div class="progress-seg${tall}">${ticks}</div></div>`;
  }

  function ring(opts) {
    opts = opts || {};
    const size = opts.size || 72;
    const thickness = opts.thickness || 8;
    const variant = opts.variant || "accent";
    const indeterminate = opts.value === null || opts.value === undefined;
    const pct = indeterminate ? null : clampPct(opts.value);
    const { r, cx, cy, circumference } = ringGeom(size, thickness);
    const centre = opts.label != null ? escapeHtml(opts.label) : (opts.showValue !== false && !indeterminate ? pctLabel(pct) : "");
    const dashOffset = indeterminate ? circumference * 0.75 : ringDashOffset(pct, r);
    return (
      `<div class="progress-ring-wrap" style="width:${size}px;height:${size}px">` +
      `<svg class="ring-svg ${variant}${indeterminate ? " is-spin" : ""}" width="${size}" height="${size}" viewBox="0 0 ${size} ${size}">` +
      `<circle class="track" cx="${cx}" cy="${cy}" r="${r}" fill="none" stroke-width="${thickness}"></circle>` +
      `<circle class="meter" cx="${cx}" cy="${cy}" r="${r}" fill="none" stroke-width="${thickness}" stroke-dasharray="${circumference}" stroke-dashoffset="${dashOffset}"></circle>` +
      `</svg>${centre ? `<div class="ring-label">${centre}</div>` : ""}</div>`
    );
  }

  function statCard(opts) {
    opts = opts || {};
    const variant = opts.variant ? ` ${opts.variant}` : "";
    const value = opts.noData ? '<span class="ui-stat-value muted">no data yet</span>' : `<span class="ui-stat-value">${opts.value}${opts.unit ? `<span class="ui-stat-unit">${escapeHtml(opts.unit)}</span>` : ""}</span>`;
    return (
      `<div class="ui-stat${variant}">${opts.noData ? value : value}` +
      `<div class="ui-stat-label">${escapeHtml(opts.label)}</div>` +
      (opts.hint ? `<div class="ui-stat-hint">${escapeHtml(opts.hint)}</div>` : "") +
      `</div>`
    );
  }

  function banner(kind, text, extra) {
    return `<div class="banner ${kind}"><div style="flex:1">${text}</div>${extra || ""}</div>`;
  }

  // ===== Busy state / result display (console round, completeness
  // contract): the one shared implementation every mutating control uses
  // for its in-flight state and its outcome, so no view hand-rolls its
  // own disabled/spinner bookkeeping or swallows a failure silently. =====
  async function withBusy(button, busyLabel, fn) {
    const originalLabel = button.textContent;
    const originalDisabled = button.disabled;
    button.disabled = true;
    if (busyLabel) button.textContent = busyLabel;
    try {
      await fn();
    } finally {
      button.disabled = originalDisabled;
      if (busyLabel) button.textContent = originalLabel;
    }
  }
  // Renders a mutation's outcome verbatim (the server's own text, never a
  // paraphrase) in the error color on failure, so a 409's tenant name, a
  // 429's Retry-After, and a 503's timeout reason all reach the operator
  // exactly as the gateway wrote them.
  function showResult(el, ok, text) {
    if (!el) return;
    el.textContent = text;
    el.style.color = ok ? "" : "var(--state-error)";
  }
  async function describeFailure(response) {
    const text = await response.text().catch(() => `HTTP ${response.status}`);
    if (response.status === 429) {
      const retryAfter = response.headers.get("Retry-After");
      return `429 rejected by admission -- retry after ${retryAfter || "?"}s`;
    }
    if (response.status === 503) return `503 ${text}`;
    if (response.status === 409) return `409 ${text}`;
    return text || `HTTP ${response.status}`;
  }

  // ===== Two-click arm (drain / drop / delete confirmation) ============
  // First click arms (visually flags red + relabels); a second click
  // within the window fires `onConfirm`; anything else (blur, timeout,
  // another arm elsewhere) disarms. No native `confirm()` dialogs.
  function wireTwoClickArm(button, armedLabel, onConfirm) {
    const originalLabel = button.textContent;
    let armed = false;
    let timer = null;
    function disarm() {
      armed = false;
      button.classList.remove("armed");
      button.textContent = originalLabel;
      if (timer) { clearTimeout(timer); timer = null; }
    }
    button.addEventListener("click", () => {
      if (!armed) {
        armed = true;
        button.classList.add("armed");
        button.textContent = armedLabel;
        timer = setTimeout(disarm, 4000);
        return;
      }
      disarm();
      onConfirm();
    });
    button.addEventListener("blur", disarm);
  }

  // ===== Theme ==========================================================
  function readTheme() {
    try {
      const q = new URL(window.location.href).searchParams.get("theme");
      if (q === "light" || q === "dark") return q;
      const raw = window.localStorage.getItem(THEME_KEY);
      if (raw === "light" || raw === "dark") return raw;
    } catch (err) { /* localStorage unavailable; fall through to default */ }
    return "dark"; // D24: dark is the default theme.
  }
  function applyTheme(theme) {
    if (theme === "light") document.documentElement.setAttribute("data-theme", "light");
    else document.documentElement.setAttribute("data-theme", "dark");
    try { window.localStorage.setItem(THEME_KEY, theme); } catch (err) { /* ignore */ }
  }
  function initTheme() {
    let theme = readTheme();
    applyTheme(theme);
    const toggle = $("#theme-toggle");
    if (!toggle) return;
    const render = () => {
      toggle.innerHTML = theme === "dark" ? ICONS.sun : ICONS.moon;
      toggle.setAttribute("aria-label", `Switch to ${theme === "dark" ? "light" : "dark"} mode`);
      toggle.title = toggle.getAttribute("aria-label");
    };
    render();
    toggle.addEventListener("click", () => {
      theme = theme === "dark" ? "light" : "dark";
      applyTheme(theme);
      render();
    });
  }

  // Inline static SVGs (lucide-style, stroke 1.75), only the icons used.
  const ICONS = {
    sun: '<svg class="ico" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="4"/><path d="M12 2v2"/><path d="M12 20v2"/><path d="m4.93 4.93 1.41 1.41"/><path d="m17.66 17.66 1.41 1.41"/><path d="M2 12h2"/><path d="M20 12h2"/><path d="m6.34 17.66-1.41 1.41"/><path d="m19.07 4.93-1.41 1.41"/></svg>',
    moon: '<svg class="ico" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round"><path d="M21 12.79A9 9 0 1 1 11.21 3 7 7 0 0 0 21 12.79z"/></svg>',
    search: '<svg class="ico" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round"><circle cx="11" cy="11" r="8"/><path d="m21 21-4.3-4.3"/></svg>',
    chevronRight: '<svg class="ico" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round"><path d="m9 18 6-6-6-6"/></svg>',
    chevronDown: '<svg class="ico" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round"><path d="m6 9 6 6 6-6"/></svg>',
    copy: '<svg class="ico" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round"><rect width="14" height="14" x="8" y="8" rx="0"/><path d="M4 16c-1.1 0-2-.9-2-2V4c0-1.1.9-2 2-2h10c1.1 0 2 .9 2 2"/></svg>',
    check: '<svg class="ico" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round"><path d="M20 6 9 17l-5-5"/></svg>',
    x: '<svg class="ico" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round"><path d="M18 6 6 18"/><path d="m6 6 12 12"/></svg>',
    plus: '<svg class="ico" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round"><path d="M5 12h14"/><path d="M12 5v14"/></svg>',
    server: '<svg class="ico" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round"><rect width="20" height="8" x="2" y="2" rx="0"/><rect width="20" height="8" x="2" y="14" rx="0"/><path d="M6 6h.01"/><path d="M6 18h.01"/></svg>',
    users: '<svg class="ico" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round"><path d="M16 21v-2a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v2"/><circle cx="9" cy="7" r="4"/><path d="M22 21v-2a4 4 0 0 0-3-3.87"/><path d="M16 3.13a4 4 0 0 1 0 7.75"/></svg>',
    gauge: '<svg class="ico" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round"><path d="m12 14 4-4"/><path d="M3.34 19a10 10 0 1 1 17.32 0"/></svg>',
    network: '<svg class="ico" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round"><rect x="16" y="16" width="6" height="6" rx="0"/><rect x="2" y="16" width="6" height="6" rx="0"/><rect x="9" y="2" width="6" height="6" rx="0"/><path d="M5 16v-3a1 1 0 0 1 1-1h12a1 1 0 0 1 1 1v3"/><path d="M12 12V8"/></svg>',
    terminal: '<svg class="ico" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round"><polyline points="4 17 10 11 4 5"/><line x1="12" x2="20" y1="19" y2="19"/></svg>',
    database: '<svg class="ico" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round"><ellipse cx="12" cy="5" rx="9" ry="3"/><path d="M3 5v14a9 3 0 0 0 18 0V5"/><path d="M3 12a9 3 0 0 0 18 0"/></svg>',
    refresh: '<svg class="ico" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round"><path d="M3 12a9 9 0 0 1 15-6.7L21 8"/><path d="M21 3v5h-5"/><path d="M21 12a9 9 0 0 1-15 6.7L3 16"/><path d="M8 16H3v5"/></svg>',
    plug: '<svg class="ico" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round"><path d="M12 22v-5"/><path d="M9 8V2"/><path d="M15 8V2"/><path d="M18 8v5a4 4 0 0 1-4 4h-4a4 4 0 0 1-4-4V8Z"/></svg>',
  };

  // ===== Token gate ======================================================
  function showGate(message) {
    $("#app").hidden = true;
    $("#token-gate").hidden = false;
    const error = $("#token-error");
    if (message) { error.textContent = message; error.hidden = false; } else { error.hidden = true; }
  }
  function showApp() { $("#token-gate").hidden = true; $("#app").hidden = false; }

  async function tryToken(token) {
    const response = await fetch("/admin/api/overview", { headers: { Authorization: "Bearer " + token } });
    if (response.status === 401) return false;
    if (!response.ok) throw new Error("unexpected status " + response.status);
    return true;
  }
  function onUnauthorized() {
    window.localStorage.removeItem(TOKEN_KEY);
    state.token = null;
    showGate("token rejected; enter a valid admin token");
  }
  async function submitToken(token) {
    token = (token || "").trim();
    if (!token) return;
    try {
      if (!(await tryToken(token))) { showGate("token rejected; enter a valid admin token"); return; }
    } catch (err) {
      showGate("could not reach the gateway: " + err.message);
      return;
    }
    window.localStorage.setItem(TOKEN_KEY, token);
    state.token = token;
    showApp();
    Burner.connectStream();
  }
  function initTokenGate() {
    $("#token-submit").addEventListener("click", () => submitToken($("#token-input").value));
    $("#token-input").addEventListener("keydown", (event) => { if (event.key === "Enter") submitToken($("#token-input").value); });

    // D21: a one-time ?token= URL param bootstraps the session (from
    // `up`'s printed dashboard link); stripped from history immediately
    // after being read so it never lingers in browser history/logs.
    let urlToken = null;
    try {
      const url = new URL(window.location.href);
      urlToken = url.searchParams.get("token");
      if (urlToken) {
        url.searchParams.delete("token");
        window.history.replaceState({}, "", url.toString());
      }
    } catch (err) { /* ignore */ }

    const stored = urlToken || window.localStorage.getItem(TOKEN_KEY);
    if (stored) { submitToken(stored); } else { showGate(null); }
  }

  // ===== Connection chip ================================================
  function setConnectionState(next) {
    state.connection = next;
    const chipEl = $("#connection-chip");
    if (!chipEl) return;
    if (next === "streaming") {
      chipEl.className = "chip chip-success";
      chipEl.innerHTML = '<span class="d pulse-dot"></span>streaming';
    } else if (next === "reconnecting") {
      chipEl.className = "chip chip-warning";
      chipEl.innerHTML = '<span class="d"></span>reconnecting';
    } else if (next === "error") {
      chipEl.className = "chip chip-error";
      chipEl.innerHTML = '<span class="d"></span>disconnected';
    } else {
      chipEl.className = "chip chip-neutral";
      chipEl.innerHTML = '<span class="d"></span>connecting';
    }
  }

  // ===== Realtime SSE (fetch + ReadableStream: token as a header, never
  // a URL query string) with exponential backoff 1s..10s. ================
  async function connectStream() {
    let backoff = RECONNECT_MIN_MS;
    for (;;) {
      try {
        const response = await fetch("/admin/api/stream", { headers: { Authorization: "Bearer " + state.token } });
        if (response.status === 401) { onUnauthorized(); return; }
        if (!response.ok || !response.body) throw new Error("unexpected status " + response.status);

        setConnectionState("streaming");
        state.reconnectAttempts = 0;
        backoff = RECONNECT_MIN_MS;

        const reader = response.body.getReader();
        const decoder = new TextDecoder();
        let buffer = "";
        for (;;) {
          const { value, done } = await reader.read();
          if (done) break;
          buffer += decoder.decode(value, { stream: true });
          let boundary;
          while ((boundary = buffer.indexOf("\n\n")) !== -1) {
            handleSseChunk(buffer.slice(0, boundary));
            buffer = buffer.slice(boundary + 2);
          }
        }
      } catch (err) {
        // fall through to reconnect below
      }

      state.reconnectAttempts += 1;
      setConnectionState(state.reconnectAttempts >= FAILURES_UNTIL_ERROR ? "error" : "reconnecting");
      await new Promise((resolve) => setTimeout(resolve, backoff));
      backoff = Math.min(backoff * 2, RECONNECT_MAX_MS);
    }
  }

  function handleSseChunk(raw) {
    let eventType = "message";
    const dataLines = [];
    for (const line of raw.split("\n")) {
      if (line.startsWith("event:")) eventType = line.slice(6).trim();
      else if (line.startsWith("data:")) dataLines.push(line.slice(5).trim());
    }
    const data = dataLines.join("\n");
    if (!data) return;
    let parsed;
    try { parsed = JSON.parse(data); } catch (err) { return; }

    if (eventType === "overview") Burner.dispatchOverview(parsed);
    else if (eventType === "decision") Burner.dispatchDecision(parsed);
    else if (eventType === "cell_change") Burner.dispatchCellChange(parsed);
    else if (eventType === "dropped") {
      const chipEl = $("#connection-chip");
      if (chipEl) chipEl.title = "dropped " + parsed + " event(s) (client fell behind)";
    }
  }

  // ===== Cell color assignment (fixed order; >4 folds to "other") ======
  // Superseded by markerFor below (visual pass): kept only because
  // nothing still calls it after this pass's view-overview.js/
  // view-cells.js updates land, and deleting a helper mid-edit risks
  // leaving a dangling reference in a file not yet touched. See
  // markerFor's own comment for why hash-based color replaced this.
  function seriesColorFor(cellId, orderedIds) {
    const idx = orderedIds.indexOf(cellId);
    if (idx < 0 || idx >= SERIES_COLORS.length) return SERIES_OTHER_COLOR;
    return SERIES_COLORS[idx];
  }

  // ===== Entity markers (visual pass): the SAME {color, shape} identity
  // for a given (kind, id) pair in EVERY place it appears -- table rows,
  // chart series, sparklines, the mesh graph, introspection panels,
  // decision-log entries that name a cell. Color is a stable hash of the
  // id into the validated dark-surface series palette (SERIES_COLORS),
  // NEVER the id's position in any list or table, so sorting or
  // filtering never repaints anything. Shape distinguishes kind: cells
  // are circles, tenants are hexagons, external/static peers are
  // diamonds.
  //
  // More entities than palette slots will exist in any real cluster, so
  // the color cycles (multiple ids can share a color) -- but read that
  // cycling as a color-REUSE mechanism only, never a license to drop the
  // label: the mono id text is always rendered beside the marker, every
  // time, full stop, exactly like the overview chart already folds >4
  // series into one summed "other" line rather than pretending a 5th
  // color exists.
  const MARKER_SHAPES = { cell: "circle", tenant: "hexagon", peer: "diamond" };

  function hashToIndex(id, modulus) {
    // FNV-1a: stable across reloads/sessions (unlike Math.random or
    // insertion order), cheap, and good enough distribution for a
    // handful of palette slots.
    let hash = 2166136261;
    const s = String(id);
    for (let i = 0; i < s.length; i++) {
      hash ^= s.charCodeAt(i);
      hash = Math.imul(hash, 16777619);
    }
    return Math.abs(hash) % modulus;
  }

  function markerFor(kind, id) {
    const color = SERIES_COLORS[hashToIndex(id, SERIES_COLORS.length)];
    const shape = MARKER_SHAPES[kind] || "circle";
    return { color, shape, className: `marker marker-${shape}` };
  }

  // Raw SVG shape markup (no wrapping <svg>) centered at (cx, cy) with
  // "radius" r, for a caller (the mesh panel) that positions many
  // markers inside one shared <svg> rather than one <svg> per marker.
  function markerShapeMarkup(shape, cx, cy, r) {
    if (shape === "diamond") {
      return `<polygon points="${cx},${(cy - r).toFixed(1)} ${(cx + r).toFixed(1)},${cy} ${cx},${(cy + r).toFixed(1)} ${(cx - r).toFixed(1)},${cy}"></polygon>`;
    }
    if (shape === "hexagon") {
      const pts = [];
      for (let i = 0; i < 6; i++) {
        const angle = (Math.PI / 3) * i - Math.PI / 2;
        pts.push(`${(cx + r * Math.cos(angle)).toFixed(1)},${(cy + r * Math.sin(angle)).toFixed(1)}`);
      }
      return `<polygon points="${pts.join(" ")}"></polygon>`;
    }
    return `<circle cx="${cx}" cy="${cy}" r="${r}"></circle>`;
  }

  // A small standalone marker glyph (a table row, a decision-log entry
  // naming a cell) -- always paired with the entity's own mono id text
  // by the caller, never used alone.
  function markerSvg(kind, id, size) {
    size = size || 12;
    const { color, shape } = markerFor(kind, id);
    const r = size / 2 - 1.5;
    const c = size / 2;
    return (
      `<svg class="marker-ico" width="${size}" height="${size}" viewBox="0 0 ${size} ${size}" aria-hidden="true">` +
      `<g fill="${color}" fill-opacity="0.28" stroke="${color}" stroke-width="1.5">${markerShapeMarkup(shape, c, c, r)}</g>` +
      `</svg>`
    );
  }

  // ===== Cross-view navigation entry points (the mesh panel's node/
  // cluster clicks, and any future cross-view shortcut): a view module
  // registers a handler via `Burner.registerViewEntry("cells", fn)`; a
  // caller elsewhere switches to that view AND runs the handler via
  // `Burner.enterView("cells", param)`, reusing the exact same nav-item
  // click path a sidebar click already takes (one switching mechanism,
  // not two that could drift).
  const viewEntryHandlers = {};
  function registerViewEntry(view, fn) {
    viewEntryHandlers[view] = fn;
  }
  function enterView(view, param) {
    const link = $(`.nav-item[data-view="${view}"]`);
    if (link) link.click();
    if (viewEntryHandlers[view]) viewEntryHandlers[view](param);
  }

  // ===== Tenant-token GraphQL transport + introspection-based schema
  // discovery, shared by the traffic generator (view-traffic-gen.js) so
  // it does not duplicate the field-kind-unwrapping logic. vertexia:
  // view-console.js's Data view still carries its own closure-local copy
  // of this same discovery logic (predates this shared helper, already
  // tested end to end); consolidating it onto this one is a follow-up,
  // not done here to avoid touching already-verified code under time
  // pressure. Honest 401/429 handling matches the admin transport.
  async function tenantGraphQLRaw(token, query) {
    let response;
    try {
      response = await fetch("/api/v0/graphql", {
        method: "POST",
        headers: { "Content-Type": "application/json", Authorization: "Bearer " + token },
        body: JSON.stringify({ query }),
      });
    } catch (err) {
      return { ok: false, message: "request failed: " + err.message };
    }
    if (response.status === 401) return { ok: false, status: 401, message: "401 unauthorized: the tenant token was rejected" };
    if (response.status === 429) {
      const retryAfter = response.headers.get("Retry-After");
      const retryAfterSecs = retryAfter ? Number(retryAfter) : null;
      return {
        ok: false,
        status: 429,
        message: `429 rejected by admission -- retry after ${retryAfter || "?"}s`,
        retryAfterSecs: Number.isFinite(retryAfterSecs) ? retryAfterSecs : null,
      };
    }
    const json = await response.json().catch(() => null);
    if (!response.ok) return { ok: false, status: response.status, message: `HTTP ${response.status}`, json };
    if (json && json.errors && json.errors.length) {
      return { ok: false, status: response.status, message: json.errors.map((e) => e.message).join("; "), json };
    }
    return { ok: true, status: response.status, json };
  }

  function unwrapGraphQLType(t) {
    let depth = 0;
    while (t && t.ofType && depth < 6) { t = t.ofType; depth++; }
    return t;
  }
  function graphqlFieldKind(fieldType) {
    const leaf = unwrapGraphQLType(fieldType);
    return (leaf && leaf.name) || "String";
  }
  function graphqlFieldIsList(fieldType) {
    let t = fieldType, depth = 0;
    while (t && depth < 6) { if (t.kind === "LIST") return true; t = t.ofType; depth++; }
    return false;
  }

  // Discovers every real collection (via the tenant's own introspection,
  // no admin access) and its scalar fields, for `token`. Mirrors the
  // exact discovery rule the Data view uses: a Query field counts as a
  // real collection only if it carries filter/limit/offset args
  // (distinguishing it from COUNT/SUM/introspection meta-fields).
  async function introspectTenantSchema(token) {
    const outcome = await tenantGraphQLRaw(token, '{ __type(name: "Query") { fields { name args { name } } } }');
    if (!outcome.ok || !outcome.json?.data?.__type) {
      return { collections: [], fieldsByCollection: new Map(), error: outcome.message || "introspection returned no Query type" };
    }
    const collections = outcome.json.data.__type.fields
      .filter((f) => {
        const argNames = f.args.map((a) => a.name);
        return argNames.includes("filter") && argNames.includes("limit") && argNames.includes("offset");
      })
      .map((f) => f.name)
      .sort();
    const fieldsByCollection = new Map();
    for (const collection of collections) {
      const fieldOutcome = await tenantGraphQLRaw(
        token,
        `{ __type(name: ${JSON.stringify(collection)}) { fields { name type { kind name ofType { kind name ofType { kind name } } } } } }`
      );
      if (!fieldOutcome.ok || !fieldOutcome.json?.data?.__type) continue;
      const fields = fieldOutcome.json.data.__type.fields
        .filter((f) => !["_docID", "_deleted", "_version"].includes(f.name))
        .map((f) => ({ name: f.name, kind: graphqlFieldKind(f.type), isList: graphqlFieldIsList(f.type) }));
      fieldsByCollection.set(collection, fields);
    }
    return { collections, fieldsByCollection, error: null };
  }

  // Bug-fix round: the one shared fact behind both the Tenants view's
  // per-tenant health line and the mesh panel's cluster caption, so the
  // two can never say something different about the identical condition.
  // `cellCount` is the number of LIVE cells in the group (not configured
  // replicas): a tenant configured for 2 replicas but currently down to
  // 1 live cell gets the same honest note as one that was only ever
  // asked for 1.
  const SINGLE_CELL_NOTE = "single cell: no replication is possible until a second cell joins this group (InsufficientPeers in the logs is expected here, not a bug)";
  function singleCellNote(cellCount) {
    return cellCount <= 1 ? SINGLE_CELL_NOTE : null;
  }

  // ===== Small floating popover (mesh panel node/cluster details): one
  // open at a time; closed by clicking outside, pressing Escape, or
  // opening another. Deliberately not a <dialog>/modal: it must not trap
  // focus or dim the rest of the panel, since hovering a sibling node
  // while a popover is open is part of the interaction.
  let openPopoverEl = null;
  let openPopoverKind = null;
  function closePopover() {
    if (openPopoverEl) {
      openPopoverEl.remove();
      openPopoverEl = null;
    }
    openPopoverKind = null;
    document.removeEventListener("click", onPopoverOutsideClick, true);
    document.removeEventListener("keydown", onPopoverEscape, true);
  }
  function closeTooltip() {
    if (openPopoverKind === "tooltip") closePopover();
  }
  function onPopoverOutsideClick(event) {
    if (!openPopoverEl) return;
    // `composedPath` covers the SVG-trigger to HTML-popover boundary that
    // `contains` alone does not reason about reliably.
    const path = typeof event.composedPath === "function" ? event.composedPath() : [];
    if (path.includes(openPopoverEl)) return;
    if (!openPopoverEl.contains(event.target)) closePopover();
  }
  function onPopoverEscape(event) {
    if (event.key === "Escape") closePopover();
  }
  // `kind` separates a hover tooltip from a clicked action popover. They
  // share one element, so without this a node's `mouseleave` closed the
  // action popover the instant the pointer moved off the node toward it
  // (the popover renders just below its anchor, so that happens
  // immediately and the actions looked like they never opened).
  // `closeTooltip` therefore closes only a tooltip; an action popover is
  // dismissed by an outside click, Escape, or its own actions.
  function openPopoverAt(anchorEl, html, opts) {
    closePopover();
    openPopoverKind = (opts && opts.kind) || "action";
    const pop = el(`<div class="ui-popover" role="dialog">${html}</div>`);
    document.body.appendChild(pop);
    const rect = anchorEl.getBoundingClientRect();
    let left = rect.left + window.scrollX;
    const top = rect.bottom + window.scrollY + 8;
    const maxLeft = window.innerWidth - pop.offsetWidth - 12;
    if (left > maxLeft) left = Math.max(8, maxLeft);
    pop.style.left = Math.max(8, left) + "px";
    pop.style.top = top + "px";
    openPopoverEl = pop;
    // Deferred so the click that opened this popover does not
    // immediately close it via the outside-click listener.
    setTimeout(() => {
      document.addEventListener("click", onPopoverOutsideClick, true);
      document.addEventListener("keydown", onPopoverEscape, true);
    }, 0);
    return pop;
  }

  // ===== Navigation ======================================================
  function initSidebar() {
    $all(".nav-item[data-view]").forEach((link) => {
      link.addEventListener("click", () => {
        $all(".nav-item[data-view]").forEach((l) => l.classList.remove("active"));
        $all(".view").forEach((v) => v.classList.remove("is-active"));
        link.classList.add("active");
        $("#view-" + link.dataset.view).classList.add("is-active");
        window.location.hash = link.dataset.view;
      });
    });
    const fromHash = window.location.hash.replace("#", "");
    const target = fromHash && $(`.nav-item[data-view="${fromHash}"]`);
    if (target) target.click();
  }

  // ===== Fetch helper (adds the admin bearer automatically) ============
  async function adminFetch(path, options) {
    options = options || {};
    const headers = Object.assign({}, options.headers, { Authorization: "Bearer " + state.token });
    if (options.body && !headers["Content-Type"]) headers["Content-Type"] = "application/json";
    const response = await fetch(path, Object.assign({}, options, { headers }));
    if (response.status === 401) onUnauthorized();
    return response;
  }

  // Registration-pattern event dispatch (console round, D25): every
  // view-*.js module registers its own handler via `Burner.onOverview(fn)`
  // etc. at load time rather than overwriting a single slot, so any
  // number of views can react to the same SSE tick independently. Each
  // handler runs even if an earlier one throws, so one view's bug can
  // never blank the rest of the dashboard.
  const overviewHandlers = [];
  const decisionHandlers = [];
  const cellChangeHandlers = [];
  function runHandlers(handlers, arg) {
    for (const fn of handlers) {
      try { fn(arg); } catch (err) { console.error("dashboard handler failed", err); }
    }
  }

  window.Burner = {
    state,
    RING_TICKS,
    SERIES_COLORS,
    SERIES_OTHER_COLOR,
    clampPct, pctLabel, ringGeom, ringDashOffset, stackedSegments, segmentsFilled,
    escapeHtml, noDataYet, humanizeBytes, peerIdOf, el, $, $all,
    chip, dot, progressBar, segmentedProgress, ring, statCard, banner,
    wireTwoClickArm, seriesColorFor,
    withBusy, showResult, describeFailure,
    markerFor, markerSvg, markerShapeMarkup, hashToIndex, singleCellNote,
    tenantGraphQLRaw, introspectTenantSchema, graphqlFieldKind, graphqlFieldIsList,
    registerViewEntry, enterView,
    openPopoverAt, closePopover, closeTooltip,
    ICONS,
    initTheme, initTokenGate, initSidebar,
    connectStream, adminFetch, setConnectionState,
    // Registration: `Burner.onOverview(fn)` adds `fn` to the dispatch
    // list; called with the raw `{tick, overview}` SSE payload.
    onOverview: (fn) => overviewHandlers.push(fn),
    onDecision: (fn) => decisionHandlers.push(fn),
    onCellChange: (fn) => cellChangeHandlers.push(fn),
    dispatchOverview: (data) => { state.overview = data.overview; state.lastTick = data.tick; runHandlers(overviewHandlers, data); },
    dispatchDecision: (entry) => runHandlers(decisionHandlers, entry),
    dispatchCellChange: (data) => runHandlers(cellChangeHandlers, data),
  };

  document.addEventListener("DOMContentLoaded", () => {
    Burner.initTheme();
    Burner.initTokenGate();
    Burner.initSidebar();
  });
})();
