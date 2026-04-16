//! Satellite — the compute node (buildkitd runner).
//!
//! A Satellite joins a Constellation by importing the Doc from a Beacon,
//! registering itself in the Doc, and listening for proxied buildkitd
//! sessions via the BuildProxy protocol.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use bytes::Bytes;
use iroh::protocol::AccessLimit;
use iroh::EndpointId;
use iroh_docs::store::Query;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::beacon::Beacon;
use crate::build_proxy::{BuildProxy, BUILD_ALPN};
use crate::router::NodeBuilder;

/// Default path to the buildkitd Unix socket.
const DEFAULT_BUILDKITD_SOCKET: &str = "/run/buildkit/buildkitd.sock";

/// How often the satellite sends a heartbeat (updates its Doc entry).
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

/// Run `satellite join` — register in the Doc and serve buildkitd connections.
///
/// 1. Generate/load node identity
/// 2. Create iroh Endpoint, wait online
/// 3. Spawn Router with Blobs + Gossip + Docs + BuildProxy (gated by AccessLimit)
/// 4. Import Doc from Beacon's DocTicket
/// 5. Subscribe to Doc events → maintain `allowed_peers` set for AccessLimit
/// 6. Write self-registration entry into Doc
/// 7. Start heartbeat: re-write entry every 30s with updated timestamp
/// 8. On shutdown: set status to "offline"
pub async fn run_satellite_join(
    beacon: Beacon,
    data_dir: &std::path::Path,
    buildkitd_socket: Option<PathBuf>,
) -> Result<()> {
    let buildkitd_socket = buildkitd_socket
        .unwrap_or_else(|| PathBuf::from(DEFAULT_BUILDKITD_SOCKET));

    let buildkitd_socket_display = buildkitd_socket.display();

    // Shared set of allowed peer EndpointIds (maintained from Doc subscription)
    let allowed_peers: Arc<RwLock<HashSet<EndpointId>>> =
        Arc::new(RwLock::new(HashSet::new()));

    // Create AccessLimit-gated BuildProxy
    let build_proxy = {
        let allowed = allowed_peers.clone();
        AccessLimit::new(
            BuildProxy::new(buildkitd_socket.clone()),
            move |endpoint_id: EndpointId| -> bool {
                // Check if this peer is in our allowed set
                // Use try_read to avoid blocking; if lock is held, deny
                match allowed.try_read() {
                    Ok(peers) => peers.contains(&endpoint_id),
                    Err(_) => {
                        tracing::warn!("allowed_peers lock contested, denying connection");
                        false
                    }
                }
            },
        )
    };

    // Build node infrastructure with the BuildProxy as an extra protocol
    let setup = NodeBuilder::from_data_dir(data_dir)?
        .accept(BUILD_ALPN, build_proxy)
        .spawn()
        .await
        .context("failed to spawn satellite node")?;

    let docs = &setup.docs;
    let endpoint = &setup.endpoint;
    let node_id = endpoint.addr().id;

    tracing::info!(node_id = ?node_id, "satellite endpoint ready");

    // Import the Doc from the Beacon
    let (doc, events) = docs
        .import_and_subscribe(beacon.doc_ticket().clone())
        .await
        .context("failed to import document from beacon")?;

    tracing::info!(doc_id = %doc.id(), "satellite joined constellation document");

    // Create an author for writing to the Doc
    let author = docs
        .author_create()
        .await
        .context("failed to create author")?;

    // Populate allowed_peers from existing Doc entries
    {
        let existing_entries = doc
            .get_many(Query::key_prefix(b"satellite/"))
            .await
            .context("failed to query existing satellites")?;
        let mut peers = allowed_peers.write().await;
        let mut stream = std::pin::pin!(existing_entries);
        use futures_lite::StreamExt;
        while let Some(entry_result) = stream.next().await {
            if let Ok(entry) = entry_result
                && let Some(node_id_hex) = entry.key().strip_prefix(b"satellite/")
                && let Some(pk) = parse_node_id_from_hex(node_id_hex)
            {
                peers.insert(pk);
            }
        }
    }

    // Write self-registration entry
    let entry_key = format!("satellite/{node_id}");
    let arch = arch_str();
    let registration = SatelliteEntry {
        arch: arch.to_string(),
        status: SatelliteStatus::Idle.to_string(),
        endpoint_addr: endpoint.addr(),
        registered_at: chrono_now_secs(),
    };

    let value = serde_json::to_vec(&registration)
        .context("failed to serialize satellite entry")?;
    doc.set_bytes(author, Bytes::from(entry_key.clone()), value)
        .await
        .context("failed to write satellite registration")?;

    tracing::info!(
        node_id = ?node_id,
        arch = %arch,
        key = %entry_key,
        "satellite registered in doc"
    );

    eprintln!("Satellite joined the Constellation.");
    eprintln!("  Node ID: {:?}", node_id);
    eprintln!("  Arch: {arch}");
    eprintln!("  buildkitd socket: {buildkitd_socket_display}");
    eprintln!();
    eprintln!("Satellite is listening... (Ctrl+C to stop)");

    // Spawn Doc event subscriber task (maintains allowed_peers)
    let doc_clone = doc.clone();
    let allowed_clone = allowed_peers.clone();
    let node_id_for_log = node_id;

    let event_handle = tokio::spawn(async move {
        use futures_lite::StreamExt;
        let mut event_stream = std::pin::pin!(events);
        while let Some(event_result) = event_stream.next().await {
            match event_result {
                Ok(live_event) => {
                    match &live_event {
                        iroh_docs::engine::LiveEvent::InsertRemote { from, entry, .. } => {
                            let key = entry.key();
                            if let Some(node_id_hex) = key.strip_prefix(b"satellite/") {
                                tracing::debug!(
                                    peer = %from,
                                    key = ?String::from_utf8_lossy(key),
                                    "satellite entry updated"
                                );
                                if let Some(pk) = parse_node_id_from_hex(node_id_hex) {
                                    allowed_clone.write().await.insert(pk);
                                }
                            }
                        }
                        iroh_docs::engine::LiveEvent::NeighborUp(peer) => {
                            tracing::debug!(peer = %peer, "new neighbor in doc swarm");
                            allowed_clone.write().await.insert(*peer);
                        }
                        iroh_docs::engine::LiveEvent::NeighborDown(peer) => {
                            tracing::debug!(peer = %peer, "neighbor left doc swarm");
                            // Don't remove — they may still be a valid member
                        }
                        _ => {}
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "doc subscription error");
                }
            }
        }
        let _ = (doc_clone, node_id_for_log);
    });

    // Spawn heartbeat task
    let heartbeat_doc = doc.clone();
    let heartbeat_key = entry_key.clone();
    let heartbeat_author = author;
    let heartbeat_arch = arch.to_string();
    let heartbeat_addr = endpoint.addr();
    let heartbeat_handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(HEARTBEAT_INTERVAL);
        loop {
            interval.tick().await;
            let registration = SatelliteEntry {
                arch: heartbeat_arch.clone(),
                status: SatelliteStatus::Idle.to_string(),
                endpoint_addr: heartbeat_addr.clone(),
                registered_at: chrono_now_secs(),
            };
            if let Ok(value) = serde_json::to_vec(&registration) {
                match heartbeat_doc
                    .set_bytes(heartbeat_author, Bytes::from(heartbeat_key.clone()), value)
                    .await
                {
                    Ok(_) => {
                        tracing::debug!("heartbeat: updated registration timestamp");
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "heartbeat: failed to update");
                    }
                }
            }
        }
    });

    // Wait for ctrl-c
    tokio::signal::ctrl_c().await?;
    eprintln!("\nShutting down satellite...");

    // Set status to offline
    let offline_entry = SatelliteEntry {
        arch: arch.to_string(),
        status: SatelliteStatus::Offline.to_string(),
        endpoint_addr: iroh::EndpointAddr::new(iroh::PublicKey::from_bytes(&[0u8; 32]).unwrap()),
        registered_at: chrono_now_secs(),
    };
    if let Ok(value) = serde_json::to_vec(&offline_entry) {
        let _ = doc.set_bytes(author, Bytes::from(entry_key), value).await;
    }

    // Clean up tasks
    event_handle.abort();
    heartbeat_handle.abort();

    setup.router.shutdown().await?;
    eprintln!("Satellite stopped.");

    Ok(())
}

