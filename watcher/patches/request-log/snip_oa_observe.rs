                                if let Some(ingest) = crate::observability::request_log::
                                    ingest_from_headers((*headers).as_ref())
                                {
                                    if status.is_success() {
                                        ingest.observe_json_body(&body);
                                    } else {
                                        ingest.observe_text(&String::from_utf8_lossy(&body));
                                    }
                                }
