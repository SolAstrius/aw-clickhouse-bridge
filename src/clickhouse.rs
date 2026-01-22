use crate::cache::DiskCache;
use aw_models::Event;
use chrono::{DateTime, TimeZone, Utc};
use clickhouse::{Client, Row};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;
use tokio::sync::{Mutex, RwLock};
use tracing::{info, warn};

#[derive(Row, Serialize)]
pub struct EventRowWrite {
    pub device_id: String,
    pub bucket_id: String,
    pub bucket_type: String,
    pub hostname: String,
    #[serde(rename = "timestamp")]
    pub timestamp_us: i64, // microseconds since epoch
    pub duration: f64,     // seconds
    pub data: String,      // JSON string - ClickHouse will parse into JSON column
    pub version: u64,      // increments on each write, ReplacingMergeTree keeps highest
}

#[derive(Row, Deserialize, Debug)]
pub struct EventRowRead {
    pub id: i64,
    pub timestamp: i64, // microseconds since epoch
    pub duration: f64,
    pub data: String,
}

#[derive(Row, Deserialize, Debug)]
pub struct CountRow {
    pub count: u64,
}

const INITIAL_RETRY_DELAY_SECS: u64 = 5;
const MAX_RETRY_DELAY_SECS: u64 = 120;

pub struct ClickHouseWriter {
    client: Client,
    device_id: String,
    pending: RwLock<Vec<(String, String, String, String, Event, u64)>>, // (device_id, bucket_id, bucket_type, hostname, event, version)
    pending_buckets: RwLock<Vec<aw_models::Bucket>>, // Buckets waiting to be saved to ClickHouse
    cache: DiskCache,
    connected: AtomicBool,
    retry_delay_secs: AtomicU64,
    last_attempt: Mutex<Option<Instant>>,
}

impl ClickHouseWriter {
    pub fn new(
        url: &str,
        device_id: String,
        database: &str,
        user: Option<&str>,
        password: Option<&str>,
        cache_path: &Path,
    ) -> std::io::Result<Self> {
        let mut client = Client::default()
            .with_url(url)
            .with_database(database)
            // JSON column support (ClickHouse 24.10+)
            .with_option("allow_experimental_json_type", "1")
            .with_option("input_format_binary_read_json_as_string", "1")
            .with_option("output_format_binary_write_json_as_string", "1");

        if let Some(u) = user {
            client = client.with_user(u);
        }
        if let Some(p) = password {
            client = client.with_password(p);
        }

        let cache = DiskCache::open(cache_path)?;

        // Log if there are cached events from a previous run
        match cache.read_all() {
            Ok(events) if !events.is_empty() => {
                info!("Loaded {} cached events from previous run", events.len());
            }
            _ => {}
        }

        Ok(Self {
            client,
            device_id,
            pending: RwLock::new(Vec::new()),
            pending_buckets: RwLock::new(Vec::new()),
            cache,
            connected: AtomicBool::new(true), // Assume connected until proven otherwise
            retry_delay_secs: AtomicU64::new(INITIAL_RETRY_DELAY_SECS),
            last_attempt: Mutex::new(None),
        })
    }

