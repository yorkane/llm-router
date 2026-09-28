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
/// Env: per-model forced effort, e.g. LMR_MODEL_EFFORT=qwen3-32b:high,glm:none
pub const ENV_MODEL_EFFORT: &str = "LMR_MODEL_EFFORT";
/// Env: per-model effort rewrites, e.g. LMR_MODEL_EFFORT_MAP=qwen:high>xhigh;qwen:low>medium
pub const ENV_MODEL_EFFORT_MAP: &str = "LMR_MODEL_EFFORT_MAP";
/// Env: per-model capability overrides, e.g. LMR_MODEL_MODALITIES=qwen:text+image,other:text
pub const ENV_MODEL_MODALITIES: &str = "LMR_MODEL_MODALITIES";
/// Env: base URL of the llm-watcher control plane used for model renames.
pub const ENV_WATCHER_URL: &str = "LMR_WATCHER_URL";

/// Values the effort picker understands (also what the Config page lists).
pub const EFFORT_LEVELS: [&str; 8] =
    ["none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra"];

/// Capability toggles the Config page exposes per model. ``text`` is always on;
/// the others map onto the webui's modalities flags (image -> vision).
pub const MODALITY_LEVELS: [&str; 4] = ["text", "image", "video", "audio"];

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

/// Everything the Config page can pin down for one model id: a context cap, a
/// default effort (used when the request carries none, carries an unknown one,
/// or a configured rewrite did not apply), a model-local effort rewrite table,
/// and an explicit capability set. ``None`` on ctx/modalities/default_effort
/// means "follow the worker / global config".
#[derive(Clone, Default)]
pub struct ModelConfig {
    pub ctx: Option<u64>,
    pub default_effort: Option<String>,
    /// requested -> replacement, checked before the global effort map.
    pub effort_map: HashMap<String, String>,
    /// text/image/video/audio subset; None = auto-detect from the worker.
    pub modalities: Option<Vec<String>>,
}

