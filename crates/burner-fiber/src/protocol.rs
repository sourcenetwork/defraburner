//! The host half of the fiber wire protocol.
//!
//! Mirrors `packages/defradb/source/protocol.rs`. The two are a contract
//! across a wasm boundary, so they cannot be one shared type: the guest is
//! a separate cargo tree built for a different target. They are instead
//! kept honest by `contract.rs`, which parses the guest's own source and
//! fails if the two ever disagree on an operation name or the frame ceiling.

use serde::{Deserialize, Serialize};

/// Largest frame either side will read or write, in bytes.
///
/// Must equal the guest's `MAX_FRAME_BYTES`; the contract test enforces it.
pub const MAX_FRAME_BYTES: u32 = 64 * 1024 * 1024;

/// One request to a fiber.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Ping,
    AddSchema { sdl: String },
    ListCollections,
    Query { graphql: String },
    Mutate { graphql: String },
    Shutdown,
}

/// One response from a fiber.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Response {
    Ok {
        #[serde(default)]
        data: Option<serde_json::Value>,
    },
    Err {
        stage: String,
        message: String,
    },
}

impl Response {
    /// The payload of a successful response, or an error naming the stage
    /// the guest failed in.
    ///
    /// This is the accessor callers should use: it turns the guest's own
    /// structured failure into an ordinary `Err`, so a caller cannot
    /// accidentally treat a failed response as a success by ignoring the
    /// variant.
    pub fn into_data(self) -> anyhow::Result<serde_json::Value> {
        match self {
            Self::Ok { data } => Ok(data.unwrap_or(serde_json::Value::Null)),
            Self::Err { stage, message } => {
                Err(anyhow::anyhow!("fiber failed at {stage}: {message}"))
            }
        }
    }

    pub fn is_ok(&self) -> bool {
        matches!(self, Self::Ok { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_encode_with_the_op_tag_the_guest_matches_on() {
        let encoded = serde_json::to_string(&Request::Ping).unwrap();
        assert_eq!(encoded, r#"{"op":"ping"}"#);

        let encoded = serde_json::to_string(&Request::AddSchema {
            sdl: "type A { b: String }".into(),
        })
        .unwrap();
        assert!(encoded.starts_with(r#"{"op":"add_schema""#), "{encoded}");
    }

    #[test]
    fn a_successful_response_yields_its_data() {
        let response: Response =
            serde_json::from_str(r#"{"status":"ok","data":{"collections":["A"]}}"#).unwrap();
        let data = response.into_data().unwrap();
        assert_eq!(data["collections"][0], "A");
    }

    #[test]
    fn an_ok_response_without_data_is_still_ok() {
        let response: Response = serde_json::from_str(r#"{"status":"ok"}"#).unwrap();
        assert!(response.is_ok());
        assert!(response.into_data().unwrap().is_null());
    }

    #[test]
    fn a_failed_response_becomes_an_error_naming_the_stage() {
        let response: Response =
            serde_json::from_str(r#"{"status":"err","stage":"query","message":"bad parse"}"#)
                .unwrap();
        assert!(!response.is_ok());
        let error = response.into_data().unwrap_err();
        let rendered = format!("{error:#}");
        assert!(rendered.contains("query"), "{rendered}");
        assert!(rendered.contains("bad parse"), "{rendered}");
    }
}
