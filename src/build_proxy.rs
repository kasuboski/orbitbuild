//! BuildProxy — ProtocolHandler for the `ORBITBUILD/BUILD/0` ALPN.
//!
//! This is the only custom ALPN protocol in OrbitBuild. It is a pure
//! bidirectional byte pipe that tunnels buildkitd's Unix socket traffic
//! between Mission Control and a Satellite.
//!
//! Authentication is handled at the Router layer via `AccessLimit` — only
//! endpoints that are known members of the Constellation Doc are allowed.
//! There is no application-level auth handshake.

use std::path::PathBuf;

use iroh::endpoint::Connection;
use iroh::protocol::{AcceptError, ProtocolHandler};
use tokio::net::UnixStream;

/// The ALPN identifier for the build proxy protocol.
pub const BUILD_ALPN: &[u8] = b"ORBITBUILD/BUILD/0";

/// A ProtocolHandler that proxies incoming QUIC connections to a local Unix socket.
///
/// Used by Satellites to expose their local buildkitd instance over the P2P network.
#[derive(Debug, Clone)]
pub struct BuildProxy {
    /// Path to the local buildkitd Unix socket.
    buildkitd_socket: PathBuf,
}

impl BuildProxy {
    /// Create a new BuildProxy that connects to the given Unix socket path.
    pub fn new(buildkitd_socket: PathBuf) -> Self {
        Self { buildkitd_socket }
    }
}

impl ProtocolHandler for BuildProxy {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        let remote_id = connection.remote_id();
        tracing::info!(remote = %remote_id, "accepting build proxy connection");

        // Accept a bidirectional QUIC stream
        let (mut quic_send, mut quic_recv) = connection
            .accept_bi()
            .await
            .map_err(AcceptError::from_err)?;

        // Connect to local buildkitd Unix socket
        let unix = match UnixStream::connect(&self.buildkitd_socket).await {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(
                    socket = %self.buildkitd_socket.display(),
                    error = %e,
                    "failed to connect to buildkitd socket"
                );
                return Err(AcceptError::from_err(e));
            }
        };

        tracing::debug!(
            remote = %remote_id,
            socket = %self.buildkitd_socket.display(),
            "proxying build session"
        );

        let (mut unix_read, mut unix_write) = tokio::io::split(unix);

        // Bidirectional copy: QUIC ↔ Unix socket using two concurrent copy operations
        let result = tokio::try_join!(
            // Unix read → QUIC send
            async {
                match tokio::io::copy(&mut unix_read, &mut quic_send).await {
                    Ok(n) => {
                        let _ = quic_send.finish();
                        Ok(n)
                    }
                    Err(e) => Err(e),
                }
            },
            // QUIC recv → Unix write
            async {
                match tokio::io::copy(&mut quic_recv, &mut unix_write).await {
                    Ok(n) => Ok(n),
                    Err(e) => Err(e),
                }
            },
        );

        match result {
            Ok((to_quic, to_unix)) => {
                tracing::info!(
                    remote = %remote_id,
                    to_quic = to_quic,
                    to_unix = to_unix,
                    "build proxy session completed"
                );
            }
            Err(e) => {
                tracing::warn!(
                    remote = %remote_id,
                    error = %e,
                    "build proxy session ended with error"
                );
            }
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_alpn_value() {
        assert_eq!(BUILD_ALPN, b"ORBITBUILD/BUILD/0");
    }

    #[test]
    fn build_proxy_new() {
        let proxy = BuildProxy::new(PathBuf::from("/var/run/buildkitd.sock"));
        assert_eq!(proxy.buildkitd_socket, PathBuf::from("/var/run/buildkitd.sock"));
    }
}
