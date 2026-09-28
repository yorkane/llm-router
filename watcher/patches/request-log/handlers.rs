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

// ============================================================================
// Router-mode model endpoints for the llama.cpp webui: /_ui/v1/models,
// /_ui/models/{load,sse,unload}
//
// role:"router" in /props switches the UI to its multi-model picker, which
// reads the model list and streams status from these paths. Every model here
// is already served by a registered worker, so the picker is a switcher:
// load is an immediate success, the status stream stays silent, and
// unload refuses (unloading would take a whole instance out of the pool for
// everybody else - that is the watcher's job, not the chat UI's).
// ============================================================================

async fn ui_models(State(state): State<Arc<AppState>>) -> Response {
    let mut data: Vec<Value> = Vec::new();
    for worker in state.context.worker_registry.get_all() {
        for model in worker.models() {
            let id = &model.id;
            if data.iter().any(|m| m["id"].as_str() == Some(id.as_str())) {
                continue;
            }
            data.push(json!({
                "id": id,
                "object": "model",
                "created": 0,
                "owned_by": "llm-router",
                // the UI keys its picker (and whether it fetches per-model
                // /props) off this field; everything registered is serving.
                "status": {"value": "loaded"},
            }));
        }
    }
    Json(json!({"object": "list", "data": data})).into_response()
}

async fn ui_model_load() -> Response {
    Json(json!({"success": true})).into_response()
}

async fn ui_model_unload() -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({"error": {"message": "模型由 router 后的实例常驻提供，聊天界面不能卸载；要摘除请在 watcher / 服务侧操作"}})),
    )
        .into_response()
}

/// The stock UI polls this for load/unload progress. Nothing ever changes
/// behind a router, so the stream exists only to keep the UI from retrying it
/// every second - it never has to send a frame.
async fn ui_models_sse() -> Response {
    let stream = futures_util::stream::pending::<Result<Event, std::convert::Infallible>>();
    Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(30)).text("ping"))
        .into_response()
}

/// Public (no auth) routes for the Logs page. The page itself is a static asset
/// and the chat traffic it displays is already visible in the router's own
/// access log, so these endpoints stay unauthenticated like the /_ui assets.
///
/// /_ui/logs/backends feeds the Logs page's provider column (port + GPU):
/// the watcher registers workers with a `gpu` label when it knows which
/// device an instance sits on; workers registered without one report null.
async fn ui_logs_backends(State(state): State<Arc<AppState>>) -> Response {
    let mut out: Vec<Value> = Vec::new();
    for worker in state.context.worker_registry.get_all() {
        let gpu = worker
            .metadata()
            .labels
            .get("gpu")
            .cloned()
            .map(Value::String);
        for model in worker.models() {
            out.push(json!({
                "url": worker.url(),
                "model": model.id,
                "gpu": gpu.clone(),
            }));
        }
    }
    Json(json!({"backends": out})).into_response()
}

fn ui_logs_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/_ui/logs", get(ui_logs))
        .route("/_ui/stats", get(ui_stats))
        .route("/_ui/logs/stream", get(ui_logs_stream))
        .route("/_ui/logs/backends", get(ui_logs_backends))
}
