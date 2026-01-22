use crate::state::AppState;
use aw_models::{Bucket, BucketsExport, Event, Info, TryVec};
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use std::collections::HashMap;
use std::sync::Arc;

// ============================================================================
// Control API types
// ============================================================================

#[derive(serde::Serialize)]
pub struct HealthResponse {
    pub status: &'static str,
}

#[derive(serde::Serialize)]
pub struct StatusResponse {
    pub device_id: String,
    pub hostname: String,
    pub version: String,
    pub clickhouse_connected: bool,
    pub retry_delay_secs: u64,
    pub pending_events: usize,
    pub in_flight_heartbeats: usize,
    pub buckets_cached: usize,
}

#[derive(serde::Serialize)]
pub struct FlushResponse {
    pub flushed: bool,
    pub error: Option<String>,
}

#[derive(serde::Serialize)]
pub struct DeviceInfo {
    pub device_id: String,
    pub hostname: String,
    pub bucket_count: u64,
    pub event_count: u64,
    pub is_current: bool,
}

#[derive(serde::Deserialize)]
pub struct HeartbeatParams {
    pub pulsetime: f64,
}

#[derive(serde::Deserialize)]
pub struct EventsGetParams {
    pub start: Option<String>,
    pub end: Option<String>,
    /// Limit number of events. Use -1 for no limit.
    pub limit: Option<i64>,
    /// Filter by device_id. Use "*" for all devices, or a specific UUID.
    /// Defaults to current device if not specified.
    pub device_id: Option<String>,
}

// GET /api/0/
pub async fn server_info(State(state): State<Arc<AppState>>) -> Json<Info> {
    Json(Info {
        hostname: state.hostname.clone(),
        version: format!("aw-clickhouse-bridge v{}", env!("CARGO_PKG_VERSION")),
        testing: false,
        device_id: state.device_id.clone(),
    })
}

// GET /api/0/buckets
pub async fn buckets_get(State(state): State<Arc<AppState>>) -> Json<HashMap<String, Bucket>> {
    Json(state.buckets.read().await.clone())
}

// GET /api/0/buckets/:id
pub async fn bucket_get(
    Path(bucket_id): Path<String>,
    State(state): State<Arc<AppState>>,
) -> Result<Json<Bucket>, StatusCode> {
    state
        .buckets
        .read()
        .await
        .get(&bucket_id)
        .cloned()
        .map(Json)
        .ok_or(StatusCode::NOT_FOUND)
}

// POST /api/0/buckets/:id
pub async fn bucket_create(
    Path(bucket_id): Path<String>,
    State(state): State<Arc<AppState>>,
    Json(mut bucket): Json<Bucket>,
) -> Result<StatusCode, (StatusCode, String)> {
    bucket.id = bucket_id.clone();
    if bucket.hostname == "!local" {
        bucket.hostname = state.hostname.clone();
        bucket
            .data
            .insert("device_id".to_string(), state.device_id.clone().into());
    }

    // Cache in memory first (so events get correct bucket_type even if CH is offline)
    state.buckets.write().await.insert(bucket_id, bucket.clone());

    // Persist to ClickHouse (queues for retry if offline, always returns Ok)
    let _ = state.writer.save_bucket(&bucket).await;

    Ok(StatusCode::OK)
}

// POST /api/0/buckets/:id/heartbeat?pulsetime=N
pub async fn heartbeat(
    Path(bucket_id): Path<String>,
    Query(params): Query<HeartbeatParams>,
    State(state): State<Arc<AppState>>,
    Json(event): Json<Event>,
) -> Json<Event> {
    Json(
        state
            .handle_heartbeat(&bucket_id, event, params.pulsetime)
            .await,
    )
}

// POST /api/0/buckets/:id/events
pub async fn events_create(
    Path(bucket_id): Path<String>,
    State(state): State<Arc<AppState>>,
    Json(events): Json<Vec<Event>>,
) -> Json<Vec<Event>> {
    let (device_id, bucket_type, hostname) = {
        let buckets = state.buckets.read().await;
        buckets
            .get(&bucket_id)
            .map(|b| {
                let device_id = b.data.get("device_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or(state.writer.default_device_id())
                    .to_string();
                (device_id, b._type.clone(), b.hostname.clone())
            })
            .unwrap_or_else(|| (state.writer.default_device_id().to_string(), "unknown".into(), state.hostname.clone()))
    };

    for event in &events {
        // Direct event creation uses version 1 (final, no updates)
        state
            .writer
            .queue(&device_id, &bucket_id, &bucket_type, &hostname, event.clone(), 1)
            .await;
    }
    Json(events)
}

// DELETE /api/0/buckets/:id
pub async fn bucket_delete(
    Path(bucket_id): Path<String>,
    State(state): State<Arc<AppState>>,
) -> Result<StatusCode, (StatusCode, String)> {
    // Delete from ClickHouse
    if let Err(e) = state.writer.delete_bucket(&bucket_id).await {
        return Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string()));
    }

    // Remove from memory cache
    state.buckets.write().await.remove(&bucket_id);
    Ok(StatusCode::OK)
}

