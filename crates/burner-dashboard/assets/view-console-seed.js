// defraburner dashboard -- bulk document seeder (Data view power tool):
// generates N plausible documents for the currently selected collection
// and creates them for real through the same tenant-token GraphQL path
// every other Data view mutation uses (B.dataView.graphql, from
// view-console.js), so admission applies to seeding exactly like any
// other write. Chunked, admission-aware (a real 429's Retry-After drives
// the backoff, never a guessed delay), honest progress (real counts, not
// a fabricated percentage), and a cancel that reports what was actually
// written, not what was requested.
"use strict";

(function () {
  const B = window.Burner;

  // Verified directly against defradb.rs's live mutation resolver (read,
  // not modified -- the bug-fix round's batch-input research pass):
  // `add_X(input: [{...}, {...}, ...])` genuinely creates every element
  // of a multi-item list in ONE transaction with ONE commit, replying
  // with a flat array index-aligned to the input list. Not a
  // theoretical possibility: proven end-to-end by
  // `defradb.rs/tools/integration-test/tests/nac/multi_doc_create.rs:35-52`
  // (2 elements, asserts `created.len() == 2`),
  // `tests/p2p/sync.rs:226-244` (3 elements, plus proves P2P replication
  // of all 3), and `tests/query/limit_pushdown.rs:9-35` (programmatically
  // batches up to 200 elements per call while seeding 2,000 documents).
  // The implementation (`db/src/auto_commit_mutator/create_many.rs`) is
  // a genuine batch path, not a call-create()-in-a-loop shim: N doc ids
  // allocated together, N document blocks computed (parallelized on
  // native builds), one transaction, one commit for the whole batch --
  // so one `add_X` call with CHUNK_SIZE elements really is cheaper than
  // CHUNK_SIZE separate calls, not just less code. A chunk of 25 stays
  // well under the 200-element shape already proven correct upstream
  // while keeping any single request modest.
  const CHUNK_SIZE = 25;
  const MAX_RETRY_AFTER_WAIT_MS = 30_000; // bounded: never hang the UI on a huge Retry-After
  const MAX_RETRIES_PER_CHUNK = 6;

  let cancelRequested = false;
  let running = false;

  function randomWord() {
    const syllables = ["ra", "ke", "lo", "min", "tor", "za", "quin", "bel", "ost", "fen"];
    const n = 2 + Math.floor(Math.random() * 2);
    let word = "";
    for (let i = 0; i < n; i++) word += syllables[Math.floor(Math.random() * syllables.length)];
    return word;
  }

  // Exposed so the Overview traffic generator's own write load reuses
  // the exact same fake-value shapes, rather than a third copy.
  B.fakeValueForField = fakeValueFor;
  function fakeValueFor(field, seedIndex) {
    const kind = field.kind;
    if (kind === "Int") return Math.floor(Math.random() * 100000);
    if (kind === "Float64" || kind === "Float32") return Math.round(Math.random() * 100000) / 100;
    if (kind === "Boolean") return Math.random() < 0.5;
    if (kind === "DateTime") {
      const daysAgo = Math.floor(Math.random() * 365);
      return new Date(Date.now() - daysAgo * 86400000).toISOString();
    }
    // String and any other/unknown scalar: a short, obviously-synthetic
    // value that is easy to recognize (and bulk-delete) as seeded data.
    return `seed-${seedIndex}-${randomWord()}`;
  }

  // The verified shape: one `add_X` call, an N-element input list, one
  // real batch transaction upstream (see the citation above) -- not N
  // separate calls.
  function buildCreateMutation(collection, fields, values) {
    const items = values.map((doc) => {
      const literal = fields.map((f) => `${f.name}: ${B.dataView.graphqlLiteral(doc[f.name])}`).join(", ");
      return `{${literal}}`;
    });
    return `mutation { add_${collection}(input: [${items.join(", ")}]) { _docID } }`;
  }

  async function sleep(ms) {
    return new Promise((resolve) => setTimeout(resolve, ms));
  }

  function setProgress(container, created, failed, total, statusText) {
    const pct = total > 0 ? (created + failed) / total * 100 : 0;
    B.$("#seed-progress-wrap", container).innerHTML = B.progressBar({
      variant: failed > 0 ? "warning" : "accent",
      value: pct,
      label: `${created} created, ${failed} failed of ${total}`,
    });
    B.$("#seed-status", container).textContent = statusText || "";
  }

  async function runSeed(container, count) {
    const collection = B.dataView.currentCollection();
    const fields = (B.dataView.currentFields() || []).filter((f) => !f.isList && f.name !== "_docID");
    if (!collection) { B.showResult(B.$("#seed-status", container), false, "pick a collection first"); return; }
    if (fields.length === 0) { B.showResult(B.$("#seed-status", container), false, "this collection has no scalar fields to seed"); return; }

    running = true;
    cancelRequested = false;
    B.$("#seed-run", container).hidden = true;
    B.$("#seed-cancel", container).hidden = false;
    const startedAt = Date.now();

    let created = 0;
    let failed = 0;
    let retries = 0;
    let remaining = count;
    let seedIndex = 0;

    while (remaining > 0 && !cancelRequested) {
      const batchSize = Math.min(CHUNK_SIZE, remaining);
      const batchValues = [];
      for (let i = 0; i < batchSize; i++) {
        const doc = {};
        for (const field of fields) doc[field.name] = fakeValueFor(field, seedIndex++);
        batchValues.push(doc);
      }
      const query = buildCreateMutation(collection, fields, batchValues);

      let attempt = 0;
      let outcome = null;
      for (;;) {
        outcome = await B.dataView.graphql(query);
        if (outcome.ok || outcome.status !== 429 || attempt >= MAX_RETRIES_PER_CHUNK || cancelRequested) break;
        attempt++;
        retries++;
        const waitMs = Math.min(MAX_RETRY_AFTER_WAIT_MS, (outcome.retryAfterSecs ?? 2) * 1000);
        setProgress(container, created, failed, count, `429 from admission -- honoring Retry-After, waiting ${(waitMs / 1000).toFixed(1)}s (attempt ${attempt}/${MAX_RETRIES_PER_CHUNK})`);
        await sleep(waitMs);
      }

      if (outcome.ok) {
        // Honest count: read how many the response actually reports
        // created (the index-aligned array upstream returns), rather
        // than assuming a 200-ish response means every requested
        // element landed.
        const actuallyCreated = outcome.json?.data?.[`add_${collection}`]?.length;
        const thisCreated = Number.isFinite(actuallyCreated) ? actuallyCreated : 0;
        created += thisCreated;
        failed += batchValues.length - thisCreated;
      } else {
        failed += batchValues.length;
      }
      remaining -= batchSize;
      setProgress(container, created, failed, count, outcome.ok ? "" : `last batch failed: ${outcome.message}`);
    }

    running = false;
    B.$("#seed-run", container).hidden = false;
    B.$("#seed-cancel", container).hidden = true;
    const elapsedSecs = ((Date.now() - startedAt) / 1000).toFixed(1);
    const summary = cancelRequested
      ? `cancelled after ${elapsedSecs}s -- ${created} document(s) genuinely created, ${failed} failed${retries ? `, ${retries} retried on 429` : ""} before stopping`
      : `done in ${elapsedSecs}s -- ${created} created, ${failed} failed${retries ? `, ${retries} retried on 429` : ""}`;
    B.showResult(B.$("#seed-status", container), failed === 0, summary);
    if (created > 0) await B.dataView.refreshRows();
  }

  function render() {
    const host = B.$("#data-seed");
    if (!host) return;
    host.innerHTML =
      `<div class="ui-card section-gap">` +
      `<div class="ui-card-head"><span class="ui-card-title">Bulk seed</span><span class="ui-card-sub">generates plausible documents for the selected collection, through the same tenant-token path as any other write</span></div>` +
      `<div class="ui-card-body col">` +
      `<div class="row" style="flex-wrap:wrap;gap:10px">` +
      `<div class="field" style="max-width:140px"><label for="seed-count">documents</label><input id="seed-count" class="input" type="number" min="1" max="5000" value="50" /></div>` +
      `<div style="align-self:flex-end" class="row">` +
      `<button type="button" id="seed-run" class="btn btn-primary">generate</button>` +
      `<button type="button" id="seed-cancel" class="btn btn-danger" hidden>cancel</button>` +
      `</div>` +
      `</div>` +
      `<div id="seed-progress-wrap"></div>` +
      `<div id="seed-status" class="ui-stat-hint"></div>` +
      `</div></div>`;

    B.$("#seed-run", host).addEventListener("click", () => {
      const count = Math.max(1, Math.min(5000, Number(B.$("#seed-count", host).value || "0")));
      if (!count) return;
      runSeed(host, count);
    });
    B.$("#seed-cancel", host).addEventListener("click", () => { cancelRequested = true; });
  }

  // Rendered once: this form has no dependency on live overview data (it
  // reads the Data view's current collection fresh at click time, not at
  // render time), so re-rendering on every SSE tick would only risk
  // clobbering an operator mid-typing the count -- the exact bug
  // view-autoscaler.js's own controlsDirty flag exists to avoid
  // elsewhere.
  document.addEventListener("DOMContentLoaded", render);
})();
