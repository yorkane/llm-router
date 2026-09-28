use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use axum::{
    extract::{Path, Query, Request, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{any, delete, get, post},
    Json, Router,
};
use rustls::crypto::ring;
use serde::Deserialize;
use serde_json::{json, Value};
use smg_mesh::{
    rate_limit_window::RateLimitWindow, MeshServerConfig, MeshServerHandler, MeshSyncManager,
};
use tokio::{signal, spawn};
use tracing::{debug, error, info, warn, Level};
use wfaas::LoggingSubscriber;

use crate::{
    app_context::AppContext,
    config::{RouterConfig, RoutingMode},
    core::{
        job_queue::{JobQueue, JobQueueConfig},
        steps::{TokenizerConfigRequest, WorkflowEngines},
        worker::{ConnectionMode, Worker, WorkerType},
        worker_manager::WorkerManager,
        Job,
    },
    middleware::{self, AuthConfig, QueuedRequest},
    observability::{
        logging::{self, LoggingConfig},
        metrics::{self, PrometheusConfig},
        otel_trace,
    },
    protocols::{
        chat::ChatCompletionRequest,
        classify::ClassifyRequest,
        completion::CompletionRequest,
        embedding::EmbeddingRequest,
        generate::GenerateRequest,
        parser::{ParseFunctionCallRequest, SeparateReasoningRequest},
        rerank::V1RerankReqInput,
        responses::{ResponsesGetParams, ResponsesRequest},
        tokenize::{AddTokenizerRequest, DetokenizeRequest, TokenizeRequest},
        validated::ValidatedJson,
        worker_spec::{WorkerConfigRequest, WorkerUpdateRequest},
    },
    routers::{
        conversations,
        mesh::{
            get_app_config, get_cluster_status, get_global_rate_limit, get_global_rate_limit_stats,
            get_mesh_health, get_policy_state, get_policy_states, get_worker_state,
            get_worker_states, set_global_rate_limit, trigger_graceful_shutdown, update_app_config,
        },
        parse,
        router_manager::RouterManager,
        tokenize, RouterTrait,
    },
    service_discovery::{start_service_discovery, ServiceDiscoveryConfig},
    tokenizer::TokenizerRegistry,
    wasm::route::{add_wasm_module, list_wasm_modules, remove_wasm_module},
};
#[derive(Clone)]
pub struct AppState {
    pub router: Arc<dyn RouterTrait>,
    pub context: Arc<AppContext>,
    pub concurrency_queue_tx: Option<tokio::sync::mpsc::Sender<QueuedRequest>>,
    pub router_manager: Option<Arc<RouterManager>>,
    pub mesh_handler: Option<Arc<MeshServerHandler>>,
    pub mesh_sync_manager: Option<Arc<MeshSyncManager>>,
}

async fn parse_function_call(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ParseFunctionCallRequest>,
) -> Response {
    parse::parse_function_call(&state.context, &req).await
}

async fn parse_reasoning(
    State(state): State<Arc<AppState>>,
    Json(req): Json<SeparateReasoningRequest>,
) -> Response {
    parse::parse_reasoning(&state.context, &req).await
}

async fn sink_handler() -> Response {
    StatusCode::NOT_FOUND.into_response()
}

/// llama.cpp webui (/_ui/) support: synthesize the /props document the UI
/// fetches for a model. Tries the backing worker's own /props first (real
/// llama.cpp servers answer it), otherwise returns a minimal document so the
/// UI can still select the model and chat through the router.
/// The webui hides the thinking-effort picker unless /props advertises a chat template
/// that mentions thinking knobs (`enable_thinking` / `reasoning_effort` / `<|im_start|>`).
/// Engines behind the router (ninfer, vllm, sglang) answer /props without
/// `chat_template` at all, so the probe always says "no thinking" and the picker never
/// renders. Effort here is forwarded as a top-level OpenAI `reasoning_effort` by the UI
/// patch (watcher/patch_ui_effort.sh) and rejected by the backend if unsupported, so the
/// capability question is the router's to answer: advertise it when the worker did not.
fn ui_props_with_thinking(mut value: Value) -> Value {
    if let Some(obj) = value.as_object_mut() {
        let advertised = obj
            .get("chat_template")
            .and_then(|v| v.as_str())
            .is_some_and(|s| s.contains("reasoning_effort") || s.contains("enable_thinking"));
        if !advertised {
            obj.insert(
                "chat_template".to_string(),
                Value::String("{% if reasoning_effort %}router-advertised{% endif %}".to_string()),
            );
        }
    }
    value
}

/// Tell the webui it is behind a router (role:"router"): the header then shows
/// the multi-model picker instead of the single-model view. No bundle patch is
/// needed: the picker's list/load/sse/unload requests go through the SvelteKit
/// base (which is /_ui when the page is served from /_ui/), so they land on the
/// aliases below as /_ui/v1/models and /_ui/models/....
///
/// Every model behind this router is already served by a registered worker, so
/// load/unload are no-ops that just confirm — the picker is a switcher, not a
/// loader — and /_ui/models/sse stays silent. Set LMR_UI_ROUTER_MODE=false to
/// go back to single-model presentation (e.g. for a llama.cpp instance whose
/// UI expects to load GGUF files itself).
fn ui_router_mode() -> bool {
    std::env::var("LMR_UI_ROUTER_MODE")
        .map(|v| {
            let v = v.trim().to_lowercase();
            !(v == "false" || v == "0" || v == "off")
        })
        .unwrap_or(true)
}

/// llama-ui drops image attachments silently (it filters out every image_url
/// part before sending) unless the model /props advertises modalities.vision.
/// vLLM and SGLang workers have no /props at all, so the fallback document
/// used to omit the field entirely and the router UI looked text-only even for
/// vision models (ninfer and llama.cpp do answer /props with modalities, which
/// is why 217.t worked). Engines still decide image support themselves, so
/// advertise vision whenever the worker did not report modalities; a text-only
/// model then fails loudly at the engine instead of the UI quietly swallowing
/// the attachment.
///
/// A Config-page capability override (per model) wins outright, including the
/// other direction: un-ticking 图片 makes the UI strip attachments client-side
/// instead of sending them to a model that cannot read them.
fn ui_props_with_modalities(mut value: Value, wanted: Option<&str>) -> Value {
    let Some(obj) = value.as_object_mut() else {
        return value;
    };
    let caps = wanted
        .map(|m| m.trim())
        .filter(|m| !m.is_empty())
        .and_then(|m| crate::runtime_config::RuntimeConfigStore::install().modalities_for(m));
    if let Some(caps) = caps {
        let has = |c: &str| caps.iter().any(|x| x == c);
        obj.insert(
            "modalities".to_string(),
            json!({
                "audio": has("audio"),
                "video": has("video"),
                "vision": has("image") || has("video"),
            }),
        );
        return value;
    }
    if !obj.contains_key("modalities") {
        obj.insert(
            "modalities".to_string(),
            json!({"audio": false, "video": false, "vision": true}),
        );
    }
    value
}

fn ui_props_with_role(mut value: Value) -> Value {
    if ui_router_mode() {
        if let Some(obj) = value.as_object_mut() {
            // Force it: a llama.cpp worker answering /props says role:"model",
            // but the *gateway* is the thing the UI is talking to.
            obj.insert("role".to_string(), Value::String("router".to_string()));
        }
    }
    value
}

/// Report the configured context cap instead of the worker's raw n_ctx when the
/// Config page set one, so the webui's context slider cannot offer more than
/// the router will actually forward (apply_ctx_cap clamps the request too).
fn ui_props_with_ctx(value: Value, wanted: Option<&str>) -> Value {
    let store = crate::runtime_config::RuntimeConfigStore::install();
    let Some(cap) = wanted.and_then(|m| store.ctx_cap(m)) else {
        return value;
    };
    let mut value = value;
    if let Some(obj) = value.as_object_mut() {
        obj.insert("n_ctx".to_string(), json!(cap));
        let n_ctx_train = obj
            .get("n_ctx_train")
            .and_then(|v| v.as_u64())
            .unwrap_or(cap);
        obj.insert(
            "n_ctx_train".to_string(),
            json!(n_ctx_train.max(cap)),
        );
    }
    value
}

async fn ui_props(state: &Arc<AppState>, wanted: Option<String>) -> Response {
    let cap_model = wanted.clone();
    // An alias asked for in the UI addresses the upstream model's props.
    let wanted: Option<String> = wanted.as_ref().map(|m| {
        crate::runtime_config::RuntimeConfigStore::install().resolve_model(m)
    });
    let candidates: Vec<Arc<dyn Worker>> = state
        .context
        .worker_registry
        .get_all()
        .into_iter()
        .filter(|w| matches!(w.connection_mode(), ConnectionMode::Http))
        .filter(|w| {
            wanted.is_none()
                || w.models()
                    .iter()
                    .any(|m| Some(&m.id) == wanted.as_ref())
        })
        .collect();

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .unwrap_or_default();
    for w in &candidates {
        let url = format!("{}/props", w.url().trim_end_matches('/'));
        let mut req = client.get(&url);
        if let Some(key) = w.api_key().as_deref() {
            req = req.bearer_auth(key);
        }
        if let Ok(resp) = req.send().await {
            if resp.status().is_success() {
                if let Ok(body) = resp.text().await {
                    if let Ok(value) = serde_json::from_str::<Value>(&body) {
                        return Json(ui_props_with_ctx(
                            ui_props_with_role(ui_props_with_modalities(
                                ui_props_with_thinking(value),
                                cap_model.as_deref(),
                            )),
                            cap_model.as_deref(),
                        ))
                        .into_response();
                    }
                    return (StatusCode::OK, [("content-type", "application/json")], body)
                        .into_response();
                }
            }
        }
    }

    let model_path = wanted.unwrap_or_else(|| {
        candidates
            .first()
            .and_then(|w| w.models().first().map(|m| m.id.clone()))
            .unwrap_or_else(|| "unknown".to_string())
    });
    Json(ui_props_with_ctx(
        ui_props_with_role(ui_props_with_modalities(
            ui_props_with_thinking(json!({
                "model_path": model_path,
                "model_alias": null,
                "webui_version": "llm-router",
            })),
            cap_model.as_deref(),
        )),
        cap_model.as_deref(),
    ))
    .into_response()
}

async fn v1_ui_props(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    ui_props(&state, params.get("model").cloned()).await
}

/// The webui polls /slots and /v1/streams/lookup to resume in-flight llama.cpp
/// server streams; the router does not own those, so report "nothing pending".
async fn v1_ui_empty() -> Response {
    Json(Vec::<Value>::new()).into_response()
}

/// llama.cpp 的 webui 在收到 /props 之前，首条消息可能不带 model 字段（或为空串），
/// IGW 对未知模型一律 503；这里从注册表取第一个 HTTP worker 的模型 id 兜底填充。
fn default_ui_model(state: &Arc<AppState>) -> Option<String> {
    state
        .context
        .worker_registry
        .get_all()
        .into_iter()
        .filter(|w| matches!(w.connection_mode(), ConnectionMode::Http))
        .find_map(|w| w.models().first().map(|m| m.id.clone()))
}

fn clean_ui_effort(value: &mut Value) {
    if let Some(obj) = value.as_object_mut() {
        match obj.get("reasoning_effort") {
            Some(Value::String(s)) if s.is_empty() => {
                obj.remove("reasoning_effort");
            }
            Some(Value::Null) => {
                obj.remove("reasoning_effort");
            }
            _ => {}
        }
    }
}

fn fill_default_model(state: &Arc<AppState>, value: &mut Value) {
    let missing = match value.get("model") {
        None | Some(Value::Null) => true,
        Some(Value::String(s)) => s.is_empty(),
        _ => false,
    };
    if missing {
        if let (Some(obj), Some(model)) = (value.as_object_mut(), default_ui_model(state)) {
            obj.insert("model".to_string(), Value::String(model));
        }
    }
}

async fn v1_ui_chat_completions(
    State(state): State<Arc<AppState>>,
    headers: http::HeaderMap,
    Json(mut value): Json<Value>,
) -> Response {
    fill_default_model(&state, &mut value);
    clean_ui_effort(&mut value);
    let req: ChatCompletionRequest = match serde_json::from_value(value) {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": {"message": format!("invalid chat request: {e}")}})),
            )
                .into_response();
        }
    };
    state
        .router
        .route_chat(Some(&headers), &req, Some(&req.model))
        .await
}

