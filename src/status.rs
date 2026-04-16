//! Status — check local bridge socket health.
//!
//! Reports whether Mission Control's Unix domain socket bridges are alive
//! and ready for Docker buildx connections. Each platform (e.g.
//! `linux/amd64`, `linux/arm64`) maps to a socket file under a configurable
//! directory.
//!
//! When `wait` is enabled, the command polls until all requested sockets are
//! connectable or the timeout elapses.

use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use tokio::time::{timeout, Duration};

use crate::docker::socket_path_for_platform;

// ---------------------------------------------------------------------------
// StatusConfig
// ---------------------------------------------------------------------------

/// Configuration for the status check.
pub struct StatusConfig {
    /// Platforms to check (e.g. `["linux/amd64", "linux/arm64"]`).
    pub platforms: Vec<String>,
    /// Directory where socket files live (e.g. `/tmp`).
    pub socket_dir: PathBuf,
    /// If true, poll until all sockets are ready or timeout.
    pub wait: bool,
    /// Timeout in seconds when `wait` is true.
    pub timeout_secs: u64,
}

// ---------------------------------------------------------------------------
// check_socket_alive
// ---------------------------------------------------------------------------

/// Check if a single Unix socket file exists and is connectable.
///
/// Tries to connect with a short timeout. Returns `true` if the socket is
/// alive (some process is listening on it).
pub async fn check_socket_alive(socket_path: &Path) -> bool {
    if !socket_path.exists() {
        return false;
    }

    // Try connecting with a short timeout. If the socket file exists but
    // nothing is listening, the connect will hang or fail immediately.
    matches!(
        timeout(Duration::from_millis(500), tokio::net::UnixStream::connect(socket_path)).await,
        Ok(Ok(_))
    )
}

// ---------------------------------------------------------------------------
// run_status
// ---------------------------------------------------------------------------

/// Check bridge socket status and print results.
///
/// Returns `Ok(())` if all requested platforms have live sockets.
/// Returns `Err` if any platform socket is missing or not connectable.
pub async fn run_status(config: StatusConfig) -> Result<()> {
    let socket_paths: Vec<(String, PathBuf)> = config
        .platforms
        .iter()
        .map(|p| {
            let path = socket_path_for_platform(&config.socket_dir, p);
            (p.clone(), path)
        })
        .collect();

    if config.wait {
        let deadline = Duration::from_secs(config.timeout_secs);
        let start = tokio::time::Instant::now();

        eprint!("Waiting for bridges ");

        loop {
            let mut all_ready = true;

            for (_platform, path) in &socket_paths {
                if !check_socket_alive(path).await {
                    all_ready = false;
                    break;
                }
            }

            if all_ready {
                eprintln!(" ready!");
                break;
            }

            if start.elapsed() >= deadline {
                eprintln!(" timed out!");
                break;
            }

            // Poll every 500ms
            tokio::time::sleep(Duration::from_millis(500)).await;
            eprint!(".");
        }
    }

    // Final status check — print results for every platform.
    let mut all_ready = true;

    for (platform, path) in &socket_paths {
        let alive = check_socket_alive(path).await;
        let status_str = if alive { "✓ Ready" } else { "✗ Not connected" };
        println!("  {platform} → {} {status_str}", path.display());

        if !alive {
            all_ready = false;
        }
    }

    if all_ready {
        Ok(())
    } else {
        bail!("one or more bridge sockets are not ready");
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener as StdUnixListener;
    use tempfile::TempDir;

    /// A socket file that doesn't exist should report `false`.
    #[tokio::test]
    async fn test_check_socket_alive_nonexistent() {
        let dir = TempDir::new().unwrap();
        let socket_path = dir.path().join("orbit-amd64.sock");

        assert!(!check_socket_alive(&socket_path).await);
    }

    /// A real Unix listener should make the socket report `true`.
    #[tokio::test]
    async fn test_check_socket_alive_with_listener() {
        let dir = TempDir::new().unwrap();
        let socket_path = dir.path().join("orbit-amd64.sock");

        // Bind a listener so something is actually accepting connections.
        let listener = StdUnixListener::bind(&socket_path).unwrap();

        assert!(check_socket_alive(&socket_path).await);

        // Keep listener alive for the duration of the check
        drop(listener);
    }

    /// A socket file with nothing listening should report `false`.
    ///
    /// We create a regular file (not a socket) — the UnixStream connect will
    /// fail because the file is not a socket.
    #[tokio::test]
    async fn test_check_socket_alive_with_dead_socket() {
        let dir = TempDir::new().unwrap();
        let socket_path = dir.path().join("orbit-amd64.sock");

        // Touch a regular file — it exists but is not a socket.
        std::fs::File::create(&socket_path).unwrap();

        assert!(!check_socket_alive(&socket_path).await);
    }

    /// `run_status` returns Ok when all sockets are alive.
    #[tokio::test]
    async fn test_run_status_all_ready() {
        let dir = TempDir::new().unwrap();
        let sock_amd = dir.path().join("orbit-amd64.sock");
        let sock_arm = dir.path().join("orbit-arm64.sock");

        let _l1 = StdUnixListener::bind(&sock_amd).unwrap();
        let _l2 = StdUnixListener::bind(&sock_arm).unwrap();

        let config = StatusConfig {
            platforms: vec!["linux/amd64".into(), "linux/arm64".into()],
            socket_dir: dir.path().to_path_buf(),
            wait: false,
            timeout_secs: 5,
        };

        assert!(run_status(config).await.is_ok());
    }

    /// `run_status` returns Err when a socket is missing.
    #[tokio::test]
    async fn test_run_status_missing_socket() {
        let dir = TempDir::new().unwrap();

        let config = StatusConfig {
            platforms: vec!["linux/amd64".into()],
            socket_dir: dir.path().to_path_buf(),
            wait: false,
            timeout_secs: 5,
        };

        assert!(run_status(config).await.is_err());
    }

    /// `run_status` with `wait` polls until sockets become available.
    #[tokio::test]
    async fn test_run_status_wait_succeeds() {
        let dir = TempDir::new().unwrap();
        let socket_path = dir.path().join("orbit-amd64.sock");

        // Spawn a task that creates the listener after 1s delay.
        let sp = socket_path.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(1000)).await;
            let _listener = StdUnixListener::bind(&sp).unwrap();
            // Keep listener alive for a bit
            tokio::time::sleep(Duration::from_secs(5)).await;
        });

        let config = StatusConfig {
            platforms: vec!["linux/amd64".into()],
            socket_dir: dir.path().to_path_buf(),
            wait: true,
            timeout_secs: 5,
        };

        assert!(run_status(config).await.is_ok());
    }

    /// `run_status` with `wait` times out if sockets never appear.
    #[tokio::test]
    async fn test_run_status_wait_timeout() {
        let dir = TempDir::new().unwrap();

        let config = StatusConfig {
            platforms: vec!["linux/amd64".into()],
            socket_dir: dir.path().to_path_buf(),
            wait: true,
            timeout_secs: 1,
        };

        assert!(run_status(config).await.is_err());
    }
}
