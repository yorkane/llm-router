        if let Some(ingest) = crate::observability::request_log::ingest_from_headers(headers) {
            let effort = effective_effort
                .as_deref()
                .or_else(|| payload.get("reasoning_effort").and_then(|v| v.as_str()));
            let provider_str = worker
                .provider_for_model(model)
                .or_else(|| worker.default_provider())
                .map(|p| p.as_str())
                .unwrap_or("openai");
            ingest.set_decision(model, Some(provider_str), effort.as_deref());
            ingest.set_selected_worker(worker.url());
            ingest.set_candidates(
                &self
                    .worker_registry
                    .get_by_model(model)
                    .iter()
                    .map(|w| crate::observability::request_log::compact_url(w.url()))
                    .collect::<Vec<_>>(),
            );
        }

