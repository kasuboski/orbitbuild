//! Bridge — Unix socket ↔ QUIC satellite tunnel management.
//!
//! Mission Control creates local Unix domain sockets (e.g.,
//! `/tmp/orbit-arm64.sock`). When Docker buildx connects to one of these
//! sockets, the bridge accepts the connection, opens a QUIC connection to
//! the matching Satellite's buildkitd, and copies bytes bidirectionally.
//!
//! This is the **client side** of the build proxy. The server side lives in
//! [`crate::build_proxy`] (running on the Satellite).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU64;
use std::sync::Arc;

use anyhow::Result;
use iroh::Endpoint;
use iroh::EndpointAddr;
use tokio::net::UnixListener;
use tokio::task::JoinHandle;

use crate::build_proxy::BUILD_ALPN;

// ---------------------------------------------------------------------------
// cleanup_socket
// ---------------------------------------------------------------------------

/// Remove a Unix socket file if it exists.
///
/// Used on startup (before binding) and on shutdown to ensure a clean state.
pub fn cleanup_socket(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => tracing::debug!(path = %path.display(), "removed stale socket file"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Nothing to clean up — perfectly normal
        }
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "failed to remove socket file"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Bridge
// ---------------------------------------------------------------------------

/// Manages a single platform bridge: one Unix socket that proxies to one
/// Satellite over QUIC.
///
/// Each `Bridge` owns a [`UnixListener`] and spawns a tokio task per
/// incoming connection. The task opens a QUIC bidirectional stream to the
/// Satellite's buildkitd and performs a bidirectional byte copy.
pub struct Bridge {
    /// Platform identifier (e.g., `linux/arm64`).
    platform: String,
    /// Local Unix socket path (e.g., `/tmp/orbit-arm64.sock`).
    socket_path: PathBuf,
    /// Remote satellite endpoint address for QUIC connections.
    satellite_addr: EndpointAddr,
    /// Iroh endpoint used to open QUIC connections.
    endpoint: Endpoint,
    /// Count of currently active proxy sessions.
    active_connections: Arc<AtomicU64>,
}

impl Bridge {
    /// Create a new bridge for the given platform.
    ///
    /// Does **not** start listening — call [`Bridge::run`] to begin accepting
    /// connections.
    pub fn new(
        platform: String,
        socket_path: PathBuf,
        satellite_addr: EndpointAddr,
        endpoint: Endpoint,
    ) -> Self {
        Self {
            platform,
            socket_path,
            satellite_addr,
            endpoint,
            active_connections: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Run the main accept loop.
    ///
    /// 1. Removes any stale socket file at `socket_path`.
    /// 2. Binds a [`UnixListener`].
    /// 3. Accepts connections in a loop, spawning a proxy task for each.
    ///
    /// Returns only if the listener itself fails.
    pub async fn run(&self) -> Result<()> {
        // Clean up any leftover socket from a previous run
        cleanup_socket(&self.socket_path);

        let listener = UnixListener::bind(&self.socket_path).map_err(|e| {
            anyhow::anyhow!(
                "failed to bind Unix socket at {}: {e}",
                self.socket_path.display()
            )
        })?;

        tracing::info!(
            platform = %self.platform,
            path = %self.socket_path.display(),
            satellite = ?self.satellite_addr,
            "bridge listening"
        );

        loop {
            let (unix_stream, peer_addr) = match listener.accept().await {
                Ok(pair) => pair,
                Err(e) => {
                    tracing::warn!(
                        platform = %self.platform,
                        error = %e,
                        "failed to accept Unix connection"
                    );
                    continue;
                }
            };

            tracing::debug!(
                platform = %self.platform,
                peer = ?peer_addr,
                "accepted Unix connection"
            );

            let endpoint = self.endpoint.clone();
            let satellite_addr = self.satellite_addr.clone();
            let platform = self.platform.clone();
            let active = self.active_connections.clone();

            tokio::spawn(async move {
                // Increment active count
                active.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

                let result = proxy_session(unix_stream, &endpoint, satellite_addr).await;

                // Decrement active count
                active.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);

                match result {
                    Ok((to_sat, to_local)) => {
                        tracing::info!(
                            platform = %platform,
                            to_satellite = to_sat,
                            to_local = to_local,
                            "proxy session completed"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            platform = %platform,
                            error = %e,
                            "proxy session ended with error"
                        );
                    }
                }
            });
        }
    }

    /// Returns the number of currently active proxy sessions.
    pub fn active_connections(&self) -> u64 {
        self.active_connections.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Returns the platform identifier (e.g., `linux/arm64`).
    pub fn platform(&self) -> &str {
        &self.platform
    }

    /// Returns the local Unix socket path.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }
}

/// Proxy a single Unix stream to the satellite over QUIC.
///
/// Opens a QUIC connection, creates a bidirectional stream, and copies
/// bytes in both directions concurrently.
async fn proxy_session(
    unix_stream: tokio::net::UnixStream,
    endpoint: &Endpoint,
    satellite_addr: EndpointAddr,
) -> Result<(u64, u64)> {
    // Open QUIC connection to satellite
    let connection = endpoint
        .connect(satellite_addr, BUILD_ALPN)
        .await
        .map_err(|e| anyhow::anyhow!("QUIC connect failed: {e}"))?;

    // Open a bidirectional QUIC stream
    let (mut quic_send, mut quic_recv) = connection
        .open_bi()
        .await
        .map_err(|e| anyhow::anyhow!("open_bi failed: {e}"))?;

    // Split the Unix stream into read and write halves
    let (mut unix_read, mut unix_write) = tokio::io::split(unix_stream);

    // Bidirectional copy using two concurrent io::copy calls
    let result = tokio::try_join!(
        // Local → Satellite
        async {
            match tokio::io::copy(&mut unix_read, &mut quic_send).await {
                Ok(n) => {
                    let _ = quic_send.finish();
                    Ok(n)
                }
                Err(e) => Err(e),
            }
        },
        // Satellite → Local
        tokio::io::copy(&mut quic_recv, &mut unix_write),
    );

    result
        .map_err(|e| anyhow::anyhow!("copy error: {e}"))
}

// ---------------------------------------------------------------------------
// BridgeManager
// ---------------------------------------------------------------------------

/// Manages all platform bridges for a Mission Control instance.
///
/// Each bridge corresponds to one platform (e.g., `linux/arm64`,
/// `linux/amd64`) and proxies Docker buildx connections to the matching
/// Satellite over QUIC.
pub struct BridgeManager {
    /// Platform → Bridge mapping (wrapped in Arc for shared ownership with spawned tasks).
    bridges: HashMap<String, Arc<Bridge>>,
    /// Running bridge accept-loop tasks.
    tasks: Vec<JoinHandle<()>>,
}

impl Default for BridgeManager {
    fn default() -> Self {
        Self::new()
    }
}

impl BridgeManager {
    /// Create an empty bridge manager.
    pub fn new() -> Self {
        Self {
            bridges: HashMap::new(),
            tasks: Vec::new(),
        }
    }

    /// Add a bridge and spawn its accept-loop as a background task.
    pub fn add_bridge(&mut self, bridge: Bridge) {
        let platform = bridge.platform().to_owned();
        let socket_path = bridge.socket_path().to_owned();
        let bridge = Arc::new(bridge);

        let bridge_clone = bridge.clone();
        let handle = tokio::spawn(async move {
            if let Err(e) = bridge_clone.run().await {
                tracing::error!(
                    platform = %bridge_clone.platform(),
                    error = %e,
                    "bridge accept loop exited with error"
                );
            }
            // Clean up the socket file when the bridge exits
            cleanup_socket(&socket_path);
        });

        let platform_str = bridge.platform().to_owned();
        self.tasks.push(handle);
        self.bridges.insert(platform, bridge);

        tracing::info!(platform = %platform_str, "bridge added and running");
    }

    /// Shut down all bridge tasks and clean up socket files.
    pub async fn shutdown(&self) {
        for handle in &self.tasks {
            handle.abort();
        }
        // Clean up socket files
        for bridge in self.bridges.values() {
            cleanup_socket(bridge.socket_path());
        }
        tracing::info!("all bridges shut down");
    }

    /// Look up a bridge by platform identifier.
    pub fn bridge_for_platform(&self, platform: &str) -> Option<&Bridge> {
        self.bridges.get(platform).map(|arc| arc.as_ref())
    }

    /// Returns an iterator over all managed platform identifiers.
    pub fn platforms(&self) -> impl Iterator<Item = &str> {
        self.bridges.keys().map(|s| s.as_str())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify basic Bridge construction and field accessors.
    #[tokio::test]
    async fn test_bridge_new() {
        let (endpoint, addr) = make_test_endpoint().await;

        let bridge = Bridge::new(
            "linux/arm64".into(),
            PathBuf::from("/tmp/orbit-arm64.sock"),
            addr,
            endpoint,
        );

        assert_eq!(bridge.platform(), "linux/arm64");
        assert_eq!(bridge.socket_path(), Path::new("/tmp/orbit-arm64.sock"));
        assert_eq!(bridge.active_connections(), 0);
    }

    /// Verify Bridge construction, platform(), and socket_path() accessors.
    #[tokio::test]
    async fn test_bridge_platform_accessor() {
        let (endpoint, addr) = make_test_endpoint().await;

        let bridge = Bridge::new(
            "linux/arm64".into(),
            PathBuf::from("/tmp/orbit-arm64.sock"),
            addr,
            endpoint,
        );

        assert_eq!(bridge.platform(), "linux/arm64");
        assert_eq!(bridge.socket_path(), Path::new("/tmp/orbit-arm64.sock"));
        assert_eq!(bridge.active_connections(), 0);
    }

    /// Verify BridgeManager can add bridges and enumerate platforms.
    #[tokio::test]
    async fn test_bridge_manager_add() {
        let mut manager = BridgeManager::new();
        assert!(manager.platforms().count() == 0);
        assert!(manager.bridge_for_platform("linux/arm64").is_none());

        let (ep1, addr1) = make_test_endpoint().await;
        let bridge1 = Bridge::new(
            "linux/arm64".into(),
            PathBuf::from("/tmp/orbit-arm64-test.sock"),
            addr1,
            ep1,
        );
        manager.add_bridge(bridge1);

        let (ep2, addr2) = make_test_endpoint().await;
        let bridge2 = Bridge::new(
            "linux/amd64".into(),
            PathBuf::from("/tmp/orbit-amd64-test.sock"),
            addr2,
            ep2,
        );
        manager.add_bridge(bridge2);

        let platforms: Vec<&str> = manager.platforms().collect();
        assert_eq!(platforms.len(), 2);
        assert!(platforms.contains(&"linux/arm64"));
        assert!(platforms.contains(&"linux/amd64"));

        assert!(manager.bridge_for_platform("linux/arm64").is_some());
        assert!(manager.bridge_for_platform("linux/amd64").is_some());
        assert!(manager.bridge_for_platform("linux/riscv64").is_none());

        // Clean up spawned tasks
        manager.shutdown().await;
    }

    /// Verify cleanup_socket removes a file and handles missing files.
    #[tokio::test]
    async fn test_cleanup_socket() {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("test.sock");

        // File doesn't exist — should succeed silently
        cleanup_socket(&socket_path);
        assert!(!socket_path.exists());

        // Create a file and then clean it up
        std::fs::File::create(&socket_path).unwrap();
        assert!(socket_path.exists());

        cleanup_socket(&socket_path);
        assert!(!socket_path.exists());

        // Cleaning up again (already removed) should be fine
        cleanup_socket(&socket_path);
        assert!(!socket_path.exists());
    }

    /// Helper: create a test Endpoint + EndpointAddr pair.
    async fn make_test_endpoint() -> (Endpoint, EndpointAddr) {
        use iroh::endpoint::presets;
        let ep = Endpoint::builder(presets::N0).bind().await.unwrap();
        let addr = ep.addr();
        (ep, addr)
    }
}