    pub async fn ensure_schema(&self) -> Result<(), clickhouse::error::Error> {
        // Try to create new table with ReplacingMergeTree
        // If table exists with old schema, we'll add the version column
        self.client
            .query(
                r#"
                CREATE TABLE IF NOT EXISTS aw_events (
                    id Int64 MATERIALIZED toInt64(toUnixTimestamp64Micro(timestamp)),
                    device_id LowCardinality(String),
                    bucket_id LowCardinality(String),
                    bucket_type LowCardinality(String),
                    hostname LowCardinality(String),
                    timestamp DateTime64(6, 'UTC'),
                    duration Float64,
                    data JSON,
                    version UInt64 DEFAULT 0
                ) ENGINE = ReplacingMergeTree(version)
                PARTITION BY toYYYYMM(timestamp)
                ORDER BY (device_id, bucket_id, timestamp)
                "#,
            )
            .execute()
            .await?;

        self.client
            .query(
                r#"
                CREATE TABLE IF NOT EXISTS aw_buckets (
                    device_id LowCardinality(String),
                    bucket_id String,
                    bucket_type LowCardinality(String),
                    client LowCardinality(String),
                    hostname LowCardinality(String),
                    created DateTime64(3, 'UTC'),
                    data String
                ) ENGINE = ReplacingMergeTree()
                ORDER BY (device_id, bucket_id)
                "#,
            )
            .execute()
            .await?;

        self.client
            .query(
                r#"
                CREATE TABLE IF NOT EXISTS aw_settings (
                    device_id LowCardinality(String),
                    key String,
                    value String
                ) ENGINE = ReplacingMergeTree()
                ORDER BY (device_id, key)
                "#,
            )
            .execute()
            .await?;

        Ok(())
    }

    pub async fn load_buckets(&self) -> Result<std::collections::HashMap<String, aw_models::Bucket>, clickhouse::error::Error> {
        #[derive(Row, Deserialize)]
        struct BucketRow {
            bucket_id: String,
            bucket_type: String,
            client: String,
            hostname: String,
            created: i64,
            data: String,
        }

        let rows: Vec<BucketRow> = self.client
            .query("SELECT bucket_id, bucket_type, client, hostname, toInt64(toUnixTimestamp64Micro(created)) as created, data FROM aw_buckets WHERE device_id = ?")
            .bind(&self.device_id)
            .fetch_all()
            .await?;

        let mut buckets = std::collections::HashMap::new();
        for row in rows {
            let bucket = aw_models::Bucket {
                bid: None,
                id: row.bucket_id.clone(),
                _type: row.bucket_type,
                client: row.client,
                hostname: row.hostname,
                created: Some(Utc.timestamp_micros(row.created).unwrap()),
                data: serde_json::from_str(&row.data).unwrap_or_default(),
                metadata: Default::default(),
                events: None,
                last_updated: None,
            };
            buckets.insert(row.bucket_id, bucket);
        }

        Ok(buckets)
    }

    /// Save bucket to ClickHouse, queueing for later if offline
    /// Returns Ok(()) even if ClickHouse is offline (bucket is queued)
    pub async fn save_bucket(&self, bucket: &aw_models::Bucket) -> Result<(), clickhouse::error::Error> {
        let result = self.client
            .query(
                "INSERT INTO aw_buckets (device_id, bucket_id, bucket_type, client, hostname, created, data) VALUES (?, ?, ?, ?, ?, ?, ?)"
            )
            .bind(&self.device_id)
            .bind(&bucket.id)
            .bind(&bucket._type)
            .bind(&bucket.client)
            .bind(&bucket.hostname)
            .bind(bucket.created.unwrap_or_else(chrono::Utc::now).timestamp_micros())
            .bind(serde_json::to_string(&bucket.data).unwrap_or_default())
            .execute()
            .await;

        if let Err(e) = result {
            // Queue bucket for later save
            warn!("Failed to save bucket {} to ClickHouse (queued for retry): {}", bucket.id, e);
            self.pending_buckets.write().await.push(bucket.clone());
        }
        Ok(())
    }

    /// Flush pending buckets to ClickHouse
    async fn flush_pending_buckets(&self) -> Result<(), clickhouse::error::Error> {
        let buckets = {
            let mut pending = self.pending_buckets.write().await;
            std::mem::take(&mut *pending)
        };

        if buckets.is_empty() {
            return Ok(());
        }

        let mut failed = Vec::new();
        for bucket in buckets {
            let result = self.client
                .query(
                    "INSERT INTO aw_buckets (device_id, bucket_id, bucket_type, client, hostname, created, data) VALUES (?, ?, ?, ?, ?, ?, ?)"
                )
                .bind(&self.device_id)
                .bind(&bucket.id)
                .bind(&bucket._type)
                .bind(&bucket.client)
                .bind(&bucket.hostname)
                .bind(bucket.created.unwrap_or_else(chrono::Utc::now).timestamp_micros())
                .bind(serde_json::to_string(&bucket.data).unwrap_or_default())
                .execute()
                .await;

            if result.is_err() {
                failed.push(bucket);
            }
        }

        // Re-queue failed buckets
        if !failed.is_empty() {
            self.pending_buckets.write().await.extend(failed);
        }

        Ok(())
    }

