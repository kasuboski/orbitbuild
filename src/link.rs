//! Link — one-shot setup for connecting to a Constellation.
//!
//! The `link` command is the "one command to rule them all" for CI/developers.
//! It:
//! 1. Spawns Mission Control as a background child process
//! 2. Waits for all bridge sockets to become ready
//! 3. Prints usage instructions
//! 4. Waits for Ctrl+C, then cleans up

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

use anyhow::{bail, Result};
use tokio::time::Duration;

use crate::beacon::Beacon;
use crate::docker::socket_path_for_platform;
use crate::status::check_socket_alive;

/// Default socket directory for bridge sockets.
const DEFAULT_SOCKET_DIR: &str = "/tmp";

/// Configuration for the link command.
pub struct LinkConfig {
    /// The Beacon to connect with.
    pub beacon: Beacon,
    /// Requested platforms (e.g. `["linux/amd64", "linux/arm64"]`).
    pub platforms: Vec<String>,
    /// Directory for node identity.
    pub data_dir: PathBuf,
}

/// Run the link command — start MC in background, wait for readiness.
///
/// Spawns the current binary as a background process with `mission-control`
/// subcommand args, polls socket readiness, prints instructions, and waits
/// for Ctrl+C.
pub async fn run_link(config: LinkConfig) -> Result<()> {
    // 1. Spawn Mission Control as a background child process
    let mut child = spawn_mission_control(&config)?;

    // 2. Wait for all bridge sockets to become alive
    let socket_dir = PathBuf::from(DEFAULT_SOCKET_DIR);
    let socket_paths: Vec<(String, PathBuf)> = config
        .platforms
        .iter()
        .map(|p| (p.clone(), socket_path_for_platform(&socket_dir, p)))
        .collect();

    eprint!("Linking to Constellation");

    let all_ready = wait_for_sockets(&socket_paths, Duration::from_secs(60)).await;

    if !all_ready {
        eprintln!(" timed out!");
        kill_child(&mut child);
        bail!("timed out waiting for bridge sockets to become ready");
    }

    eprintln!(" ready!");

    // 3. Print usage instructions
    let platforms_comma = config.platforms.join(",");
    println!();
    println!("✓ Linked to Constellation!");
    println!();
    println!("Build multi-arch images:");
    println!("  docker buildx build --builder orbit --platform {platforms_comma} -t myapp .");
    println!();
    println!("Connected platforms:");
    for (platform, path) in &socket_paths {
        println!("  {platform} → {}", path.display());
    }
    println!();
    println!("Press Ctrl+C to disconnect...");

    // 4. Wait for Ctrl+C
    tokio::signal::ctrl_c().await?;
    println!();
    println!("Disconnected.");

    kill_child(&mut child);

    Ok(())
}

