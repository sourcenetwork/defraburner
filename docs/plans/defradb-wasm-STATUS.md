# Status: defradb as an AOT-compiled burn package, one persistent wasm DefraDB per DefraCell

Source plan: docs/plans/defradb-wasm.md. Updated 2026-09-01.
Legend: done | partial | not-started.

| Phase / workstream | Status | Done | Remaining |
|---|---|---|---|
| Phase 0 (de-risk) | done | 0a compile matrix (18 crates green for wasm32-wasip1, 3 red with causes named); 0b persistence PASSED on both the clean and the crash path; 0c burn CLI rust path PASSED (28015-byte precompiled .afb); 0d measured (spike wasm 773.5 KiB, cwasm 1913.3 KiB, cold start JIT 14-15 ms vs AOT 6-7 ms) | nothing; gate met, no kill criterion tripped |
| Phase 1 (native upgrade to defradb.rs main) | done | regolith port across 14 files (D36); workspace compiles against upstream main for the first time; fmt/clippy -D warnings/doc all clean; suite green except the pre-existing D20 detector | nothing |
| Phase 2 (packages/defradb) | done | package builds the whole engine to one 5.1 MiB wasm32-wasip1 module; `burn compile` produces a 1.44 MiB AOT .afb (sha256:df173a5b); framed protocol with 9 tests passing as wasm under wasmtime; `just package-defradb` | host-side loader is Phase 3 |
| Phase 3 (loader) + fibers-are-cells (D40) | done | burner-fiber crate (11 unit + 7 integration tests against the real .afb); every cell owns one wasm DefraDB, born and drained with it; /admin/cells/{id}/db; dashboard Databases view; 3 mechanically-enforced coverage rows; verified live: spawn brings a database, drain removes it, autoscaler scale-down carried it away with no fiber-specific code | tenant traffic still routes to the cell's native node, gated on the mesh bridge |
| Phase 4 (consumers) | not-started | | SDL validation, policy query semantics, dashboard dry-run |
| Phase 5 (mesh bridge) | not-started | | p2p Send-bound fix (operator call: upstream vs vendored), guest P2PTransport over host imports, native transport service, replication golden set over fibers |
| Phase 6 (wasm-only flip) | not-started | | default flip, native deletion, autoscaler over fibers, parity-vs-native golden set, end state: many tenants, many databases, autoscaled, meshed |

## Phase 0 evidence (fresh output, 2026-09-01, this host)