    pub async fn load_settings(&self) -> Result<std::collections::HashMap<String, serde_json::Value>, clickhouse::error::Error> {
        #[derive(Row, Deserialize)]
        struct SettingRow {
            key: String,
            value: String,
        }

        let rows: Vec<SettingRow> = self.client
            .query("SELECT key, value FROM aw_settings FINAL WHERE device_id = ?")
            .bind(&self.device_id)
            .fetch_all()
            .await?;

        let mut settings = std::collections::HashMap::new();
        for row in rows {
            if let Ok(value) = serde_json::from_str(&row.value) {
                settings.insert(row.key, value);
            }
        }

        Ok(settings)
    }

    pub async fn save_setting(&self, key: &str, value: &serde_json::Value) -> Result<(), clickhouse::error::Error> {
        self.client
            .query("INSERT INTO aw_settings (device_id, key, value) VALUES (?, ?, ?)")
            .bind(&self.device_id)
            .bind(key)
            .bind(serde_json::to_string(value).unwrap_or_default())
            .execute()
            .await?;
        Ok(())
    }

    pub async fn delete_setting(&self, key: &str) -> Result<(), clickhouse::error::Error> {
        self.client
            .query("ALTER TABLE aw_settings DELETE WHERE device_id = ? AND key = ?")
            .bind(&self.device_id)
            .bind(key)
            .execute()
            .await?;
        Ok(())
    }

    /// Get the default device_id for this writer
    pub fn default_device_id(&self) -> &str {
        &self.device_id
    }

    pub async fn queue(
        &self,
        device_id: &str,
        bucket_id: &str,
        bucket_type: &str,
        hostname: &str,
        event: Event,
        version: u64,
    ) {
        self.pending.write().await.push((
            device_id.to_string(),
            bucket_id.to_string(),
            bucket_type.to_string(),
            hostname.to_string(),
            event,
            version,
        ));
    }

    /// Check if we should attempt a flush (respects backoff when disconnected)
    pub async fn should_retry(&self) -> bool {
        if self.connected.load(Ordering::Relaxed) {
            return true;
        }

        let last_attempt = self.last_attempt.lock().await;
        match *last_attempt {
            Some(instant) => {
                let delay = std::time::Duration::from_secs(
                    self.retry_delay_secs.load(Ordering::Relaxed),
                );
                instant.elapsed() >= delay
            }
            None => true,
        }
    }

    /// Returns true if connected to ClickHouse
    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    /// Get current retry delay in seconds
    pub fn retry_delay(&self) -> u64 {
        self.retry_delay_secs.load(Ordering::Relaxed)
    }

    /// Get count of pending events (in memory + on disk)
    pub async fn pending_count(&self) -> usize {
        let memory_count = self.pending.read().await.len();
        let disk_count = self.cache.read_all().map(|v| v.len()).unwrap_or(0);
        memory_count + disk_count
    }

    /// Ping ClickHouse to test connectivity (used for recovery when no events pending)
    pub async fn ping(&self) -> Result<(), clickhouse::error::Error> {
        // Update last attempt time
        *self.last_attempt.lock().await = Some(Instant::now());

        // Simple query to test connectivity
        let result = self.client.query("SELECT 1").execute().await;

        match result {
            Ok(()) => {
                let was_offline = !self.connected.swap(true, Ordering::Relaxed);
                self.retry_delay_secs
                    .store(INITIAL_RETRY_DELAY_SECS, Ordering::Relaxed);
                if was_offline {
                    info!("ClickHouse connection recovered");
                    // Flush any pending buckets now that we're back online
                    let _ = self.flush_pending_buckets().await;
                }
                Ok(())
            }
            Err(e) => {
                self.connected.store(false, Ordering::Relaxed);
                let current_delay = self.retry_delay_secs.load(Ordering::Relaxed);
                let new_delay = (current_delay * 2).min(MAX_RETRY_DELAY_SECS);
                self.retry_delay_secs.store(new_delay, Ordering::Relaxed);
                Err(e)
            }
        }
    }

