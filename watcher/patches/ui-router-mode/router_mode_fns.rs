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

