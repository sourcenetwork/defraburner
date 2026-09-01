# defraburner/defradb

One persistent DefraDB database, compiled to `wasm32-wasip1` and packed by
the `burn` CLI into an AOT-compiled `.afb`. This is the data plane of a
DefraCell, running inside the sandbox instead of beside it.

Build it (and run its tests on the real target):

```sh
just package-defradb
```

That produces `defraburner-defradb-0.1.0.afb` containing
`precompiled/wasm32-wasip1/main.wasm`. The `.afb` is a build output, not
source.

## What is in the module

The whole engine: collections, schema, CRDT merge, indexes, transactions,
and the GraphQL planner and executor (`db`, `query`, `schema`, `document`,
`crdt`, `storage`, `events`). Persistence is real: regolith writes a
`MANIFEST`, a WAL, and SST files into the directory the host preopens at
`/data`, so a fiber restarts with its data intact.

## What is deliberately not in it

Networking. WASI preview1 has no sockets, so libp2p and iroh cannot be
compiled in at all (see `docs/plans/defradb-wasm.md` section 1 for the
evidence). Replication is the host's job and reaches the fiber over the
protocol. The node is assembled from `db` directly rather than through
`embedded`, because `embedded` hard-requires `p2p/libp2p-transport`.

## Protocol

Length-prefixed JSON, 4-byte big-endian length then that many bytes of
UTF-8 JSON, in both directions. `main` blocks reading frames until stdin
closes, so the host keeps one instance alive per cell: one open store, one
collection cache, one memtable. Closing stdin, or an explicit `shutdown`,
ends the fiber cleanly.

Operations: `ping`, `add_schema`, `list_collections`, `query`, `mutate`,
`shutdown`. Every failure is an ordinary `{"status":"err","stage":...}`
response, never a process exit: a query that fails to parse is a normal
event in a database's life and must not take the cell down.

Frames are capped at 64 MiB and the cap is enforced on the length header,
before any buffer is allocated, so a corrupt or hostile header cannot make
the guest reserve an arbitrary allocation.

## Execution model

Single-threaded by construction: `wasm32-wasip1` has no threads, so there
is no tokio runtime, engine futures are driven by
`futures::executor::block_on`, and regolith compacts on the calling
thread, the mode its own portability notes document for this target. One
request is in flight per fiber; concurrency comes from running many
fibers, not from threading one.

## Why it pins upstream's lockfile

`Cargo.lock` is copied from defradb.rs. `db` reaches `reqwest`
unconditionally, and a fresh resolution picks a `socket2` that does not
build for wasip1; upstream's pins are the resolution that does. See D37.