    pub async fn flush(&self) -> Result<(), clickhouse::error::Error> {
        // Take pending events from memory
        let memory_batch = {
            let mut pending = self.pending.write().await;
            std::mem::take(&mut *pending)
        };

        // Load cached events from disk
        let cached_batch = self.cache.read_all().unwrap_or_default();

        // Combine: cached first (older), then memory (newer)
        let mut batch = cached_batch;
        batch.extend(memory_batch.clone());

        if batch.is_empty() {
            return Ok(());
        }

        let event_count = batch.len();

        // Update last attempt time
        *self.last_attempt.lock().await = Some(Instant::now());

        // Attempt batch insert
        let result = self.do_insert(&batch).await;

        match result {
            Ok(()) => {
                info!("Flushed {} events to ClickHouse", event_count);
                // Success: clear cache and reset backoff
                if let Err(e) = self.cache.clear() {
                    warn!("Failed to clear cache file: {}", e);
                }
                self.connected.store(true, Ordering::Relaxed);
                self.retry_delay_secs
                    .store(INITIAL_RETRY_DELAY_SECS, Ordering::Relaxed);
                // Also flush any pending buckets
                let _ = self.flush_pending_buckets().await;
                Ok(())
            }
            Err(e) => {
                // Failure: cache all events to disk
                let was_connected = self.connected.swap(false, Ordering::Relaxed);

                // Only cache the new memory events (disk events are already cached)
                if !memory_batch.is_empty() {
                    if let Err(cache_err) = self.cache.append(&memory_batch) {
                        warn!("Failed to cache events to disk: {}", cache_err);
                    }
                }

                // Increase backoff (exponential, capped)
                let current_delay = self.retry_delay_secs.load(Ordering::Relaxed);
                let new_delay = (current_delay * 2).min(MAX_RETRY_DELAY_SECS);
                self.retry_delay_secs.store(new_delay, Ordering::Relaxed);

                let total_cached = self.cache.read_all().map(|v| v.len()).unwrap_or(0);

                if was_connected {
                    warn!(
                        "ClickHouse offline, cached {} events (retry in {}s): {}",
                        total_cached, new_delay, e
                    );
                } else {
                    info!(
                        "ClickHouse still offline, {} events cached (retry in {}s)",
                        total_cached, new_delay
                    );
                }

                Err(e)
            }
        }
    }

    async fn do_insert(
        &self,
        batch: &[(String, String, String, String, Event, u64)],
    ) -> Result<(), clickhouse::error::Error> {
        let mut insert = self.client.insert::<EventRowWrite>("aw_events").await?;
        for (device_id, bucket_id, bucket_type, hostname, event, version) in batch {
            insert
                .write(&EventRowWrite {
                    device_id: device_id.clone(),
                    bucket_id: bucket_id.clone(),
                    bucket_type: bucket_type.clone(),
                    hostname: hostname.clone(),
                    timestamp_us: event.timestamp.timestamp_micros(),
                    duration: event.duration.num_nanoseconds().unwrap_or(0) as f64 / 1e9,
                    data: serde_json::to_string(&event.data).unwrap_or_default(),
                    version: *version,
                })
                .await?;
        }
        insert.end().await?;
        Ok(())
    }

