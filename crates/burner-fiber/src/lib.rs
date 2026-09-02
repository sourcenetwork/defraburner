//! Runs one persistent DefraDB wasm fiber per cell.
//!
//! A fiber is the `packages/defradb` module (see the plan in
//! `docs/plans/defradb-wasm.md`) instantiated once and kept alive: one
//! open regolith store, one collection
//! cache, one memtable, for the life of the cell. Requests go in and
//! responses come back over a length-prefixed frame protocol.
//!
//! # Why this crate exists rather than afterburner's own runner
//!
//! afterburner's sealed path is one-shot by design: it builds its WASI
//! context as `stdin/stdout/stderr` only, with no preopened directory, and
//! it uses a fresh `Store` per call. Both are correct for a sandboxed UDF
//! and both are fatal for a database, which needs a filesystem and needs to
//! survive between calls. Its long-lived `DaemonRuntime` is JavaScript-only.
//! So the package is still built and shipped by the `burn` toolchain, and
//! this crate is what loads it.
//!
//! # Threading
//!
//! The guest is a WASI *command*: entering it means calling `_start`, which
//! does not return until the guest's loop ends. So each fiber owns a
//! dedicated OS thread parked inside `_start`, and the host talks to it
//! through real OS pipes. That is exactly the shape `wasmtime run` uses;
//! nothing here is a workaround for the guest's design, it is that design's
//! host half.
//!
//! Calls into one fiber are serialized by [`Fiber::request`] taking `&mut
//! self`: the frame protocol is a strict request/response alternation, and
//! two concurrent callers would interleave frames and desynchronize the
//! stream. Concurrency across cells comes from running many fibers.

mod afb;
mod contract;
mod protocol;

use std::io::{Read, Write};
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use wasmtime::{Engine, Linker, Module, Store};
use wasmtime_wasi::cli::{InputFile, OutputFile};
use wasmtime_wasi::p1::WasiP1Ctx;
use wasmtime_wasi::{DirPerms, FilePerms, WasiCtxBuilder};

pub use afb::extract_module;
pub use protocol::{MAX_FRAME_BYTES, Request, Response};

/// Guest path the cell's data directory is preopened at. Must match the
/// package's own `DATA_DIR` (`packages/defradb/source/main.rs`); the two
/// are a contract, so a change to either is a change to both.
const GUEST_DATA_DIR: &str = "/data";

/// A compiled fiber module, shared by every fiber in the process.
///
/// Compiling the 5 MiB engine module is the expensive part of starting a
/// fiber, and it is identical for every cell, so it happens once here and
/// every [`Fiber::spawn`] instantiates from the result. `Engine` and
/// `Module` are both internally shared, so cloning this is cheap.
#[derive(Clone)]
pub struct FiberImage {
    engine: Engine,
    module: Module,
}

impl FiberImage {
    /// Compiles the module from raw wasm bytes.
    pub fn from_wasm(wasm: &[u8]) -> Result<Self> {
        let mut config = wasmtime::Config::new();
        // The guest blocks on stdin, so it must not be preempted by an
        // epoch or fuel deadline the way a UDF would be: a fiber waiting
        // for work is healthy, not runaway.
        config.consume_fuel(false);
        let engine = Engine::new(&config)
            .map_err(anyhow::Error::from)
            .context("building the wasm engine")?;
        let module = Module::new(&engine, wasm)
            .map_err(anyhow::Error::from)
            .context("compiling the defradb fiber module")?;
        Ok(Self { engine, module })
    }

    /// Compiles the module carried by a `.afb` produced by `burn compile`.
    pub fn from_afb(afb_bytes: &[u8]) -> Result<Self> {
        let wasm = afb::extract_module(afb_bytes)?;
        Self::from_wasm(&wasm)
    }

    /// Reads and compiles the `.afb` at `path`.
    pub fn from_afb_path(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path)
            .with_context(|| format!("reading the fiber package at {}", path.display()))?;
        Self::from_afb(&bytes)
    }
}

