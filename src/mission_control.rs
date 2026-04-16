//! Mission Control — the ephemeral CI/developer bridge to the Constellation.
//!
//! Mission Control (MC) runs in CI or on a developer's machine. It:
//! 1. Imports the Doc from a Beacon to join the Constellation
//! 2. Discovers idle satellites matching requested platforms from the Doc
//! 3. Creates local Unix sockets (e.g., `/tmp/orbit-arm64.sock`) via Bridges
//! 4. When Docker connects to a local socket, MC dials the satellite's
//!    buildkitd via QUIC and proxies bytes
//!
//! MC is the **client** side — it dials out via `endpoint.connect()`.
//! It does NOT accept any build connections.

use std::path::PathBuf;

use anyhow::{Context, Result, bail, ensure};
use futures_lite::StreamExt;

use crate::beacon::Beacon;
use crate::bridge::{Bridge, BridgeManager};
use crate::docker::{buildx_ensure_builder, buildx_remove_builder, platform_to_arch, socket_path_for_platform};
use crate::satellite::SatelliteEntry;

/// Configuration for running Mission Control.
pub struct MissionControlConfig {
    /// The Beacon (cryptographic ticket) for the Constellation.
    pub beacon: Beacon,
    /// Requested platforms (e.g., `["linux/amd64", "linux/arm64"]`).
    pub platforms: Vec<String>,
    /// Directory for Unix sockets (e.g., `/tmp`).
    pub socket_dir: PathBuf,
    /// Directory for node identity (secret key storage).
    pub data_dir: PathBuf,
    /// Docker buildx builder name (default: `"orbit"`).
    pub builder_name: String,
}

