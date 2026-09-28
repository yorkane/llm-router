// ============================================================================
// Config-page API for the llama.cpp webui: /_ui/config
// Hot-mutable thinking effort (default + rewrite map) and per-model context
// caps; model renames are proxied to the llm-watcher. Same no-auth posture as
// the other /_ui routes (the deployments run on trusted LANs; the chat API
// aliases right next door are unauthenticated the same way).
// ============================================================================

/// Registered (model, worker url) pairs, used to key the per-model cards.
fn registered_models(state: &Arc<AppState>) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for worker in state.context.worker_registry.get_all() {
        for model in worker.models() {
            out.push((model.id.clone(), worker.url().to_string()));
        }
    }
    out
}

async fn ui_config_get(State(state): State<Arc<AppState>>) -> Response {
    let store = crate::runtime_config::RuntimeConfigStore::install();
    let mut doc = store.document().await;
    doc["models"] = store.models_document(registered_models(&state));
    Json(doc).into_response()
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

/// One per-model config card: {model, ctx, default_effort, effort_map,
/// modalities} where an absent field is left alone and null clears it, plus
/// {model, remove:true} to drop the card. The legacy /ctx endpoint still works.
async fn ui_config_model(State(state): State<Arc<AppState>>, Json(patch): Json<Value>) -> Response {
    let store = crate::runtime_config::RuntimeConfigStore::install();
    match store.apply_model_config(&patch) {
        Ok(_) => {
            let mut doc = store.document().await;
            doc["models"] = store.models_document(registered_models(&state));
            Json(doc).into_response()
        }
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": e})),
        )
            .into_response(),
    }
}

/// Whole-document replace for the Config page's JSON editor. Absent sections
/// are cleared (the JSON view is the source of truth); validation happens
/// before anything is written, so a typo cannot half-apply. An optional
/// model_map section is forwarded to the watcher so one paste can also rename.
async fn ui_config_apply(
    State(state): State<Arc<AppState>>,
    Json(patch): Json<Value>,
) -> Response {
    let store = crate::runtime_config::RuntimeConfigStore::install();
    let model_map = patch.get("model_map").cloned();
    let mut body = patch;
    if let Some(obj) = body.as_object_mut() {
        obj.remove("model_map");
    }
    let mut doc = match store.apply_document(&body) {
        Ok(_) => store.document().await,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": e})),
            )
                .into_response()
        }
    };
    let mut warning = None;
    if let Some(map) = model_map {
        let map = match map {
            Value::Object(_) => map,
            Value::String(s) => json!({"map": s}),
            _ => {
                warning = Some("model_map must be an object or an orig:new string".to_string());
                Value::Null
            }
        };
        if !map.is_null() {
            match store.watcher_url().as_deref() {
                None => warning = Some("watcher not configured; model_map not applied".to_string()),
                Some(url) => match crate::runtime_config::proxy_model_map(url, map).await {
                    Ok(v) => doc["watcher_model_map"] = v,
                    Err(e) => warning = Some(format!("model_map not applied: {}", e)),
                },
            }
        }
    }
    doc["models"] = store.models_document(registered_models(&state));
    if let Some(w) = warning {
        doc["warning"] = json!(w);
    }
    Json(doc).into_response()
}

fn ui_config_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/_ui/config", get(ui_config_get).post(ui_config_effort))
        .route("/_ui/config/effort", post(ui_config_effort))
        .route("/_ui/config/ctx", post(ui_config_ctx))
        .route("/_ui/config/model", post(ui_config_model))
        .route("/_ui/config/apply", post(ui_config_apply))
        .route("/_ui/config/model-map", post(ui_config_model_map))
}

