mod api;
mod cache;
mod clickhouse;
mod config;
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
use tower_http::cors::CorsLayer;
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
    let cache_dir = dirs::data_local_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("aw-clickhouse-bridge");
    let cache_path = cache_dir.join("cache.jsonl");

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

    // Background flush task
    let state_bg = state.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(5));
        loop {
            interval.tick().await;
            // Move stale heartbeats to pending queue (final version)
            state_bg.flush_stale().await;
            // Queue in-progress events for real-time visibility
            state_bg.queue_in_progress().await;

            // Only attempt flush if backoff allows it
            if state_bg.writer.should_retry().await {
                // Errors are already logged inside flush()
                let _ = state_bg.writer.flush().await;
            }
        }
    });

    let app = Router::new()
        // API routes
        .route("/api/0/", get(api::server_info))
        .route("/api/0/info", get(api::server_info_alt))
        .route("/api/0/buckets", get(api::buckets_get))
        .route("/api/0/buckets/:id", get(api::bucket_get).post(api::bucket_create).delete(api::bucket_delete))
        .route("/api/0/buckets/:id/events", get(api::events_get).post(api::events_create))
        .route("/api/0/buckets/:id/events/count", get(api::events_count))
        .route("/api/0/buckets/:id/events/:event_id", get(api::event_get).delete(api::event_delete))
        .route("/api/0/buckets/:id/heartbeat", post(api::heartbeat))
        .route("/api/0/buckets/:id/export", get(api::bucket_export))
        .route("/api/0/export", get(api::export_all))
        .route("/api/0/import", post(api::import_buckets))
        .route("/api/0/settings", get(api::settings_get))
        .route("/api/0/settings/:key", get(api::setting_get).post(api::setting_set).delete(api::setting_delete))
        // WebUI routes
        .route("/", get(webui::index))
        .route("/favicon.ico", get(webui::favicon))
        .route("/logo.png", get(webui::logo))
        .route("/manifest.json", get(webui::manifest))
        .route("/dark.css", get(webui::dark_css))
        .route("/css/*path", get(webui::css_file))
        .route("/js/*path", get(webui::js_file))
        .route("/fonts/*path", get(webui::fonts_file))
        .route("/static/*path", get(webui::static_file))
        // Middleware
        .layer(TraceLayer::new_for_http())
        .layer(CorsLayer::permissive())
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