// GET /api/0/buckets/:id/events?start=...&end=...&limit=...
pub async fn events_get(
    Path(bucket_id): Path<String>,
    Query(params): Query<EventsGetParams>,
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<Event>>, (StatusCode, String)> {
    use chrono::DateTime;

    let start = params
        .start
        .as_ref()
        .map(|s| DateTime::parse_from_rfc3339(s))
        .transpose()
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("Invalid start time: {}", e)))?
        .map(|dt| dt.with_timezone(&chrono::Utc));

    let end = params
        .end
        .as_ref()
        .map(|s| DateTime::parse_from_rfc3339(s))
        .transpose()
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("Invalid end time: {}", e)))?
        .map(|dt| dt.with_timezone(&chrono::Utc));

    // Convert limit: -1 or negative means no limit
    let limit = params.limit.and_then(|l| if l < 0 { None } else { Some(l as u64) });

    state
        .writer
        .get_events(&bucket_id, start, end, limit, params.device_id.as_deref())
        .await
        .map(Json)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

// GET /api/0/buckets/:id/events/:event_id
pub async fn event_get(
    Path((bucket_id, event_id)): Path<(String, i64)>,
    State(state): State<Arc<AppState>>,
) -> Result<Json<Event>, (StatusCode, String)> {
    state
        .writer
        .get_event_by_id(&bucket_id, event_id)
        .await
        .map(Json)
        .map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))
}

#[derive(serde::Deserialize)]
pub struct CountParams {
    pub device_id: Option<String>,
}

