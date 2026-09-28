        // Request-log ingest (/_ui/logs): the routing decision is only visible
        // here - worker, policy name, and the candidate set for the model.
        // The body must be serialized to rewrite it: runtime-config policy
        // (effort map + context caps) applies to the OpenAI-style endpoints;
        // /generate uses different sampling fields and is left alone.
        let mut payload = match serde_json::to_value(typed_req) {
            Ok(v) => v,
            Err(e) => {
                return error::internal_error(
                    "serialization_failed",
                    format!("Failed to serialize request: {}", e),
                )
            }
        };
        let (requested_effort, effective_effort) = if route == "/generate" {
            (None, None)
        } else {
            crate::runtime_config::apply_effort_policy(&mut payload)
        };
        if route != "/generate" {
            crate::runtime_config::apply_ctx_cap(&mut payload, model_id.unwrap_or("unknown"));
        }

        if let Some(ingest) = crate::observability::request_log::ingest_from_headers(headers) {
            let session =
                crate::observability::request_log::head_fields_from_body(&payload).2;
            ingest.note_request(
                model_id.unwrap_or("unknown"),
                requested_effort.as_deref(),
                session,
                is_stream,
            );
            ingest.set_route_type(policy.name());
            ingest.set_selected_worker(worker.url());
            let provider_str = worker
                .metadata()
                .labels
                .get("engine")
                .cloned()
                .unwrap_or_else(|| "sglang".to_string());
            ingest.set_decision(
                model_id.unwrap_or("unknown"),
                Some(&provider_str),
                effective_effort.as_deref(),
            );
            ingest.set_candidates(
                &self
                    .worker_registry
                    .get_by_model(model_id.unwrap_or(""))
                    .iter()
                    .map(|w| crate::observability::request_log::compact_url(w.url()))
                    .collect::<Vec<_>>(),
            );
        }

