//! Key management utilities for OrbitBuild nodes.
//!
//! Every node (Station, Satellite, Mission Control) has a permanent ed25519 identity
//! represented by a [`SecretKey`]. This module handles persisting and loading these keys
//! so that a node retains its identity across restarts.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use iroh::SecretKey;

/// Default directory name for OrbitBuild data.
pub const ORBITBUILD_DIR: &str = ".orbitbuild";

/// File name for the node's secret key.
pub const KEY_FILE: &str = "node.key";

/// Returns the default data directory for OrbitBuild.
///
/// Uses `$HOME/.orbitbuild` on Unix systems.
pub fn default_data_dir() -> Result<PathBuf> {
    let home = dirs_home()?;
    Ok(home.join(ORBITBUILD_DIR))
}

/// Returns the path to the node's secret key file.
pub fn key_path(data_dir: &Path) -> PathBuf {
    data_dir.join(KEY_FILE)
}

/// Load a secret key from disk, or generate and persist a new one.
///
/// The key is stored as raw 32 bytes (the canonical form of an ed25519 secret key).
pub fn load_or_generate_secret_key(path: &Path) -> Result<SecretKey> {
    if path.exists() {
        load_secret_key(path)
    } else {
        let key = SecretKey::from_bytes(&rand::random::<[u8; 32]>());
        save_secret_key(path, &key)?;
        tracing::info!(path = %path.display(), "generated new node identity");
        Ok(key)
    }
}

/// Load a secret key from a file.
fn load_secret_key(path: &Path) -> Result<SecretKey> {
    let bytes = std::fs::read(path).with_context(|| {
        format!("failed to read secret key from {}", path.display())
    })?;
    let arr: [u8; 32] = bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("secret key file must be exactly 32 bytes"))?;
    let key = SecretKey::from_bytes(&arr);
    tracing::debug!(path = %path.display(), "loaded existing node identity");
    Ok(key)
}

/// Save a secret key to a file.
fn save_secret_key(path: &Path, key: &SecretKey) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!("failed to create directory {}", parent.display())
        })?;
    }
    std::fs::write(path, key.to_bytes()).with_context(|| {
        format!("failed to write secret key to {}", path.display())
    })?;
    Ok(())
}

/// Get the home directory, with a useful error message if unavailable.
fn dirs_home() -> Result<PathBuf> {
    // Use the `home` crate logic (inline to avoid extra dep)
    std::env::var("HOME")
        .map(PathBuf::from)
        .or_else(|_| std::env::var("USERPROFILE").map(PathBuf::from))
        .context("could not determine home directory: neither $HOME nor $USERPROFILE is set")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn load_or_generate_creates_key() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.key");

        assert!(!path.exists());
        let key = load_or_generate_secret_key(&path).unwrap();
        assert!(path.exists());

        // File should be exactly 32 bytes
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(bytes.len(), 32);

        // Loading again should return the same key
        let key2 = load_or_generate_secret_key(&path).unwrap();
        assert_eq!(key.to_bytes(), key2.to_bytes());
    }

    #[test]
    fn load_existing_key() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("existing.key");

        let original = SecretKey::from_bytes(&rand::random::<[u8; 32]>());
        save_secret_key(&path, &original).unwrap();

        let loaded = load_secret_key(&path).unwrap();
        assert_eq!(original.to_bytes(), loaded.to_bytes());
    }

    #[test]
    fn rejects_wrong_size_key_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("bad.key");
        std::fs::write(&path, [0u8; 16]).unwrap();

        let result = load_secret_key(&path);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("32 bytes"));
    }

    #[test]
    fn creates_parent_directories() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nested").join("dir").join("key");

        let key = load_or_generate_secret_key(&path).unwrap();
        assert!(path.exists());
        let loaded = load_secret_key(&path).unwrap();
        assert_eq!(key.to_bytes(), loaded.to_bytes());
    }

    #[test]
    fn key_path_constructed_correctly() {
        let data = Path::new("/tmp/orbitbuild");
        assert_eq!(key_path(data), PathBuf::from("/tmp/orbitbuild/node.key"));
    }
}
