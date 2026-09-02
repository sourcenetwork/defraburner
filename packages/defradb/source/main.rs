//! One persistent DefraDB database, compiled to `wasm32-wasip1` and shipped
//! as an AOT-compiled afterburner package.
//!
//! # What this is
//!
//! A DefraCell's data plane, running inside the sandbox instead of beside
//! it. The whole engine is here: collections, schema, CRDT merge, indexes,
//! transactions, and the GraphQL query planner and executor. It persists to
//! a real directory the host preopens, so a fiber survives restart with its
//! data intact.
//!
//! # What this is not
//!
//! There is no networking in this module and there cannot be: WASI preview1
//! has no sockets, so libp2p and iroh cannot be compiled in (verified, see
//! docs/plans/defradb-wasm.md section 1). Replication is the host's job,
//! bridged in over the protocol rather than dialed from here. The engine is
//! built from `db` directly rather than through `embedded`, because
//! `embedded` hard-requires `p2p/libp2p-transport`.
//!
//! # Execution model
//!
//! Single-threaded by construction. `wasm32-wasip1` has no threads, so
//! there is no tokio runtime; engine futures are driven by
//! `futures::executor::block_on` and regolith runs compaction on the
//! calling thread, which is the mode its own portability notes document
//! for this target. One request is in flight at a time per fiber; the host
//! gets its concurrency from running many fibers, not from threading one.

mod protocol;

use std::io::{Read, Write};
use std::sync::Arc;

use db::{AutoCommitMutator, DbCollectionProvider, LensedAutoCommitFetcher, DB};
use futures::executor::block_on;
use protocol::{Request, Response, MAX_FRAME_BYTES};
use query::runner::QueryRunner;
use storage::RegolithStore;

/// Where the host preopens this fiber's data directory.
///
/// A fixed guest path, not a configurable one: the guest has no environment
/// and no arguments under the sealed WASI context, and the host is free to
/// map any host directory onto it. The cell identity lives on the host
/// side, where it belongs.
const DATA_DIR: &str = "/data";

/// The database directory inside [`DATA_DIR`].
const DB_SUBDIR: &str = "db";

