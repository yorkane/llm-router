        // Runtime config (/_ui/config): thinking-effort policy and per-model
        // context caps, hot-mutable without a restart. Env baselines:
        // LMR_DEFAULT_EFFORT / LMR_EFFORT_MAP / LMR_MODEL_CTX.
        // (the requested effort is already on the log row via note_request,
        // which reads the body before the policy runs)
        let (_requested_effort, effective_effort) =
            crate::runtime_config::apply_effort_policy(&mut payload);
        crate::runtime_config::apply_ctx_cap(&mut payload, model);