/// Spawn Mission Control as a background child process.
///
/// Uses the current executable with appropriate args.
fn spawn_mission_control(config: &LinkConfig) -> Result<Child> {
    let current_exe = std::env::current_exe()?;
    let data_dir_str = config.data_dir.to_str().unwrap_or_else(|| {
        panic!("data_dir path is not valid UTF-8: {:?}", config.data_dir)
    });
    let beacon_string = config.beacon.to_string();
    let platforms_string = config.platforms.join(",");

    let child = Command::new(current_exe)
        .args([
            "--data-dir",
            data_dir_str,
            "mission-control",
            "--beacon",
            &beacon_string,
            "--platforms",
            &platforms_string,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| anyhow::anyhow!("failed to spawn mission control: {e}"))?;

    tracing::info!("spawned mission control as background process (pid: {})", child.id());

    Ok(child)
}

/// Poll socket readiness every 500ms up to the given deadline.
///
/// Returns `true` if all sockets become alive before the deadline.
async fn wait_for_sockets(
    socket_paths: &[(String, PathBuf)],
    deadline: Duration,
) -> bool {
    let start = tokio::time::Instant::now();

    loop {
        let mut all_ready = true;
        for (_platform, path) in socket_paths {
            if !check_socket_alive(path).await {
                all_ready = false;
                break;
            }
        }

        if all_ready {
            return true;
        }

        if start.elapsed() >= deadline {
            return false;
        }

        tokio::time::sleep(Duration::from_millis(500)).await;
        eprint!(".");
    }
}

/// Kill a child process (best-effort).
fn kill_child(child: &mut Child) {
    match child.kill() {
        Ok(()) => {
            tracing::info!("killed mission control process (pid: {})", child.id());
            // Wait to reap the zombie
            let _ = child.wait();
        }
        Err(e) => {
            tracing::warn!(error = %e, "failed to kill mission control process");
        }
    }
}

/// Build the command args that would be used to spawn Mission Control.
///
/// Exposed for testing — verifies correct argument construction.
#[cfg(test)]
fn build_mc_args(config: &LinkConfig) -> Vec<String> {
    let data_dir_str = config.data_dir.to_str().unwrap_or_default().to_string();
    let beacon_string = config.beacon.to_string();
    let platforms_string = config.platforms.join(",");

    vec![
        "--data-dir".to_string(),
        data_dir_str,
        "mission-control".to_string(),
        "--beacon".to_string(),
        beacon_string,
        "--platforms".to_string(),
        platforms_string,
    ]
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use iroh::PublicKey;
    use iroh_docs::{Capability, DocTicket, NamespaceId};

    fn make_test_beacon() -> Beacon {
        let pk = PublicKey::from_bytes(&[0u8; 32]).unwrap();
        let addr = iroh::EndpointAddr::new(pk);
        let namespace_id = NamespaceId::from([42u8; 32]);
        let ticket = DocTicket::new(Capability::Read(namespace_id), vec![addr]);
        Beacon::new(ticket)
    }

    #[test]
    fn test_link_builds_correct_command() {
        let beacon = make_test_beacon();
        let beacon_str = beacon.to_string();
        let config = LinkConfig {
            beacon,
            platforms: vec!["linux/amd64".to_string(), "linux/arm64".to_string()],
            data_dir: PathBuf::from("/tmp/orbit-test"),
        };

        let args = build_mc_args(&config);

        assert_eq!(
            args,
            vec![
                "--data-dir",
                "/tmp/orbit-test",
                "mission-control",
                "--beacon",
                &beacon_str,
                "--platforms",
                "linux/amd64,linux/arm64",
            ]
        );
    }

    #[test]
    fn test_link_config_from_parts() {
        let beacon = make_test_beacon();
        let beacon_display = beacon.to_string();

        let config = LinkConfig {
            beacon: beacon.clone(),
            platforms: vec!["linux/amd64".to_string()],
            data_dir: PathBuf::from("/home/user/.orbitbuild"),
        };

        // Verify fields round-trip
        assert_eq!(config.platforms, vec!["linux/amd64"]);
        assert_eq!(config.data_dir, PathBuf::from("/home/user/.orbitbuild"));
        assert_eq!(config.beacon.to_string(), beacon_display);
    }

    #[tokio::test]
    async fn test_wait_for_sockets_timeout() {
        // No sockets exist — should time out
        let socket_paths = vec![
            ("linux/amd64".to_string(), PathBuf::from("/tmp/orbit-test-nonexistent-amd64.sock")),
        ];

        let result = wait_for_sockets(&socket_paths, Duration::from_millis(100)).await;
        assert!(!result);
    }

    #[tokio::test]
    async fn test_wait_for_sockets_ready() {
        use std::os::unix::net::UnixListener as StdUnixListener;
        use tempfile::TempDir;

        let dir = TempDir::new().unwrap();
        let socket_path = dir.path().join("orbit-amd64.sock");
        let _listener = StdUnixListener::bind(&socket_path).unwrap();

        let socket_paths = vec![
            ("linux/amd64".to_string(), socket_path),
        ];

        let result = wait_for_sockets(&socket_paths, Duration::from_secs(5)).await;
        assert!(result);
    }
}
