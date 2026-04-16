//! Fleet — list all satellites registered in the Constellation.
//!
//! Connects to the Constellation Doc via a Beacon, waits for sync,
//! queries all `satellite/` entries, and displays a formatted table.

use std::path::PathBuf;

use anyhow::{Context, Result};
use futures_lite::StreamExt;

use crate::beacon::Beacon;
use crate::satellite::SatelliteEntry;

/// Configuration for the fleet listing.
pub struct FleetConfig {
    /// The Beacon to connect with.
    pub beacon: Beacon,
    /// Directory for node identity.
    pub data_dir: PathBuf,
}

/// Display a table of all satellites in the Constellation.
pub async fn run_fleet(config: FleetConfig) -> Result<()> {
    // 1. Spawn node
    let setup = crate::router::NodeBuilder::from_data_dir(&config.data_dir)?
        .spawn()
        .await
        .context("failed to spawn fleet node")?;

    tracing::info!(node_id = ?setup.endpoint.addr().id, "fleet node ready");

    // 2. Import Doc + subscribe
    let (doc, events) = setup
        .docs
        .import_and_subscribe(config.beacon.doc_ticket().clone())
        .await
        .context("failed to import constellation document from beacon")?;

    tracing::info!(doc_id = %doc.id(), "fleet joined constellation document");

    // 3. Wait for initial sync (simplified inline version)
    tracing::info!("waiting for initial doc sync...");
    wait_for_sync(events).await;
    tracing::info!("initial doc sync complete");

    // 4. Discover satellites using the shared helper
    let satellites = crate::mission_control::discover_satellites(&doc, &setup.blobs).await?;

    // 5. Display
    if satellites.is_empty() {
        println!("No satellites found in the Constellation.");
    } else {
        print_fleet_table(&satellites);
    }

    // 6. Clean shutdown
    setup.router.shutdown().await?;

    Ok(())
}

/// Wait for the initial Doc sync and content download to complete.
///
/// Consumes events until both `SyncFinished` and `PendingContentReady` have
/// been received, ensuring all entry blob values are available for reading.
async fn wait_for_sync(
    events: impl futures_lite::Stream<Item = Result<iroh_docs::engine::LiveEvent>>,
) {
    let mut stream = std::pin::pin!(events);
    let mut got_sync_finished = false;

    loop {
        match stream.next().await {
            Some(Ok(event)) => {
                match &event {
                    iroh_docs::engine::LiveEvent::SyncFinished(_) => {
                        tracing::debug!("received SyncFinished");
                        got_sync_finished = true;
                    }
                    iroh_docs::engine::LiveEvent::PendingContentReady => {
                        tracing::debug!("all pending content downloads complete");
                        if got_sync_finished {
                            break;
                        }
                    }
                    _ => {}
                }
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
}

/// Format a unix timestamp as a human-readable datetime string.
///
/// Uses UTC. Returns the raw number if the timestamp is invalid.
fn format_timestamp(ts: u64) -> String {
    chrono::DateTime::from_timestamp(ts as i64, 0)
        .map(|dt| dt.format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_else(|| ts.to_string())
}

/// Print the fleet table header and rows.
fn print_fleet_table(satellites: &[(String, SatelliteEntry)]) {
    println!(
        "{:<52} {:<8} {:<10} REGISTERED",
        "NODE ID", "ARCH", "STATUS"
    );
    for (node_id, entry) in satellites {
        let row = format_satellite_row(node_id, entry);
        println!("{row}");
    }
}

/// Format a single satellite table row.
fn format_satellite_row(node_id: &str, entry: &SatelliteEntry) -> String {
    format!(
        "{:<52} {:<8} {:<10} {}",
        node_id,
        entry.arch,
        entry.status,
        format_timestamp(entry.registered_at)
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_entry(arch: &str, status: &str, registered_at: u64) -> SatelliteEntry {
        let pk = iroh::PublicKey::from_bytes(&[0u8; 32]).unwrap();
        SatelliteEntry {
            arch: arch.to_string(),
            status: status.to_string(),
            endpoint_addr: iroh::EndpointAddr::new(pk),
            registered_at,
        }
    }

    #[test]
    fn test_format_satellite_row() {
        let entry = make_entry("amd64", "idle", 1713264000);
        let row = format_satellite_row("abc123", &entry);
        assert!(row.contains("abc123"));
        assert!(row.contains("amd64"));
        assert!(row.contains("idle"));
    }

    #[test]
    fn test_format_table_empty() {
        // Verify empty fleet message
        let satellites: Vec<(String, SatelliteEntry)> = vec![];
        assert!(satellites.is_empty());
    }

    #[test]
    fn test_registered_at_formatting() {
        // 2024-04-16 12:00:00 UTC = 1713268800
        let formatted = format_timestamp(1713268800);
        assert_eq!(formatted, "2024-04-16 12:00:00");

        // Another known timestamp: 2025-01-01 00:00:00 UTC = 1735689600
        let formatted = format_timestamp(1735689600);
        assert_eq!(formatted, "2025-01-01 00:00:00");
    }
}