    /// Get events from ClickHouse
    /// device_filter: None = current device, Some("*") = all devices, Some(uuid) = specific device
    pub async fn get_events(
        &self,
        bucket_id: &str,
        start: Option<DateTime<Utc>>,
        end: Option<DateTime<Utc>>,
        limit: Option<u64>,
        device_filter: Option<&str>,
    ) -> Result<Vec<Event>, clickhouse::error::Error> {
        let all_devices = device_filter.map(|d| d == "*").unwrap_or(false);
        let device_id = device_filter
            .filter(|d| *d != "*")
            .unwrap_or(&self.device_id);

        let mut query = if all_devices {
            String::from("SELECT id, toInt64(toUnixTimestamp64Micro(timestamp)) as timestamp, duration, toString(data) as data FROM aw_events FINAL WHERE bucket_id = ?")
        } else {
            String::from("SELECT id, toInt64(toUnixTimestamp64Micro(timestamp)) as timestamp, duration, toString(data) as data FROM aw_events FINAL WHERE device_id = ? AND bucket_id = ?")
        };

        if start.is_some() {
            query.push_str(" AND timestamp >= ?");
        }
        if end.is_some() {
            query.push_str(" AND timestamp <= ?");
        }
        query.push_str(" ORDER BY timestamp DESC");
        if let Some(l) = limit {
            query.push_str(&format!(" LIMIT {}", l));
        }

        let mut q = if all_devices {
            self.client.query(&query).bind(bucket_id)
        } else {
            self.client.query(&query).bind(device_id).bind(bucket_id)
        };
        if let Some(s) = start {
            q = q.bind(s.timestamp_micros());
        }
        if let Some(e) = end {
            q = q.bind(e.timestamp_micros());
        }

        let rows: Vec<EventRowRead> = q.fetch_all().await?;

        let events = rows
            .into_iter()
            .map(|r| {
                let data: serde_json::Map<String, serde_json::Value> =
                    serde_json::from_str(&r.data).unwrap_or_default();
                Event {
                    id: Some(r.id),
                    timestamp: Utc.timestamp_micros(r.timestamp).unwrap(),
                    duration: chrono::Duration::nanoseconds((r.duration * 1e9) as i64),
                    data,
                }
            })
            .collect();

        Ok(events)
    }

    /// Get single event by ID
    pub async fn get_event_by_id(
        &self,
        bucket_id: &str,
        event_id: i64,
    ) -> Result<Event, clickhouse::error::Error> {
        let query = "SELECT id, toInt64(toUnixTimestamp64Micro(timestamp)) as timestamp, duration, toString(data) as data FROM aw_events FINAL WHERE device_id = ? AND bucket_id = ? AND id = ? LIMIT 1";

        let row: EventRowRead = self
            .client
            .query(query)
            .bind(&self.device_id)
            .bind(bucket_id)
            .bind(event_id)
            .fetch_one()
            .await?;

        let data: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(&row.data).unwrap_or_default();

        Ok(Event {
            id: Some(row.id),
            timestamp: Utc.timestamp_micros(row.timestamp).unwrap(),
            duration: chrono::Duration::nanoseconds((row.duration * 1e9) as i64),
            data,
        })
    }

    /// Get event count from ClickHouse
    pub async fn get_event_count(
        &self,
        bucket_id: &str,
        start: Option<DateTime<Utc>>,
        end: Option<DateTime<Utc>>,
        device_filter: Option<&str>,
    ) -> Result<u64, clickhouse::error::Error> {
        let all_devices = device_filter.map(|d| d == "*").unwrap_or(false);
        let device_id = device_filter
            .filter(|d| *d != "*")
            .unwrap_or(&self.device_id);

        let mut query = if all_devices {
            String::from("SELECT count() as count FROM aw_events FINAL WHERE bucket_id = ?")
        } else {
            String::from("SELECT count() as count FROM aw_events FINAL WHERE device_id = ? AND bucket_id = ?")
        };

        if start.is_some() {
            query.push_str(" AND timestamp >= ?");
        }
        if end.is_some() {
            query.push_str(" AND timestamp <= ?");
        }

        let mut q = if all_devices {
            self.client.query(&query).bind(bucket_id)
        } else {
            self.client.query(&query).bind(device_id).bind(bucket_id)
        };
        if let Some(s) = start {
            q = q.bind(s.timestamp_micros());
        }
        if let Some(e) = end {
            q = q.bind(e.timestamp_micros());
        }

        let row: CountRow = q.fetch_one().await?;
        Ok(row.count)
    }

