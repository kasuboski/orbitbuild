//! Docker buildx CLI integration for OrbitBuild Mission Control.
//!
//! This module provides functions to manage Docker buildx builders.
//! Mission Control uses these to register remote buildkitd instances
//! (accessible via Unix domain sockets) as buildx builders.
//!
//! The API is idempotent — [`buildx_ensure_builder`] checks if a builder
//! exists and creates or appends as needed.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Ensure a buildx builder exists with the given platform endpoint appended.
///
/// This is the main entry point for Mission Control's Docker integration.
/// It is **idempotent**:
/// - If the builder does not exist, it is created with `docker buildx create`.
/// - If the builder already exists, the endpoint is appended with `--append`.
/// - If the builder exists and already has this endpoint, the command is a no-op
///   (Docker handles the dedup).
///
/// Returns `Ok(())` on success, or an error if Docker is not installed or
/// the command fails.
pub fn buildx_ensure_builder(
    builder_name: &str,
    socket_path: &Path,
    platform: &str,
) -> anyhow::Result<()> {
    let exists = buildx_builder_exists(builder_name);

    let mut cmd = buildx_create_command(builder_name, socket_path, platform, exists);
    tracing::info!(
        builder = builder_name,
        socket = %socket_path.display(),
        platform,
        exists,
        "running docker buildx create"
    );
    let status = cmd
        .status()
        .map_err(|e| anyhow::anyhow!("failed to run docker buildx create — is docker installed? {e}"))?;

    if !status.success() {
        // This can happen if the builder was removed between our check and the create,
        // or if there's a Docker daemon issue. Log but don't crash — the bridge is
        // still running and the user can manually create the builder.
        tracing::warn!(
            builder = builder_name,
            platform,
            code = ?status.code(),
            "docker buildx create failed"
        );
    }

    Ok(())
}

/// Check if a buildx builder with the given name exists.
///
/// Runs `docker buildx inspect <name>` — returns true if exit code is 0.
fn buildx_builder_exists(builder_name: &str) -> bool {
    let mut cmd = Command::new("docker");
    cmd.args(["buildx", "inspect", builder_name]);

    match cmd.output() {
        Ok(output) => output.status.success(),
        Err(_) => {
            // Docker not installed — builder definitely doesn't exist
            false
        }
    }
}

/// Remove a buildx builder by name.
///
/// Used during Mission Control shutdown to clean up. Errors are logged
/// but not propagated — a stale builder is harmless.
pub fn buildx_remove_builder(builder_name: &str) {
    let mut cmd = Command::new("docker");
    cmd.args(["buildx", "rm", builder_name]);

    match cmd.status() {
        Ok(status) => {
            if !status.success() {
                tracing::debug!(
                    builder = builder_name,
                    "docker buildx rm failed (builder may not exist)"
                );
            }
        }
        Err(e) => {
            tracing::debug!(error = %e, "failed to run docker buildx rm");
        }
    }
}

/// Generate a `docker buildx create` command.
///
/// The builder connects to a remote buildkitd via a Unix socket bridge
/// maintained by Mission Control.
///
/// # Arguments
///
/// * `builder_name` — The buildx builder instance name (e.g. `"orbit"`).
/// * `socket_path`  — Absolute path to the Unix socket for the platform bridge.
/// * `platform`     — Full platform string (e.g. `"linux/arm64"`, `"linux/amd64"`).
/// * `append`       — If true, append to an existing builder via `--append`.
pub fn buildx_create_command(
    builder_name: &str,
    socket_path: &Path,
    platform: &str,
    append: bool,
) -> Command {
    let socket_url = format!("unix://{}", socket_path.display());

    let mut cmd = Command::new("docker");
    cmd.args([
        "buildx",
        "create",
        "--name",
        builder_name,
        "--driver",
        "remote",
        &socket_url,
        "--platform",
        platform,
    ]);

    if append {
        cmd.arg("--append");
    }

    tracing::debug!(
        builder = builder_name,
        socket = %socket_path.display(),
        platform,
        append,
        "built buildx create command"
    );

    cmd
}

/// Generate a `docker buildx inspect` command.
///
/// Returns exit code 0 if the builder exists.
pub fn buildx_inspect_command(builder_name: &str) -> Command {
    let mut cmd = Command::new("docker");
    cmd.args(["buildx", "inspect", builder_name]);
    cmd
}