async fn v1_ui_completions(
    State(state): State<Arc<AppState>>,
    headers: http::HeaderMap,
    Json(mut value): Json<Value>,
) -> Response {
    fill_default_model(&state, &mut value);
    clean_ui_effort(&mut value);
    let req: CompletionRequest = match serde_json::from_value(value) {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": {"message": format!("invalid completion request: {e}")}})),
            )
                .into_response();
        }
    };
    state
        .router
        .route_completion(Some(&headers), &req, Some(&req.model))
        .await
}

async fn v1_ui_unsupported() -> Response {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(json!({"error": "llama.cpp server stream/control not available through the router"})),
    )
        .into_response()
}

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
    let virtual_aliases = crate::runtime_config::RuntimeConfigStore::install().virtual_models_list();
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
    for (alias, target) in virtual_aliases {
        if data.iter().any(|m| m["id"].as_str() == Some(alias.as_str())) {
            continue;
        }
        data.push(json!({
            "id": alias,
            "object": "model",
            "created": 0,
            "owned_by": format!("llm-router->{}", target),
            "status": {"value": "loaded"},
        }));
    }
    data.sort_by(|a, b| a["id"].as_str().unwrap_or("").cmp(b["id"].as_str().unwrap_or("")));
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

/// Whole-list replace of the virtual model table: {entries: [{model, target}]}.
/// Aliases are advertised in /v1/models next to the real models and resolve to
/// their target at routing time.
async fn ui_config_virtual(Json(body): Json<Value>) -> Response {
    let store = crate::runtime_config::RuntimeConfigStore::install();
    let entries = body.get("entries").cloned().unwrap_or_else(|| json!([]));
    match store.apply_virtual_models(&entries) {
        Ok(_) => Json(store.document().await).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response(),
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
        .route("/_ui/config/virtual", post(ui_config_virtual))
        .route("/_ui/config/apply", post(ui_config_apply))
        .route("/_ui/config/model-map", post(ui_config_model_map))
}

async fn liveness() -> Response {
    (StatusCode::OK, "OK").into_response()
}

async fn readiness(State(state): State<Arc<AppState>>) -> Response {
    let workers = state.context.worker_registry.get_all();
    let healthy_workers: Vec<_> = workers.iter().filter(|w| w.is_healthy()).collect();

    let is_ready = if state.context.router_config.enable_igw {
        !healthy_workers.is_empty()
    } else {
        match &state.context.router_config.mode {
            RoutingMode::PrefillDecode { .. } => {
                let has_prefill = healthy_workers
                    .iter()
                    .any(|w| matches!(w.worker_type(), WorkerType::Prefill { .. }));
                let has_decode = healthy_workers
                    .iter()
                    .any(|w| matches!(w.worker_type(), WorkerType::Decode));
                has_prefill && has_decode
            }
            RoutingMode::Regular { .. } => !healthy_workers.is_empty(),
            RoutingMode::OpenAI { .. } => !healthy_workers.is_empty(),
        }
    };

    if is_ready {
        (
            StatusCode::OK,
            Json(json!({
                "status": "ready",
                "healthy_workers": healthy_workers.len(),
                "total_workers": workers.len()
            })),
        )
            .into_response()
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "status": "not ready",
                "reason": "insufficient healthy workers"
            })),
        )
            .into_response()
    }
}