fn main() {
    // Everything is reported through the protocol where possible. A failure
    // this early (the store will not open at all) has no frame to answer,
    // so it goes to stderr and a non-zero exit, which the host surfaces as
    // the cell's ignition error rather than a silent dead fiber.
    if let Err(message) = run() {
        eprintln!("defradb fiber: {message}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let mut fiber = Fiber::open()?;
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let stdout = std::io::stdout();
    let mut output = stdout.lock();

    loop {
        let frame = match read_frame(&mut input)? {
            // Stdin closed: the host dropped the pipe. That is the ordinary
            // shutdown path, so close the store cleanly rather than letting
            // the process die with a hot memtable.
            None => break,
            Some(frame) => frame,
        };

        let response = match serde_json::from_slice::<Request>(&frame) {
            Ok(Request::Shutdown) => {
                write_frame(&mut output, &Response::ok())?;
                break;
            }
            Ok(request) => fiber.handle(request),
            Err(error) => Response::err("decode", error.to_string()),
        };
        write_frame(&mut output, &response)?;
    }

    fiber.close();
    Ok(())
}

/// One open database plus the query machinery over it.
struct Fiber {
    db: Arc<DB<RegolithStore>>,
    runner: QueryRunner<LensedAutoCommitFetcher<RegolithStore>>,
}

impl Fiber {
    /// Opens (or creates) the database under the host-preopened directory
    /// and loads the collections already stored there, so a restarted fiber
    /// comes back with its schema rather than an empty catalog.
    // The database and its mutator are not `Send`/`Sync`: wasm drops those
    // bounds, since the target is single-threaded. Sharing them by `Arc` is
    // what the query runner's own API expects, and there are no threads
    // here to send them across. Same call and same reasoning as upstream's
    // browser client (defradb.rs crates/wasm/src/client.rs).
    #[allow(clippy::arc_with_non_send_sync)]
    fn open() -> Result<Self, String> {
        let path = format!("{DATA_DIR}/{DB_SUBDIR}");
        let store = RegolithStore::open(&path)
            .map_err(|error| format!("opening the regolith store at {path}: {error}"))?;

        let mut db = DB::new(store).map_err(|error| format!("creating the database: {error}"))?;
        db.set_event_bus(Arc::new(events::ChannelBus::new()));
        block_on(db.load_collections())
            .map_err(|error| format!("loading stored collections: {error}"))?;

        let db = Arc::new(db);
        let runner = QueryRunner::with_provider(
            LensedAutoCommitFetcher::new(Arc::clone(&db)),
            DbCollectionProvider::new_arc(Arc::clone(&db)),
        )
        .with_mutator(Arc::new(AutoCommitMutator::new(Arc::clone(&db))))
        .with_collection_truncator(db::DbCollectionTruncator::new_arc(Arc::clone(&db)));

        Ok(Self { db, runner })
    }

    fn handle(&mut self, request: Request) -> Response {
        match request {
            Request::Ping => Response::data(serde_json::json!({ "pong": true })),
            Request::AddSchema { sdl } => self.add_schema(&sdl),
            Request::ListCollections => match self.db.list_collections() {
                Ok(names) => Response::data(serde_json::json!({ "collections": names })),
                Err(error) => Response::err("list_collections", error.to_string()),
            },
            Request::Query { graphql } => match block_on(self.runner.execute_query(&graphql)) {
                Ok(value) => Response::data(value),
                Err(error) => Response::err("query", error.to_string()),
            },
            Request::Mutate { graphql } => match block_on(self.runner.execute_mutation(&graphql)) {
                Ok(value) => Response::data(value),
                Err(error) => Response::err("mutate", error.to_string()),
            },
            // Handled by the loop, which must stop reading rather than
            // merely answer; reaching here would be a bug in that dispatch.
            Request::Shutdown => Response::err("shutdown", "shutdown is handled by the loop"),
        }
    }

    /// Parses and validates SDL before creating anything, so a fragment that
    /// is half-valid does not leave half its collections registered.
    fn add_schema(&self, sdl: &str) -> Response {
        let collections = match query::sdl_parse::parse_sdl(sdl) {
            Ok(collections) => collections,
            Err(error) => return Response::err("add_schema", format!("parsing SDL: {error}")),
        };
        if let Err(error) = schema::definition_validation::validate_new_collections(&collections) {
            return Response::err("add_schema", format!("validating SDL: {error}"));
        }

        let mut added = Vec::with_capacity(collections.len());
        for collection in collections {
            let name = collection.name.clone();
            if let Err(error) = block_on(self.db.create_collection(collection)) {
                return Response::err(
                    "add_schema",
                    format!("creating collection '{name}': {error}"),
                );
            }
            added.push(name);
        }
        Response::data(serde_json::json!({ "collections_added": added }))
    }

    /// Closes the store so the WAL is durable and no transaction is left
    /// in flight. A failure here is reported, never swallowed, but cannot
    /// be answered over the protocol: by the time this runs the loop has
    /// already stopped reading.
    fn close(self) {
        use storage::Store;
        if let Err(error) = block_on(self.db.store().close()) {
            eprintln!("defradb fiber: closing the store: {error}");
        }
    }
}

/// Reads one length-prefixed frame. `Ok(None)` means a clean EOF at a frame
/// boundary, which is the host closing the pipe.
fn read_frame(input: &mut impl Read) -> Result<Option<Vec<u8>>, String> {
    let mut header = [0u8; 4];
    if !read_exact_or_eof(input, &mut header)? {
        return Ok(None);
    }
    let len = u32::from_be_bytes(header);
    if len > MAX_FRAME_BYTES {
        // Refused before allocating: this is the whole point of checking the
        // header rather than trusting it.
        return Err(format!(
            "frame header claims {len} bytes, over the {MAX_FRAME_BYTES}-byte ceiling"
        ));
    }
    let mut body = vec![0u8; len as usize];
    if !read_exact_or_eof(input, &mut body)? {
        // A header arrived and then nothing did: the frame is incomplete,
        // whatever the pipe's state.
        return Err(format!("stdin closed after a header promising {len} bytes"));
    }
    Ok(Some(body))
}

/// Fills `buf` completely.
///
/// `Ok(false)` means EOF arrived at a clean boundary, before any byte of
/// this buffer was read. EOF *part-way* through is an error, not a clean
/// boundary: a half-delivered header or body means the host died mid-write,
/// and reporting that as a normal shutdown would turn a truncated stream
/// into a silent, successful-looking exit.
fn read_exact_or_eof(input: &mut impl Read, buf: &mut [u8]) -> Result<bool, String> {
    let mut filled = 0;
    while filled < buf.len() {
        match input.read(&mut buf[filled..]) {
            Ok(0) if filled == 0 => return Ok(false),
            Ok(0) => {
                return Err(format!(
                    "stdin closed mid-frame after {filled} of {} bytes",
                    buf.len()
                ));
            }
            Ok(n) => filled += n,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(format!("reading stdin: {error}")),
        }
    }
    Ok(true)
}

fn write_frame(output: &mut impl Write, response: &Response) -> Result<(), String> {
    let body =
        serde_json::to_vec(response).map_err(|error| format!("encoding the response: {error}"))?;
    let len = u32::try_from(body.len())
        .map_err(|_| format!("response of {} bytes exceeds a u32 length", body.len()))?;
    if len > MAX_FRAME_BYTES {
        return Err(format!(
            "response of {len} bytes exceeds the {MAX_FRAME_BYTES}-byte ceiling"
        ));
    }
    output
        .write_all(&len.to_be_bytes())
        .and_then(|()| output.write_all(&body))
        .and_then(|()| output.flush())
        .map_err(|error| format!("writing stdout: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frame_round_trips_through_the_reader() {
        let mut buffer = Vec::new();
        write_frame(&mut buffer, &Response::data(serde_json::json!({"a": 1}))).unwrap();
        let mut cursor = std::io::Cursor::new(buffer);
        let frame = read_frame(&mut cursor).unwrap().expect("one frame");
        let value: serde_json::Value = serde_json::from_slice(&frame).unwrap();
        assert_eq!(value["status"], "ok");
        assert_eq!(value["data"]["a"], 1);
    }

    #[test]
    fn a_clean_eof_at_a_boundary_is_not_an_error() {
        let mut cursor = std::io::Cursor::new(Vec::new());
        assert!(read_frame(&mut cursor).unwrap().is_none());
    }

    #[test]
    fn a_truncated_frame_is_an_error_not_a_silent_eof() {
        // A header promising 16 bytes, followed by 3.
        let mut bytes = 16u32.to_be_bytes().to_vec();
        bytes.extend_from_slice(b"abc");
        let mut cursor = std::io::Cursor::new(bytes);
        let error = read_frame(&mut cursor).unwrap_err();
        assert!(error.contains("mid-frame"), "unexpected error: {error}");
    }

    #[test]
    fn an_oversized_header_is_refused_before_allocating() {
        let bytes = (MAX_FRAME_BYTES + 1).to_be_bytes().to_vec();
        let mut cursor = std::io::Cursor::new(bytes);
        let error = read_frame(&mut cursor).unwrap_err();
        assert!(error.contains("ceiling"), "unexpected error: {error}");
    }

    #[test]
    fn several_frames_read_back_in_order() {
        let mut buffer = Vec::new();
        for i in 0..3 {
            write_frame(&mut buffer, &Response::data(serde_json::json!({ "i": i }))).unwrap();
        }
        let mut cursor = std::io::Cursor::new(buffer);
        for i in 0..3 {
            let frame = read_frame(&mut cursor).unwrap().expect("frame");
            let value: serde_json::Value = serde_json::from_slice(&frame).unwrap();
            assert_eq!(value["data"]["i"], i);
        }
        assert!(read_frame(&mut cursor).unwrap().is_none());
    }
}