/// Parse a node ID from hex bytes (the part after "satellite/" in a Doc key).
/// Returns None if the hex is invalid or not 32 bytes.
fn parse_node_id_from_hex(hex_bytes: &[u8]) -> Option<iroh::PublicKey> {
    let hex_str = std::str::from_utf8(hex_bytes).ok()?;
    let bytes = data_encoding::HEXLOWER.decode(hex_str.as_bytes()).ok()?;
    if bytes.len() == 32 {
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&bytes);
        iroh::PublicKey::from_bytes(&arr).ok()
    } else {
        None
    }
}

/// Satellite entry stored in the Doc.
///
/// Serialized as JSON into Doc values with key `satellite/<node_id_hex>`.
/// The `endpoint_addr` field uses iroh's serde support so Mission Control
/// can deserialize it directly into an `EndpointAddr` for `endpoint.connect()`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SatelliteEntry {
    /// Architecture (e.g., "arm64", "amd64").
    pub arch: String,
    /// Current status: "idle", "busy", "offline".
    pub status: String,
    /// The satellite's endpoint address for direct connection.
    /// Stored via serde (JSON) — PublicKey serializes as hex string,
    /// TransportAddr as tagged enum. MC deserializes into EndpointAddr directly.
    pub endpoint_addr: iroh::EndpointAddr,
    /// Unix timestamp of registration/last heartbeat.
    pub registered_at: u64,
}

