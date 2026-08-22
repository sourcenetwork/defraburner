// defraburner dashboard -- Console section: two tabs sharing one tenant
// token. "Data" is a generated CRUD UI driven entirely by the tenant's
// own GraphQL introspection (no admin backdoor: every read/write goes
// through /api/v0/graphql with the tenant bearer, exactly like any real
// client). "Raw GraphQL" is the power-user textarea console plus a
// "copy as curl" escape hatch.
//
// Verified wire shapes (see the console round's research pass over
// defradb.rs's live query-parse/query/introspection code, not the dead
// schema_gen CLI-only path):
//   list:   query { Foo(filter:{f:{_eq:"v"}}, limit:N, offset:M) { _docID ... } }
//   count:  query { COUNT(Foo: {filter:{...}}) }                -- bare Int!
//   create: mutation { add_Foo(input: [{...}]) { _docID ... } } -- ALWAYS an array reply
//   update: mutation { update_Foo(docID:"id", input:{...}) { _docID ... } }
//   delete: mutation { delete_Foo(docID:"id") { _docID } }
// update_/delete_ with NEITHER docID NOR filter given operate on EVERY
// document in the collection -- this UI always sends an explicit docID
// for row-level actions, never omits it.
"use strict";

(function () {
  const B = window.Burner;
  const TOKEN_PREFIX = "defraburner_tenant_token.";
  const PAGE_SIZE = 20;

  const data = {
    tenant: null,
    token: null,
    collections: [],
    collection: null,
    fieldsByCollection: new Map(),
    fields: [], // [{name, kind: 'String'|'Int'|...}] for the selected collection
    rows: [],
    total: null,
    collectionsError: null,
    offset: 0,
    filterField: "",
    filterValue: "",
    editingDocId: null, // non-null while the create/edit form is editing an existing row
  };

  // ===== GraphQL transport (tenant token; honest 429/401 handling) =====
  async function tenantGraphQL(query) {
    let response;
    try {
      response = await fetch("/api/v0/graphql", {
        method: "POST",
        headers: { "Content-Type": "application/json", Authorization: "Bearer " + data.token },
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

  // ===== Introspection-driven discovery (D25 addendum: no admin
  // backdoor, discover exactly what a real client would see) ===========
  // Delegated to Burner.introspectTenantSchema, the one discovery shared
  // with the traffic generator, so this view and the load it generates
  // can never disagree about which fields a collection has.

  function inputTypeFor(kind) {
    switch (kind) {
      case "Int": case "Float64": case "Float32": return "number";
      case "Boolean": return "checkbox";
      case "DateTime": return "datetime-local";
      default: return "text";
    }
  }
  function coerceForWire(kind, rawValue) {
    if (kind === "Int") return parseInt(rawValue, 10);
    if (kind === "Float64" || kind === "Float32") return parseFloat(rawValue);
    if (kind === "Boolean") return !!rawValue;
    if (kind === "DateTime") return rawValue ? new Date(rawValue).toISOString() : null;
    return rawValue;
  }
  function graphqlLiteral(value) {
    if (value === null || value === undefined) return "null";
    if (typeof value === "number" || typeof value === "boolean") return String(value);
    return JSON.stringify(value);
  }

  // ===== Tenant / token selection =======================================
  function tenantOptions() {
    const tenants = (B.state.overview && B.state.overview.tenants) || [];
    return tenants.map((t) => `<option value="${B.escapeHtml(t.name)}">${B.escapeHtml(t.name)}</option>`).join("");
  }
  function refreshTenantPicker() {
    const select = B.$("#data-tenant-select");
    if (!select) return;
    const tenants = (B.state.overview && B.state.overview.tenants) || [];
    const current = select.value;
    select.innerHTML = `<option value="">select a tenant...</option>` + tenantOptions();
    if (current) select.value = current;
    const emptyHint = B.$("#data-no-tenants-hint");
    if (emptyHint) emptyHint.hidden = tenants.length > 0;
  }

  function loadStoredToken(tenant) {
    try { return window.localStorage.getItem(TOKEN_PREFIX + tenant); } catch (err) { return null; }
  }
  function storeToken(tenant, token) {
    try { window.localStorage.setItem(TOKEN_PREFIX + tenant, token); } catch (err) { /* ignore */ }
  }

  async function onTenantChange() {
    const tenant = B.$("#data-tenant-select").value;
    data.tenant = tenant || null;
    data.collections = []; data.collection = null; data.fields = []; data.rows = []; data.total = null;
    data.fieldsByCollection = new Map();
    const stored = tenant ? loadStoredToken(tenant) : null;
    B.$("#data-token-input").value = stored || "";
    data.token = stored || null;
    renderTokenState();
    if (stored) await loadCollections();
    renderCollections();
    renderTable();
  }

  function renderTokenState() {
    const hint = B.$("#data-token-hint");
    if (!data.tenant) { hint.textContent = ""; return; }
    hint.innerHTML = data.token
      ? "token stored for this tenant in this browser."
      : "the host only stores a token's hash and cannot show you an existing token -- paste one, or rotate to mint a fresh one.";
  }

  async function onTokenSave() {
    const token = B.$("#data-token-input").value.trim();
    if (!data.tenant || !token) return;
    data.token = token;
    storeToken(data.tenant, token);
    renderTokenState();
    await loadCollections();
    renderCollections();
  }

  async function onRotateToken() {
    if (!data.tenant) return;
    const btn = B.$("#data-rotate-token");
    await B.withBusy(btn, "rotating...", async () => {
      try {
        const response = await B.adminFetch(`/admin/tenants/${encodeURIComponent(data.tenant)}/rotate-token`, { method: "POST" });
        if (!response.ok) { B.$("#data-outcome").innerHTML = B.banner("error", B.escapeHtml(await B.describeFailure(response))); return; }
        const body = await response.json();
        B.$("#data-outcome").innerHTML = "";
        data.token = body.token;
        storeToken(data.tenant, body.token);
        B.$("#data-token-input").value = body.token;
        renderTokenState();
        await loadCollections();
        renderCollections();
      } catch (err) {
        B.$("#data-outcome").innerHTML = B.banner("error", "request failed: " + B.escapeHtml(err.message));
      }
    });
  }

  // ===== Collections / schema ===========================================
  async function loadCollections() {
    const schema = await B.introspectTenantSchema(data.token);
    data.collections = schema.collections;
    data.fieldsByCollection = schema.fieldsByCollection;
    data.collectionsError = schema.error;
  }
  function renderCollections() {
    const list = B.$("#data-collections");
    if (!data.token) { list.innerHTML = '<div class="ui-stat-hint">select a tenant and provide its token first</div>'; return; }
    if (data.collectionsError) { list.innerHTML = B.banner("error", B.escapeHtml(data.collectionsError)); return; }
    if (data.collections.length === 0) { list.innerHTML = '<div class="ui-stat-hint">no collections discovered (the schema declares none)</div>'; return; }
    const hint = `<div class="ui-stat-hint" style="margin-bottom:8px">discovered via this tenant's own GraphQL introspection (the same token you gave above, no admin access used)</div>`;
    list.innerHTML = hint + data.collections
      .map((c) => `<button type="button" class="btn ${c === data.collection ? "btn-primary" : "btn-secondary"} btn-sm" data-pick-collection="${B.escapeHtml(c)}">${B.escapeHtml(c)}</button>`)
      .join(" ");
    B.$all("[data-pick-collection]", list).forEach((btn) => {
      btn.addEventListener("click", () => selectCollection(btn.dataset.pickCollection));
    });
  }

  async function selectCollection(name) {
    data.collection = name;
    data.offset = 0;
    data.filterField = ""; data.filterValue = "";
    renderCollections();
    data.fields = data.fieldsByCollection.get(name) || [];
    renderCreateForm();
    await refreshRows();
  }

  // ===== Document table ==================================================
  function buildFilter() {
    if (!data.filterField || data.filterValue === "") return "";
    return `filter: {${data.filterField}: {_eq: ${graphqlLiteral(coerceForWire(fieldKind(data.filterField), data.filterValue))}}}, `;
  }
  function fieldKind(name) {
    const f = data.fields.find((x) => x.name === name);
    return f ? f.kind : "String";
  }
  function scalarFieldNames() {
    return data.fields.filter((f) => !f.isList && !["_docID"].includes(f.name)).map((f) => f.name);
  }

  async function refreshRows() {
    if (!data.collection) return;
    const selectFields = ["_docID", ...scalarFieldNames()].join(" ");
    const filterClause = buildFilter();
    const query =
      `query { rows: ${data.collection}(${filterClause}limit: ${PAGE_SIZE}, offset: ${data.offset}) { ${selectFields} } ` +
      `total: COUNT(${data.collection}: {${filterClause.replace(/, $/, "")}}) }`;
    const outcome = await tenantGraphQL(query);
    if (!outcome.ok) { renderOutcomeBanner(outcome); data.rows = []; data.total = null; renderTable(); return; }
    B.$("#data-outcome").innerHTML = "";
    data.rows = (outcome.json.data && outcome.json.data.rows) || [];
    data.total = outcome.json.data ? outcome.json.data.total : null;
    renderTable();
  }

  function renderOutcomeBanner(outcome) {
    B.$("#data-outcome").innerHTML = B.banner(outcome.status === 429 ? "warning" : "error", B.escapeHtml(outcome.message));
  }

  function renderTable() {
    const wrap = B.$("#data-table-wrap");
    if (!data.collection) { wrap.innerHTML = '<div class="ui-stat-hint">pick a collection above</div>'; return; }
    const cols = scalarFieldNames();
    const filterOptions = cols.map((c) => `<option value="${B.escapeHtml(c)}" ${c === data.filterField ? "selected" : ""}>${B.escapeHtml(c)}</option>`).join("");

    const rowsHtml = data.rows.length
      ? data.rows
          .map((row) => {
            const cells = cols.map((c) => `<td>${B.escapeHtml(row[c])}</td>`).join("");
            return (
              `<tr><td class="mono" data-copy-docid="${B.escapeHtml(row._docID)}" title="click to copy" style="cursor:pointer">${B.escapeHtml((row._docID || "").slice(0, 12))}...</td>${cells}` +
              `<td><button type="button" class="btn btn-secondary btn-sm" data-edit-row="${B.escapeHtml(row._docID)}">edit</button> ` +
              `<button type="button" class="btn btn-danger btn-sm" data-delete-row="${B.escapeHtml(row._docID)}">delete</button></td></tr>`
            );
          })
          .join("")
      : `<tr><td colspan="${cols.length + 2}" class="ui-data-table-empty">${
          data.filterField && data.filterValue !== ""
            ? "no documents match this filter"
            : "this collection has no documents yet -- create one below"
        }</td></tr>`;

    const from = data.rows.length ? data.offset + 1 : 0;
    const to = data.offset + data.rows.length;
    const countLabel = data.total !== null ? `showing ${from}-${to} of ${data.total}` : `showing ${from}-${to} of the page`;

    wrap.innerHTML =
      `<div class="ui-toolbar">` +
      `<select class="select" id="data-filter-field" style="max-width:160px"><option value="">no filter</option>${filterOptions}</select>` +
      `<input class="input" id="data-filter-value" placeholder="equals..." style="max-width:200px" value="${B.escapeHtml(data.filterValue)}" />` +
      `<button type="button" class="btn btn-secondary btn-sm" id="data-filter-apply">apply</button>` +
      `<span class="ui-stat-hint">v1: equality only (no contains/gt/lt yet)</span>` +
      `<span class="spacer"></span>` +
      `<span class="ui-stat-hint">${countLabel}</span>` +
      `<button type="button" class="btn btn-ghost btn-sm" id="data-refresh">${B.ICONS.refresh} refresh</button>` +
      `<button type="button" class="btn btn-secondary btn-sm" id="data-prev" ${data.offset === 0 ? "disabled" : ""}>prev</button>` +
      `<button type="button" class="btn btn-secondary btn-sm" id="data-next" ${data.rows.length < PAGE_SIZE ? "disabled" : ""}>next</button>` +
      `</div>` +
      `<div class="scroll-area"><table class="ui-data-table"><thead><tr><th>_docID</th>${cols.map((c) => `<th>${B.escapeHtml(c)}</th>`).join("")}<th></th></tr></thead>` +
      `<tbody>${rowsHtml}</tbody></table></div>`;

    B.$("#data-filter-apply").addEventListener("click", () => {
      data.filterField = B.$("#data-filter-field").value;
      data.filterValue = B.$("#data-filter-value").value;
      data.offset = 0;
      refreshRows();
    });
    B.$("#data-refresh").addEventListener("click", refreshRows);
    B.$("#data-prev").addEventListener("click", () => { data.offset = Math.max(0, data.offset - PAGE_SIZE); refreshRows(); });
    B.$("#data-next").addEventListener("click", () => { data.offset += PAGE_SIZE; refreshRows(); });
    B.$all("[data-copy-docid]", wrap).forEach((td) => td.addEventListener("click", () => navigator.clipboard?.writeText(td.dataset.copyDocid).catch(() => {})));
    B.$all("[data-edit-row]", wrap).forEach((btn) => btn.addEventListener("click", () => editRow(btn.dataset.editRow)));
    B.$all("[data-delete-row]", wrap).forEach((btn) =>
      B.wireTwoClickArm(btn, "confirm", () => B.withBusy(btn, "deleting...", () => deleteRow(btn.dataset.deleteRow)))
    );
  }

  // ===== Create / update form ============================================
  function renderCreateForm() {
    const host = B.$("#data-form");
    if (!data.collection || data.fields.length === 0) { host.innerHTML = ""; return; }
    const inputs = scalarFieldNames()
      .map((name) => {
        const kind = fieldKind(name);
        const type = inputTypeFor(kind);
        if (type === "checkbox") {
          return `<label class="checkbox"><input type="checkbox" data-form-field="${B.escapeHtml(name)}" data-kind="${kind}" /><span>${B.escapeHtml(name)}</span></label>`;
        }
        return (
          `<div class="field"><label>${B.escapeHtml(name)} <span class="fg-3">(${kind})</span></label>` +
          `<input class="input" type="${type}" data-form-field="${B.escapeHtml(name)}" data-kind="${kind}" /></div>`
        );
      })
      .join("");
    host.innerHTML =
      `<div class="ui-card"><div class="ui-card-head"><span class="ui-card-title" id="data-form-title">Create in ${B.escapeHtml(data.collection)}</span>` +
      `<div class="ui-card-actions"><button type="button" class="btn btn-ghost btn-sm" id="data-form-cancel" hidden>cancel edit</button></div></div>` +
      `<div class="ui-card-body col">${inputs}` +
      `<div class="row"><button type="button" class="btn btn-primary" id="data-form-submit">submit</button></div>` +
      `<div id="data-form-result"></div></div></div>`;
    B.$("#data-form-submit").addEventListener("click", () => {
      B.withBusy(B.$("#data-form-submit"), data.editingDocId ? "saving..." : "creating...", submitForm);
    });
    B.$("#data-form-cancel").addEventListener("click", cancelEdit);
  }

  function collectFormValues() {
    const values = {};
    B.$all("[data-form-field]").forEach((input) => {
      const name = input.dataset.formField;
      const kind = input.dataset.kind;
      const raw = input.type === "checkbox" ? input.checked : input.value;
      if (input.type !== "checkbox" && raw === "") return; // omit untouched fields
      values[name] = coerceForWire(kind, raw);
    });
    return values;
  }

  function editRow(docId) {
    const row = data.rows.find((r) => r._docID === docId);
    if (!row) return;
    data.editingDocId = docId;
    B.$("#data-form-title").textContent = `Update ${docId.slice(0, 12)}... in ${data.collection}`;
    B.$("#data-form-cancel").hidden = false;
    B.$all("[data-form-field]").forEach((input) => {
      const name = input.dataset.formField;
      if (row[name] === undefined || row[name] === null) return;
      if (input.type === "checkbox") input.checked = !!row[name];
      else if (input.type === "datetime-local") input.value = String(row[name]).slice(0, 16);
      else input.value = row[name];
    });
  }
  function cancelEdit() {
    data.editingDocId = null;
    renderCreateForm();
  }

  async function submitForm() {
    const values = collectFormValues();
    const resultEl = B.$("#data-form-result");
    if (data.editingDocId) {
      const original = data.rows.find((r) => r._docID === data.editingDocId) || {};
      const changed = Object.fromEntries(Object.entries(values).filter(([k, v]) => String(original[k]) !== String(v)));
      if (Object.keys(changed).length === 0) { resultEl.innerHTML = B.banner("warning", "no changes to submit"); return; }
      const inputLiteral = Object.entries(changed).map(([k, v]) => `${k}: ${graphqlLiteral(v)}`).join(", ");
      const query = `mutation { update_${data.collection}(docID: ${graphqlLiteral(data.editingDocId)}, input: {${inputLiteral}}) { _docID ${scalarFieldNames().join(" ")} } }`;
      const outcome = await tenantGraphQL(query);
      if (!outcome.ok) { renderOutcomeBanner(outcome); return; }
      const after = outcome.json.data[`update_${data.collection}`][0];
      resultEl.innerHTML = `<div class="ui-section-label">before</div><pre class="code">${B.escapeHtml(JSON.stringify(original, null, 2))}</pre>` +
        `<div class="ui-section-label">after</div><pre class="code">${B.escapeHtml(JSON.stringify(after, null, 2))}</pre>`;
      data.editingDocId = null;
      B.$("#data-form-cancel").hidden = true;
      await refreshRows();
    } else {
      const inputLiteral = Object.entries(values).map(([k, v]) => `${k}: ${graphqlLiteral(v)}`).join(", ");
      const query = `mutation { add_${data.collection}(input: [{${inputLiteral}}]) { _docID ${scalarFieldNames().join(" ")} } }`;
      const outcome = await tenantGraphQL(query);
      if (!outcome.ok) { resultEl.innerHTML = B.banner("error", B.escapeHtml(outcome.message)); return; }
      const created = outcome.json.data[`add_${data.collection}`][0];
      resultEl.innerHTML = B.banner("success", `created _docID: ${B.escapeHtml(created._docID)}`);
      await refreshRows();
    }
  }

  async function deleteRow(docId) {
    const query = `mutation { delete_${data.collection}(docID: ${graphqlLiteral(docId)}) { _docID } }`;
    const outcome = await tenantGraphQL(query);
    if (!outcome.ok) { renderOutcomeBanner(outcome); return; }
    await refreshRows();
  }

  // ===== Raw GraphQL tab ==================================================
  function initRawConsole() {
    B.$("#console-run").addEventListener("click", () => {
      const btn = B.$("#console-run");
      B.withBusy(btn, "running...", async () => {
        const token = B.$("#console-token").value.trim();
        const query = B.$("#console-query").value;
        const output = B.$("#console-output");
        output.style.color = "";
        output.textContent = "running...";
        try {
          const response = await fetch("/api/v0/graphql", {
            method: "POST",
            headers: { "Content-Type": "application/json", Authorization: "Bearer " + token },
            body: JSON.stringify({ query }),
          });
          const json = await response.json().catch(() => ({ error: "invalid JSON response" }));
          output.textContent = JSON.stringify(json, null, 2);
          if (!response.ok || json.errors) output.style.color = "var(--state-error)";
          if (response.status === 429) {
            output.textContent = `429 rejected by admission -- Retry-After: ${response.headers.get("Retry-After") || "?"}s\n\n` + output.textContent;
          }
        } catch (err) {
          output.style.color = "var(--state-error)";
          output.textContent = "request failed: " + err.message;
        }
      });
    });
    B.$("#console-copy-curl").addEventListener("click", () => {
      const token = B.$("#console-token").value.trim();
      const query = B.$("#console-query").value;
      const body = JSON.stringify({ query });
      const curl = `curl -sS -X POST '${window.location.origin}/api/v0/graphql' -H 'Authorization: Bearer ${token}' -H 'Content-Type: application/json' -d ${shellQuote(body)}`;
      navigator.clipboard?.writeText(curl).catch(() => {});
    });
  }
  function shellQuote(value) {
    return "'" + value.replace(/'/g, `'\\''`) + "'";
  }

  document.addEventListener("DOMContentLoaded", () => {
    initRawConsole();
    B.$("#data-tenant-select").addEventListener("change", onTenantChange);
    B.$("#data-token-save").addEventListener("click", onTokenSave);
    B.$("#data-rotate-token").addEventListener("click", onRotateToken);
  });
  B.onOverview(() => refreshTenantPicker());

  // Cross-view navigation entry point (the mesh panel's tenant popover
  // "open in Data view" link): reuses the exact same tenant-select +
  // onTenantChange path a manual dropdown pick already takes, never a
  // second selection mechanism.
  B.registerViewEntry("console", (tenantName) => {
    if (!tenantName) return;
    const select = B.$("#data-tenant-select");
    if (!select) return;
    select.value = tenantName;
    onTenantChange();
  });

  // Exposed for view-console-seed.js (the bulk seeder): reuses this
  // view's own tenant-token GraphQL transport and current selection
  // state, so the seeder never opens a second connection or duplicates
  // the 401/429/error handling `tenantGraphQL` already does.
  B.dataView = {
    tokenStorageKey: (name) => TOKEN_PREFIX + name,
    currentTenant: () => data.tenant,
    currentCollection: () => data.collection,
    currentFields: () => data.fields,
    scalarFieldNames,
    coerceForWire,
    graphqlLiteral,
    graphql: (query) => tenantGraphQL(query),
    refreshRows: () => refreshRows(),
  };
})();
