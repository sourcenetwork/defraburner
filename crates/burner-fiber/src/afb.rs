//! Pulling the precompiled wasm module out of a `.afb`.
//!
//! A `.afb` is a zstd-compressed tar (the format `burn compile` writes and
//! `burner-policy` already unpacks for policy packages). The AOT-compiled
//! module lives at a fixed archive path.

use anyhow::{Context, Result, anyhow, bail};
use std::io::Read;

/// Archive path of the AOT-compiled module inside a `.afb`.
///
/// The same convention `burner_policy::engine` reads for policy packages,
/// and the one afterburner's own packer documents (`precompiled/<target>/`).
const PRECOMPILED_ENTRY_PATH: &str = "precompiled/wasm32-wasip1/main.wasm";

/// Largest module this will extract, in bytes.
///
/// Bounds the decompressed size rather than the archive's: a zstd bomb is
/// small on disk and enormous in memory, so the compressed length is not
/// the resource that fails here. The fiber module measures about 5 MiB, so
/// 256 MiB is far above any honest growth and far below a memory incident.
const MAX_MODULE_BYTES: u64 = 256 * 1024 * 1024;

/// Extracts `precompiled/wasm32-wasip1/main.wasm` from `.afb` bytes.
///
/// Fails loudly, naming what it found, when the entry is absent: a package
/// built without `burn compile` (or one whose source-only fallback was
/// packed instead) is a real configuration error, and reporting it as an
/// empty module would surface much later as an opaque instantiation
/// failure.
pub fn extract_module(afb_bytes: &[u8]) -> Result<Vec<u8>> {
    let decoder = zstd::Decoder::new(afb_bytes).context("decompressing the .afb (zstd)")?;
    let mut archive = tar::Archive::new(decoder);
    let mut seen = Vec::new();

    for entry in archive.entries().context("reading the .afb archive")? {
        let mut entry = entry.context("reading a .afb entry")?;
        let path = entry
            .path()
            .context("reading a .afb entry path")?
            .to_string_lossy()
            .into_owned();

        if path != PRECOMPILED_ENTRY_PATH {
            seen.push(path);
            continue;
        }

        let declared = entry.header().size().context("reading the entry size")?;
        if declared > MAX_MODULE_BYTES {
            bail!(
                "{PRECOMPILED_ENTRY_PATH} declares {declared} bytes, over the \
                 {MAX_MODULE_BYTES}-byte ceiling"
            );
        }
        // Read through a limited reader rather than trusting the header:
        // the declared size and the actual stream are separate claims, and
        // only the second one allocates.
        let mut module = Vec::with_capacity(declared as usize);
        let read = entry
            .by_ref()
            .take(MAX_MODULE_BYTES + 1)
            .read_to_end(&mut module)
            .context("reading the precompiled module")?;
        if read as u64 > MAX_MODULE_BYTES {
            bail!("the precompiled module exceeds the {MAX_MODULE_BYTES}-byte ceiling");
        }
        if module.is_empty() {
            bail!("{PRECOMPILED_ENTRY_PATH} is present but empty");
        }
        return Ok(module);
    }

    Err(anyhow!(
        "the package has no {PRECOMPILED_ENTRY_PATH}; it holds {} entr{} ({}). \
         Build it with `just package-defradb`, which runs `burn compile`.",
        seen.len(),
        if seen.len() == 1 { "y" } else { "ies" },
        if seen.is_empty() {
            "none".to_string()
        } else {
            seen.join(", ")
        }
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a `.afb`-shaped archive (zstd over tar) from `(path, bytes)`.
    fn pack(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut tar_bytes = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_bytes);
            for (path, bytes) in entries {
                let mut header = tar::Header::new_gnu();
                header.set_size(bytes.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                builder.append_data(&mut header, path, *bytes).unwrap();
            }
            builder.finish().unwrap();
        }
        zstd::encode_all(&tar_bytes[..], 3).unwrap()
    }

    #[test]
    fn extracts_the_precompiled_module() {
        let afb = pack(&[
            ("afb.toml", b"[package]"),
            (PRECOMPILED_ENTRY_PATH, b"\0asm\x01\0\0\0"),
        ]);
        assert_eq!(extract_module(&afb).unwrap(), b"\0asm\x01\0\0\0");
    }

    #[test]
    fn a_package_without_a_precompiled_module_names_what_it_found() {
        let afb = pack(&[
            ("afb.toml", b"[package]"),
            ("source/main.rs", b"fn main(){}"),
        ]);
        let error = extract_module(&afb).unwrap_err();
        let rendered = format!("{error:#}");
        assert!(rendered.contains("has no precompiled"), "{rendered}");
        assert!(rendered.contains("source/main.rs"), "{rendered}");
        assert!(rendered.contains("just package-defradb"), "{rendered}");
    }

    #[test]
    fn an_empty_module_entry_is_an_error_not_an_empty_module() {
        let afb = pack(&[(PRECOMPILED_ENTRY_PATH, b"")]);
        let error = extract_module(&afb).unwrap_err();
        assert!(format!("{error:#}").contains("empty"));
    }

    /// Garbage in must produce an ordinary error, never a panic. The exact
    /// stage it fails at is zstd's business (its decoder validates lazily,
    /// so the failure surfaces while reading the archive rather than while
    /// constructing the decoder); what this pins is that it fails at all,
    /// with a message, instead of unwinding through the loader.
    #[test]
    fn a_non_afb_input_fails_cleanly_rather_than_panicking() {
        let error = extract_module(b"this is not zstd at all").unwrap_err();
        let rendered = format!("{error:#}");
        assert!(!rendered.is_empty(), "an error must carry a message");
        assert!(
            rendered.contains(".afb"),
            "the error should name the stage it failed in: {rendered}"
        );
    }
}