#[derive(Clone, Default)]
pub struct RuntimeConfig {
    /// Injected when the request has no reasoning_effort at all.
    pub default_effort: Option<String>,
    /// original -> replacement, applied after the default injection.
    pub effort_map: HashMap<String, String>,
    /// model id -> max context tokens the router will let through.
    pub model_ctx: HashMap<String, u64>,
    /// model id -> forced effort applied to every request for that model
    /// (whether or not the request carries a reasoning_effort). Legacy: the
    /// Config page now edits ``model_configs``; LMR_MODEL_EFFORT still wins
    /// while an entry is present, and saving a card clears it for that model.
    pub model_effort: HashMap<String, String>,
    /// model id -> the per-model card (ctx / default effort / effort map / caps).
    pub model_configs: HashMap<String, ModelConfig>,
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
        let model_effort = parse_pairs(std::env::var(ENV_MODEL_EFFORT).ok().as_deref())
            .into_iter()
            .filter_map(|(model, effort)| {
                let effort = normalize_effort(Some(&effort))?;
                if model.trim().is_empty() {
                    return None;
                }
                Some((model.trim().to_string(), effort))
            })
            .collect();
        // model:from>to ( ';' or ',' or newline separated, '>' between levels)
        let mut per_model_effort_map: HashMap<String, HashMap<String, String>> = HashMap::new();
        for (model, pair) in parse_pairs(std::env::var(ENV_MODEL_EFFORT_MAP).ok().as_deref()) {
            let Some((from, to)) = pair.split_once('>') else {
                continue;
            };
            let Some(from) = normalize_effort(Some(from.trim())) else {
                continue;
            };
            let Some(to) = normalize_effort(Some(to.trim())) else {
                continue;
            };
            if model.trim().is_empty() {
                continue;
            }
            per_model_effort_map
                .entry(model.trim().to_string())
                .or_default()
                .insert(from, to);
        }
        // model:cap1+cap2 (caps joined with '+')
        let mut per_model_caps: HashMap<String, Vec<String>> = HashMap::new();
        for (model, raw) in parse_pairs(std::env::var(ENV_MODEL_MODALITIES).ok().as_deref()) {
            let mut caps: Vec<String> = raw
                .split(['+', ',', ' '])
                .map(|c| c.trim().to_lowercase())
                .filter(|c| MODALITY_LEVELS.contains(&c.as_str()))
                .collect();
            caps.insert(0, "text".to_string());
            caps.dedup();
            if model.trim().is_empty() {
                continue;
            }
            per_model_caps.insert(model.trim().to_string(), caps);
        }
        let mut model_configs: HashMap<String, ModelConfig> = HashMap::new();
        for (model, map) in per_model_effort_map {
            model_configs.entry(model).or_default().effort_map = map;
        }
        for (model, caps) in per_model_caps {
            model_configs.entry(model).or_default().modalities = Some(caps);
        }
        RuntimeConfig {
            default_effort,
            effort_map,
            model_ctx,
            model_effort,
            model_configs,
        }
    }

    /// Merge one patch into a model card. Absent field = leave alone; JSON null
    /// (or an empty list for modalities/effort_map) = clear it back to "auto".
    fn merge_model_patch(current: &mut ModelConfig, patch: &Value) -> Result<(), String> {
        if patch.get("ctx").is_some() {
            current.ctx = match patch.get("ctx") {
                Some(Value::Null) | None => None,
                Some(Value::Number(n)) => match n.as_u64() {
                    Some(0) | None => return Err("ctx must be greater than zero".to_string()),
                    Some(v) => Some(v),
                },
                Some(Value::String(s)) => {
                    let s = s.trim();
                    if s.is_empty() {
                        None
                    } else {
                        Some(s.parse::<u64>().map_err(|_| format!("ctx must be a number: {}", s))?)
                    }
                }
                _ => return Err("ctx must be a number or null".to_string()),
            };
        }
        if patch.get("default_effort").is_some() {
            current.default_effort = match patch.get("default_effort") {
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
                                    "unknown effort for default_effort: {} (want one of {})",
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
            if entries.is_null() {
                current.effort_map.clear();
            } else {
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
                        continue; // deleting this row
                    }
                    let Some(to) = normalize_effort(Some(to.trim())) else {
                        return Err(format!(
                            "unknown effort in map target: {} (want one of {})",
                            to.trim(),
                            EFFORT_LEVELS.join(", ")
                        ));
                    };
                    next.insert(from, to);
                }
                current.effort_map = next;
            }
        }
        if patch.get("modalities").is_some() {
            current.modalities = match patch.get("modalities") {
                Some(Value::Null) => None,
                Some(Value::Array(list)) => {
                    let mut caps: Vec<String> = list
                        .iter()
                        .filter_map(|v| v.as_str())
                        .map(|s| s.trim().to_lowercase())
                        .filter(|c| MODALITY_LEVELS.contains(&c.as_str()))
                        .collect();
                    if caps.is_empty() {
                        // Explicit empty array means "text only", not "auto".
                        Some(vec!["text".to_string()])
                    } else {
                        caps.insert(0, "text".to_string());
                        caps.sort();
                        caps.dedup();
                        Some(caps)
                    }
                }
                _ => return Err("modalities must be an array or null".to_string()),
            };
        }
        Ok(())
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
        let mut meff: Vec<(&String, &String)> = self.model_effort.iter().collect();
        meff.sort_by(|a, b| a.0.cmp(b.0));
        let model_effort: Vec<Value> = meff
            .into_iter()
            .map(|(model, effort)| json!({"model": model, "effort": effort}))
            .collect();
        let mut mcfg: Vec<(&String, &ModelConfig)> = self.model_configs.iter().collect();
        mcfg.sort_by(|a, b| a.0.cmp(b.0));
        let model_configs: Vec<Value> = mcfg
            .into_iter()
            .map(|(model, c)| {
                let mut map: Vec<(&String, &String)> = c.effort_map.iter().collect();
                map.sort_by(|a, b| a.0.cmp(b.0));
                json!({
                    "model": model,
                    "ctx": c.ctx,
                    "default_effort": c.default_effort,
                    "effort_map": map
                        .into_iter()
                        .map(|(from, to)| json!({"from": from, "to": to}))
                        .collect::<Vec<Value>>(),
                    "modalities": c.modalities,
                })
            })
            .collect();
        json!({
            "default_effort": self.default_effort,
            "effort_map": effort_map,
            "model_ctx": model_ctx,
            "model_effort": model_effort,
            "model_configs": model_configs,
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
        if let Some(entries) = patch.get("model_effort") {
            let list = entries.as_array().ok_or_else(|| {
                "model_effort must be an array of {model, effort}".to_string()
            })?;
            let mut next = HashMap::new();
            for entry in list {
                let model = entry.get("model").and_then(|v| v.as_str()).unwrap_or("");
                if model.trim().is_empty() {
                    continue;
                }
                let raw = entry
                    .get("effort")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let trimmed = raw.trim();
                if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("null") {
                    continue; // entry removed this override
                }
                let effort = normalize_effort(Some(trimmed)).ok_or_else(|| {
                    format!(
                        "unknown effort for {}: {} (want one of {})",
                        model.trim(),
                        trimmed,
                        EFFORT_LEVELS.join(", ")
                    )
                })?;
                next.insert(model.trim().to_string(), effort);
            }
            cfg.model_effort = next;
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

    /// Effective effort for one request.
    ///
    /// Order: a legacy forced override (LMR_MODEL_EFFORT / model_effort) wins
    /// while present; then the model card -- its own rewrite table, falling back
    /// to the card default when the requested level is unknown to that model or
    /// when the rewrite did not apply; then the global map + global default.
    pub fn request_effort_for(
        &self,
        model: Option<&str>,
        requested: Option<&str>,
    ) -> Option<String> {
        let cfg = self.read();
        let model_key = model.map(|s| s.trim()).filter(|s| !s.is_empty());
        let card = model_key.and_then(|m| cfg.model_configs.get(m));
        if let Some(m) = model_key {
            if let Some(forced) = cfg.model_effort.get(m) {
                return Some(forced.clone());
            }
        }
        let wanted = requested
            .map(|s| s.trim().to_lowercase())
            .filter(|s| !s.is_empty() && s != "null" && s != "default");
        if let Some(card) = card {
            match wanted.as_deref().and_then(|v| normalize_effort(Some(v))) {
                Some(level) => {
                    if let Some(mapped) = card.effort_map.get(level.as_str()) {
                        return Some(mapped.clone());
                    }
                    // The card exists and declares mappings that did not match,
                    // or the level is not a level at all: use the model default.
                    if !card.effort_map.is_empty() || card.default_effort.is_some() {
                        return Some(card.default_effort.clone().unwrap_or_else(|| level.to_string()));
                    }
                    return Some(cfg.effort_map.get(level.as_str()).cloned().unwrap_or_else(|| level.to_string()));
                }
                // Request omitted the field (or sent null): model default first.
                None => {
                    if card.default_effort.is_some() {
                        return card.default_effort.clone();
                    }
                    if wanted.is_some() {
                        // Unparseable value from the client: do not forward junk.
                        return Some(
                            card.default_effort
                                .clone()
                                .or_else(|| cfg.default_effort.clone())
                                .unwrap_or_else(|| wanted.clone().unwrap()),
                        );
                    }
                    return cfg.default_effort.clone();
                }
            }
        }
        match wanted {
            Some(v) => cfg.effort_map.get(&v).cloned().or(Some(v)),
            None => cfg.default_effort.clone(),
        }
    }

    /// Context cap for a model: the card wins over the legacy LMR_MODEL_CTX map.
    pub fn ctx_cap(&self, model: &str) -> Option<u64> {
        let cfg = self.read();
        cfg.model_configs
            .get(model)
            .and_then(|c| c.ctx)
            .or_else(|| cfg.model_ctx.get(model).copied())
    }

    /// Capability override for a model; None = let the worker answer for itself.
    pub fn modalities_for(&self, model: &str) -> Option<Vec<String>> {
        self.read()
            .model_configs
            .get(model)
            .and_then(|c| c.modalities.clone())
    }

    /// Apply one model-card patch. Absent fields are untouched; `remove: true`
    /// drops the whole card. Saving a card also clears the legacy forced-effort
    /// entry for that model so the card is the single source of truth.
    pub fn apply_model_config(&self, patch: &Value) -> Result<Value, String> {
        let model = patch
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if model.is_empty() {
            return Err("model is required".to_string());
        }
        let mut cfg = self.read();
        if patch.get("remove").and_then(|v| v.as_bool()).unwrap_or(false) {
            cfg.model_configs.remove(&model);
            cfg.model_ctx.remove(&model);
            cfg.model_effort.remove(&model);
            self.write(cfg);
            return Ok(self.read().snapshot());
        }
        let mut card = cfg.model_configs.get(&model).cloned().unwrap_or_default();
        RuntimeConfig::merge_model_patch(&mut card, patch)?;
        if patch.get("default_effort").is_some() {
            cfg.model_effort.remove(&model);
        }
        if patch.get("ctx").is_some() {
            cfg.model_ctx.remove(&model);
        }
        cfg.model_configs.insert(model, card);
        self.write(cfg);
        Ok(self.read().snapshot())
    }

    /// Whole-document replace used by the Config page's JSON editor: every
    /// section is rebuilt from the payload and absent sections are cleared,
    /// which is what makes the JSON view a faithful source of truth rather than
    /// a patch overlay. Everything is validated into a detached config before
    /// anything is written, so a typo in one model card cannot leave the store
    /// half-applied.
    pub fn apply_document(&self, patch: &Value) -> Result<Value, String> {
        if !patch.is_object() {
            return Err("body must be a JSON object".to_string());
        }
        let mut next = RuntimeConfig::default();
        if patch.get("default_effort").is_some() {
            next.default_effort = match patch.get("default_effort") {
                Some(Value::Null) => None,
                Some(Value::String(s)) => {
                    let trimmed = s.trim();
                    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("null") {
                        None
                    } else {
                        match normalize_effort(Some(trimmed)) {
                            Some(v) => Some(v),
                            None => {
                                return Err(format!("unknown default_effort: {}", trimmed));
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
            for entry in list {
                let from = entry.get("from").and_then(|v| v.as_str()).unwrap_or("");
                let to = entry.get("to").and_then(|v| v.as_str()).unwrap_or("");
                let Some(from) = normalize_effort(Some(from)) else {
                    return Err(format!("unknown effort in effort_map: {}", from));
                };
                if to.trim().is_empty() {
                    continue;
                }
                let Some(to) = normalize_effort(Some(to.trim())) else {
                    return Err(format!(
                        "unknown effort in effort_map target: {}",
                        to.trim()
                    ));
                };
                next.effort_map.insert(from, to);
            }
        }
        if let Some(entries) = patch.get("model_ctx") {
            let list = entries
                .as_array()
                .ok_or_else(|| "model_ctx must be an array".to_string())?;
            for entry in list {
                let model = entry
                    .get("model")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim();
                if model.is_empty() {
                    continue;
                }
                let ctx = entry
                    .get("ctx")
                    .and_then(|v| v.as_u64())
                    .ok_or_else(|| format!("model_ctx for {} needs a numeric ctx", model))?;
                if ctx == 0 {
                    return Err(format!(
                        "model_ctx for {} must be greater than zero",
                        model
                    ));
                }
                next.model_ctx.insert(model.to_string(), ctx);
            }
        }
        if let Some(entries) = patch.get("model_effort") {
            let list = entries
                .as_array()
                .ok_or_else(|| "model_effort must be an array".to_string())?;
            for entry in list {
                let model = entry
                    .get("model")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim();
                if model.is_empty() {
                    continue;
                }
                let raw = entry.get("effort").and_then(|v| v.as_str()).unwrap_or("");
                let trimmed = raw.trim();
                if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("null") {
                    continue;
                }
                let effort = normalize_effort(Some(trimmed))
                    .ok_or_else(|| format!("unknown effort for {}", model))?;
                next.model_effort.insert(model.to_string(), effort);
            }
        }
        if let Some(entries) = patch.get("model_configs") {
            let list = entries
                .as_array()
                .ok_or_else(|| "model_configs must be an array".to_string())?;
            let mut built: HashMap<String, ModelConfig> = HashMap::new();
            for entry in list {
                let model = entry
                    .get("model")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim()
                    .to_string();
                if model.is_empty() {
                    return Err("every model_configs entry needs a model".to_string());
                }
                let mut card = ModelConfig::default();
                RuntimeConfig::merge_model_patch(&mut card, entry)?;
                built.insert(model, card);
            }
            next.model_configs = built;
        }
        self.write(next);
        Ok(self.read().snapshot())
    }

    /// Registered models (id -> worker urls) merged with the config cards, for
    /// the Config page. A name served by two providers is one card on purpose.
    pub fn models_document(&self, registered: Vec<(String, String)>) -> Value {
        let mut order: Vec<String> = Vec::new();
        let mut sources: HashMap<String, Vec<String>> = HashMap::new();
        for (model, url) in registered {
            if !sources.contains_key(&model) {
                order.push(model.clone());
            }
            sources.entry(model).or_default().push(url);
        }
        let cfg = self.read();
        for model in cfg.model_configs.keys() {
            if !sources.contains_key(model) {
                order.push(model.clone());
                sources.insert(model.clone(), Vec::new());
            }
        }
        order.sort();
        let models: Vec<Value> = order
            .into_iter()
            .map(|model| {
                let card = cfg.model_configs.get(&model);
                let mut map: Vec<(&String, &String)> = card
                    .map(|c| c.effort_map.iter().collect())
                    .unwrap_or_default();
                map.sort_by(|a, b| a.0.cmp(b.0));
                json!({
                    "model": model,
                    "registered": !sources[&model].is_empty(),
                    "sources": sources[&model],
                    "ctx": card.and_then(|c| c.ctx),
                    "default_effort": card.and_then(|c| c.default_effort.clone()),
                    "effort_map": map
                        .into_iter()
                        .map(|(from, to)| json!({"from": from, "to": to}))
                        .collect::<Vec<Value>>(),
                    "modalities": card.and_then(|c| c.modalities.clone()),
                })
            })
            .collect();
        json!(models)
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
    let model = payload
        .get("model")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    match store.request_effort_for(model.as_deref(), requested.as_deref()) {
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
