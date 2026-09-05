//! CORS policy.
//!
//! This service has no authentication and binds loopback by default, so the
//! browser is the only thing standing between a visited web page and the
//! user's entire activity history. `CorsLayer::permissive()` (what this used
//! to be) sends `Access-Control-Allow-Origin: *`, which hands every site the
//! user visits read access to `/api/0/buckets`, `/api/0/export` and the events
//! underneath, plus write access to everything else.
//!
//! The policy mirrors aw-server-rust (endpoints/cors.rs + the
//! ExtensionCorsScope fairing added in #637):
//!
//!   * the service's own origin, so the bundled web UI works;
//!   * the aw-watcher-web Chrome extension, which has a fixed ID;
//!   * any Firefox extension, which cannot be pinned -- Mozilla randomises the
//!     ID per install to prevent fingerprinting, and host permissions do NOT
//!     exempt a background script from CORS, so dropping the wildcard would
//!     silently break aw-watcher-web.
//!
//! Because that last entry trusts every installed Firefox extension, origins
//! matched by the wildcard are additionally confined to the three endpoints
//! aw-watcher-web actually needs (see `extension_scope`). An extension that
//! declared no host permissions -- and so never showed the user a prompt
//! naming ActivityWatch -- can then report tab activity but cannot read any
//! of it back.

use axum::{
    extract::Request,
    http::{header::ORIGIN, HeaderValue, Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use tower_http::cors::{AllowOrigin, Any, CorsLayer};

/// aw-watcher-web's Chrome extension. Chrome IDs are stable, so this one is
/// pinned exactly rather than going through the wildcard.
const CHROME_WATCHER_WEB: &str = "chrome-extension://nglaklhklhcoonedhgnpgddginnjdadi";

/// Origins we cannot enumerate, and therefore confine in `extension_scope`.
fn is_wildcard_extension(origin: &str) -> bool {
    origin.starts_with("moz-extension://")
}

fn is_allowed_origin(origin: &str, ports: &[u16]) -> bool {
    origin == CHROME_WATCHER_WEB
        || is_wildcard_extension(origin)
        || ports.iter().any(|p| {
            origin == format!("http://127.0.0.1:{p}") || origin == format!("http://localhost:{p}")
        })
}

/// `ports` must be the ports actually bound, not `config.port()`: BIND_ADDR
/// carries its own port and does not update that field, so keying the
/// self-origin off the config default silently stops allowing the bundled web
/// UI whenever the two disagree.
pub fn cors_layer(ports: Vec<u16>) -> CorsLayer {
    CorsLayer::new()
        .allow_origin(AllowOrigin::predicate(move |origin: &HeaderValue, _| {
            origin
                .to_str()
                .map(|o| is_allowed_origin(o, &ports))
                .unwrap_or(false)
        }))
        .allow_methods([Method::GET, Method::POST, Method::DELETE])
        .allow_headers(Any)
        // Never true with an echoed origin, and aw-watcher-web does not use
        // cookies; stated explicitly so it is not quietly turned on later.
        .allow_credentials(false)
}

/// Is this (method, path) one of the three things aw-watcher-web needs?
///
/// Matched against the raw path, which is correct here: axum's router matches
/// raw too (`/%61pi/0/export` 404s rather than reaching the export handler),
/// so this gate cannot desynchronise from the handler it protects. That
/// differs from aw-server-rust, whose Rocket router decodes segments first and
/// therefore has to decode here as well.
fn is_watcher_web_endpoint(method: &Method, path: &str) -> bool {
    let seg: Vec<&str> = path.trim_matches('/').split('/').collect();
    match (method, seg.as_slice()) {
        // Server identity: what the watcher probes on startup.
        (&Method::GET, ["api", "0"]) | (&Method::GET, ["api", "0", "info"]) => true,
        // Create-or-update its own bucket.
        (&Method::POST, ["api", "0", "buckets", _]) => true,
        // Report activity.
        (&Method::POST, ["api", "0", "buckets", _, "heartbeat"]) => true,
        _ => false,
    }
}

/// Reject out-of-scope requests from wildcard-matched extension origins.
///
/// This must run OUTSIDE `cors_layer`, for two reasons. CORS alone only hides
/// the response from the caller: a "simple" POST is still delivered, so the
/// handler would run and the write would land even though the extension never
/// sees the reply. And a 403 produced inside the CORS layer would come back
/// wearing `Access-Control-Allow-Origin`, making it readable; produced outside,
/// it carries no CORS headers at all, so preflights fail too.
pub async fn extension_scope(req: Request, next: Next) -> Response {
    let from_wildcard_extension = req
        .headers()
        .get(ORIGIN)
        .and_then(|o| o.to_str().ok())
        .map(is_wildcard_extension)
        .unwrap_or(false);

    if from_wildcard_extension {
        // Preflight carries the real method in Access-Control-Request-Method.
        let effective = if req.method() == Method::OPTIONS {
            req.headers()
                .get("access-control-request-method")
                .and_then(|m| m.to_str().ok())
                .and_then(|m| m.parse::<Method>().ok())
                .unwrap_or(Method::OPTIONS)
        } else {
            req.method().clone()
        };

        if !is_watcher_web_endpoint(&effective, req.uri().path()) {
            return (StatusCode::FORBIDDEN, "origin not permitted for this endpoint")
                .into_response();
        }
    }

    next.run(req).await
}