async fn health(_state: State<Arc<AppState>>) -> Response {
    liveness().await
}

async fn health_generate(State(state): State<Arc<AppState>>, req: Request) -> Response {
    state.router.health_generate(req).await
}

async fn engine_metrics(State(state): State<Arc<AppState>>) -> Response {
    WorkerManager::get_engine_metrics(&state.context.worker_registry, &state.context.client)
        .await
        .into_response()
}

async fn get_server_info(State(state): State<Arc<AppState>>, req: Request) -> Response {
    state.router.get_server_info(req).await
}

async fn v1_models(State(state): State<Arc<AppState>>, req: Request) -> Response {
    let response = state.router.get_models(req).await;
    inject_virtual_models(response).await
}

/// Advertise the virtual model aliases next to the real ones: discovery
/// (/v1/models) must show both so clients can pick an alias, and the aliases
/// stay live as long as their target is registered.
async fn inject_virtual_models(response: Response) -> Response {
    let store = crate::runtime_config::RuntimeConfigStore::install();
    let aliases = store.virtual_models_list();
    if aliases.is_empty() {
        return response;
    }
    let (parts, body) = response.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, 4 * 1024 * 1024).await else {
        return Response::from_parts(parts, axum::body::Body::empty());
    };
    let Ok(mut value) = serde_json::from_slice::<Value>(&bytes) else {
        return Response::from_parts(parts, axum::body::Body::from(bytes));
    };
    if let Some(list) = value.get_mut("data").and_then(|v| v.as_array_mut()) {
        for (alias, target) in aliases {
            if list
                .iter()
                .any(|m| m.get("id").and_then(|v| v.as_str()) == Some(alias.as_str()))
            {
                continue; // a real worker already serves this name
            }
            list.push(json!({
                "id": alias,
                "object": "model",
                "created": 0,
                "owned_by": format!("llm-router->{}", target),
            }));
        }
        list.sort_by(|a, b| {
            a.get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .cmp(b.get("id").and_then(|v| v.as_str()).unwrap_or(""))
        });
        return (parts.status, parts.headers, Json(value)).into_response();
    }
    Response::from_parts(parts, axum::body::Body::from(bytes))
}

async fn get_model_info(State(state): State<Arc<AppState>>, req: Request) -> Response {
    state.router.get_model_info(req).await
}

async fn generate(
    State(state): State<Arc<AppState>>,
    headers: http::HeaderMap,
    Json(body): Json<GenerateRequest>,
) -> Response {
    let model_id = body.model.as_deref();
    state
        .router
        .route_generate(Some(&headers), &body, model_id)
        .await
}

async fn v1_chat_completions(
    State(state): State<Arc<AppState>>,
    headers: http::HeaderMap,
    ValidatedJson(body): ValidatedJson<ChatCompletionRequest>,
) -> Response {
    state
        .router
        .route_chat(Some(&headers), &body, Some(&body.model))
        .await
}

async fn v1_completions(
    State(state): State<Arc<AppState>>,
    headers: http::HeaderMap,
    Json(body): Json<CompletionRequest>,
) -> Response {
    state
        .router
        .route_completion(Some(&headers), &body, Some(&body.model))
        .await
}

async fn v1_rerank(
    State(state): State<Arc<AppState>>,
    headers: http::HeaderMap,
    Json(body): Json<V1RerankReqInput>,
) -> Response {
    let rerank_body = &body.into();
    state
        .router
        .route_rerank(Some(&headers), rerank_body, Some(&rerank_body.model))
        .await
}

