        // Runtime config (/_ui/config): same thinking/ctx policy as chat.
        crate::runtime_config::apply_effort_policy(&mut payload);
        crate::runtime_config::apply_ctx_cap(&mut payload, model);

