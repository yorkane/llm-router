//! Runtime-mutable router settings surfaced by the /_ui/ Config page.
//!
//! Everything here starts from an environment default (so a container restart
//! comes back with a sane baseline) and can then be changed at runtime through
//! the /_ui/config API without a restart. The state is process-global and tiny:
//! a default reasoning effort, an effort rewrite table, and per-model context
//! caps. Model renaming is NOT owned here - that lives in the llm-watcher
//! ledger, which re-registers workers under the new ids; the Config page just
//! proxies those edits to the watcher control plane.

use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

use once_cell::sync::OnceCell;
use serde_json::{json, Value};

/// Env: default reasoning effort when a request does not carry one.
pub const ENV_DEFAULT_EFFORT: &str = "LMR_DEFAULT_EFFORT";
/// Env: effort rewrites, e.g. LMR_EFFORT_MAP=high:xhigh,low:medium
pub const ENV_EFFORT_MAP: &str = "LMR_EFFORT_MAP";
/// Env: per-model context caps, e.g. LMR_MODEL_CTX=qwen3-32b:32768,glm:8192
pub const ENV_MODEL_CTX: &str = "LMR_MODEL_CTX";
/// Env: base URL of the llm-watcher control plane used for model renames.
pub const ENV_WATCHER_URL: &str = "LMR_WATCHER_URL";

/// Values the effort picker understands (also what the Config page lists).
pub const EFFORT_LEVELS: [&str; 8] =
    ["none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra"];

pub fn normalize_effort(value: Option<&str>) -> Option<String> {
    let v = value?.trim().to_lowercase();
    if v.is_empty() || v == "null" || v == "default" {
        return None;
    }
    if EFFORT_LEVELS.contains(&v.as_str()) {
        Some(v)
    } else {
        None
    }
}

#[derive(Clone, Default)]
pub struct RuntimeConfig {
    /// Injected when the request has no reasoning_effort at all.
    pub default_effort: Option<String>,
    /// original -> replacement, applied after the default injection.
    pub effort_map: HashMap<String, String>,
    /// model id -> max context tokens the router will let through.
    pub model_ctx: HashMap<String, u64>,
}

impl RuntimeConfig {
    fn from_env() -> Self {
        let default_effort = std::env::var(ENV_DEFAULT_EFFORT)
            .ok()
            .and_then(|v| normalize_effort(Some(&v)));
        let effort_map = parse_pairs(std::env::var(ENV_EFFORT_MAP).ok().as_deref())
            .into_iter()
            .filter_map(|(from, to)| {
                let from = normalize_effort(Some(&from))?;
                if to.trim().is_empty() {
                    return None;
                }
                Some((from, to.trim().to_string()))
            })
            .collect();
        let model_ctx = parse_pairs(std::env::var(ENV_MODEL_CTX).ok().as_deref())
            .into_iter()
            .filter_map(|(model, ctx)| {
                let ctx = ctx.trim().parse::<u64>().ok()?;
                if ctx == 0 || model.trim().is_empty() {
                    return None;
                }
                Some((model.trim().to_string(), ctx))
            })
            .collect();
        RuntimeConfig {
            default_effort,
            effort_map,
            model_ctx,
        }
    }

    fn snapshot(&self) -> Value {
        let mut map: Vec<(&String, &String)> = self.effort_map.iter().collect();
        map.sort_by(|a, b| a.0.cmp(b.0));
        let effort_map: Vec<Value> = map
            .into_iter()
            .map(|(from, to)| json!({"from": from, "to": to}))
            .collect();
        let mut ctx: Vec<(&String, &u64)> = self.model_ctx.iter().collect();
        ctx.sort_by(|a, b| a.0.cmp(b.0));
        let model_ctx: Vec<Value> = ctx
            .into_iter()
            .map(|(model, cap)| json!({"model": model, "ctx": cap}))
            .collect();
        json!({
            "default_effort": self.default_effort,
            "effort_map": effort_map,
            "model_ctx": model_ctx,
        })
    }
}

/// One store per process, installed at startup from the environment.
static CONFIG: OnceCell<Arc<RuntimeConfigStore>> = OnceCell::new();

pub struct RuntimeConfigStore {
    current: RwLock<RuntimeConfig>,
    env_defaults: Value,
    watcher_url: Option<String>,
}

impl RuntimeConfigStore {
    fn new() -> Arc<Self> {
        let from_env = RuntimeConfig::from_env();
        let env_defaults = from_env.snapshot();
        let watcher_url = std::env::var(ENV_WATCHER_URL)
            .ok()
            .map(|v| v.trim().trim_end_matches('/').to_string())
            .filter(|v| !v.is_empty());
        Arc::new(Self {
            current: RwLock::new(from_env),
            env_defaults,
            watcher_url,
        })
    }

    pub fn install() -> Arc<RuntimeConfigStore> {
        CONFIG.get_or_init(RuntimeConfigStore::new).clone()
    }

    pub fn current() -> Option<Arc<RuntimeConfigStore>> {
        CONFIG.get().cloned()
    }