async fn v1_responses(
    State(state): State<Arc<AppState>>,
    headers: http::HeaderMap,
    ValidatedJson(body): ValidatedJson<ResponsesRequest>,
) -> Response {
    state
        .router
        .route_responses(Some(&headers), &body, Some(&body.model))
        .await
}

async fn v1_embeddings(
    State(state): State<Arc<AppState>>,
    headers: http::HeaderMap,
    Json(body): Json<EmbeddingRequest>,
) -> Response {
    state
        .router
        .route_embeddings(Some(&headers), &body, Some(&body.model))
        .await
}

async fn v1_classify(
    State(state): State<Arc<AppState>>,
    headers: http::HeaderMap,
    Json(body): Json<ClassifyRequest>,
) -> Response {
    state
        .router
        .route_classify(Some(&headers), &body, Some(&body.model))
        .await
}

async fn v1_responses_get(
    State(state): State<Arc<AppState>>,
    Path(response_id): Path<String>,
    headers: http::HeaderMap,
    Query(params): Query<ResponsesGetParams>,
) -> Response {
    state
        .router
        .get_response(Some(&headers), &response_id, &params)
        .await
}

async fn v1_responses_cancel(
    State(state): State<Arc<AppState>>,
    Path(response_id): Path<String>,
    headers: http::HeaderMap,
) -> Response {
    state
        .router
        .cancel_response(Some(&headers), &response_id)
        .await
}

async fn v1_responses_delete(
    State(state): State<Arc<AppState>>,
    Path(response_id): Path<String>,
    headers: http::HeaderMap,
) -> Response {
    state
        .router
        .delete_response(Some(&headers), &response_id)
        .await
}

async fn v1_responses_list_input_items(
    State(state): State<Arc<AppState>>,
    Path(response_id): Path<String>,
    headers: http::HeaderMap,
) -> Response {
    state
        .router
        .list_response_input_items(Some(&headers), &response_id)
        .await
}

