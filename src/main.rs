mod api;
mod cache;
mod clickhouse;
mod config;
mod cors;
mod query;
mod device_id;
mod state;
mod webui;

use axum::{
    routing::{get, post},
    Router,
};
use state::AppState;
use std::sync::Arc;
use std::time::Duration;
use tokio::signal;
use axum::extract::Request;
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::ServiceExt;
use tower_http::trace::TraceLayer;
use tracing::info;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let config = config::Config::load();

    let device_id = device_id::get_or_create();
    let hostname = gethostname::gethostname().to_string_lossy().to_string();

    let ch_url = config.clickhouse_url();
    let ch_db = config.clickhouse_database();
    let ch_user = config.clickhouse_user().map(String::from);
    let ch_pass = config.clickhouse_password().map(String::from);

    let bind_addrs = config.bind_addrs();

    // Cache file location
    let cache_path = std::env::var("AW_CACHE_PATH")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            let cache_dir = std::env::var("AW_DATA_DIR")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|_| {
                    dirs::data_local_dir()
                        .unwrap_or_else(|| std::path::PathBuf::from("."))
                        .join("aw-clickhouse-bridge")
                });
            std::fs::create_dir_all(&cache_dir).ok();
            cache_dir.join("cache.jsonl")
        });

    info!("Device ID: {}", device_id);
    info!("Hostname: {}", hostname);
    info!("ClickHouse URL: {}", ch_url);
    info!("Cache path: {}", cache_path.display());

    let writer = clickhouse::ClickHouseWriter::new(
        &ch_url,
        device_id.clone(),
        &ch_db,
        ch_user.as_deref(),
        ch_pass.as_deref(),
        &cache_path,
    )?;

    // Create schema if needed (with short timeout so we don't block startup)
    match tokio::time::timeout(Duration::from_secs(3), writer.ensure_schema()).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::warn!("Failed to ensure ClickHouse schema: {} (continuing anyway)", e),
        Err(_) => tracing::warn!("ClickHouse schema check timed out (continuing anyway)"),
    }

    // Load existing buckets from ClickHouse (with short timeout)
    let buckets = match tokio::time::timeout(Duration::from_secs(3), writer.load_buckets()).await {
        Ok(Ok(b)) => {
            info!("Loaded {} buckets from ClickHouse", b.len());
            b
        }
        Ok(Err(e)) => {
            tracing::warn!("Failed to load buckets: {} (starting with empty)", e);
            Default::default()
        }
        Err(_) => {
            tracing::warn!("Loading buckets timed out (starting with empty)");
            Default::default()
        }
    };

    let state = Arc::new(AppState::new(device_id, hostname, buckets, writer));

    // Background flush task.
    //
    // Woken by the writer when something is queued, rather than polling. A
    // fixed 5s tick meant ~17k timer wakeups a day on a laptop that is idle
    // most of them, and each one paid for a full parse of the disk cache just
    // to ask whether it was empty. Now a busy loop still runs at ACTIVE_TICK
    // (heartbeats need to be swept into the pending queue promptly), while an
    // idle one sleeps up to IDLE_TICK and is woken immediately by real work.
    const ACTIVE_TICK: Duration = Duration::from_secs(5);
    const IDLE_TICK: Duration = Duration::from_secs(60);

    let state_bg = state.clone();
    let work = state.writer.work_notifier();
    tokio::spawn(async move {
        let mut tick = ACTIVE_TICK;
        loop {
            tokio::select! {
                _ = tokio::time::sleep(tick) => {}
                _ = work.notified() => {}
            }

            // Move stale heartbeats to pending queue (final version)
            let swept = state_bg.flush_stale().await;
            // Queue in-progress events for real-time visibility
            state_bg.queue_in_progress().await;

            let mut did_work = swept;

            // Only attempt flush/ping if backoff allows it
            if state_bg.writer.should_retry().await {
                if state_bg.writer.has_pending_work().await {
                    // Events and/or buckets to write.
                    let _ = state_bg.writer.flush().await;
                    state_bg.writer.flush_buckets().await;
                    did_work = true;
                } else if !state_bg.writer.is_connected() {
                    // Nothing queued but offline - ping to test recovery.
                    let _ = state_bg.writer.ping().await;
                    did_work = true;
                }
            }

            // Stay responsive while there is traffic; wind down when there is
            // not. Any queued work fires the notifier above, so backing off
            // costs no latency.
            tick = if did_work || state_bg.in_flight_heartbeat_count().await > 0 {
                ACTIVE_TICK
            } else {
                IDLE_TICK
            };
        }
    });

    let app = Router::new()
        // API routes
        //
        // Registered WITHOUT a trailing slash on purpose. normalize_path (see
        // below) rewrites the URI before routing, so a request for "/api/0/"
        // arrives here as "/api/0"; a route spelled "/api/0/" can therefore
        // never match and returns 404. This is ActivityWatch's server-info
        // endpoint, which is what clients probe first, so it must answer.
        .route("/api/0", get(api::server_info))
        .route("/api/0/info", get(api::server_info_alt))
        .route("/api/0/buckets", get(api::buckets_get))
        .route("/api/0/buckets/{id}", get(api::bucket_get).post(api::bucket_create).delete(api::bucket_delete))
        .route("/api/0/buckets/{id}/events", get(api::events_get).post(api::events_create))
        .route("/api/0/buckets/{id}/events/count", get(api::events_count))
        .route("/api/0/buckets/{id}/events/{event_id}", get(api::event_get).delete(api::event_delete))
        .route("/api/0/buckets/{id}/heartbeat", post(api::heartbeat))
        .route("/api/0/buckets/{id}/export", get(api::bucket_export))
        .route("/api/0/export", get(api::export_all))
        .route("/api/0/import", post(api::import_buckets))
        .route("/api/0/query", post(query::query))
        .route("/api/0/settings", get(api::settings_get))
        .route("/api/0/settings/{key}", get(api::setting_get).post(api::setting_set).delete(api::setting_delete))
        // Control API routes
        .route("/api/0/health", get(api::health))
        .route("/api/0/status", get(api::status))
        .route("/api/0/flush", post(api::flush))
        .route("/api/0/devices", get(api::devices_list))
        // WebUI routes
        .route("/", get(webui::index))
        .route("/_debug/assets", get(webui::list_assets))
        .route("/favicon.ico", get(webui::favicon))
        .route("/logo.png", get(webui::logo))
        .route("/manifest.json", get(webui::manifest))
        .route("/dark.css", get(webui::dark_css))
        .route("/css/{*path}", get(webui::css_file))
        .route("/js/{*path}", get(webui::js_file))
        .route("/fonts/{*path}", get(webui::fonts_file))
        .route("/assets/{*path}", get(webui::assets_file))
        .route("/static/{*path}", get(webui::static_file))
        // Middleware
        .layer(TraceLayer::new_for_http())
        // Origin allowlist. NOTE ordering: axum applies the LAST .layer() as
        // the outermost, so extension_scope wraps the CORS layer and its 403
        // is emitted without any Access-Control-* headers -- see cors.rs.
        .layer(cors::cors_layer(
            bind_addrs
                .iter()
                .filter_map(|a| a.rsplit(':').next()?.parse::<u16>().ok())
                .collect(),
        ))
        .layer(middleware::from_fn(cors::extension_scope))
        .with_state(state.clone());

    // Wrap entire service with path normalization (must happen BEFORE routing)
    let app = tower::ServiceBuilder::new()
        .layer(middleware::from_fn(normalize_path))
        .service(app);

    // Create listeners for all bind addresses
    let mut listeners = Vec::new();
    for addr in &bind_addrs {
        match tokio::net::TcpListener::bind(addr).await {
            Ok(listener) => {
                info!("Listening on {}", listener.local_addr()?);
                listeners.push(listener);
            }
            Err(e) => {
                tracing::error!("Failed to bind to {}: {}", addr, e);
                return Err(e.into());
            }
        }
    }

    // Spawn a server task for each listener
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);

    let mut server_handles = Vec::new();
    for listener in listeners {
        let app = app.clone();
        let handle = tokio::spawn(async move {
            axum::serve(listener, app.into_make_service())
                .await
                .ok();
        });
        server_handles.push(handle);
    }

    // Wait for shutdown signal
    shutdown.await;

    // Abort all server tasks
    for handle in server_handles {
        handle.abort();
    }

    // Server has stopped - now flush remaining events
    info!("Flushing remaining events...");
    state.flush_all().await;
    info!("Done.");

    Ok(())
}