/// Generate a `docker buildx rm` command.
pub fn buildx_remove_command(builder_name: &str) -> Command {
    let mut cmd = Command::new("docker");
    cmd.args(["buildx", "rm", builder_name]);
    cmd
}

/// Generate a `docker buildx use` command.
pub fn buildx_use_command(builder_name: &str) -> Command {
    let mut cmd = Command::new("docker");
    cmd.args(["buildx", "use", builder_name]);
    cmd
}

/// Derive the Unix socket path for a given platform.
///
/// Converts a platform string like `linux/arm64` into a socket path
/// under the given directory, e.g. `/tmp/orbit-arm64.sock`.
///
/// # Example
///
/// ```
/// use std::path::Path;
/// use orbitbuild::docker::socket_path_for_platform;
///
/// let path = socket_path_for_platform(Path::new("/tmp"), "linux/arm64");
/// assert_eq!(path.to_str().unwrap(), "/tmp/orbit-arm64.sock");
/// ```
pub fn socket_path_for_platform(socket_dir: &Path, platform: &str) -> PathBuf {
    let arch = platform_to_arch(platform);
    socket_dir.join(format!("orbit-{arch}.sock"))
}

/// Extract the architecture from a full platform string.
///
/// Strips everything before the last `/`, so `linux/arm64` becomes `arm64`
/// and `linux/amd64` becomes `amd64`.
///
/// # Panics
///
/// Panics if the platform string does not contain a `/`.
pub fn platform_to_arch(platform: &str) -> &str {
    platform
        .rsplit_once('/')
        .unwrap_or_else(|| panic!("platform string must contain '/': got '{platform}'"))
        .1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_buildx_create_command_no_append() {
        let cmd =
            buildx_create_command("orbit", Path::new("/tmp/orbit-arm64.sock"), "linux/arm64", false);

        let args: Vec<&str> = cmd.get_args().map(|s| s.to_str().unwrap()).collect();

        assert_eq!(
            args,
            &[
                "buildx", "create", "--name", "orbit",
                "--driver", "remote",
                "unix:///tmp/orbit-arm64.sock",
                "--platform", "linux/arm64",
            ]
        );
        assert!(!args.contains(&"--append"));
    }

    #[test]
    fn test_buildx_create_command_with_append() {
        let cmd =
            buildx_create_command("orbit", Path::new("/tmp/orbit-amd64.sock"), "linux/amd64", true);

        let args: Vec<&str> = cmd.get_args().map(|s| s.to_str().unwrap()).collect();

        assert!(args.contains(&"--append"));
        assert_eq!(
            args,
            &[
                "buildx", "create", "--name", "orbit",
                "--driver", "remote",
                "unix:///tmp/orbit-amd64.sock",
                "--platform", "linux/amd64",
                "--append",
            ]
        );
    }

    #[test]
    fn test_buildx_inspect_command() {
        let cmd = buildx_inspect_command("orbit");
        let args: Vec<&str> = cmd.get_args().map(|s| s.to_str().unwrap()).collect();
        assert_eq!(args, &["buildx", "inspect", "orbit"]);
    }

    #[test]
    fn test_buildx_remove_command_args() {
        let cmd = buildx_remove_command("orbit");
        let args: Vec<&str> = cmd.get_args().map(|s| s.to_str().unwrap()).collect();
        assert_eq!(args, &["buildx", "rm", "orbit"]);
    }

    #[test]
    fn test_socket_path_for_platform() {
        assert_eq!(
            socket_path_for_platform(Path::new("/tmp"), "linux/arm64"),
            PathBuf::from("/tmp/orbit-arm64.sock")
        );
        assert_eq!(
            socket_path_for_platform(Path::new("/var/run"), "linux/amd64"),
            PathBuf::from("/var/run/orbit-amd64.sock")
        );
    }

    #[test]
    fn test_platform_to_arch() {
        assert_eq!(platform_to_arch("linux/arm64"), "arm64");
        assert_eq!(platform_to_arch("linux/amd64"), "amd64");
    }

    #[test]
    #[should_panic(expected = "must contain '/'")]
    fn test_platform_to_arch_no_slash() {
        let _ = platform_to_arch("arm64");
    }
}
