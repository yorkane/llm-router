// ============================================================================
// Logs-page API for the llama.cpp webui: /_ui/logs, /_ui/stats, /_ui/logs/stream
// ============================================================================

use axum::response::sse::{Event, KeepAlive, Sse};
use futures_util::StreamExt as _;
use tokio_stream::wrappers::BroadcastStream;

/// Query parameters for the paginated log endpoint.
#[derive(Debug, Default, serde::Deserialize)]
pub struct LogsQuery {
    /// Highest record sequence the caller has already seen; only newer rows are
    /// returned. Omit for a full pull of the buffer.
    pub cursor: Option<u64>,
    /// Maximum rows per response (the store clamps to 2000).
    pub limit: Option<usize>,
}

fn request_log_store() -> Option<&'static Arc<crate::observability::request_log::RequestLogStore>> {
    crate::observability::request_log::RequestLogStore::current()
}

fn request_log_disabled() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({"error": "request log not enabled"})),
    )
        .into_response()
}

/// GET /_ui/logs?cursor=&limit= - the ring buffer as JSON, oldest first.
async fn ui_logs(Query(query): Query<LogsQuery>) -> Response {
    let Some(store) = request_log_store() else {
        return request_log_disabled();
    };
    let (cursor, requests) = store.snapshot(query.cursor.unwrap_or(0), query.limit.unwrap_or(500));
    Json(json!({
        "cursor": cursor,
        "capacity": store.capacity(),
        "requests": requests,
    }))
    .into_response()
}

/// GET /_ui/stats - live counters for the summary strip: current concurrency,
/// aggregate input/output token rates over a sliding window, and price settings.
async fn ui_stats() -> Response {
    let Some(store) = request_log_store() else {
        return request_log_disabled();
    };
    Json(store.stats()).into_response()
}

/// GET /_ui/logs/stream - SSE fan-out of finished requests. A subscriber that
/// falls behind only loses frames (Lagged) and heals on its next cursor poll, so
/// the stream never needs to end on its own; the browser reconnects on drop.
async fn ui_logs_stream() -> Response {
    let Some(store) = request_log_store() else {
        return request_log_disabled();
    };
    let rx = store.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(|item| async move {
        match item {
            Ok(record) => Event::default()
                .json_data(record)
                .map(|event| Ok::<_, std::convert::Infallible>(event))
                .ok(),
            Err(_) => None, // Lagged: the UI re-syncs with a cursor poll.
        }
    });
    Sse::new(stream)
        .keep_alive(
            KeepAlive::new().interval(Duration::from_secs(15))
                .text("ping"),
        )
        .into_response()
}

/// Public (no auth) routes for the Logs page. The page itself is a static asset
/// and the chat traffic it displays is already visible in the router's own
/// access log, so these endpoints stay unauthenticated like the /_ui assets.
fn ui_logs_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/_ui/logs", get(ui_logs))
        .route("/_ui/stats", get(ui_stats))
        .route("/_ui/logs/stream", get(ui_logs_stream))
}