    fn read(&self) -> RuntimeConfig {
        self.current
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn write(&self, cfg: RuntimeConfig) {
        *self.current.write().unwrap_or_else(|e| e.into_inner()) = cfg;
    }

    /// Full document for the Config page, including the watcher's rename table.
    pub async fn document(&self) -> Value {
        let mut doc = self.read().snapshot();
        doc["env_defaults"] = self.env_defaults.clone();
        let (reachable, model_map) = match self.watcher_url.as_deref() {
            None => (false, Value::Null),
            Some(url) => match fetch_watcher_model_map(url).await {
                Ok(map) => (true, map),
                Err(_) => (false, Value::Null),
            },
        };
        doc["watcher"] = json!({
            "url": self.watcher_url,
            "reachable": reachable,
            "model_map": model_map,
        });
        doc
    }

    pub fn watcher_url(&self) -> Option<String> {
        self.watcher_url.clone()
    }

    /// Apply an effort edit; returns the new snapshot (without watcher).
    pub fn apply_effort(&self, patch: &Value) -> Result<Value, String> {
        let mut cfg = self.read();
        if patch.get("default_effort").is_some() {
            cfg.default_effort = match patch.get("default_effort") {
                Some(Value::Null) => None,
                Some(Value::String(s)) => {
                    let trimmed = s.trim();
                    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("null") {
                        None
                    } else {
                        match normalize_effort(Some(trimmed)) {
                            Some(v) => Some(v),
                            None => {
                                return Err(format!(
                                    "unknown effort: {} (want one of {})",
                                    trimmed,
                                    EFFORT_LEVELS.join(", ")
                                ))
                            }
                        }
                    }
                }
                _ => return Err("default_effort must be a string or null".to_string()),
            };
        }
        if let Some(entries) = patch.get("effort_map") {
            let list = entries
                .as_array()
                .ok_or_else(|| "effort_map must be an array".to_string())?;
            let mut next = HashMap::new();
            for entry in list {
                let from = entry.get("from").and_then(|v| v.as_str()).unwrap_or("");
                let to = entry.get("to").and_then(|v| v.as_str()).unwrap_or("");
                let Some(from) = normalize_effort(Some(from)) else {
                    return Err(format!("unknown effort in map: {}", from));
                };
                if to.trim().is_empty() {
                    continue; // deleting this mapping
                }
                let to_norm = normalize_effort(Some(to.trim())).ok_or_else(|| {
                    format!(
                        "unknown effort in map target: {} (want one of {})",
                        to.trim(),
                        EFFORT_LEVELS.join(", ")
                    )
                })?;
                next.insert(from, to_norm);
            }
            cfg.effort_map = next;
        }
        self.write(cfg);
        Ok(self.read().snapshot())
    }

    /// Apply one per-model context cap; null removes it.
    pub fn apply_ctx(&self, model: &str, ctx: Option<u64>) -> Result<Value, String> {
        if model.trim().is_empty() {
            return Err("model is required".to_string());
        }
        let mut cfg = self.read();
        match ctx {
            None => {
                cfg.model_ctx.remove(model.trim());
            }
            Some(0) => return Err("ctx must be greater than zero".to_string()),
            Some(cap) => {
                cfg.model_ctx.insert(model.trim().to_string(), cap);
            }
        }
        self.write(cfg);
        Ok(self.read().snapshot())
    }

    /// --- hot-path readers (lock + immediate drop; never held across await) ---

    pub fn request_effort(&self, requested: Option<&str>) -> Option<String> {
        let cfg = self.read();
        match requested.map(|s| s.trim()).filter(|s| !s.is_empty()) {
            Some(v) => cfg
                .effort_map
                .get(v)
                .cloned()
                .or_else(|| Some(v.to_string())),
            None => cfg.default_effort.clone(),
        }
    }

    pub fn ctx_cap(&self, model: &str) -> Option<u64> {
        self.read().model_ctx.get(model).copied()
    }
}

fn parse_pairs(raw: Option<&str>) -> Vec<(String, String)> {
    raw.into_iter()
        .flat_map(|s| s.split([',', ';', '\n']))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter_map(|part| {
            let (from, to) = match part.split_once(':') {
                Some(pair) => pair,
                None => return None,
            };
            if from.trim().is_empty() {
                None
            } else {
                Some((from.trim().to_string(), to.trim().to_string()))
            }
        })
        .collect()
}

async fn fetch_watcher_model_map(url: &str) -> Result<Value, String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client
        .get(format!("{}/model-map", url))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("watcher returned {}", resp.status()));
    }
    resp.json().await.map_err(|e| e.to_string())
}

/// Forward a rename request ("orig:new,..." or an object) to the watcher.
pub async fn proxy_model_map(watcher_url: &str, body: Value) -> Result<Value, String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client
        .post(format!("{}/model-map", watcher_url))
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("watcher unreachable: {}", e))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    let parsed: Value = serde_json::from_str(&text).unwrap_or_else(|_| json!({"raw": text}));
    if !status.is_success() {
        let detail = parsed
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("watcher rejected the request");
        return Err(format!("watcher said {}: {}", status.as_u16(), detail));
    }
    Ok(parsed)
}

/// Apply the effort policy to an OpenAI-style payload (chat / responses).
/// Returns (requested, effective) so the request log can show high -> xhigh.
pub fn apply_effort_policy(payload: &mut Value) -> (Option<String>, Option<String>) {
    let store = RuntimeConfigStore::install();
    let requested = payload
        .get("reasoning_effort")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    match store.request_effort(requested.as_deref()) {
        Some(effective) => {
            payload["reasoning_effort"] = json!(effective);
            (requested, Some(effective))
        }
        // Nothing configured and nothing requested: leave the field alone.
        None => (requested.clone(), requested),
    }
}

/// Clamp max_tokens (and the max_completion_tokens alias) to the model cap.
pub fn apply_ctx_cap(payload: &mut Value, model: &str) -> Option<u64> {
    let store = RuntimeConfigStore::install();
    let cap = store.ctx_cap(model)?;
    for key in ["max_tokens", "max_completion_tokens"] {
        let current = payload.get(key).and_then(|v| v.as_u64());
        if current.is_none_or(|v| v > cap) {
            payload[key] = json!(cap);
        }
    }
    Some(cap)
}
