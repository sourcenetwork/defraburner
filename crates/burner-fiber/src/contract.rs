//! Guards the one hazard of a protocol that exists on both sides of a wasm
//! boundary: silent drift.
//!
//! The host's [`crate::protocol`] and the guest's
//! `packages/defradb/source/protocol.rs` cannot be the same Rust type. The
//! guest is a separate cargo tree, built for a different target, with a
//! dependency set that does not compile for the host at all. Two copies is
//! therefore forced, not a choice.
//!
//! What is a choice is whether the copies are allowed to diverge quietly.
//! These tests read the guest's actual source and fail if an operation is
//! added, renamed, or removed on one side only, or if the frame ceilings
//! stop matching. A protocol mismatch would otherwise appear at runtime as
//! a fiber that hangs or answers `{"status":"err","stage":"decode"}`, which
//! is a far worse place to learn about it than a red test.

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    /// The guest's protocol source, resolved from this crate's own location
    /// so the test does not depend on the working directory.
    fn guest_protocol_source() -> String {
        let path: PathBuf = [
            env!("CARGO_MANIFEST_DIR"),
            "..",
            "..",
            "packages",
            "defradb",
            "source",
            "protocol.rs",
        ]
        .iter()
        .collect();
        std::fs::read_to_string(&path).unwrap_or_else(|error| {
            panic!(
                "cannot read the guest protocol at {}: {error}. \
                 The host/guest protocol contract cannot be verified without it.",
                path.display()
            )
        })
    }

    /// Operation names the host can send. Kept as a literal list rather
    /// than derived from the enum, so that adding a host variant without
    /// adding the guest's arm fails here instead of at runtime.
    const HOST_OPS: &[&str] = &[
        "ping",
        "add_schema",
        "list_collections",
        "query",
        "mutate",
        "shutdown",
    ];

    /// Maps a guest `Request` variant name to its serde `snake_case` tag,
    /// which is what actually travels on the wire.
    fn snake_case(variant: &str) -> String {
        let mut out = String::new();
        for (i, ch) in variant.char_indices() {
            if ch.is_uppercase() {
                if i != 0 {
                    out.push('_');
                }
                out.extend(ch.to_lowercase());
            } else {
                out.push(ch);
            }
        }
        out
    }

    /// Variant names declared in the guest's `enum Request`.
    fn guest_request_ops(source: &str) -> Vec<String> {
        let start = source
            .find("pub enum Request {")
            .expect("the guest declares `pub enum Request`");
        let body = &source[start..];
        let end = body.find("\n}").expect("the Request enum is closed");
        body[..end]
            .lines()
            .skip(1)
            .filter_map(|line| {
                let line = line.trim();
                if line.is_empty() || line.starts_with("//") || line.starts_with('#') {
                    return None;
                }
                // `Ping,` or `AddSchema { sdl: String },`
                let name: String = line
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                    .collect();
                let first = name.chars().next()?;
                if first.is_uppercase() {
                    Some(snake_case(&name))
                } else {
                    None
                }
            })
            .collect()
    }

    #[test]
    fn host_and_guest_agree_on_every_operation() {
        let guest_ops = guest_request_ops(&guest_protocol_source());
        assert!(
            !guest_ops.is_empty(),
            "parsed no operations out of the guest protocol; the parser is broken, \
             which would make this contract test vacuously pass"
        );

        let mut expected: Vec<String> = HOST_OPS.iter().map(|s| s.to_string()).collect();
        let mut actual = guest_ops;
        expected.sort();
        actual.sort();
        assert_eq!(
            expected, actual,
            "the host and guest protocols have drifted. Every operation the host \
             sends must exist on the guest and vice versa; update both \
             crates/burner-fiber/src/protocol.rs and \
             packages/defradb/source/protocol.rs together."
        );
    }

    #[test]
    fn host_and_guest_agree_on_the_frame_ceiling() {
        let source = guest_protocol_source();
        let line = source
            .lines()
            .find(|line| line.contains("pub const MAX_FRAME_BYTES"))
            .expect("the guest declares MAX_FRAME_BYTES");
        // `pub const MAX_FRAME_BYTES: u32 = 64 * 1024 * 1024;`
        let expression = line
            .split('=')
            .nth(1)
            .expect("MAX_FRAME_BYTES has a value")
            .trim()
            .trim_end_matches(';');
        let guest_value: u32 = expression
            .split('*')
            .map(|part| part.trim().parse::<u32>().expect("a numeric factor"))
            .product();
        assert_eq!(
            guest_value,
            crate::protocol::MAX_FRAME_BYTES,
            "the host and guest frame ceilings differ; a frame one side accepts \
             would be refused by the other."
        );
    }

    #[test]
    fn the_guest_preopen_path_matches_the_host_mount() {
        let path: std::path::PathBuf = [
            env!("CARGO_MANIFEST_DIR"),
            "..",
            "..",
            "packages",
            "defradb",
            "source",
            "main.rs",
        ]
        .iter()
        .collect();
        let source = std::fs::read_to_string(&path).expect("the guest main.rs is readable");
        let line = source
            .lines()
            .find(|line| line.contains("const DATA_DIR"))
            .expect("the guest declares DATA_DIR");
        let guest_dir = line
            .split('"')
            .nth(1)
            .expect("DATA_DIR is a string literal");
        assert_eq!(
            guest_dir,
            crate::GUEST_DATA_DIR,
            "the host preopens a different guest path than the guest opens its \
             database under; the fiber would come up with an empty database."
        );
    }
}