async fn shutdown_signal() {
    signal::ctrl_c().await.ok();
    info!("Shutting down...");
}

/// Middleware to normalize paths:
/// - Collapse multiple slashes (//api -> /api)
/// - Remove trailing slashes (/api/0/buckets/ -> /api/0/buckets)
async fn normalize_path(mut req: Request, next: Next) -> Response {
    use axum::http::uri::{PathAndQuery, Uri};

    let uri = req.uri().clone();
    let path = uri.path();

    // Check if path needs normalization
    let needs_normalize = path.contains("//") || (path.len() > 1 && path.ends_with('/'));

    if needs_normalize {
        let mut normalized = path.replace("//", "/");
        // Remove trailing slash (but keep "/" as is)
        if normalized.len() > 1 && normalized.ends_with('/') {
            normalized.pop();
        }

        let query = uri.query().map(|q| q.to_string());
        let new_path_and_query = if let Some(q) = query {
            format!("{}?{}", normalized, q)
        } else {
            normalized
        };
        if let Ok(pq) = new_path_and_query.parse::<PathAndQuery>() {
            let mut parts = uri.into_parts();
            parts.path_and_query = Some(pq);
            if let Ok(new_uri) = Uri::from_parts(parts) {
                *req.uri_mut() = new_uri;
            }
        }
    }

    next.run(req).await
}
