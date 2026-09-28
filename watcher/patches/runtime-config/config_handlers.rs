// ============================================================================
// Config-page API for the llama.cpp webui: /_ui/config
// Hot-mutable thinking effort (default + rewrite map) and per-model context
// caps; model renames are proxied to the llm-watcher. Same no-auth posture as
// the other /_ui routes (the deployments run on trusted LANs; the chat API
// aliases right next door are unauthenticated the same way).
// ============================================================================

async fn ui_config_get() -> Response {
    let store = crate::runtime_config::RuntimeConfigStore::install();
    Json(store.document().await).into_response()
}

async fn ui_config_effort(Json(body): Json<Value>) -> Response {
    let store = crate::runtime_config::RuntimeConfigStore::install();
    match store.apply_effort(&body) {
        Ok(_) => Json(store.document().await).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": e})),
        )
            .into_response(),
    }
}

/// One per-model context cap edit. ctx null (or absent) removes the cap.
#[derive(Debug, Default, serde::Deserialize)]
pub struct CtxPatch {
    pub model: String,
    #[serde(default)]
    pub ctx: Option<u64>,
}

async fn ui_config_ctx(Json(patch): Json<CtxPatch>) -> Response {
    let store = crate::runtime_config::RuntimeConfigStore::install();
    match store.apply_ctx(&patch.model, patch.ctx) {
        Ok(_) => Json(store.document().await).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": e})),
        )
            .into_response(),
    }
}

/// Rename request forwarded verbatim to the watcher control plane. The body is
/// either {"map":"orig:new,..."} or an object; the watcher re-registers the
/// owned workers on its next pass and persists the table in its ledger.
async fn ui_config_model_map(Json(body): Json<Value>) -> Response {
    let store = crate::runtime_config::RuntimeConfigStore::install();
    let Some(url) = store.watcher_url() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"ok": false, "error": "watcher not configured (set LMR_WATCHER_URL)"})),
        )
            .into_response();
    };
    match crate::runtime_config::proxy_model_map(&url, body).await {
        Ok(mut value) => {
            value["ok"] = json!(true);
            Json(value).into_response()
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({"ok": false, "error": e})),
        )
            .into_response(),
    }
}

fn ui_config_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/_ui/config", get(ui_config_get).post(ui_config_effort))
        .route("/_ui/config/effort", post(ui_config_effort))
        .route("/_ui/config/ctx", post(ui_config_ctx))
        .route("/_ui/config/model-map", post(ui_config_model_map))
}
