// ============================================================================
// Request log (the /_ui/ Logs page)
// ============================================================================

/// Tower Layer that records one row per inference request into the in-memory
/// request-log store (see observability::request_log). Nothing is persisted:
/// the store is a fixed-capacity ring buffer, so the Logs page can show live
/// traffic without ever touching a disk.
#[derive(Clone)]
pub struct RequestLogLayer;

impl<S> Layer<S> for RequestLogLayer {
    type Service = RequestLogMiddleware<S>;

    fn layer(&self, inner: S) -> Self::Service {
        RequestLogMiddleware { inner }
    }
}

#[derive(Clone)]
pub struct RequestLogMiddleware<S> {
    inner: S,
}

fn is_tracked_path(path: &str) -> bool {
    request_log_endpoint(path) != "other"
}

/// Static endpoint label for a request path (the /_ui aliases collapse onto the
/// same labels as the canonical routes).
pub fn request_log_endpoint(path: &str) -> &'static str {
    let p = path.strip_prefix("/_ui").unwrap_or(path);
    match p {
        "/generate" => "generate",
        "/v1/chat/completions" => "chat",
        "/v1/completions" => "completion",
        "/v1/responses" => "responses",
        "/v1/embeddings" => "embeddings",
        "/v1/rerank" => "rerank",
        "/v1/classify" => "classify",
        _ => "other",
    }
}

impl<S> Service<Request> for RequestLogMiddleware<S>
where
    S: Service<Request, Response = Response> + Clone + Send + 'static,
    S::Future: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future =
        Pin<Box<dyn std::future::Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: Request) -> Self::Future {
        let store = match crate::observability::request_log::RequestLogStore::current() {
            Some(store) if is_tracked_path(req.uri().path()) => store.clone(),
            _ => {
                let mut inner = self.inner.clone();
                return Box::pin(async move { inner.call(req).await });
            }
        };

        let path = req.uri().path().to_string();
        let head = crate::observability::request_log::RequestHead {
            id: crate::observability::request_log::RequestLogStore::new_request_id().into(),
            ts_ms: crate::observability::request_log::now_ms(),
            started: Instant::now(),
            method: req.method().as_str().to_string(),
            endpoint: request_log_endpoint(&path).to_string(),
            path,
        };
        let request_id: Arc<str> = head.id.clone();
        let pending = store.start(head);

        // Routers only receive Option<&HeaderMap>, so the id travels to them as a
        // request header and they look the ingest handle up in the store.
        if let Ok(value) = HeaderValue::from_str(&request_id) {
            req.headers_mut().insert(
                crate::observability::request_log::REQUEST_ID_HEADER,
                value,
            );
        }

        let mut inner = self.inner.clone();
        Box::pin(async move {
            let response = inner.call(req).await?;
            let (parts, body) = response.into_parts();
            let status = parts.status.as_u16();
            // SSE responses are scanned chunk by chunk (first token, streamed
            // usage); everything else is buffered and parsed once as a JSON body.
            let streaming = parts
                .headers
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.starts_with("text/event-stream"));
            let expected = http_body::Body::size_hint(&body).exact();
            let tracked = TrackedBody {
                inner: body,
                pending: Some(pending),
                status,
                streaming,
                expected,
                written: 0,
                json: Vec::new(),
            };
            Ok(Response::from_parts(parts, Body::new(tracked)))
        })
    }
}
/// Response body wrapper that closes the request-log row and releases the
/// in-flight slot exactly once, whichever way the body leaves: fully written,
/// errored mid-stream, or dropped because the client hung up.
struct TrackedBody {
    inner: Body,
    pending: Option<PendingRequest>,
    status: u16,
    /// text/event-stream: chunks are scanned live for the first token and usage
    /// instead of being buffered as one JSON body.
    streaming: bool,
    /// Declared size of a fixed-length (content-length) body, and how many bytes
    /// were actually handed out. hyper stops polling as soon as a known-length
    /// body is exhausted, so Ready(None) never arrives for those: the byte count
    /// is what proves the response really completed.
    expected: Option<u64>,
    written: u64,
    /// Buffered non-streaming body, so usage (and the error text of a 4xx) can be
    /// recovered even for routers that do not report it themselves.
    json: Vec<u8>,
}

/// Upper bound on the buffered body; a long completion stays far below this.
const LOG_BODY_BUFFER: usize = 256 * 1024;

impl TrackedBody {
    /// Record one outgoing data frame in the ingest.
    fn observe(&mut self, data: &[u8]) {
        if self.pending.is_none() {
            return;
        }
        if self.streaming {
            if let Some(pending) = self.pending.as_ref() {
                pending.ingest.observe_stream_chunk(data);
            }
            return;
        }
        let room = LOG_BODY_BUFFER.saturating_sub(self.json.len());
        let take = room.min(data.len());
        if take > 0 {
            self.json.extend_from_slice(&data[..take]);
        }
    }

    /// Close the row. A buffered JSON body is parsed first so that token counts
    /// and error text are captured whatever the endpoint.
    fn close(&mut self, status: u16, error: Option<String>) {
        let Some(mut pending) = self.pending.take() else {
            return;
        };
        let buffered = std::mem::take(&mut self.json);
        if !buffered.is_empty() {
            if self.streaming {
                pending.ingest.observe_stream_chunk(&buffered);
            } else {
                pending.ingest.observe_json_body(&buffered);
            }
        }
        pending.finalize(status, error);
    }

    /// Short single-line snippet of an error body, for the Logs error column.
    fn error_text(&self) -> Option<String> {
        if self.status < 400 || self.json.is_empty() {
            return None;
        }
        crate::observability::request_log::error_snippet(&self.json)
    }
}

impl http_body::Body for TrackedBody {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();
        let polled = Pin::new(&mut this.inner).poll_frame(cx);
        match polled {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    this.written += data.len() as u64;
                    this.observe(data);
                }
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(Some(Err(err))) => {
                let status = if this.status < 400 { 502 } else { this.status };
                this.close(status, Some(err.to_string()));
                Poll::Ready(Some(Err(err)))
            }
            Poll::Ready(None) => {
                this.close(this.status, this.error_text());
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }
}

impl Drop for TrackedBody {
    fn drop(&mut self) {
        if self.pending.is_none() {
            return;
        }
        // A fixed-length body is complete once every declared byte was handed out
        // (hyper never polls past that point). A buffered JSON body that carries
        // no declared size still arrived whole, because the routers read it fully
        // before answering. Only an SSE stream that stops before Ready(None) was
        // genuinely cut short by the client hanging up.
        let complete = match self.expected {
            Some(expected) => self.written >= expected,
            None => !self.streaming,
        };
        if complete || self.status >= 400 {
            self.close(self.status, self.error_text());
        } else {
            self.close(
                499,
                Some("client disconnected before the response completed".to_string()),
            );
        }
    }
}

