# burner-fiber

Loads the AOT-compiled `packages/defradb` `.afb` and runs one persistent
wasm DefraDB per cell.

A fiber is not a separate resource from a cell (D40): every cell owns
exactly one, sharing its id and its lifetime. `cell::ignite` spawns it;
dropping the cell shuts it down. Its data lives at
`<data_root>/cells/<id>/fiber/`, inside the cell's own directory, so
removing a cell removes its database and cannot orphan one.

## Why this crate rather than afterburner's own runner

afterburner's sealed path is one-shot by design: its WASI context is
stdin/stdout/stderr with no preopened directory, and it uses a fresh
`Store` per call. Both are correct for a sandboxed UDF and both are fatal
for a database, which needs a filesystem and needs to survive between
calls. Its long-lived `DaemonRuntime` is JavaScript-only. So the package
is still built and shipped by the `burn` toolchain, and this crate loads
it, on wasmtime directly.

## Threading

The guest is a WASI *command*: entering it means calling `_start`, which
does not return until the guest's loop ends. Each fiber therefore owns a
dedicated OS thread parked inside `_start`, and the host talks to it over
real OS pipes, which is the same shape `wasmtime run` uses.

`Fiber::request` takes `&mut self`, which is the serialization: the frame
protocol is a strict request/response alternation, so two concurrent
callers would interleave frames and desynchronize the stream.
Concurrency across cells comes from running many fibers.

## The protocol contract

The wire protocol exists twice, here and in
`packages/defradb/source/protocol.rs`. It cannot be one shared type: the
guest is a separate cargo tree built for a different target, with a `db`
configuration that does not compile for the host at all.

`contract.rs` is what keeps the copies honest. It parses the guest's own
source and fails if an operation is added, renamed, or removed on one
side only, if the frame ceilings diverge, or if the guest's `DATA_DIR`
stops matching the host's preopen path. Its parser asserts it found
operations at all, so a broken parser cannot make the check vacuously
pass.

## A measured caveat

regolith does **not** lock its data directory: a second fiber opened on a
live directory succeeds. Single-writer safety is therefore structural,
not enforced by the store - one fiber per cell, a directory derived from
the cell id, and a cluster manifest that already refuses a duplicate id.
Two tests in `tests/fiber_lifecycle.rs` pin this, and neither claims a
guarantee this stack does not provide.
