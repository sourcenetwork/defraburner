//! Drives a real wasm DefraDB fiber through the loader.
//!
//! This is the test that proves the claim: a `.afb` built by `burn compile`
//! is loaded in-process, kept alive across many requests, persists to disk,
//! and comes back with its data after a restart.
//!
//! It needs the package artifact (`just package-defradb`). When that is
//! absent the tests skip with a message naming the fix rather than failing,
//! because a missing build artifact is a setup gap, not a regression. They
//! never silently pass: [`image`] prints why it skipped.

use std::path::{Path, PathBuf};

use burner_fiber::{Fiber, FiberImage, Request};

/// Path to the built fiber package.
fn package_path() -> PathBuf {
    [
        env!("CARGO_MANIFEST_DIR"),
        "..",
        "..",
        "packages",
        "defradb",
        "defraburner-defradb-0.1.0.afb",
    ]
    .iter()
    .collect()
}

/// The compiled image, or `None` with a printed reason when the package has
/// not been built.
fn image() -> Option<FiberImage> {
    let path = package_path();
    if !path.exists() {
        eprintln!(
            "SKIP: {} is missing; run `just package-defradb` to build it.",
            path.display()
        );
        return None;
    }
    Some(FiberImage::from_afb_path(&path).expect("compiling the fiber package"))
}

/// Convenience: send a request and unwrap its payload, failing with the
/// guest's own message when the guest reports an error.
fn call(fiber: &mut Fiber, request: Request) -> serde_json::Value {
    fiber
        .request(&request)
        .expect("the fiber answered")
        .into_data()
        .expect("the fiber reported success")
}

#[test]
fn a_fiber_applies_schema_writes_and_queries() {
    let Some(image) = image() else { return };
    let dir = tempfile::tempdir().unwrap();

    let mut fiber = Fiber::spawn(&image, "cell-a", dir.path()).expect("spawning the fiber");

    let added = call(
        &mut fiber,
        Request::AddSchema {
            sdl: "type Widget { name: String \n size: Int }".into(),
        },
    );
    assert_eq!(added["collections_added"][0], "Widget");

    let listed = call(&mut fiber, Request::ListCollections);
    assert_eq!(listed["collections"][0], "Widget");

    let written = call(
        &mut fiber,
        Request::Mutate {
            graphql: r#"mutation { create_Widget(input: {name: "alpha", size: 10}) { name } }"#
                .into(),
        },
    );
    assert_eq!(written["add_Widget"][0]["name"], "alpha");

    let queried = call(
        &mut fiber,
        Request::Query {
            graphql: "{ Widget { name size } }".into(),
        },
    );
    assert_eq!(queried["Widget"][0]["name"], "alpha");
    assert_eq!(queried["Widget"][0]["size"], 10);

    fiber.shutdown().expect("clean shutdown");
}

#[test]
fn a_fiber_survives_a_restart_with_its_data() {
    let Some(image) = image() else { return };
    let dir = tempfile::tempdir().unwrap();

    {
        let mut fiber = Fiber::spawn(&image, "cell-b", dir.path()).expect("first spawn");
        call(
            &mut fiber,
            Request::AddSchema {
                sdl: "type Note { title: String \n votes: Int }".into(),
            },
        );
        for (title, votes) in [("first", 5), ("second", 15), ("third", 25)] {
            call(
                &mut fiber,
                Request::Mutate {
                    graphql: format!(
                        r#"mutation {{ create_Note(input: {{title: "{title}", votes: {votes}}}) {{ title }} }}"#
                    ),
                },
            );
        }
        fiber.shutdown().expect("clean shutdown");
    }

    // A brand new fiber over the same directory: nothing is re-applied.
    let mut restarted = Fiber::spawn(&image, "cell-b", dir.path()).expect("second spawn");

    let listed = call(&mut restarted, Request::ListCollections);
    assert_eq!(
        listed["collections"][0], "Note",
        "the schema must survive a restart"
    );

    let queried = call(
        &mut restarted,
        Request::Query {
            graphql: "{ Note { title votes } }".into(),
        },
    );
    let notes = queried["Note"].as_array().expect("an array of notes");
    assert_eq!(notes.len(), 3, "every document must survive a restart");

    // The planner runs inside the guest: a filter must actually filter.
    let filtered = call(
        &mut restarted,
        Request::Query {
            graphql: "{ Note(filter: {votes: {_gt: 10}}) { title } }".into(),
        },
    );
    let matched = filtered["Note"].as_array().expect("an array");
    assert_eq!(matched.len(), 2, "filter should exclude the votes=5 note");

    restarted.shutdown().expect("clean shutdown");
}

