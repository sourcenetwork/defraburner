//! The wire protocol between the host (defraburner's fiber loader) and one
//! persistent DefraDB fiber.
//!
//! # Framing
//!
//! Every message, both directions, is a 4-byte big-endian length followed by
//! that many bytes of UTF-8 JSON. Length-prefixed rather than newline
//! delimited because a document value can contain anything, newlines
//! included, and a delimiter a payload can forge is a framing bug waiting
//! for the right document.
//!
//! The frame ceiling is enforced on read ([`MAX_FRAME_BYTES`]): a length
//! header claiming more than that is refused before a single byte of body
//! is allocated, so a corrupt or hostile header cannot make the guest
//! reserve an arbitrary buffer. That is the guest half of the plan's
//! "bound the resource that actually fails" rule; the host enforces the
//! same ceiling on its side.
//!
//! # Why a loop and not one shot
//!
//! A WASI command runs `main` and exits, which would mean reopening the
//! database per request and losing every cache. Instead `main` blocks
//! reading frames until stdin closes, so the host keeps one instance (and
//! therefore one open regolith store, one collection cache, one memtable)
//! alive for the life of the cell. Closing stdin is the shutdown signal.

use serde::{Deserialize, Serialize};

/// Largest frame this guest will read or write, in bytes.
///
/// 64 MiB is far above any single document or query result the cell is
/// expected to carry and far below the wasm32 4 GiB address space, so it
/// bounds a bad header without constraining honest traffic.
pub const MAX_FRAME_BYTES: u32 = 64 * 1024 * 1024;

/// One request from the host.
#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// Liveness and identity probe. Cheap on purpose: the host calls it to
    /// confirm a freshly-instantiated fiber reached its loop.
    Ping,
    /// Apply SDL, creating the collections it declares.
    AddSchema { sdl: String },
    /// The collections currently registered on this fiber.
    ListCollections,
    /// Execute a GraphQL query (read).
    Query { graphql: String },
    /// Execute a GraphQL mutation (write).
    Mutate { graphql: String },
    /// Flush and close the store, then stop the loop. The host may also
    /// simply close stdin; this exists so a graceful drain can be
    /// acknowledged before the pipe goes away.
    Shutdown,
}

/// One response to the host.
///
/// Success and failure are both ordinary responses, never a process exit:
/// a query that fails to parse is a normal event in a database's life and
/// must not take the cell down with it. The loop only exits on `Shutdown`,
/// on stdin EOF, or on a framing error it cannot resynchronize from.
#[derive(Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Response {
    Ok {
        #[serde(skip_serializing_if = "Option::is_none")]
        data: Option<serde_json::Value>,
    },
    /// A failure the caller can act on, carried with the stage it happened
    /// in so the host can log something better than "it failed".
    Err {
        stage: &'static str,
        message: String,
    },
}

impl Response {
    pub fn ok() -> Self {
        Self::Ok { data: None }
    }

    pub fn data(value: serde_json::Value) -> Self {
        Self::Ok { data: Some(value) }
    }

    pub fn err(stage: &'static str, message: impl Into<String>) -> Self {
        Self::Err {
            stage,
            message: message.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_tag_round_trips_every_op() {
        let cases = [
            (r#"{"op":"ping"}"#, "Ping"),
            (
                r#"{"op":"add_schema","sdl":"type A { b: String }"}"#,
                "AddSchema",
            ),
            (r#"{"op":"list_collections"}"#, "ListCollections"),
            (r#"{"op":"query","graphql":"{ A { b } }"}"#, "Query"),
            (r#"{"op":"mutate","graphql":"mutation { }"}"#, "Mutate"),
            (r#"{"op":"shutdown"}"#, "Shutdown"),
        ];
        for (json, what) in cases {
            serde_json::from_str::<Request>(json)
                .unwrap_or_else(|e| panic!("{what} should parse: {e}"));
        }
    }

    #[test]
    fn an_unknown_op_is_rejected_rather_than_silently_ignored() {
        assert!(serde_json::from_str::<Request>(r#"{"op":"drop_everything"}"#).is_err());
    }

    #[test]
    fn ok_without_data_omits_the_field_entirely() {
        assert_eq!(
            serde_json::to_string(&Response::ok()).unwrap(),
            r#"{"status":"ok"}"#
        );
    }

    #[test]
    fn err_carries_the_stage_it_failed_in() {
        let encoded = serde_json::to_string(&Response::err("add_schema", "bad sdl")).unwrap();
        assert!(encoded.contains(r#""status":"err""#));
        assert!(encoded.contains(r#""stage":"add_schema""#));
        assert!(encoded.contains("bad sdl"));
    }
}