async fn v1_conversations_create(
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Response {
    conversations::create_conversation(&state.context.conversation_storage, body).await
}

async fn v1_conversations_get(
    State(state): State<Arc<AppState>>,
    Path(conversation_id): Path<String>,
) -> Response {
    conversations::get_conversation(&state.context.conversation_storage, &conversation_id).await
}

async fn v1_conversations_update(
    State(state): State<Arc<AppState>>,
    Path(conversation_id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    conversations::update_conversation(&state.context.conversation_storage, &conversation_id, body)
        .await
}

async fn v1_conversations_delete(
    State(state): State<Arc<AppState>>,
    Path(conversation_id): Path<String>,
) -> Response {
    conversations::delete_conversation(&state.context.conversation_storage, &conversation_id).await
}

#[derive(Deserialize, Default)]
struct ListItemsQuery {
    limit: Option<usize>,
    order: Option<String>,
    after: Option<String>,
}

async fn v1_conversations_list_items(
    State(state): State<Arc<AppState>>,
    Path(conversation_id): Path<String>,
    Query(ListItemsQuery {
        limit,
        order,
        after,
    }): Query<ListItemsQuery>,
) -> Response {
    conversations::list_conversation_items(
        &state.context.conversation_storage,
        &state.context.conversation_item_storage,
        &conversation_id,
        limit,
        order.as_deref(),
        after.as_deref(),
    )
    .await
}

#[derive(Deserialize, Default)]
struct GetItemQuery {
    /// Additional fields to include in response (not yet implemented)
    include: Option<Vec<String>>,
}

async fn v1_conversations_create_items(
    State(state): State<Arc<AppState>>,
    Path(conversation_id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    conversations::create_conversation_items(
        &state.context.conversation_storage,
        &state.context.conversation_item_storage,
        &conversation_id,
        body,
    )
    .await
}

async fn v1_conversations_get_item(
    State(state): State<Arc<AppState>>,
    Path((conversation_id, item_id)): Path<(String, String)>,
    Query(query): Query<GetItemQuery>,
) -> Response {
    conversations::get_conversation_item(
        &state.context.conversation_storage,
        &state.context.conversation_item_storage,
        &conversation_id,
        &item_id,
        query.include,
    )
    .await
}

async fn v1_conversations_delete_item(
    State(state): State<Arc<AppState>>,
    Path((conversation_id, item_id)): Path<(String, String)>,
) -> Response {
    conversations::delete_conversation_item(
        &state.context.conversation_storage,
        &state.context.conversation_item_storage,
        &conversation_id,
        &item_id,
    )
    .await
}

async fn flush_cache(State(state): State<Arc<AppState>>, _req: Request) -> Response {
    WorkerManager::flush_cache_all(&state.context.worker_registry, &state.context.client)
        .await
        .into_response()
}

async fn get_loads(State(state): State<Arc<AppState>>, _req: Request) -> Response {
    WorkerManager::get_all_worker_loads(&state.context.worker_registry, &state.context.client)
        .await
        .into_response()
}

async fn create_worker(
    State(state): State<Arc<AppState>>,
    Json(config): Json<WorkerConfigRequest>,
) -> Response {
    match state.context.worker_service.create_worker(config).await {
        Ok(result) => result.into_response(),
        Err(err) => err.into_response(),
    }
}

async fn list_workers_rest(State(state): State<Arc<AppState>>) -> Response {
    state.context.worker_service.list_workers().into_response()
}

async fn get_worker(
    State(state): State<Arc<AppState>>,
    Path(worker_id_raw): Path<String>,
) -> Response {
    match state.context.worker_service.get_worker(&worker_id_raw) {
        Ok(result) => result.into_response(),
        Err(err) => err.into_response(),
    }
}

async fn delete_worker(
    State(state): State<Arc<AppState>>,
    Path(worker_id_raw): Path<String>,
) -> Response {
    match state
        .context
        .worker_service
        .delete_worker(&worker_id_raw)
        .await
    {
        Ok(result) => result.into_response(),
        Err(err) => err.into_response(),
    }
}

async fn update_worker(
    State(state): State<Arc<AppState>>,
    Path(worker_id_raw): Path<String>,
    Json(update): Json<WorkerUpdateRequest>,
) -> Response {
    match state
        .context
        .worker_service
        .update_worker(&worker_id_raw, update)
        .await
    {
        Ok(result) => result.into_response(),
        Err(err) => err.into_response(),
    }
}

// ============================================================================
// Tokenize / Detokenize Handlers
// ============================================================================

async fn v1_tokenize(
    State(state): State<Arc<AppState>>,
    Json(request): Json<TokenizeRequest>,
) -> Response {
    tokenize::tokenize(&state.context.tokenizer_registry, request).await
}

async fn v1_detokenize(
    State(state): State<Arc<AppState>>,
    Json(request): Json<DetokenizeRequest>,
) -> Response {
    tokenize::detokenize(&state.context.tokenizer_registry, request).await
}

async fn v1_tokenizers_add(
    State(state): State<Arc<AppState>>,
    Json(request): Json<AddTokenizerRequest>,
) -> Response {
    tokenize::add_tokenizer(&state.context, request).await
}

async fn v1_tokenizers_list(State(state): State<Arc<AppState>>) -> Response {
    tokenize::list_tokenizers(&state.context.tokenizer_registry).await
}

async fn v1_tokenizers_get(
    State(state): State<Arc<AppState>>,
    Path(tokenizer_id): Path<String>,
) -> Response {
    tokenize::get_tokenizer_info(&state.context, &tokenizer_id).await
}

async fn v1_tokenizers_status(
    State(state): State<Arc<AppState>>,
    Path(tokenizer_id): Path<String>,
) -> Response {
    tokenize::get_tokenizer_status(&state.context, &tokenizer_id).await
}

async fn v1_tokenizers_remove(
    State(state): State<Arc<AppState>>,
    Path(tokenizer_id): Path<String>,
) -> Response {
    tokenize::remove_tokenizer(&state.context, &tokenizer_id).await
}

pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    pub router_config: RouterConfig,
    pub max_payload_size: usize,
    pub log_dir: Option<String>,
    pub log_level: Option<String>,
    pub json_log: bool,
    pub service_discovery_config: Option<ServiceDiscoveryConfig>,
    pub prometheus_config: Option<PrometheusConfig>,
    pub request_timeout_secs: u64,
    pub request_id_headers: Option<Vec<String>>,
    pub shutdown_grace_period_secs: u64,
    /// Control plane authentication configuration
    pub control_plane_auth: Option<crate::auth::ControlPlaneAuthConfig>,
    pub mesh_server_config: Option<MeshServerConfig>,
    /// Directory holding the unpacked llama.cpp webui assets; when set, they
    /// are served under /_ui/ together with the API aliases the UI needs.
    pub ui_dir: Option<String>,
}

/// API aliases for the llama.cpp webui served under /_ui/.
///
/// The bundled UI is patched (watcher/patch_ui.sh) to call /_ui/v1/... and
/// /_ui/props; map those back onto the regular handlers so the UI chats
/// through the same router pipeline (auth, body limit, metrics included).
/// Stream resume/control have no router-side equivalent and answer honestly.
fn ui_api_routes(auth_config: AuthConfig) -> Router<Arc<AppState>> {
    Router::new()
        .route("/_ui/v1/chat/completions", post(v1_ui_chat_completions))
        .route("/_ui/v1/completions", post(v1_ui_completions))
        // Router-mode picker (role:"router" in /props). The SvelteKit base is
        // /_ui, so the bundle's "/v1/models" and "/models/{load,sse,unload}"
        // literals already arrive under /_ui/... — no bundle patch needed.
        .route("/_ui/v1/models", get(ui_models))
        .route("/_ui/models/load", post(ui_model_load))
        .route("/_ui/models/unload", post(ui_model_unload))
        .route("/_ui/models/sse", get(ui_models_sse))
        .route("/_ui/props", get(v1_ui_props))
        .route("/_ui/slots", any(v1_ui_empty))
        .route("/_ui/v1/streams/lookup", any(v1_ui_empty))
        .route("/_ui/tools", any(v1_ui_empty))
        .route("/_ui/v1/chat/completions/control", any(v1_ui_unsupported))
        .route("/_ui/v1/stream", any(v1_ui_unsupported))
        .route_layer(axum::middleware::from_fn_with_state(
            auth_config,
            middleware::auth_middleware,
        ))
}

pub fn build_app(
    app_state: Arc<AppState>,
    auth_config: AuthConfig,
    control_plane_auth_state: Option<crate::auth::ControlPlaneAuthState>,
    max_payload_size: usize,
    request_id_headers: Vec<String>,
    cors_allowed_origins: Vec<String>,
) -> Router {
    let protected_routes = Router::new()
        .route("/generate", post(generate))
        .route("/v1/chat/completions", post(v1_chat_completions))
        .route("/v1/completions", post(v1_completions))
        .route("/v1/rerank", post(v1_rerank))
        .route("/v1/responses", post(v1_responses))
        .route("/v1/embeddings", post(v1_embeddings))
        .route("/v1/classify", post(v1_classify))
        .route("/v1/responses/{response_id}", get(v1_responses_get))
        .route(
            "/v1/responses/{response_id}/cancel",
            post(v1_responses_cancel),
        )
        .route("/v1/responses/{response_id}", delete(v1_responses_delete))
        .route(
            "/v1/responses/{response_id}/input_items",
            get(v1_responses_list_input_items),
        )
        .route("/v1/conversations", post(v1_conversations_create))
        .route(
            "/v1/conversations/{conversation_id}",
            get(v1_conversations_get)
                .post(v1_conversations_update)
                .delete(v1_conversations_delete),
        )
        .route(
            "/v1/conversations/{conversation_id}/items",
            get(v1_conversations_list_items).post(v1_conversations_create_items),
        )
        .route(
            "/v1/conversations/{conversation_id}/items/{item_id}",
            get(v1_conversations_get_item).delete(v1_conversations_delete_item),
        )
        // Tokenize / Detokenize endpoints
        .route("/v1/tokenize", post(v1_tokenize))
        .route("/v1/detokenize", post(v1_detokenize))
        .route_layer(axum::middleware::from_fn_with_state(
            app_state.clone(),
            middleware::concurrency_limit_middleware,
        ))
        .route_layer(axum::middleware::from_fn_with_state(
            auth_config.clone(),
            middleware::auth_middleware,
        ))
        .route_layer(axum::middleware::from_fn_with_state(
            app_state.clone(),
            middleware::wasm_middleware,
        ));

    let public_routes = Router::new()
        .route("/liveness", get(liveness))
        .route("/readiness", get(readiness))
        .route("/health", get(health))
        .route("/health_generate", get(health_generate))
        .route("/engine_metrics", get(engine_metrics))
        .route("/v1/models", get(v1_models))
        .route("/model_info", get(get_model_info))
        // TODO: Remove `/get_model_info` alias after one release-cycle deprecation window.
        .route("/get_model_info", get(get_model_info))
        .route("/server_info", get(get_server_info))
        // TODO: Remove `/get_server_info` alias after one release-cycle deprecation window.
        .route("/get_server_info", get(get_server_info));

    // Build admin routes with control plane auth if configured, otherwise use simple API key auth
    let admin_routes = Router::new()
        .route("/flush_cache", post(flush_cache))
        .route("/v1/loads", get(get_loads))
        // TODO: Remove `/get_loads` alias after one release-cycle deprecation window.
        .route("/get_loads", get(get_loads))
        .route("/parse/function_call", post(parse_function_call))
        .route("/parse/reasoning", post(parse_reasoning))
        .route("/wasm", post(add_wasm_module))
        .route("/wasm/{module_uuid}", delete(remove_wasm_module))
        .route("/wasm", get(list_wasm_modules))
        // Tokenizer management endpoints
        .route(
            "/v1/tokenizers",
            post(v1_tokenizers_add).get(v1_tokenizers_list),
        )
        .route(
            "/v1/tokenizers/{tokenizer_id}",
            get(v1_tokenizers_get).delete(v1_tokenizers_remove),
        )
        .route(
            "/v1/tokenizers/{tokenizer_id}/status",
            get(v1_tokenizers_status),
        );

    // Build worker routes
    let worker_routes = Router::new()
        .route("/workers", post(create_worker).get(list_workers_rest))
        .route(
            "/workers/{worker_id}",
            get(get_worker).put(update_worker).delete(delete_worker),
        );

    // Apply authentication middleware to control plane routes
    let apply_control_plane_auth = |routes: Router<Arc<AppState>>| {
        if let Some(ref cp_state) = control_plane_auth_state {
            routes.route_layer(axum::middleware::from_fn_with_state(
                cp_state.clone(),
                crate::auth::control_plane_auth_middleware,
            ))
        } else {
            routes.route_layer(axum::middleware::from_fn_with_state(
                auth_config.clone(),
                middleware::auth_middleware,
            ))
        }
    };
    let admin_routes = apply_control_plane_auth(admin_routes);
    let worker_routes = apply_control_plane_auth(worker_routes);

    // HA management routes
    let mesh_routes = Router::new()
        .route("/ha/status", get(get_cluster_status))
        .route("/ha/health", get(get_mesh_health))
        .route("/ha/workers", get(get_worker_states))
        .route("/ha/workers/{worker_id}", get(get_worker_state))
        .route("/ha/policies", get(get_policy_states))
        .route("/ha/policies/{model_id}", get(get_policy_state))
        .route("/ha/config/{key}", get(get_app_config))
        .route("/ha/config", post(update_app_config))
        .route("/ha/rate-limit", post(set_global_rate_limit))
        .route("/ha/rate-limit", get(get_global_rate_limit))
        .route("/ha/rate-limit/stats", get(get_global_rate_limit_stats))
        .route("/ha/shutdown", post(trigger_graceful_shutdown))
        .route_layer(axum::middleware::from_fn_with_state(
            auth_config.clone(),
            middleware::auth_middleware,
        ));

    Router::new()
        .merge(protected_routes)
        .merge(public_routes)
        .merge(ui_api_routes(auth_config.clone()))
        .merge(ui_logs_routes())
        .merge(ui_config_routes())
        .merge(admin_routes)
        .merge(worker_routes)
        .merge(mesh_routes)
        .layer(axum::extract::DefaultBodyLimit::max(max_payload_size))
        .layer(tower_http::limit::RequestBodyLimitLayer::new(
            max_payload_size,
        ))
        .layer(middleware::create_logging_layer())
        .layer(middleware::HttpMetricsLayer::new(
            app_state.context.inflight_tracker.clone(),
        ))
        .layer(middleware::RequestIdLayer::new(request_id_headers))
        .layer(middleware::RequestLogLayer)
        .layer(create_cors_layer(cors_allowed_origins))
        .fallback(sink_handler)
        .with_state(app_state)
}

pub async fn startup(config: ServerConfig) -> Result<(), Box<dyn std::error::Error>> {
    static LOGGING_INITIALIZED: AtomicBool = AtomicBool::new(false);

    if let Some(trace_config) = &config.router_config.trace_config {
        otel_trace::otel_tracing_init(
            trace_config.enable_trace,
            Some(&trace_config.otlp_traces_endpoint),
        )?;
    }

    let _log_guard = if !LOGGING_INITIALIZED.swap(true, Ordering::SeqCst) {
        Some(logging::init_logging(
            LoggingConfig {
                level: config
                    .log_level
                    .as_deref()
                    .and_then(|s| match s.to_uppercase().parse::<Level>() {
                        Ok(l) => Some(l),
                        Err(_) => {
                            warn!("Invalid log level string: '{s}'. Defaulting to INFO.");
                            None
                        }
                    })
                    .unwrap_or(Level::INFO),
                json_format: config.json_log,
                log_dir: config.log_dir.clone(),
                colorize: true,
                log_file_name: "smg".to_string(),
                log_targets: None,
            },
            config.router_config.trace_config.clone(),
        ))
    } else {
        None
    };

    if let Some(prometheus_config) = &config.prometheus_config {
        metrics::start_prometheus(prometheus_config.clone());
    }

    // In-memory request log for the /_ui/ Logs page: a ring buffer plus sliding
    // token-rate windows, nothing ever hits disk. LMR_REQUEST_LOG_CAPACITY=0
    // keeps the feature entirely off.
    {
        use crate::observability::request_log::RequestLogStore;
        let capacity = std::env::var("LMR_REQUEST_LOG_CAPACITY")
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .unwrap_or(crate::observability::request_log::DEFAULT_CAPACITY);
        if capacity > 0 {
            RequestLogStore::install(RequestLogStore::new(capacity));
            info!("request log enabled: in-memory ring of {} records (GET /_ui/logs)", capacity);
        }
    }

    let (mesh_handler, mesh_sync_manager) = if let Some(mesh_server_config) =
        &config.mesh_server_config
    {
        // Create HA sync manager with stores first
        use smg_mesh::{partition::PartitionDetector, stores::StateStores, sync::MeshSyncManager};
        let stores = Arc::new(StateStores::with_self_name(
            mesh_server_config.self_name.clone(),
        ));
        let sync_manager = Arc::new(MeshSyncManager::new(
            stores.clone(),
            mesh_server_config.self_name.clone(),
        ));

        // Create partition detector
        let partition_detector = Arc::new(PartitionDetector::default());

        // Initialize rate-limit hash ring with current membership
        sync_manager.update_rate_limit_membership();

        // Start rate limit window reset task
        let window_manager = RateLimitWindow::new(sync_manager.clone(), 1); // Reset every 1 second
        spawn(async move {
            window_manager.start_reset_task().await;
        });

        // Create mesh server builder and build with stores
        use smg_mesh::service::MeshServerBuilder;
        let builder = MeshServerBuilder::new(
            mesh_server_config.self_name.clone(),
            mesh_server_config.self_addr,
            mesh_server_config.init_peer,
        );
        let (mesh_server, handler) = builder.build_with_stores(Some(stores.clone()));

        // Spawn the mesh server with stores and partition detector
        let stores_for_server = stores.clone();
        let sync_manager_for_server = sync_manager.clone();
        let partition_detector_for_server = partition_detector.clone();
        spawn(async move {
            if let Err(e) = mesh_server
                .start_serve_with_stores(
                    Some(stores_for_server),
                    Some(sync_manager_for_server),
                    Some(partition_detector_for_server),
                )
                .await
            {
                tracing::error!("Mesh server failed: {}", e);
            }
        });

        (Some(Arc::new(handler)), Some(sync_manager))
    } else {
        (None, None)
    };

    info!(
        "Starting router on {}:{} | mode: {:?} | policy: {:?} | max_payload: {}MB",
        config.host,
        config.port,
        config.router_config.mode,
        config.router_config.policy,
        config.max_payload_size / (1024 * 1024)
    );

    let app_context = Arc::new(
        AppContext::from_config(config.router_config.clone(), config.request_timeout_secs).await?,
    );

    if config.prometheus_config.is_some() {
        app_context.inflight_tracker.start_sampler(20);
    }

    let weak_context = Arc::downgrade(&app_context);
    let worker_job_queue = JobQueue::new(JobQueueConfig::default(), weak_context);
    app_context
        .worker_job_queue
        .set(worker_job_queue)
        .expect("JobQueue should only be initialized once");

    // Initialize typed workflow engines
    let engines = WorkflowEngines::new(&config.router_config);

    // Subscribe logging to all workflow engines
    engines.subscribe_all(Arc::new(LoggingSubscriber)).await;

    app_context
        .workflow_engines
        .set(engines)
        .expect("WorkflowEngines should only be initialized once");
    debug!(
        "Workflow engines initialized (health check timeout: {}s)",
        config.router_config.health_check.timeout_secs
    );

    // Submit startup tokenizer job if tokenizer path is configured
    // This runs before worker initialization to ensure tokenizer is available
    if let Some(tokenizer_source) = config
        .router_config
        .tokenizer_path
        .as_ref()
        .or(config.router_config.model_path.as_ref())
    {
        info!("Loading startup tokenizer from: {}", tokenizer_source);

        let job_queue = app_context
            .worker_job_queue
            .get()
            .expect("JobQueue should be initialized");

        let tokenizer_config = TokenizerConfigRequest {
            id: TokenizerRegistry::generate_id(),
            name: tokenizer_source.clone(),
            source: tokenizer_source.clone(),
            chat_template_path: config.router_config.chat_template.clone(),
            cache_config: config.router_config.tokenizer_cache.to_option(),
            fail_on_duplicate: false,
        };

        let job = Job::AddTokenizer {
            config: Box::new(tokenizer_config),
        };

        job_queue
            .submit(job)
            .await
            .map_err(|e| format!("Failed to submit startup tokenizer job: {}", e))?;

        info!("Startup tokenizer job submitted (will complete in background)");
    }

    info!(
        "Initializing workers for routing mode: {:?}",
        config.router_config.mode
    );

    // Submit worker initialization job to queue
    let job_queue = app_context
        .worker_job_queue
        .get()
        .expect("JobQueue should be initialized");
    let job = Job::InitializeWorkersFromConfig {
        router_config: Box::new(config.router_config.clone()),
    };
    job_queue
        .submit(job)
        .await
        .map_err(|e| format!("Failed to submit worker initialization job: {}", e))?;

    info!("Worker initialization job submitted (will complete in background)");

    if let Some(mcp_config) = &config.router_config.mcp_config {
        info!("Found {} MCP server(s) in config", mcp_config.servers.len());
        let mcp_job = Job::InitializeMcpServers {
            mcp_config: Box::new(mcp_config.clone()),
        };
        job_queue
            .submit(mcp_job)
            .await
            .map_err(|e| format!("Failed to submit MCP initialization job: {}", e))?;
    } else {
        info!("No MCP config provided, skipping MCP server initialization");
    }

    // Start background refresh for ALL MCP servers (static + dynamic in LRU cache)
    if let Some(mcp_manager) = app_context.mcp_manager.get() {
        let refresh_interval = Duration::from_secs(600); // 10 minutes
        let _refresh_handle =
            Arc::clone(mcp_manager).spawn_background_refresh_all(refresh_interval);
        debug!("Started background refresh for all MCP servers (every 10 minutes)");
    }

    let worker_stats = app_context.worker_registry.stats();
    info!(
        "Workers initialized: {} total, {} healthy",
        worker_stats.total_workers, worker_stats.healthy_workers
    );

    let router_manager = RouterManager::from_config(&config, &app_context).await?;
    let router: Arc<dyn RouterTrait> = router_manager.clone();

    if !config.router_config.health_check.disable_health_check {
        let _health_checker = app_context
            .worker_registry
            .start_health_checker(config.router_config.health_check.check_interval_secs);
        debug!(
            "Started health checker for workers with {}s interval",
            config.router_config.health_check.check_interval_secs
        );
    } else {
        info!("Global health checks disabled via CLI/config; skipping health checker");
    }

    if let Some(ref load_monitor) = app_context.load_monitor {
        load_monitor.start().await;
        debug!("Started LoadMonitor for PowerOfTwo policies");
    }

    let (limiter, processor) = middleware::ConcurrencyLimiter::new(
        app_context.rate_limiter.clone(),
        config.router_config.queue_size,
        Duration::from_secs(config.router_config.queue_timeout_secs),
    );

    if app_context.rate_limiter.is_none() {
        info!("Rate limiting is disabled (max_concurrent_requests = -1)");
    }

    match processor {
        Some(proc) => {
            spawn(proc.run());
            debug!(
                "Started request queue (size: {}, timeout: {}s)",
                config.router_config.queue_size, config.router_config.queue_timeout_secs
            );
        }
        None => {
            debug!(
                "Rate limiting enabled (max_concurrent_requests = {}, queue disabled)",
                config.router_config.max_concurrent_requests
            );
        }
    }

    // Set mesh sync manager to worker registry and policy registry if mesh is enabled
    // This allows these components to sync state across mesh nodes when mesh is enabled,
    // but they work independently without mesh when mesh is disabled.
    // Using thread-safe set_mesh_sync method that works with Arc-wrapped registries
    if let Some(ref sync_manager) = mesh_sync_manager {
        app_context
            .worker_registry
            .set_mesh_sync(Some(sync_manager.clone()));
        info!("Mesh sync manager set on worker registry");

        app_context
            .policy_registry
            .set_mesh_sync(Some(sync_manager.clone()));
        info!("Mesh sync manager set on policy registry");
    }

    // Get mesh cluster state and port before moving mesh_handler into app_state
    let mesh_cluster_state = mesh_handler.as_ref().map(|h| h.state.clone());
    let mesh_port = config
        .mesh_server_config
        .as_ref()
        .map(|c| c.self_addr.port());

    let app_state = Arc::new(AppState {
        router,
        context: app_context.clone(),
        concurrency_queue_tx: limiter.queue_tx.clone(),
        router_manager: Some(router_manager),
        mesh_handler,
        mesh_sync_manager,
    });
    if let Some(service_discovery_config) = config.service_discovery_config {
        if service_discovery_config.enabled {
            let app_context_arc = Arc::clone(&app_state.context);

            match start_service_discovery(
                service_discovery_config,
                app_context_arc,
                mesh_cluster_state,
                mesh_port,
            )
            .await
            {
                Ok(handle) => {
                    info!("Service discovery started");
                    spawn(async move {
                        if let Err(e) = handle.await {
                            error!("Service discovery task failed: {:?}", e);
                        }
                    });
                }
                Err(e) => {
                    error!("Failed to start service discovery: {e}");
                    warn!("Continuing without service discovery");
                }
            }
        }
    }

    info!(
        "Router ready | workers: {:?}",
        WorkerManager::get_worker_urls(&app_state.context.worker_registry)
    );

    let request_id_headers = config.request_id_headers.clone().unwrap_or_else(|| {
        vec![
            "x-request-id".to_string(),
            "x-correlation-id".to_string(),
            "x-trace-id".to_string(),
            "request-id".to_string(),
        ]
    });

    let auth_config = AuthConfig {
        api_key: config.router_config.api_key.clone(),
    };

    // Initialize control plane authentication if configured
    let control_plane_auth_state =
        crate::auth::ControlPlaneAuthState::try_init(config.control_plane_auth.as_ref()).await;

    let app = build_app(
        app_state,
        auth_config,
        control_plane_auth_state,
        config.max_payload_size,
        request_id_headers,
        config.router_config.cors_allowed_origins.clone(),
    );

    // llama.cpp webui: static SPA under /_ui/ (exact /_ui/v1/* and /_ui/props
    // API routes registered in build_app win over the nested wildcard).
    let app = match config.ui_dir.as_deref() {
        Some(dir) if !dir.is_empty() => {
            if std::path::Path::new(dir).is_dir() {
                info!("Serving llama.cpp webui at /_ui/ from {}", dir);
                app.nest_service(
                    "/_ui",
                    tower_http::services::ServeDir::new(dir)
                        .append_index_html_on_directories(true),
                )
            } else {
                warn!("ui_dir {} does not exist; webui disabled", dir);
                app
            }
        }
        _ => app,
    };

    // TcpListener::bind accepts &str and handles IPv4/IPv6 via ToSocketAddrs
    let bind_addr = format!("{}:{}", config.host, config.port);
    info!("Starting server on {}", bind_addr);

    // Parse address and set up graceful shutdown (common to both TLS and non-TLS)
    let addr: std::net::SocketAddr = bind_addr
        .parse()
        .map_err(|e| format!("Invalid address: {}", e))?;

    let handle = axum_server::Handle::new();
    let handle_clone = handle.clone();
    let grace_period = Duration::from_secs(config.shutdown_grace_period_secs);
    spawn(async move {
        shutdown_signal().await;
        handle_clone.graceful_shutdown(Some(grace_period));
    });

    if let (Some(cert), Some(key)) = (
        &config.router_config.server_cert,
        &config.router_config.server_key,
    ) {
        info!("TLS enabled");
        ring::default_provider()
            .install_default()
            .map_err(|e| format!("Failed to install rustls ring provider: {e:?}"))?;

        let tls_config = axum_server::tls_rustls::RustlsConfig::from_pem(cert.clone(), key.clone())
            .await
            .map_err(|e| format!("Failed to create TLS config: {}", e))?;

        axum_server::bind_rustls(addr, tls_config)
            .handle(handle)
            .serve(app.into_make_service())
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error>)?;
    } else {
        axum_server::bind(addr)
            .handle(handle)
            .serve(app.into_make_service())
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error>)?;
    }

    // HA handler shutdown is handled by the signal in mesh_run! macro
    // No need to manually shutdown here

    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {
            info!("Received Ctrl+C, starting graceful shutdown");
        },
        _ = terminate => {
            info!("Received terminate signal, starting graceful shutdown");
        },
    }
}

fn create_cors_layer(allowed_origins: Vec<String>) -> tower_http::cors::CorsLayer {
    use tower_http::cors::Any;

    let cors = if allowed_origins.is_empty() {
        tower_http::cors::CorsLayer::new()
            .allow_origin(Any)
            .allow_methods(Any)
            .allow_headers(Any)
            .expose_headers(Any)
    } else {
        let origins: Vec<http::HeaderValue> = allowed_origins
            .into_iter()
            .filter_map(|origin| origin.parse().ok())
            .collect();

        tower_http::cors::CorsLayer::new()
            .allow_origin(origins)
            .allow_methods([http::Method::GET, http::Method::POST, http::Method::OPTIONS])
            .allow_headers([http::header::CONTENT_TYPE, http::header::AUTHORIZATION])
            .expose_headers([http::header::HeaderName::from_static("x-request-id")])
    };

    cors.max_age(Duration::from_secs(3600))
}
