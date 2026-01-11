use crate::clickhouse::ClickHouseWriter;
use aw_models::{Bucket, Event};
use aw_transform::heartbeat;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tracing::debug;

/// Stored heartbeat with its pulsetime for proper stale detection
struct PendingHeartbeat {
    event: Event,
    last_seen: Instant,
    pulsetime: f64,
    version: u64, // increments on each update, used for ReplacingMergeTree
}

pub struct AppState {
    pub device_id: String,
    pub hostname: String,
    pub buckets: RwLock<HashMap<String, Bucket>>,
    last_heartbeat: RwLock<HashMap<String, PendingHeartbeat>>,
    pub writer: ClickHouseWriter,
}

impl AppState {
    pub fn new(device_id: String, hostname: String, buckets: HashMap<String, Bucket>, writer: ClickHouseWriter) -> Self {
        Self {
            device_id,
            hostname,
            buckets: RwLock::new(buckets),
            last_heartbeat: RwLock::new(HashMap::new()),
            writer,
        }
    }

    pub async fn handle_heartbeat(
        self: &Arc<Self>,
        bucket_id: &str,
        event: Event,
        pulsetime: f64,
    ) -> Event {
        let mut hb_map = self.last_heartbeat.write().await;

        // Get bucket info for the event
        let (device_id, bucket_type, hostname) = {
            let buckets = self.buckets.read().await;
            buckets
                .get(bucket_id)
                .map(|b| {
                    let device_id = b.data.get("device_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or(self.writer.default_device_id())
                        .to_string();
                    (device_id, b._type.clone(), b.hostname.clone())
                })
                .unwrap_or_else(|| (self.writer.default_device_id().to_string(), "unknown".into(), self.hostname.clone()))
        };

        if let Some(pending) = hb_map.get(bucket_id) {
            let prev_version = pending.version;
            match heartbeat(&pending.event, &event, pulsetime) {
                Some(merged) => {
                    // Successfully merged - update in-memory state, increment version
                    debug!("Merged heartbeat for {}", bucket_id);
                    hb_map.insert(bucket_id.to_string(), PendingHeartbeat {
                        event: merged.clone(),
                        last_seen: Instant::now(),
                        pulsetime,
                        version: prev_version + 1,
                    });
                    return merged;
                }
                None => {
                    // Can't merge - flush old event (final), start new
                    debug!("New event for {} (couldn't merge)", bucket_id);
                    let old = hb_map
                        .insert(bucket_id.to_string(), PendingHeartbeat {
                            event: event.clone(),
                            last_seen: Instant::now(),
                            pulsetime,
                            version: 1,
                        })
                        .unwrap();
                    // Queue final version of old event
                    self.writer
                        .queue(&device_id, bucket_id, &bucket_type, &hostname, old.event, old.version)
                        .await;
                    return event;
                }
            }
        }

        // First heartbeat for this bucket
        debug!("First heartbeat for {}", bucket_id);
        hb_map.insert(bucket_id.to_string(), PendingHeartbeat {
            event: event.clone(),
            last_seen: Instant::now(),
            pulsetime,
            version: 1,
        });
        event
    }

    /// Queue in-progress events for real-time visibility in ClickHouse
    /// These are written with their current duration and version
    pub async fn queue_in_progress(&self) {
        let hb_map = self.last_heartbeat.read().await;
        let buckets = self.buckets.read().await;

        for (bucket_id, pending) in hb_map.iter() {
            let (device_id, bucket_type, hostname) = buckets
                .get(bucket_id)
                .map(|b| {
                    let device_id = b.data.get("device_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or(self.writer.default_device_id())
                        .to_string();
                    (device_id, b._type.clone(), b.hostname.clone())
                })
                .unwrap_or_else(|| (self.writer.default_device_id().to_string(), "unknown".into(), self.hostname.clone()));

            self.writer
                .queue(&device_id, bucket_id, &bucket_type, &hostname, pending.event.clone(), pending.version)
                .await;
        }
    }

    /// Flush stale heartbeats (no update longer than their pulsetime)
    pub async fn flush_stale(&self) {
        let mut hb_map = self.last_heartbeat.write().await;
        let buckets = self.buckets.read().await;
        let now = Instant::now();

        // Find stale heartbeats - those that haven't been updated within their pulsetime
        let stale: Vec<_> = hb_map
            .iter()
            .filter(|(_, pending)| {
                now.duration_since(pending.last_seen) > Duration::from_secs_f64(pending.pulsetime)
            })
            .map(|(k, _)| k.clone())
            .collect();

        for bucket_id in stale {
            if let Some(pending) = hb_map.remove(&bucket_id) {
                let (device_id, bucket_type, hostname) = buckets
                    .get(&bucket_id)
                    .map(|b| {
                        let device_id = b.data.get("device_id")
                            .and_then(|v| v.as_str())
                            .unwrap_or(self.writer.default_device_id())
                            .to_string();
                        (device_id, b._type.clone(), b.hostname.clone())
                    })
                    .unwrap_or_else(|| (self.writer.default_device_id().to_string(), "unknown".into(), self.hostname.clone()));

                debug!("Flushing stale heartbeat for {}", bucket_id);
                self.writer
                    .queue(&device_id, &bucket_id, &bucket_type, &hostname, pending.event, pending.version)
                    .await;
            }
        }
    }

    /// Get count of in-flight heartbeats
    pub async fn in_flight_heartbeat_count(&self) -> usize {
        self.last_heartbeat.read().await.len()
    }

    /// Flush all pending events (for shutdown)
    pub async fn flush_all(&self) {
        let mut hb_map = self.last_heartbeat.write().await;
        let buckets = self.buckets.read().await;

        for (bucket_id, pending) in hb_map.drain() {
            let (device_id, bucket_type, hostname) = buckets
                .get(&bucket_id)
                .map(|b| {
                    let device_id = b.data.get("device_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or(self.writer.default_device_id())
                        .to_string();
                    (device_id, b._type.clone(), b.hostname.clone())
                })
                .unwrap_or_else(|| (self.writer.default_device_id().to_string(), "unknown".into(), self.hostname.clone()));

            self.writer
                .queue(&device_id, &bucket_id, &bucket_type, &hostname, pending.event, pending.version)
                .await;
        }
        drop(hb_map);
        drop(buckets);

        if let Err(e) = self.writer.flush().await {
            tracing::error!("Failed to flush events on shutdown: {}", e);
        }
    }
}
