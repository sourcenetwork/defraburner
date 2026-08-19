// defraburner dashboard -- Overview traffic generator: schema-driven
// mixed read/write load across every placed tenant, through the gateway,
// using each tenant's own token (never the admin token) so admission
// applies exactly like real traffic. A visible top-bar marker while
// running, so a moving chart is never mistaken for real production load.
"use strict";

(function () {
  const B = window.Burner;

  const TICK_MS = 250;
  const READ_FRACTION = 0.7; // fixed, documented mix: 70% reads, 30% writes
  const DEFAULT_RATE = 20; // requests/sec
  const MAX_RATE = 500; // bound: a UI control is not a load-test harness

  let running = false;
  let timerId = null;
  let tenants = []; // [{name, token, collections, fieldsByCollection}]
  let requestsPerTick = 0;
  let seedCounter = 0;
  const counters = { reads: 0, writes: 0, errors: 0, rejected429: 0 };
  const recentTimestamps = []; // for a real, measured req/s readout
  let startedAt = 0;

  function setMarker(visible) {
    const marker = B.$("#synthetic-traffic-marker");
    if (marker) marker.hidden = !visible;
  }

  function tokenFor(tenantSpec) {
    try {
      return window.localStorage.getItem(B.dataView.tokenStorageKey(tenantSpec.name));
    } catch (err) {
      return null;
    }
  }

  async function mintToken(name) {
    const response = await B.adminFetch(`/admin/tenants/${encodeURIComponent(name)}/rotate-token`, { method: "POST" });
    if (!response.ok) return null;
    const body = await response.json();
    try {
      window.localStorage.setItem(B.dataView.tokenStorageKey(name), body.token);
    } catch (err) {
      /* localStorage unavailable; token still usable for this run */
    }
    return body.token;
  }

  async function buildTenantContexts(mintMissing, statusEl) {
    const overview = B.state.overview;
    const placed = (overview && overview.tenants || []).filter((t) => t.status === "placed" && (t.cells || []).length > 0);
    const contexts = [];
    const skipped = [];
    for (const tenant of placed) {
      let token = tokenFor(tenant);
      if (!token && mintMissing) {
        statusEl.textContent = `minting a token for '${tenant.name}' (rotate-token; its previous token, if any, stops working)...`;
        token = await mintToken(tenant.name);
      }
      if (!token) { skipped.push(tenant.name); continue; }
      const schema = await B.introspectTenantSchema(token);
      if (schema.error || schema.collections.length === 0) { skipped.push(tenant.name); continue; }
      contexts.push({ name: tenant.name, token, collections: schema.collections, fieldsByCollection: schema.fieldsByCollection });
    }
    return { contexts, skipped };
  }

  function scalarFields(fields) {
    return (fields || []).filter((f) => !f.isList);
  }

  async function fireOne() {
    if (tenants.length === 0) return;
    const tenant = tenants[Math.floor(Math.random() * tenants.length)];
    const collection = tenant.collections[Math.floor(Math.random() * tenant.collections.length)];
    const fields = scalarFields(tenant.fieldsByCollection.get(collection));
    if (fields.length === 0) return;

    const isWrite = Math.random() > READ_FRACTION;
    let outcome;
    if (isWrite) {
      const doc = {};
      for (const field of fields) doc[field.name] = B.fakeValueForField(field, seedCounter++);
      const literal = fields.map((f) => `${f.name}: ${B.dataView.graphqlLiteral(doc[f.name])}`).join(", ");
      outcome = await B.tenantGraphQLRaw(tenant.token, `mutation { add_${collection}(input: [{${literal}}]) { _docID } }`);
    } else {
      const names = fields.slice(0, 6).map((f) => f.name).join(" ");
      outcome = await B.tenantGraphQLRaw(tenant.token, `query { ${collection}(limit: 10) { _docID ${names} } }`);
    }

    recentTimestamps.push(Date.now());
    if (outcome.ok) {
      counters[isWrite ? "writes" : "reads"]++;
    } else if (outcome.status === 429) {
      counters.rejected429++;
    } else {
      counters.errors++;
    }
  }

  function currentRatePerSec() {
    const cutoff = Date.now() - 2000;
    while (recentTimestamps.length && recentTimestamps[0] < cutoff) recentTimestamps.shift();
    return recentTimestamps.length / 2;
  }

  async function tick() {
    if (!running) return;
    const batch = [];
    for (let i = 0; i < requestsPerTick; i++) batch.push(fireOne());
    await Promise.all(batch);
    renderCounters();
  }

  function renderCounters() {
    const el = B.$("#traffic-gen-counters");
    if (!el) return;
    const elapsed = ((Date.now() - startedAt) / 1000).toFixed(0);
    el.innerHTML =
      `<span class="mono">${counters.reads} reads</span> &middot; ` +
      `<span class="mono">${counters.writes} writes</span> &middot; ` +
      `<span class="mono" style="${counters.rejected429 ? "color:var(--accent-2)" : ""}">${counters.rejected429} rejected (429)</span> &middot; ` +
      `<span class="mono" style="${counters.errors ? "color:var(--state-error)" : ""}">${counters.errors} errors</span> &middot; ` +
      `<span class="mono">${currentRatePerSec().toFixed(1)} req/s</span> &middot; ` +
      `<span class="fg-3">${elapsed}s elapsed across ${tenants.length} tenant(s)</span>`;
  }

  async function start(container) {
    const statusEl = B.$("#traffic-gen-status", container);
    const mintMissing = B.$("#traffic-gen-mint", container).checked;
    const rate = Math.max(1, Math.min(MAX_RATE, Number(B.$("#traffic-gen-rate", container).value || DEFAULT_RATE)));
    requestsPerTick = Math.max(1, Math.round((rate * TICK_MS) / 1000));

    statusEl.textContent = "discovering placed tenants and their schemas...";
    const { contexts, skipped } = await buildTenantContexts(mintMissing, statusEl);
    tenants = contexts;
    if (tenants.length === 0) {
      B.showResult(
        statusEl,
        false,
        skipped.length
          ? `no eligible tenant: ${skipped.join(", ")} skipped (no stored token${mintMissing ? "/introspection failed" : " -- check 'mint missing tokens' or add one in the Data view"})`
          : "no placed tenants with a live cell yet -- create one in the Tenants view first"
      );
      return;
    }

    counters.reads = 0; counters.writes = 0; counters.errors = 0; counters.rejected429 = 0;
    recentTimestamps.length = 0;
    startedAt = Date.now();
    running = true;
    setMarker(true);
    B.$("#traffic-gen-start", container).hidden = true;
    B.$("#traffic-gen-stop", container).hidden = false;
    statusEl.textContent = skipped.length
      ? `running against ${tenants.length} tenant(s) (skipped: ${skipped.join(", ")})`
      : `running against ${tenants.length} tenant(s)`;
    renderCounters();
    timerId = setInterval(tick, TICK_MS);
  }

  function stop(container) {
    running = false;
    clearInterval(timerId);
    setMarker(false);
    B.$("#traffic-gen-start", container).hidden = false;
    B.$("#traffic-gen-stop", container).hidden = true;
    renderCounters();
    B.$("#traffic-gen-status", container).textContent += " -- stopped";
  }

  function render() {
    const host = B.$("#traffic-gen-panel");
    if (!host) return;
    host.innerHTML =
      `<div class="col" style="gap:10px">` +
      `<div class="row" style="flex-wrap:wrap;gap:14px">` +
      `<div class="field" style="max-width:140px"><label for="traffic-gen-rate">requests/sec</label><input id="traffic-gen-rate" class="input" type="number" min="1" max="${MAX_RATE}" value="${DEFAULT_RATE}" /></div>` +
      `<label class="checkbox" style="align-self:flex-end;padding-bottom:9px"><input type="checkbox" id="traffic-gen-mint" /><span>mint tokens for tenants without one stored (rotates their token)</span></label>` +
      `<div style="align-self:flex-end"><button type="button" id="traffic-gen-start" class="btn btn-primary">start</button><button type="button" id="traffic-gen-stop" class="btn btn-danger" hidden>stop</button></div>` +
      `</div>` +
      `<div id="traffic-gen-counters" class="ui-stat-hint">not running</div>` +
      `<div id="traffic-gen-status" class="ui-stat-hint"></div>` +
      `<div class="ui-stat-hint">70% reads / 30% writes, fixed. Reads list up to 10 documents; writes create one plausible document at a time. Uses each tenant's own token through the normal gateway path, so admission (429s) applies exactly like real traffic.</div>` +
      "</div>";
    B.$("#traffic-gen-start", host).addEventListener("click", () => start(host));
    B.$("#traffic-gen-stop", host).addEventListener("click", () => stop(host));
  }

  document.addEventListener("DOMContentLoaded", render);
})();