| Probe | Result |
|---|---|
| Engine crates for wasm32-wasip1 | green: db, query, storage, schema, document, crdt, defra-core, events, acp, kms, blockstore, datastore, identity, crypto, cursor, defra-version, zanzibar, lens |
| p2p for wasm32-wasip1 | red: tokio `fs` in the manifest, then 85 errors from one root cause (storage's `Store` is `async_trait(?Send)` on wasm32, p2p's sync layer wants `Send`); 5 files implicated |
| replication-filter | red only via its p2p dependency; turns green with the same fix |
| sourcehub | red: `acp-light-client` pulls `tokio/full`; already optional upstream, stays out |
| regolith persistence, clean close/reopen | found=100/100 mismatch=0; real MANIFEST + wal + sst on the host FS |
| regolith persistence, crash-like (no close) | WAL-only on disk, fresh process recovered found=100/100 mismatch=0 |
| burn compile, language = "rust" | precompiled wasm32-wasip1, 28015 bytes, sha256:b1b0bfec... |
| **Real defradb fiber module** | **5,322,263 bytes wasm; 1,476,487-byte .afb, sha256:df173a5b** |
| **Fiber end to end, session 1** | **schema applied, 2 documents written with real content-addressed docIDs, GraphQL query returned both** |
| **Fiber end to end, session 2 (fresh process, same dir)** | **schema and both documents recovered with no re-apply; new write appended; filtered query `size: {_gt: 15}` correctly returned only the 2 matching docs** |
| **Package tests on the real target** | **9 passed, 0 failed, running as wasm under wasmtime** |
| **Fiber loader (Phase 3), in-process** | **6 integration tests green against the real .afb: schema/write/query, restart-with-data, error isolation, per-fiber isolation, pool reuse, frame ceiling** |
| **Fiber live through the running binary** | **ignite -> schema -> mutate (docID bae-3c09f1c0...) -> query, all 200; isolation confirmed (fiber-2 cannot see fiber-1's collection); bad SDL 400, unknown fiber 404, no auth 401; host stayed up throughout** |
| **Fiber persistence across a full process restart** | **SIGTERM the binary, restart, re-ignite: Product{name:widget,price:42} still present with no schema re-applied** |
| **Fibers = cells (D40), live** | **cells ignite with their database attached (no separate step); `POST /admin/cells` -> new cell answers on /db immediately; `DELETE /admin/cells/{id}` takes its database with it; autoscaler scale_down of an idle cell drained that cell's database with no fiber-specific code in the autoscaler** |
| **regolith directory locking (measured)** | **NOT locked: a second fiber opens a live directory successfully. Single-writer safety is structural (one fiber per cell + id-derived directory + manifest refusing duplicate ids), not enforced by the store. Two tests pin this without claiming a guarantee that does not exist.** |
| AOT vs JIT cold start (storage-only spike) | 6-7 ms vs 14-15 ms, 5 runs each |

## Deferred, with reason (never hidden)

| Item | Why deferred | Where it belongs |
|---|---|---|
| p2p Send-bound relaxation (5 files) + the tokio `fs` line | defradb.rs is read-only to this project; follows an existing upstream pattern (db 49, query 65, crdt 9, blockstore 2 sites; p2p 0) | Explicit operator call at the Phase 5 gate: upstream fix or vendored copy |
| iroh/libp2p inside the wasm module | Impossible on wasm32-wasip1 (browser-only wasm gates in iroh 1.0.1, tokio's wasm feature wall, no UDP in WASI preview1) | Native side stays; Phase 5 bridges around it |
| sourcehub in the wasm package | `acp-light-client` -> `tokio/full`; optional upstream feature, on-chain by nature | Out of scope; native only |
| New upstream knobs on native cells (at-rest encryption, query_limits, rate limits, concurrency caps) | Operator redirected scope to the wasm package; port stays minimal-green | Phase 1 follow-up, on request |
| Native cell deletion | Gated on fiber parity AND the Phase 5 bridge | Phase 6 end state |
| Real defradb fiber cold-start time | Phase 2 measured size (5.1 MiB wasm / 1.44 MiB .afb) but not instantiate latency, which is only meaningful under the real loader | Phase 3, with the fiber loader |

## Honest bottom line

Phases 0, 1 and 2 are done. defraburner compiles and tests against defradb.rs main again, and
`packages/defradb` is a real persistent DefraDB in wasm: driven over its protocol it applied a
schema, wrote documents with genuine content-addressed docIDs, and then a *separate process*
reopened the same directory and recovered all of it, answering a filtered GraphQL query
correctly. That is the plan's central claim, demonstrated rather than argued.

Phase 3 is done too. defraburner now loads the package in-process and runs persistent wasm
DefraDB fibers: verified live through the running binary, including a full SIGTERM restart after
which a re-ignited fiber still held its data. Every fiber capability is operable from the
dashboard and mechanically enforced by the console-coverage test.

Fibers are now cells (D40), not a parallel surface: every cell owns exactly one persistent wasm
DefraDB, created and destroyed with it, and the autoscaler moves them without knowing they exist.

What is NOT done, and matters: tenant traffic still routes to the cell's native embedded node.
A wasm database cannot replicate, because WASI preview1 has no sockets, so routing tenants at
fibers before the Phase 5 mesh bridge would silently downgrade every multi-replica tenant to a
single-node database. That is the one remaining step to the wasm-only end state, and it is
gated on the operator's call on the p2p Send-bound seam. Fiber cold-start latency is still
unmeasured.
