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

