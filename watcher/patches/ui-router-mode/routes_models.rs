        // Router-mode picker (role:"router" in /props). The SvelteKit base is
        // /_ui, so the bundle's "/v1/models" and "/models/{load,sse,unload}"
        // literals already arrive under /_ui/... — no bundle patch needed.
        .route("/_ui/v1/models", get(ui_models))
        .route("/_ui/models/load", post(ui_model_load))
        .route("/_ui/models/unload", post(ui_model_unload))
        .route("/_ui/models/sse", get(ui_models_sse))