    /// Delete a bucket and all its events from ClickHouse
    pub async fn delete_bucket(&self, bucket_id: &str) -> Result<(), clickhouse::error::Error> {
        // Delete events first
        self.client
            .query("ALTER TABLE aw_events DELETE WHERE device_id = ? AND bucket_id = ?")
            .bind(&self.device_id)
            .bind(bucket_id)
            .execute()
            .await?;

        // Delete bucket
        self.client
            .query("ALTER TABLE aw_buckets DELETE WHERE device_id = ? AND bucket_id = ?")
            .bind(&self.device_id)
            .bind(bucket_id)
            .execute()
            .await?;

        info!("Deleted bucket {} and its events", bucket_id);
        Ok(())
    }

    /// Delete a single event by ID
    pub async fn delete_event(
        &self,
        bucket_id: &str,
        event_id: i64,
    ) -> Result<(), clickhouse::error::Error> {
        self.client
            .query("ALTER TABLE aw_events DELETE WHERE device_id = ? AND bucket_id = ? AND id = ?")
            .bind(&self.device_id)
            .bind(bucket_id)
            .bind(event_id)
            .execute()
            .await?;

        info!("Deleted event {} from bucket {}", event_id, bucket_id);
        Ok(())
    }

    /// Get all devices with their stats
    /// Returns: Vec<(device_id, hostname, bucket_count, event_count)>
    pub async fn get_devices(&self) -> Result<Vec<(String, String, u64, u64)>, clickhouse::error::Error> {
        #[derive(Row, Deserialize)]
        struct DeviceRow {
            device_id: String,
            hostname: String,
            bucket_count: u64,
            event_count: u64,
        }

        let rows: Vec<DeviceRow> = self.client
            .query(r#"
                SELECT
                    b.device_id,
                    any(b.hostname) as hostname,
                    count(DISTINCT b.bucket_id) as bucket_count,
                    sum(e.cnt) as event_count
                FROM aw_buckets b
                LEFT JOIN (
                    SELECT device_id, bucket_id, count() as cnt
                    FROM aw_events FINAL
                    GROUP BY device_id, bucket_id
                ) e ON b.device_id = e.device_id AND b.bucket_id = e.bucket_id
                GROUP BY b.device_id
                ORDER BY event_count DESC
            "#)
            .fetch_all()
            .await?;

        Ok(rows.into_iter().map(|r| (r.device_id, r.hostname, r.bucket_count, r.event_count)).collect())
    }

    /// Get bucket metadata (start/end timestamps) from events
    pub async fn get_bucket_metadata(
        &self,
        bucket_id: &str,
    ) -> Result<aw_models::BucketMetadata, clickhouse::error::Error> {
        #[derive(Row, Deserialize)]
        struct MetadataRow {
            min_ts: i64,
            max_ts: i64,
        }

        let result: Result<MetadataRow, _> = self
            .client
            .query(
                "SELECT \
                    toInt64(toUnixTimestamp64Micro(min(timestamp))) as min_ts, \
                    toInt64(toUnixTimestamp64Micro(max(timestamp))) as max_ts \
                FROM aw_events WHERE device_id = ? AND bucket_id = ?",
            )
            .bind(&self.device_id)
            .bind(bucket_id)
            .fetch_one()
            .await;

        match result {
            Ok(row) if row.min_ts > 0 => Ok(aw_models::BucketMetadata {
                start: Utc.timestamp_micros(row.min_ts).single(),
                end: Utc.timestamp_micros(row.max_ts).single(),
            }),
            _ => Ok(aw_models::BucketMetadata::default()),
        }
    }
}