/// One live fiber: a wasm DefraDB instance with its own data directory.
pub struct Fiber {
    /// Host end of the guest's stdin. Dropping it closes the guest's stdin,
    /// which is the guest's shutdown signal.
    to_guest: Option<std::fs::File>,
    /// Host end of the guest's stdout.
    from_guest: std::io::BufReader<std::fs::File>,
    /// The thread parked inside `_start`. Joined on drop so a dropped fiber
    /// never leaks a thread or leaves a store open.
    worker: Option<std::thread::JoinHandle<Result<()>>>,
    /// Identifier for logs and errors, so a failure names the cell.
    cell_id: String,
}

impl Fiber {
    /// Instantiates a fiber over `data_dir`, which is preopened read-write
    /// at the guest's `/data`, and waits for it to answer a `Ping`.
    ///
    /// The ping is not ceremony: instantiation returning says the thread
    /// started, not that the guest opened its database. A store that fails
    /// to open must surface here, as an ignition failure with the guest's
    /// own message, rather than as a mysterious timeout on the cell's first
    /// real request.
    pub fn spawn(image: &FiberImage, cell_id: &str, data_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(data_dir)
            .with_context(|| format!("creating the fiber data dir {}", data_dir.display()))?;

        let (guest_stdin, to_guest) = os_pipe().context("creating the guest's stdin pipe")?;
        let (from_guest, guest_stdout) = os_pipe().context("creating the guest's stdout pipe")?;

        let wasi = WasiCtxBuilder::new()
            .stdin(InputFile::new(guest_stdin))
            .stdout(OutputFile::new(guest_stdout))
            // Guest stderr is inherited so a guest-side panic or a store
            // error reaches the operator's logs instead of vanishing.
            .inherit_stderr()
            .preopened_dir(data_dir, GUEST_DATA_DIR, DirPerms::all(), FilePerms::all())
            .map_err(anyhow::Error::from)
            .with_context(|| format!("preopening {}", data_dir.display()))?
            .build_p1();

        let engine = image.engine.clone();
        let module = image.module.clone();
        let thread_name = format!("fiber-{cell_id}");
        let worker = std::thread::Builder::new()
            .name(thread_name.clone())
            .spawn(move || run_guest(engine, module, wasi))
            .with_context(|| format!("spawning the fiber thread for cell '{cell_id}'"))?;

        let mut fiber = Self {
            to_guest: Some(to_guest),
            from_guest: std::io::BufReader::new(from_guest),
            worker: Some(worker),
            cell_id: cell_id.to_string(),
        };

        match fiber.request(&Request::Ping) {
            Ok(Response::Ok { .. }) => Ok(fiber),
            Ok(Response::Err { stage, message }) => {
                bail!("fiber for cell '{cell_id}' failed at {stage}: {message}")
            }
            Err(error) => Err(error.context(format!(
                "fiber for cell '{cell_id}' did not answer its readiness ping"
            ))),
        }
    }

    /// Sends one request and reads its response.
    ///
    /// `&mut self` is the serialization: see the crate doc comment.
    pub fn request(&mut self, request: &Request) -> Result<Response> {
        let body = serde_json::to_vec(request).context("encoding a fiber request")?;
        let len = u32::try_from(body.len())
            .map_err(|_| anyhow!("request of {} bytes exceeds a u32 length", body.len()))?;
        if len > MAX_FRAME_BYTES {
            bail!(
                "request of {len} bytes exceeds the {MAX_FRAME_BYTES}-byte frame ceiling; \
                 the guest would refuse it"
            );
        }

        let pipe = self
            .to_guest
            .as_mut()
            .ok_or_else(|| anyhow!("fiber for cell '{}' is already shut down", self.cell_id))?;
        pipe.write_all(&len.to_be_bytes())
            .and_then(|()| pipe.write_all(&body))
            .and_then(|()| pipe.flush())
            .with_context(|| format!("writing to fiber '{}'", self.cell_id))?;

        self.read_response()
    }

