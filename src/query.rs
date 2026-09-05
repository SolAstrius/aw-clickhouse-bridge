//! `POST /api/0/query/` -- the AQL endpoint the ActivityWatch web UI uses.
//!
//! Every panel on the Activity page (Top Applications, Category Tree, the
//! sunburst, the barchart) is built by posting a query here. Without it the UI
//! reports "missing data from a required watcher" and "Time active: 0s" no
//! matter how much data has been collected, because the panels never get an
//! answer at all.
//!
//! The interpreter is upstream's, vendored in crates/aw-query and re-backed
//! onto the QuerySource trait so it reads from ClickHouse rather than SQLite.

use std::collections::HashMap;
use std::sync::Arc;

use aw_models::{Bucket, Event, Query};
use aw_query::{QueryError, QuerySource, QuerySourceError};
use axum::{extract::State, http::StatusCode, Json};
use chrono::{DateTime, Utc};
use serde_json::{json, Value};

use crate::state::AppState;

/// Adapts the bridge's storage to what the query interpreter needs.
///
/// The interpreter is synchronous while every read here is async, so it holds a
/// runtime handle and blocks on it. That is only sound because the whole query
/// runs inside spawn_blocking (see `query` below) -- calling block_on from a
/// runtime worker thread would deadlock.
struct ClickHouseQuerySource {
    state: Arc<AppState>,
    handle: tokio::runtime::Handle,
}

impl QuerySource for ClickHouseQuerySource {
    fn get_buckets(&self) -> Result<HashMap<String, Bucket>, QuerySourceError> {
        // Served from the in-memory map rather than ClickHouse, so that the
        // set of buckets a query can see is exactly the set /api/0/buckets
        // reports to the UI.
        Ok(self.state.buckets.blocking_read().clone())
    }

    fn get_events(
        &self,
        bucket_id: &str,
        start: Option<DateTime<Utc>>,
        end: Option<DateTime<Utc>>,
        limit: Option<u64>,
    ) -> Result<Vec<Event>, QuerySourceError> {
        // Distinguish "no such bucket" from "no events": the interpreter turns
        // the former into QueryError::BucketNotFound, which the UI reports
        // meaningfully, whereas an empty Vec would silently look like an idle
        // day.
        if !self.state.buckets.blocking_read().contains_key(bucket_id) {
            return Err(QuerySourceError::NoSuchBucket(bucket_id.to_string()));
        }

        self.handle
            .block_on(
                self.state
                    .writer
                    .get_events(bucket_id, start, end, limit, None),
            )
            .map_err(|e| QuerySourceError::Other(e.to_string()))
    }
}

/// Client errors are the query's fault; a storage failure is ours. Mirrors
/// aw-server's mapping so the UI reacts the same way.
fn status_for(e: &QueryError) -> StatusCode {
    match e {
        QueryError::BucketQueryError(_) => StatusCode::INTERNAL_SERVER_ERROR,
        _ => StatusCode::BAD_REQUEST,
    }
}

// POST /api/0/query
pub async fn query(
    State(state): State<Arc<AppState>>,
    Json(req): Json<Query>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let code = req.query.join("\n");
    let intervals = req.timeperiods;
    let handle = tokio::runtime::Handle::current();

    // The interpreter is CPU-bound and blocking; keep it off the async workers.
    let result = tokio::task::spawn_blocking(move || {
        let source = ClickHouseQuerySource { state, handle };
        let mut results = Vec::with_capacity(intervals.len());
        for interval in &intervals {
            match aw_query::query(&code, interval, &source) {
                Ok(data) => results.push(data),
                Err(e) => return Err((status_for(&e), e.to_string())),
            }
        }
        Ok(results)
    })
    .await;

    match result {
        Ok(Ok(results)) => Ok(Json(json!(results))),
        Ok(Err(e)) => Err(e),
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("query task panicked: {e}"),
        )),
    }
}
