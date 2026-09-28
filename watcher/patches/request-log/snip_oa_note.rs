        // Request-log ingest (/_ui/logs): the middleware created the row and only
        // the router knows what the typed request actually carried.
        {
            use crate::observability::request_log::{head_fields_from_body, ingest_from_headers};
            if let Some(ingest) = ingest_from_headers(headers) {
                let (bm, effort, session) = match to_value(body) {
                    Ok(v) => head_fields_from_body(&v),
                    Err(_) => (None, None, None),
                };
                ingest.note_request(
                    bm.as_deref().unwrap_or(model),
                    effort.as_deref(),
                    session,
                    streaming,
                );
                ingest.set_route_type("openai-model-card");
            }
        }