/// Run the Mission Control daemon.
///
/// This is the main entry point for the `mission-control` CLI command.
pub async fn run_mission_control(config: MissionControlConfig) -> Result<()> {
    // 1. Spawn node infrastructure (no extra protocols — MC never accepts builds)
    let setup = crate::router::NodeBuilder::from_data_dir(&config.data_dir)?
        .spawn()
        .await
        .context("failed to spawn mission control node")?;

    tracing::info!(node_id = ?setup.endpoint.addr().id, "mission control endpoint ready");

    // 2. Import Doc + subscribe
    let (doc, events) = setup
        .docs
        .import_and_subscribe(config.beacon.doc_ticket().clone())
        .await
        .context("failed to import constellation document from beacon")?;

    tracing::info!(doc_id = %doc.id(), "mission control joined constellation document");

    // 3. Wait for initial Doc sync before querying.
    //    import_and_subscribe returns immediately, but entries replicate async.
    //    We wait for the first SyncFinished event to ensure we have satellite data.
    tracing::info!("waiting for initial doc sync...");
    let events = wait_for_sync(events).await;
    tracing::info!("initial doc sync complete");

    // 4. Discover satellites from Doc
    let satellite_entries = discover_satellites(&doc, &setup.blobs).await?;

    // 4. Create bridges — one per requested platform
    //    BridgeManager::add_bridge() spawns the accept loop immediately
    let mut manager = BridgeManager::new();
    for platform in &config.platforms {
        let arch = platform_to_arch(platform);

        if let Some(satellite_entry) = select_satellite_for_arch(&satellite_entries, arch) {
            let socket_path = socket_path_for_platform(&config.socket_dir, platform);
            tracing::info!(
                platform = %platform,
                arch = %arch,
                socket = %socket_path.display(),
                satellite = ?satellite_entry.endpoint_addr,
                "selected satellite for platform"
            );
            let bridge = Bridge::new(
                platform.clone(),
                socket_path,
                satellite_entry.endpoint_addr.clone(),
                setup.endpoint.clone(),
            );
            manager.add_bridge(bridge);
        } else {
            tracing::warn!(
                platform = %platform,
                arch = %arch,
                "no idle satellite found for platform — skipping"
            );
        }
    }

    if manager.platforms().count() == 0 {
        bail!(
            "no idle satellites found for any requested platform: {:?}",
            config.platforms
        );
    }

    // 5. Run docker buildx create for each bridge (idempotent)
    for platform in manager.platforms() {
        let socket = socket_path_for_platform(&config.socket_dir, platform);
        if let Err(e) = buildx_ensure_builder(&config.builder_name, &socket, platform) {
            tracing::warn!(platform = %platform, error = %e, "docker buildx setup failed");
        }
    }

    // 6. Print status
    eprintln!("Mission Control is running.");
    for platform in manager.platforms() {
        let socket = socket_path_for_platform(&config.socket_dir, platform);
        eprintln!("  {platform} → {}", socket.display());
    }
    eprintln!();
    eprintln!("Waiting for connections... (Ctrl+C to stop)");

    // 7. Spawn Doc subscription task — monitor for satellite changes
    let event_blobs = setup.blobs.clone();
    let events = events; // consumed from sync wait, now move to subscription task
    let event_handle = tokio::spawn(async move {
        let mut event_stream = std::pin::pin!(events);
        while let Some(event_result) = event_stream.next().await {
            match event_result {
                Ok(live_event) => {
                    match &live_event {
                        iroh_docs::engine::LiveEvent::InsertRemote { entry, .. } => {
                            let key = entry.key();
                            if key.starts_with(b"satellite/") {
                                // Read the blob content to get the SatelliteEntry
                                match read_entry_blob(&event_blobs, entry.content_hash()).await {
                                    Ok(value_bytes) => {
                                        match serde_json::from_slice::<SatelliteEntry>(&value_bytes)
                                        {
                                            Ok(sat_entry) => {
                                                if let Some(node_id_hex_bytes) =
                                                    key.strip_prefix(b"satellite/")
                                                {
                                                    let node_id_hex =
                                                        String::from_utf8_lossy(node_id_hex_bytes);
                                                    tracing::info!(
                                                        satellite = %node_id_hex,
                                                        arch = %sat_entry.arch,
                                                        status = %sat_entry.status,
                                                        "satellite entry updated in doc"
                                                    );
                                                    if sat_entry.status == "offline" {
                                                        tracing::warn!(
                                                            satellite = %node_id_hex,
                                                            arch = %sat_entry.arch,
                                                            "satellite went offline"
                                                        );
                                                    }
                                                }
                                            }
                                            Err(e) => {
                                                tracing::warn!(
                                                    key = ?String::from_utf8_lossy(key),
                                                    error = %e,
                                                    "failed to deserialize satellite entry update"
                                                );
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        tracing::warn!(
                                            key = ?String::from_utf8_lossy(key),
                                            hash = %entry.content_hash(),
                                            error = %e,
                                            "failed to read satellite entry blob"
                                        );
                                    }
                                }
                            }
                        }
                        iroh_docs::engine::LiveEvent::NeighborUp(peer) => {
                            tracing::debug!(peer = %peer, "new neighbor in doc swarm");
                        }
                        iroh_docs::engine::LiveEvent::NeighborDown(peer) => {
                            tracing::debug!(peer = %peer, "neighbor left doc swarm");
                        }
                        _ => {}
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "doc subscription error");
                }
            }
        }
    });

    // 8. Wait for ctrl-c → cleanup
    tokio::signal::ctrl_c().await?;
    eprintln!("\nShutting down Mission Control...");

    // Abort event subscription
    event_handle.abort();

    // Shut down bridges (aborts tasks + cleans up sockets)
    manager.shutdown().await;

    // Remove the docker buildx builder (best-effort)
    buildx_remove_builder(&config.builder_name);

    setup.router.shutdown().await?;
    eprintln!("Mission Control stopped.");

    Ok(())
}

/// Wait for the initial Doc sync and content download to complete.
///
/// iroh-docs syncs entry metadata first (keys + content_hash), then downloads
/// blob content asynchronously. We wait for both `SyncFinished` and
/// `PendingContentReady` to ensure entry values are readable.
///
/// Returns the remaining stream for the subscription task.
async fn wait_for_sync(
    events: impl futures_lite::Stream<Item = Result<iroh_docs::engine::LiveEvent>>,
) -> std::pin::Pin<Box<dyn futures_lite::Stream<Item = Result<iroh_docs::engine::LiveEvent>> + Send>> {
    use futures_lite::StreamExt;
    let mut stream = std::pin::pin!(events);

    let mut buffered: Vec<Result<iroh_docs::engine::LiveEvent>> = Vec::new();
    let mut got_sync_finished = false;

    loop {
        match stream.next().await {
            Some(Ok(event)) => {
                match &event {
                    iroh_docs::engine::LiveEvent::SyncFinished(_) => {
                        tracing::debug!("received SyncFinished");
                        got_sync_finished = true;
                    }
                    iroh_docs::engine::LiveEvent::ContentReady { hash } => {
                        tracing::debug!(hash = %hash, "blob content ready");
                    }
                    iroh_docs::engine::LiveEvent::PendingContentReady => {
                        tracing::debug!("all pending content downloads complete");
                        if got_sync_finished {
                            buffered.push(Ok(event));
                            break;
                        }
                    }
                    iroh_docs::engine::LiveEvent::InsertRemote {
                        content_status,
                        entry,
                        ..
                    } => {
                        tracing::debug!(
                            key = ?String::from_utf8_lossy(entry.key()),
                            content_status = ?content_status,
                            "InsertRemote during initial sync"
                        );
                    }
                    iroh_docs::engine::LiveEvent::NeighborUp(peer) => {
                        tracing::debug!(peer = %peer, "neighbor up");
                    }
                    _ => {}
                }
                buffered.push(Ok(event));
            }
            Some(Err(e)) => {
                tracing::warn!(error = %e, "error during initial sync");
            }
            None => {
                tracing::warn!("doc event stream ended before sync completed");
                break;
            }
        }
    }

    Box::pin(futures_lite::stream::iter(buffered))
}

/// Read a blob from the store by its content hash.
pub async fn read_entry_blob(
    blobs: &iroh_blobs::store::mem::MemStore,
    hash: iroh_blobs::Hash,
) -> Result<Vec<u8>> {
    blobs
        .blobs()
        .get_bytes(hash)
        .await
        .map_err(|e| anyhow::anyhow!("failed to read blob {hash}: {e}"))
        .map(|b| b.to_vec())
}

/// Query the Doc for satellite entries.
///
/// Returns a list of (node_id_hex, SatelliteEntry) pairs for all entries
/// with key prefix `satellite/`.
pub async fn discover_satellites(
    doc: &iroh_docs::api::Doc,
    blobs: &iroh_blobs::store::mem::MemStore,
) -> Result<Vec<(String, SatelliteEntry)>> {
    use iroh_docs::store::Query;

    let entries_stream = doc
        .get_many(Query::key_prefix(b"satellite/"))
        .await
        .context("failed to query satellite entries from doc")?;

    let mut satellite_entries: Vec<(String, SatelliteEntry)> = Vec::new();
    let mut stream = std::pin::pin!(entries_stream);
    while let Some(entry_result) = stream.next().await {
        let entry = entry_result.context("failed to read doc entry")?;
        let key = entry.key();
        if let Some(node_id_hex_bytes) = key.strip_prefix(b"satellite/") {
            let node_id_hex = String::from_utf8_lossy(node_id_hex_bytes).to_string();

            // Read the blob content for this entry
            match read_entry_blob(blobs, entry.content_hash()).await {
                Ok(value_bytes) => {
                    match serde_json::from_slice::<SatelliteEntry>(&value_bytes) {
                        Ok(sat_entry) => {
                            tracing::debug!(
                                node_id = %node_id_hex,
                                arch = %sat_entry.arch,
                                status = %sat_entry.status,
                                "discovered satellite"
                            );
                            satellite_entries.push((node_id_hex, sat_entry));
                        }
                        Err(e) => {
                            tracing::warn!(
                                key = ?String::from_utf8_lossy(key),
                                error = %e,
                                "failed to deserialize satellite entry"
                            );
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        key = ?String::from_utf8_lossy(key),
                        hash = %entry.content_hash(),
                        error = %e,
                        "failed to read satellite entry blob — may not be synced yet"
                    );
                }
            }
        }
    }

    Ok(satellite_entries)
}

/// Select the best satellite for a given architecture.
///
/// Picks idle satellites matching the requested arch. If multiple matches,
/// returns the one with the most recent `registered_at` timestamp.
fn select_satellite_for_arch<'a>(
    entries: &'a [(String, SatelliteEntry)],
    arch: &str,
) -> Option<&'a SatelliteEntry> {
    entries
        .iter()
        .filter(|(_, entry)| entry.arch == arch && entry.status == "idle")
        .max_by_key(|(_, entry)| entry.registered_at)
        .map(|(_, entry)| entry)
}

/// Parse a comma-separated platforms string into a Vec of platform strings.
///
/// Validates that each platform starts with `"linux/"`.
///
/// # Examples
/// - `"linux/amd64"` → `vec!["linux/amd64"]`
/// - `"linux/amd64,linux/arm64"` → `vec!["linux/amd64", "linux/arm64"]`
pub fn parse_platforms(platforms_str: &str) -> Result<Vec<String>> {
    let platforms: Vec<String> = platforms_str
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    ensure!(
        !platforms.is_empty(),
        "at least one platform must be specified"
    );

    for platform in &platforms {
        ensure!(
            platform.starts_with("linux/"),
            "unsupported platform '{}': only linux/* platforms are supported",
            platform
        );
    }

    Ok(platforms)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_platforms_single() {
        let result = parse_platforms("linux/amd64").unwrap();
        assert_eq!(result, vec!["linux/amd64"]);
    }

    #[test]
    fn test_parse_platforms_multiple() {
        let result = parse_platforms("linux/amd64,linux/arm64").unwrap();
        assert_eq!(result, vec!["linux/amd64", "linux/arm64"]);
    }

    #[test]
    fn test_parse_platforms_trims_whitespace() {
        let result = parse_platforms("linux/amd64, linux/arm64").unwrap();
        assert_eq!(result, vec!["linux/amd64", "linux/arm64"]);
    }

    #[test]
    fn test_parse_platforms_rejects_non_linux() {
        let result = parse_platforms("darwin/amd64");
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("unsupported platform"));
    }

    #[test]
    fn test_parse_platforms_empty() {
        let result = parse_platforms("");
        assert!(result.is_err());
    }

    #[test]
    fn test_select_satellite_for_arch() {
        let pk1 = iroh::PublicKey::from_bytes(&[0u8; 32]).unwrap();
        let pk2 = iroh::SecretKey::generate(&mut rand::rng()).public();

        let entries = vec![
            (
                "node1".to_string(),
                SatelliteEntry {
                    arch: "amd64".to_string(),
                    status: "idle".to_string(),
                    endpoint_addr: iroh::EndpointAddr::new(pk1),
                    registered_at: 1000,
                },
            ),
            (
                "node2".to_string(),
                SatelliteEntry {
                    arch: "arm64".to_string(),
                    status: "idle".to_string(),
                    endpoint_addr: iroh::EndpointAddr::new(pk2),
                    registered_at: 2000,
                },
            ),
        ];

        let result = select_satellite_for_arch(&entries, "amd64");
        assert!(result.is_some());
        assert_eq!(result.unwrap().arch, "amd64");

        let result = select_satellite_for_arch(&entries, "arm64");
        assert!(result.is_some());
        assert_eq!(result.unwrap().arch, "arm64");
    }

    #[test]
    fn test_select_satellite_for_arch_none_available() {
        let pk = iroh::PublicKey::from_bytes(&[0u8; 32]).unwrap();
        let pk2 = iroh::PublicKey::from_bytes(&[0u8; 32]).unwrap();
        let entries = vec![(
            "node1".to_string(),
            SatelliteEntry {
                arch: "amd64".to_string(),
                status: "busy".to_string(),
                endpoint_addr: iroh::EndpointAddr::new(pk),
                registered_at: 1000,
            },
        )];

        // No match: wrong status
        let result = select_satellite_for_arch(&entries, "amd64");
        assert!(result.is_none());

        // No match: wrong arch
        let entries_idle = vec![(
            "node1".to_string(),
            SatelliteEntry {
                arch: "amd64".to_string(),
                status: "idle".to_string(),
                endpoint_addr: iroh::EndpointAddr::new(pk2),
                registered_at: 1000,
            },
        )];
        let result = select_satellite_for_arch(&entries_idle, "arm64");
        assert!(result.is_none());

        // Empty list
        let result = select_satellite_for_arch(&[], "amd64");
        assert!(result.is_none());
    }

    #[test]
    fn test_select_satellite_for_arch_prefers_recent() {
        let pk1 = iroh::PublicKey::from_bytes(&[0u8; 32]).unwrap();
        let pk2 = iroh::SecretKey::generate(&mut rand::rng()).public();

        let entries = vec![
            (
                "node1".to_string(),
                SatelliteEntry {
                    arch: "amd64".to_string(),
                    status: "idle".to_string(),
                    endpoint_addr: iroh::EndpointAddr::new(pk1),
                    registered_at: 1000, // older
                },
            ),
            (
                "node2".to_string(),
                SatelliteEntry {
                    arch: "amd64".to_string(),
                    status: "idle".to_string(),
                    endpoint_addr: iroh::EndpointAddr::new(pk2),
                    registered_at: 5000, // newer
                },
            ),
        ];

        let result = select_satellite_for_arch(&entries, "amd64");
        assert!(result.is_some());
        // Should pick the one with registered_at = 5000 (node2)
        assert_eq!(result.unwrap().registered_at, 5000);
    }
}