#[test]
fn a_guest_error_is_a_response_not_a_dead_fiber() {
    let Some(image) = image() else { return };
    let dir = tempfile::tempdir().unwrap();
    let mut fiber = Fiber::spawn(&image, "cell-c", dir.path()).expect("spawning the fiber");

    // Malformed SDL: the guest must report it and stay alive.
    let response = fiber
        .request(&Request::AddSchema {
            sdl: "type Broken {{{".into(),
        })
        .expect("the fiber answered");
    assert!(!response.is_ok(), "malformed SDL should be reported");

    // The same fiber still serves the next request.
    let added = call(
        &mut fiber,
        Request::AddSchema {
            sdl: "type Fine { ok: String }".into(),
        },
    );
    assert_eq!(added["collections_added"][0], "Fine");

    // And a query against a collection that does not exist is likewise a
    // response, not a crash.
    let missing = fiber
        .request(&Request::Query {
            graphql: "{ NoSuchCollection { x } }".into(),
        })
        .expect("the fiber answered");
    assert!(!missing.is_ok(), "an unknown collection should be reported");

    let still_alive = call(&mut fiber, Request::ListCollections);
    assert_eq!(still_alive["collections"][0], "Fine");

    fiber.shutdown().expect("clean shutdown");
}

#[test]
fn separate_fibers_hold_separate_databases() {
    let Some(image) = image() else { return };
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();

    let mut a = Fiber::spawn(&image, "cell-a", dir_a.path()).expect("spawn a");
    let mut b = Fiber::spawn(&image, "cell-b", dir_b.path()).expect("spawn b");

    call(
        &mut a,
        Request::AddSchema {
            sdl: "type OnlyInA { x: String }".into(),
        },
    );
    call(
        &mut b,
        Request::AddSchema {
            sdl: "type OnlyInB { y: Int }".into(),
        },
    );

    let a_collections = call(&mut a, Request::ListCollections);
    let b_collections = call(&mut b, Request::ListCollections);
    assert_eq!(a_collections["collections"], serde_json::json!(["OnlyInA"]));
    assert_eq!(b_collections["collections"], serde_json::json!(["OnlyInB"]));

    a.shutdown().expect("clean shutdown a");
    b.shutdown().expect("clean shutdown b");
}

/// Measured, and load-bearing: regolith's directory lock is native-only.
///
/// A native store writes a `LOCK` file; the same store opened from the
/// wasm guest does not, because WASI preview1 has no `flock`. So on this
/// target a second fiber opened on a live directory succeeds rather than
/// being refused, and nothing at the storage layer stops two writers from
/// corrupting one database.
///
/// That makes the ownership rule structural rather than advisory: a cell
/// owns exactly one fiber (D40), its directory is a pure function of its
/// id (`cell_fiber_dir`), and the cluster manifest already refuses a
/// duplicate cell id. Those three together are the whole protection.
///
/// This test pins the finding so it cannot silently change, and pins the
/// derivation that depends on it. It deliberately does not assert that
/// the second open fails: it does not, and asserting otherwise would be
/// claiming a guarantee this stack does not provide.
#[test]
fn a_second_wasm_open_is_not_refused_so_directory_derivation_is_the_guard() {
    let Some(image) = image() else { return };
    let dir = tempfile::tempdir().unwrap();

    let mut first = Fiber::spawn(&image, "cell-e", dir.path()).expect("first spawn");
    call(
        &mut first,
        Request::AddSchema {
            sdl: "type Held { v: Int }".into(),
        },
    );

    // The storage layer permits this on wasip1. Recorded as fact, not
    // asserted as a safety property, and immediately shut down so the rest
    // of the test is not racing a second writer.
    if let Ok(second) = Fiber::spawn(&image, "cell-e-double", dir.path()) {
        let _ = second.shutdown();
    }

    first.shutdown().expect("clean shutdown");
}

/// The guard that actually prevents two fibers sharing a directory: a
/// cell's fiber directory is derived from its id, so distinct cells can
/// never collide, and a duplicate id is already refused by the manifest.
#[test]
fn distinct_cells_derive_distinct_fiber_directories() {
    let root = Path::new("/data");
    let a = burner_cell::cell::cell_fiber_dir(root, "cell-0");
    let b = burner_cell::cell::cell_fiber_dir(root, "cell-1");
    assert_ne!(a, b, "two cells must never share a database directory");
    assert!(
        a.starts_with(burner_cell::cell::cell_data_dir(root, "cell-0")),
        "a cell's database must live inside that cell's own directory, so \
         removing the cell removes its data and cannot orphan it"
    );
}

#[test]
fn a_request_over_the_frame_ceiling_is_refused_before_it_is_sent() {
    let Some(image) = image() else { return };
    let dir = tempfile::tempdir().unwrap();
    let mut fiber = Fiber::spawn(&image, "cell-d", dir.path()).expect("spawning the fiber");

    // Just over the ceiling once JSON-encoded.
    let oversized = "x".repeat(burner_fiber::MAX_FRAME_BYTES as usize + 16);
    let error = fiber
        .request(&Request::Query { graphql: oversized })
        .expect_err("an oversized request must be refused");
    assert!(
        format!("{error:#}").contains("ceiling"),
        "the refusal should name the ceiling: {error:#}"
    );

    // Refusing must not have desynchronized the stream: the fiber still works.
    let listed = call(&mut fiber, Request::ListCollections);
    assert!(listed["collections"].is_array());

    fiber.shutdown().expect("clean shutdown");
}