// GET /api/0/buckets/:id/events/count
pub async fn events_count(
    Path(bucket_id): Path<String>,
    Query(params): Query<CountParams>,
    State(state): State<Arc<AppState>>,
) -> Result<Json<u64>, (StatusCode, String)> {
    state
        .writer
        .get_event_count(&bucket_id, None, None, params.device_id.as_deref())
        .await
        .map(Json)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

// GET /api/0/info - same as /api/0/ but webui expects this path
pub async fn server_info_alt(State(state): State<Arc<AppState>>) -> Json<Info> {
    server_info(State(state)).await
}

// GET /api/0/settings
pub async fn settings_get(
    State(state): State<Arc<AppState>>,
) -> Result<Json<HashMap<String, serde_json::Value>>, (StatusCode, String)> {
    state
        .writer
        .load_settings()
        .await
        .map(Json)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

// GET /api/0/settings/:key
pub async fn setting_get(
    Path(key): Path<String>,
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    state
        .writer
        .load_settings()
        .await
        .ok()
        .and_then(|s| s.get(&key).cloned())
        .map(Json)
        .ok_or(StatusCode::NOT_FOUND)
}

// POST /api/0/settings/:key
pub async fn setting_set(
    Path(key): Path<String>,
    State(state): State<Arc<AppState>>,
    Json(value): Json<serde_json::Value>,
) -> Result<StatusCode, (StatusCode, String)> {
    state
        .writer
        .save_setting(&key, &value)
        .await
        .map(|_| StatusCode::OK)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

// DELETE /api/0/settings/:key
pub async fn setting_delete(
    Path(key): Path<String>,
    State(state): State<Arc<AppState>>,
) -> Result<StatusCode, (StatusCode, String)> {
    state
        .writer
        .delete_setting(&key)
        .await
        .map(|_| StatusCode::OK)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

// DELETE /api/0/buckets/:id/events/:event_id
pub async fn event_delete(
    Path((bucket_id, event_id)): Path<(String, i64)>,
    State(state): State<Arc<AppState>>,
) -> Result<StatusCode, (StatusCode, String)> {
    state
        .writer
        .delete_event(&bucket_id, event_id)
        .await
        .map(|_| StatusCode::OK)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

// GET /api/0/buckets/:id/export
pub async fn bucket_export(
    Path(bucket_id): Path<String>,
    State(state): State<Arc<AppState>>,
) -> Result<Json<BucketsExport>, (StatusCode, String)> {
    // Get bucket from cache
    let mut bucket = state
        .buckets
        .read()
        .await
        .get(&bucket_id)
        .cloned()
        .ok_or_else(|| (StatusCode::NOT_FOUND, "Bucket not found".to_string()))?;

    // Fetch all events for this bucket
    let events = state
        .writer
        .get_events(&bucket_id, None, None, None, None)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    // Get metadata
    bucket.metadata = state
        .writer
        .get_bucket_metadata(&bucket_id)
        .await
        .unwrap_or_default();

    // Attach events to bucket
    bucket.events = Some(TryVec::new(events));

    let mut export = BucketsExport {
        buckets: HashMap::new(),
    };
    export.buckets.insert(bucket_id, bucket);

    Ok(Json(export))
}

// GET /api/0/export - Export all buckets
pub async fn export_all(
    State(state): State<Arc<AppState>>,
) -> Result<Json<BucketsExport>, (StatusCode, String)> {
    let bucket_ids: Vec<String> = state.buckets.read().await.keys().cloned().collect();

    let mut export = BucketsExport {
        buckets: HashMap::new(),
    };

    for bucket_id in bucket_ids {
        let mut bucket = state
            .buckets
            .read()
            .await
            .get(&bucket_id)
            .cloned()
            .ok_or_else(|| (StatusCode::NOT_FOUND, "Bucket not found".to_string()))?;

        let events = state
            .writer
            .get_events(&bucket_id, None, None, None, None)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

        bucket.metadata = state
            .writer
            .get_bucket_metadata(&bucket_id)
            .await
            .unwrap_or_default();

        bucket.events = Some(TryVec::new(events));
        export.buckets.insert(bucket_id, bucket);
    }

    Ok(Json(export))
}

// POST /api/0/import - Import buckets from JSON
pub async fn import_buckets(
    State(state): State<Arc<AppState>>,
    Json(import): Json<BucketsExport>,
) -> Result<StatusCode, (StatusCode, String)> {
    for (bucket_id, mut bucket) in import.buckets {
        // Ensure bucket ID matches
        bucket.id = bucket_id.clone();

        // Handle !local hostname
        if bucket.hostname == "!local" {
            bucket.hostname = state.hostname.clone();
            bucket
                .data
                .insert("device_id".to_string(), state.device_id.clone().into());
        }

        // Extract events before saving bucket
        let events = bucket.events.take();

        // Save bucket to ClickHouse
        if let Err(e) = state.writer.save_bucket(&bucket).await {
            return Err((StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to save bucket {}: {}", bucket_id, e)));
        }

        // Cache bucket in memory
        state.buckets.write().await.insert(bucket_id.clone(), bucket.clone());

        // Import events if present
        if let Some(events) = events {
            let device_id = bucket.data.get("device_id")
                .and_then(|v| v.as_str())
                .unwrap_or(state.writer.default_device_id());
            for event in events.take_inner() {
                // Imported events are final, version 1
                state
                    .writer
                    .queue(device_id, &bucket_id, &bucket._type, &bucket.hostname, event, 1)
                    .await;
            }
        }
    }

    // Flush imported events immediately
    let _ = state.writer.flush().await;

    Ok(StatusCode::OK)
}

// ============================================================================
// Control API handlers
// ============================================================================

// GET /api/0/health
pub async fn health() -> Json<HealthResponse> {
    Json(HealthResponse { status: "ok" })
}

// GET /api/0/status
pub async fn status(State(state): State<Arc<AppState>>) -> Json<StatusResponse> {
    Json(StatusResponse {
        device_id: state.device_id.clone(),
        hostname: state.hostname.clone(),
        version: format!("aw-clickhouse-bridge v{}", env!("CARGO_PKG_VERSION")),
        clickhouse_connected: state.writer.is_connected(),
        retry_delay_secs: state.writer.retry_delay(),
        pending_events: state.writer.pending_count().await,
        in_flight_heartbeats: state.in_flight_heartbeat_count().await,
        buckets_cached: state.buckets.read().await.len(),
    })
}

// POST /api/0/flush
pub async fn flush(State(state): State<Arc<AppState>>) -> Json<FlushResponse> {
    // Flush stale heartbeats first
    state.flush_stale().await;
    state.queue_in_progress().await;

    // Then flush to ClickHouse
    match state.writer.flush().await {
        Ok(()) => Json(FlushResponse {
            flushed: true,
            error: None,
        }),
        Err(e) => Json(FlushResponse {
            flushed: false,
            error: Some(e.to_string()),
        }),
    }
}

// GET /api/0/devices
pub async fn devices_list(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<DeviceInfo>>, (StatusCode, String)> {
    state
        .writer
        .get_devices()
        .await
        .map(|devices| {
            Json(
                devices
                    .into_iter()
                    .map(|(device_id, hostname, bucket_count, event_count)| DeviceInfo {
                        is_current: device_id == state.device_id,
                        device_id,
                        hostname,
                        bucket_count,
                        event_count,
                    })
                    .collect(),
            )
        })
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}