/// Satellite status values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SatelliteStatus {
    Idle,
    Busy,
    Offline,
}

impl std::fmt::Display for SatelliteStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Idle => write!(f, "idle"),
            Self::Busy => write!(f, "busy"),
            Self::Offline => write!(f, "offline"),
        }
    }
}

/// Map Rust's TARGET_ARCH to OrbitBuild arch strings.
pub fn arch_str() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" | "arm64" => "arm64",
        "x86_64" | "amd64" => "amd64",
        other => other,
    }
}

/// Get current time as Unix timestamp seconds.
fn chrono_now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arch_mapping() {
        let arch = arch_str();
        assert!(
            ["arm64", "amd64", "riscv64", "loongarch64"].contains(&arch),
            "unexpected arch: {arch}"
        );
    }

    #[test]
    fn satellite_entry_serialization() {
        let pk = iroh::PublicKey::from_bytes(&[42u8; 32]).unwrap();
        let addr = iroh::EndpointAddr::new(pk)
            .with_relay_url("https://relay.iroh.network".parse().unwrap());
        let entry = SatelliteEntry {
            arch: "arm64".into(),
            status: "idle".into(),
            endpoint_addr: addr,
            registered_at: 1713123456,
        };
        let json = serde_json::to_string(&entry).unwrap();
        let decoded: SatelliteEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, entry);
    }

    #[test]
    fn satellite_status_display() {
        assert_eq!(SatelliteStatus::Idle.to_string(), "idle");
        assert_eq!(SatelliteStatus::Busy.to_string(), "busy");
        assert_eq!(SatelliteStatus::Offline.to_string(), "offline");
    }

    #[test]
    fn satellite_entry_json_format() {
        let pk = iroh::PublicKey::from_bytes(&[0u8; 32]).unwrap();
        let entry = SatelliteEntry {
            arch: "amd64".into(),
            status: "idle".into(),
            endpoint_addr: iroh::EndpointAddr::new(pk),
            registered_at: 12345,
        };
        let json = serde_json::to_string_pretty(&entry).unwrap();
        assert!(json.contains("\"arch\""));
        assert!(json.contains("\"status\""));
        assert!(json.contains("\"endpoint_addr\""));
        assert!(json.contains("\"registered_at\""));
    }

    #[test]
    fn parse_node_id_from_hex_valid() {
        // Create a valid PublicKey and test round-trip through hex
        let pk = iroh::PublicKey::from_bytes(&[0u8; 32]).unwrap();
        let hex = data_encoding::HEXLOWER.encode(pk.as_bytes());
        let parsed = parse_node_id_from_hex(hex.as_bytes());
        assert_eq!(parsed, Some(pk));
    }

    #[test]
    fn parse_node_id_from_hex_invalid() {
        assert!(parse_node_id_from_hex(b"not-hex").is_none());
        assert!(parse_node_id_from_hex(b"abcdef").is_none()); // too short
    }
}
