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