    fn read_response(&mut self) -> Result<Response> {
        let mut header = [0u8; 4];
        self.from_guest.read_exact(&mut header).with_context(|| {
            format!("reading the response header from fiber '{}'", self.cell_id)
        })?;
        let len = u32::from_be_bytes(header);
        if len > MAX_FRAME_BYTES {
            bail!(
                "fiber '{}' announced a {len}-byte response, over the \
                 {MAX_FRAME_BYTES}-byte ceiling",
                self.cell_id
            );
        }
        let mut body = vec![0u8; len as usize];
        self.from_guest
            .read_exact(&mut body)
            .with_context(|| format!("reading the response body from fiber '{}'", self.cell_id))?;
        serde_json::from_slice(&body)
            .with_context(|| format!("decoding the response from fiber '{}'", self.cell_id))
    }

    /// Asks the guest to close its store, then waits for its thread.
    ///
    /// Returns the guest's own exit result, so a store that failed to flush
    /// is reported rather than swallowed.
    pub fn shutdown(mut self) -> Result<()> {
        self.shutdown_inner()
    }

    fn shutdown_inner(&mut self) -> Result<()> {
        if self.to_guest.is_some() {
            // Best-effort: if the guest already died, the write fails and
            // the real error is whatever the worker returns below.
            let _ = self.request(&Request::Shutdown);
            // Dropping the write end closes the guest's stdin, which ends
            // its loop even if the Shutdown frame never landed.
            self.to_guest = None;
        }
        match self.worker.take() {
            None => Ok(()),
            Some(worker) => match worker.join() {
                Ok(result) => result,
                Err(_) => bail!("the fiber thread for cell '{}' panicked", self.cell_id),
            },
        }
    }
}

impl Drop for Fiber {
    fn drop(&mut self) {
        if let Err(error) = self.shutdown_inner() {
            tracing::warn!(
                cell_id = %self.cell_id,
                error = %format!("{error:#}"),
                "fiber did not shut down cleanly"
            );
        }
    }
}

/// Instantiates the module and calls `_start`, which parks here until the
/// guest's loop ends.
fn run_guest(engine: Engine, module: Module, wasi: WasiP1Ctx) -> Result<()> {
    let mut linker: Linker<WasiP1Ctx> = Linker::new(&engine);
    wasmtime_wasi::p1::add_to_linker_sync(&mut linker, |ctx| ctx)
        .map_err(anyhow::Error::from)
        .context("wiring WASI into the fiber linker")?;

    let mut store = Store::new(&engine, wasi);
    let instance = linker
        .instantiate(&mut store, &module)
        .map_err(anyhow::Error::from)
        .context("instantiating the fiber module")?;
    let start = instance
        .get_typed_func::<(), ()>(&mut store, "_start")
        .map_err(anyhow::Error::from)
        .context("the fiber module has no _start export")?;

    match start.call(&mut store, ()) {
        Ok(()) => Ok(()),
        Err(error) => {
            // A guest that called `exit(0)` unwinds as an I32Exit trap; that
            // is a clean stop, not a failure, and must not be reported as one.
            if let Some(exit) = error.downcast_ref::<wasmtime_wasi::I32Exit>() {
                if exit.0 == 0 {
                    return Ok(());
                }
                bail!("the fiber exited with status {}", exit.0);
            }
            Err(anyhow!(error).context("the fiber trapped"))
        }
    }
}

/// A unidirectional OS pipe as `(read_end, write_end)`.
///
/// `std::io::pipe` (stable since 1.87) rather than an FFI call: the standard
/// library already owns this, including the close-on-exec and error handling
/// a hand-rolled `pipe(2)` would have to repeat. The `File` conversion goes
/// through `OwnedFd`, which is a move of the descriptor, not a dup.
fn os_pipe() -> Result<(std::fs::File, std::fs::File)> {
    let (reader, writer) = std::io::pipe().context("creating an OS pipe")?;
    Ok((
        std::fs::File::from(std::os::fd::OwnedFd::from(reader)),
        std::fs::File::from(std::os::fd::OwnedFd::from(writer)),
    ))
}
