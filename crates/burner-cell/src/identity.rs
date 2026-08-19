//! Per-cell Ed25519 signing key management.
//!
//! Persisted format: the raw 32-byte Ed25519 seed, written to
//! `<data_root>/keys/<cell-id>.ed25519` with 0600 permissions.
//!
//! `embedded::SigningKey::Ed25519` does **not** take a bare 32-byte seed: it
//! is fed straight to `crypto::Ed25519PrivateKey::from_bytes`, which
//! requires exactly 64 bytes (32-byte seed + 32-byte derived public key) and
//! validates that the second half matches the key derived from the first
//! (verified: `defradb.rs crates/crypto/src/keys/ed25519.rs:74-115`, and the
//! call site `defradb.rs crates/embedded/src/node_identity.rs:25-29`). So a
//! loaded seed is expanded to that 64-byte form before it is handed to
//! `embedded`. This is exactly what upstream's own
//! `crypto::keys::ed25519::ed25519_key_from_seed` does
//! (`crates/crypto/src/keys/ed25519.rs:289-300`); `ed25519-dalek` is used
//! directly here instead of depending on defradb.rs's internal `crypto`
//! crate, since dalek is the vetted primitive doing the real work and is
//! already the pinned dependency `crypto` itself builds on
//! (`defradb.rs/Cargo.toml`: `ed25519-dalek = { version = "2.1", ... }`).

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use ed25519_dalek::SigningKey as DalekSigningKey;
use rand::RngCore;
use rand::rngs::OsRng;

const SEED_LEN: usize = 32;

/// The path a cell's signing key lives at under `data_root`.
pub fn key_path(data_root: &Path, cell_id: &str) -> PathBuf {
    data_root.join("keys").join(format!("{cell_id}.ed25519"))
}

/// Generates a fresh 32-byte Ed25519 seed and persists it at `path` with
/// 0600 permissions. Fails if a key already exists at `path`: provisioning
/// must never silently overwrite an existing cell's identity.
pub async fn provision(path: &Path) -> Result<()> {
    let mut seed = [0u8; SEED_LEN];
    OsRng
        .try_fill_bytes(&mut seed)
        .context("generating ed25519 seed")?;

    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("creating key directory {}", parent.display()))?;
    }

    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || write_seed(&path, &seed))
        .await
        .context("key provisioning task panicked")??;
    Ok(())
}

/// Loads the persisted seed at `path` and expands it into the 64-byte
/// `(seed || derived public key)` representation `SigningKey::Ed25519`
/// expects.
pub async fn load_signing_key_bytes(path: &Path) -> Result<Vec<u8>> {
    let owned_path = path.to_path_buf();
    let seed_bytes = tokio::task::spawn_blocking(move || fs::read(&owned_path))
        .await
        .context("key load task panicked")?
        .with_context(|| format!("reading key file {}", path.display()))?;

    let seed: [u8; SEED_LEN] = seed_bytes.as_slice().try_into().with_context(|| {
        format!(
            "key file is {} bytes, expected {SEED_LEN} (a raw Ed25519 seed)",
            seed_bytes.len()
        )
    })?;

    Ok(expand_seed(&seed))
}

fn write_seed(path: &Path, seed: &[u8; SEED_LEN]) -> Result<()> {
    if path.exists() {
        bail!(
            "signing key already exists at {}; refusing to overwrite an existing cell identity",
            path.display()
        );
    }

    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("creating key file {}", path.display()))?;
    file.write_all(seed)
        .with_context(|| format!("writing key file {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("fsyncing key file {}", path.display()))?;
    drop(file);

    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("setting permissions on {}", path.display()))?;

    // fsync the parent directory too, so the new directory entry survives a
    // crash right after provisioning (mirrors manifest.rs's save()).
    if let Some(parent) = path.parent() {
        File::open(parent)
            .and_then(|dir| dir.sync_all())
            .with_context(|| format!("fsyncing key directory {}", parent.display()))?;
    }
    Ok(())
}

/// Expands a 32-byte Ed25519 seed into the 64-byte `(seed || public key)`
/// format, mirroring `defradb.rs`'s own
/// `crypto::keys::ed25519::ed25519_key_from_seed`.
fn expand_seed(seed: &[u8; SEED_LEN]) -> Vec<u8> {
    let signing_key = DalekSigningKey::from_bytes(seed);
    let verifying_key = signing_key.verifying_key();
    let mut bytes = Vec::with_capacity(64);
    bytes.extend_from_slice(seed);
    bytes.extend_from_slice(verifying_key.as_bytes());
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn provision_then_load_round_trips_to_a_64_byte_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = key_path(dir.path(), "cell-0");

        provision(&path).await.unwrap();

        let metadata = fs::metadata(&path).unwrap();
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);

        let key_bytes = load_signing_key_bytes(&path).await.unwrap();
        assert_eq!(key_bytes.len(), 64);

        // The expansion is deterministic: loading twice yields the same
        // 64-byte key, and it validates as a well-formed Ed25519 key (the
        // second half really is the public key derived from the first).
        let key_bytes_again = load_signing_key_bytes(&path).await.unwrap();
        assert_eq!(key_bytes, key_bytes_again);

        let signing_key = DalekSigningKey::from_bytes(&key_bytes[..32].try_into().unwrap());
        assert_eq!(signing_key.verifying_key().as_bytes(), &key_bytes[32..64]);
    }

    #[tokio::test]
    async fn provision_refuses_to_overwrite_an_existing_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = key_path(dir.path(), "cell-0");

        provision(&path).await.unwrap();
        let error = provision(&path).await.unwrap_err();
        assert!(error.to_string().contains("already exists"));
    }

    #[tokio::test]
    async fn load_rejects_a_wrong_sized_key_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = key_path(dir.path(), "cell-0");
        tokio::fs::create_dir_all(path.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(&path, b"too short").await.unwrap();

        let error = load_signing_key_bytes(&path).await.unwrap_err();
        assert!(error.to_string().contains("expected 32"));
    }

    #[test]
    fn different_seeds_expand_to_different_keys() {
        let seed_a = [1u8; SEED_LEN];
        let seed_b = [2u8; SEED_LEN];
        assert_ne!(expand_seed(&seed_a), expand_seed(&seed_b));
    }
}
