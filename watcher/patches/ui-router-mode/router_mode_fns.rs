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

