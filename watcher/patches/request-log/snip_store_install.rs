    // In-memory request log for the /_ui/ Logs page: a ring buffer plus sliding
    // token-rate windows, nothing ever hits disk. LMR_REQUEST_LOG_CAPACITY=0
    // keeps the feature entirely off.
    {
        use crate::observability::request_log::RequestLogStore;
        let capacity = std::env::var("LMR_REQUEST_LOG_CAPACITY")
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .unwrap_or(crate::observability::request_log::DEFAULT_CAPACITY);
        if capacity > 0 {
            RequestLogStore::install(RequestLogStore::new(capacity));
            info!("request log enabled: in-memory ring of {} records (GET /_ui/logs)", capacity);
        }
    }

