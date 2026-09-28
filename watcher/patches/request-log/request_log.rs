//! In-memory request log + live throughput/concurrency stats for the /_ui/ Logs page.
//!
//! Design constraints (see doc/logs-ui.md):
//! - nothing touches disk: a fixed-capacity ring buffer of recent requests
//!   (default 1000) plus sliding token-rate windows, all behind one small mutex;
//! - the hot path stays cheap: handlers hand us a few borrowed strings, and the
//!   per-chunk SSE scan only happens on streamed responses;
//! - the JSON snapshot and the SSE fan-out read the same records, so a record is
//!   finalized once and never mutated afterwards.
//!
//! Transport note: axum's Body is type-erased, so a downstream router cannot
//! attach data to the response for an upstream middleware to read back. Instead
//! the ingest middleware hands the router an Ingest handle (matched by request id
//! via crate::middleware::IngestHeader), and the router records into it.

use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, OnceLock,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use axum::http::HeaderMap;
use rand::Rng;
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::sync::broadcast;

/// How many finished requests to keep in memory.
pub const DEFAULT_CAPACITY: usize = 1000;
/// Token rates are aggregated over this sliding window.
pub const RATE_WINDOW: Duration = Duration::from_secs(10);
/// Below this many milliseconds the per-request sample is too short for a tok/s
/// figure to mean anything, so it is left null and the UI prints its own note.
const MIN_RATE_SAMPLE_MS: u64 = 100;

/// One line in the Logs table. snake_case field names are the wire contract with
/// ui/logs.html; keep them in sync.
#[derive(Clone, Serialize)]
pub struct RequestRecord {
    pub seq: u64,
    pub id: String,
    pub ts_ms: u64,
    pub method: String,
    pub path: String,
    pub endpoint: String,
    pub status: u16,
    pub stream: bool,
    pub model: Option<String>,
    pub requested_model: Option<String>,
    pub provider: Option<String>,
    pub worker: Option<String>,
    pub requested_effort: Option<String>,
    pub effort: Option<String>,
    pub route_type: Option<String>,
    pub selected: Option<String>,
    pub candidates: Vec<String>,
    pub session: Option<String>,
    pub duration_ms: u64,
    pub ttft_ms: Option<u64>,
    pub prompt_tokens: u64,
    pub cached_tokens: u64,
    pub completion_tokens: u64,
    pub reasoning_tokens: u64,
    /// Tokens were counted from the streamed text because the upstream sent no
    /// usage object (llama.cpp omits it unless stream_options asks for one).
    pub tokens_estimated: bool,
    pub tok_per_s: Option<f64>,
    pub error: Option<String>,
}

impl Default for RequestRecord {
    fn default() -> Self {
        Self {
            seq: 0,
            id: String::new(),
            ts_ms: 0,
            method: String::new(),
            path: String::new(),
            endpoint: String::new(),
            status: 0,
            stream: false,
            model: None,
            requested_model: None,
            provider: None,
            worker: None,
            requested_effort: None,
            effort: None,
            route_type: None,
            selected: None,
            candidates: Vec::new(),
            session: None,
            duration_ms: 0,
            ttft_ms: None,
            prompt_tokens: 0,
            cached_tokens: 0,
            completion_tokens: 0,
            reasoning_tokens: 0,
            tokens_estimated: false,
            tok_per_s: None,
            error: None,
        }
    }
}

/// Workers are displayed as host:port rather than a full URL; the table column
/// is narrow and the scheme carries no information.
pub fn compact_url(url: &str) -> String {
    url.trim_end_matches('/')
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .to_string()
}

pub fn error_snippet(body: &[u8]) -> Option<String> {
    if body.is_empty() {
        return None;
    }
    let text = String::from_utf8_lossy(body);
    let one_line: String = text.chars().filter(|c| *c != '\n' && *c != '\r').collect();
    let cut: String = one_line.chars().take(300).collect();
    if cut.is_empty() {
        None
    } else {
        Some(cut)
    }
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(char::from_digit((b >> 4) as u32, 16).unwrap());
        s.push(char::from_digit((b & 0xf) as u32, 16).unwrap());
    }
    s
}

/// Stable conversation key: an explicit OpenAI-ish conversation/user field when
/// present, otherwise a hash of the leading messages so a multi-turn chat
/// collapses onto one row group in the UI's session filter.
pub fn session_key(body: &Value) -> Option<String> {
    if let Some(v) = ["prompt_cache_key", "user", "conversation", "session_id"]
        .iter()
        .find_map(|k| body.get(*k).and_then(|v| v.as_str()))
    {
        if !v.is_empty() {
            let mut h = Sha256::new();
            h.update(v.as_bytes());
            return Some(hex_lower(&h.finalize()));
        }
    }

    let messages = body.get("messages").and_then(|v| v.as_array())?;
    if messages.len() < 2 {
        return None;
    }
    let first = messages.first()?;
    // content is a string for most clients, or an array of typed parts for the
    // OpenAI multimodal shape; both identify the same conversation.
    let content: String = match first.get("content")? {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(|v| v.as_str()))
            .collect::<Vec<_>>()
            .join(""),
        _ => return None,
    };
    if content.is_empty() {
        return None;
    }
    let mut h = Sha256::new();
    h.update(first.get("role").and_then(|v| v.as_str()).unwrap_or("").as_bytes());
    h.update([0u8]);
    h.update(content.as_bytes());
    Some(hex_lower(&h.finalize()))
}

/// Rough token estimate for streamed text when the upstream sends no usage.
/// Latin text lands near 4 chars/token, CJK near 1.4, so count them apart.
pub fn estimate_tokens(text: &str) -> u64 {
    let mut ascii = 0u64;
    let mut wide = 0u64;
    for ch in text.chars() {
        if ch.is_ascii() {
            ascii += 1;
        } else {
            wide += 1;
        }
    }
    (ascii as f64 / 4.0 + wide as f64 / 1.4) as u64
}

#[inline]
fn json_u64(v: Option<&Value>) -> u64 {
    match v {
        Some(Value::Number(n)) => n
            .as_u64()
            .or_else(|| n.as_f64().map(|f| f as u64))
            .unwrap_or(0),
        _ => 0,
    }
}

/// What we can see in one SSE chunk.
#[derive(Debug, PartialEq, Eq)]
pub enum ChunkInfo {
    /// First byte that carries an actual token, with the delta text.
    FirstToken(String),
    /// A usage object: (prompt, cached, completion, reasoning).
    Usage(u64, u64, u64, u64),
    /// The stream announced its end ([DONE], finish_reason, llama.cpp stop).
    Done,
}

/// Inspect one SSE chunk for the fields the log needs. Deliberately substring
/// gated before touching serde, because this runs on every network chunk.
pub fn scan_chunk(buf: &[u8]) -> Vec<ChunkInfo> {
    let mut out = Vec::new();
    let text = match std::str::from_utf8(buf) {
        Ok(t) => t,
        Err(_) => return out,
    };
    if !text.contains("data:") {
        return out;
    }
    for line in text.lines() {
        let payload = match line.strip_prefix("data:") {
            Some(p) => p.trim(),
            None => continue,
        };
        if payload == "[DONE]" {
            out.push(ChunkInfo::Done);
            continue;
        }
        if payload.is_empty() || !payload.starts_with('{') {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(payload) else {
            continue;
        };

        let mut token = false;
        let mut piece_and_reasoning = String::new();
        if let Some(choices) = value.get("choices").and_then(|v| v.as_array()) {
            for choice in choices {
                let delta = choice.get("delta");
                let piece = delta
                    .and_then(|d| d.get("content"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let reasoning = delta
                    .and_then(|d| d.get("reasoning_content"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if !piece.is_empty() || !reasoning.is_empty() {
                    piece_and_reasoning.push_str(piece);
                    piece_and_reasoning.push_str(reasoning);
                    token = true;
                }
                if choice
                    .get("finish_reason")
                    .is_some_and(|v| !v.is_null())
                {
                    out.push(ChunkInfo::Done);
                }
            }
        }
        // llama.cpp style: {"content":"...","stop":false}
        if let Some(content) = value.get("content").and_then(|v| v.as_str()) {
            if !content.is_empty() {
                token = true;
            }
        }
        if value.get("done").and_then(|v| v.as_bool()) == Some(true)
            || value.get("stop").and_then(|v| v.as_bool()) == Some(true)
        {
            out.push(ChunkInfo::Done);
        }
        if token {
            out.push(ChunkInfo::FirstToken(piece_and_reasoning));
        }

        if let Some(usage) = value.get("usage").filter(|v| !v.is_null()) {
            let prompt =
                json_u64(usage.get("prompt_tokens")).max(json_u64(usage.get("input_tokens")));
            let completion = json_u64(usage.get("completion_tokens"))
                .max(json_u64(usage.get("output_tokens")));
            let cached = usage
                .get("prompt_tokens_details")
                .and_then(|d| d.get("cached_tokens"))
                .map(|v| json_u64(Some(v)))
                .unwrap_or_else(|| json_u64(usage.get("cached_tokens")));
            let reasoning = usage
                .get("completion_tokens_details")
                .and_then(|d| d.get("reasoning_tokens"))
                .map(|v| json_u64(Some(v)))
                .unwrap_or_else(|| json_u64(usage.get("reasoning_tokens")));
            out.push(ChunkInfo::Usage(prompt, cached, completion, reasoning));
        }
    }
    out
}

/// Live answer for the summary strip.
#[derive(Serialize)]
pub struct StatsSnapshot {
    pub inflight: usize,
    pub uptime_s: f64,
    pub requests_total: u64,
    pub output_tok_s: f64,
    pub input_tok_s: f64,
    pub window_s: f64,
    pub requests_window: usize,
    pub errors_window: usize,
    pub avg_ttft_ms: Option<f64>,
    pub avg_duration_ms: Option<f64>,
    pub tokens_estimated_share: f64,
    pub price_in_per_mtok: Option<f64>,
    pub price_out_per_mtok: Option<f64>,
    pub capacity: usize,
    pub buffered: usize,
    pub started_at_ms: u64,
}

struct RateWindow {
    events: VecDeque<(Instant, u64)>,
    total: u64,
}

impl RateWindow {
    fn new() -> Self {
        Self {
            events: VecDeque::new(),
            total: 0,
        }
    }

    fn add(&mut self, at: Instant, amount: u64) {
        if amount == 0 {
            return;
        }
        self.total = self.total.saturating_add(amount);
        self.events.push_back((at, amount));
        self.trim(at);
    }

    fn trim(&mut self, now: Instant) {
        while let Some((at, amount)) = self.events.front().copied() {
            if now.duration_since(at) > RATE_WINDOW {
                self.total = self.total.saturating_sub(amount);
                self.events.pop_front();
            } else {
                break;
            }
        }
    }

    /// Tokens per second over the window. A window that has not filled up yet
    /// divides by the elapsed span instead of the full window, so a cold router
    /// does not under-report by an order of magnitude for its first 10 seconds.
    fn rate(&mut self, now: Instant) -> f64 {
        self.trim(now);
        let span = match self.events.front().copied() {
            Some((first, _)) => now.duration_since(first).max(Duration::from_millis(250)),
            None => RATE_WINDOW,
        };
        self.total as f64 / span.as_secs_f64()
    }
}

/// A finalized record plus its token counts and completion instant.
struct Entry {
    record: RequestRecord,
    at: Instant,
}

struct Inner {
    records: VecDeque<Entry>,
    next_seq: u64,
    requests_total: u64,
    inflight: usize,
    output: RateWindow,
    input: RateWindow,
}

/// Shared request-log store; one per router process.
pub struct RequestLogStore {
    inner: Mutex<Inner>,
    capacity: usize,
    started_at: Instant,
    started_at_ms: u64,
    tx: broadcast::Sender<RequestRecord>,
    price_in: Option<f64>,
    price_out: Option<f64>,
    pending: dashmap::DashMap<Arc<str>, Ingest>,
}

impl RequestLogStore {
    pub fn new(capacity: usize) -> Arc<Self> {
        let capacity = capacity.max(16);
        // Reading prices from the env is what lets the UI's ~$ column show a real
        // number; absence is reported honestly instead of guessed.
        let price_in = std::env::var("LMR_PRICE_IN_PER_MTOK")
            .ok()
            .and_then(|v| v.trim().parse().ok());
        let price_out = std::env::var("LMR_PRICE_OUT_PER_MTOK")
            .ok()
            .and_then(|v| v.trim().parse().ok());
        let (tx, _rx) = broadcast::channel(512);
        Arc::new(Self {
            inner: Mutex::new(Inner {
                records: VecDeque::with_capacity(capacity.min(4096)),
                next_seq: 1,
                requests_total: 0,
                inflight: 0,
                output: RateWindow::new(),
                input: RateWindow::new(),
            }),
            capacity,
            started_at: Instant::now(),
            started_at_ms: now_ms(),
            tx,
            price_in,
            price_out,
            pending: dashmap::DashMap::new(),
        })
    }

    /// New requests get an id like lmr-8f3c1a2b4d5e6f70, i.e. the same shape as
    /// the opencodex dashboard's ocx- ids so they survive copy/paste into notes.
    pub fn new_request_id() -> String {
        let mut bytes = [0u8; 8];
        rand::rng().fill(&mut bytes);
        format!("lmr-{}", hex_lower(&bytes))
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Register a request as in-flight; the returned guard decrements on drop.
    pub fn begin(&self) -> InflightGuard<'_> {
        let mut inner = self.lock();
        inner.inflight += 1;
        InflightGuard { store: self }
    }

    /// Owned counterpart of begin(): the ingest middleware moves the handle into
    /// the response body, so the counter drops exactly when that body is fully
    /// written or dropped (including client disconnects).
    pub fn begin_handle(self: &Arc<Self>) -> InflightHandle {
        self.lock().inflight += 1;
        InflightHandle {
            store: Some(self.clone()),
        }
    }

    /// Register a request as tracked. Routers find the handle back through
    /// REQUEST_ID_HEADER, which the middleware injects into the request.
    pub fn start(self: &Arc<Self>, head: RequestHead) -> PendingRequest {
        let ingest = Ingest::new(self.clone(), head);
        let id: Arc<str> = ingest.id.clone();
        self.pending.insert(id.clone(), ingest.clone());
        PendingRequest {
            ingest,
            inflight: self.begin_handle(),
            finished: AtomicBool::new(false),
        }
    }

    /// Router-side lookup: a DashMap read keyed by the injected request id.
    pub fn ingest_for(&self, id: &str) -> Option<Ingest> {
        self.pending.get(id).map(|entry| entry.clone())
    }

    fn forget(&self, id: &str) {
        self.pending.remove(id);
    }

    /// A request that was accepted but never answered (client hung up): it still
    /// deserves a row, reported as 499.
    pub fn record_aborted(&self, mut record: RequestRecord) {
        record.status = 499;
        if record.error.is_none() {
            record.error = Some("client disconnected before the response completed".to_string());
        }
        self.publish(record, Instant::now());
    }

    fn publish(&self, mut record: RequestRecord, at: Instant) {
        // Per-request rate over the decode window when there is one (a streamed
        // request spends most of its duration decoding), otherwise over the total
        // duration. Very short samples are dropped rather than reported as an
        // absurdly optimistic figure.
        if record.tok_per_s.is_none() && record.completion_tokens > 0 {
            let decode_ms = record.duration_ms.saturating_sub(record.ttft_ms.unwrap_or(0));
            let window = if decode_ms >= MIN_RATE_SAMPLE_MS {
                decode_ms
            } else if record.duration_ms >= MIN_RATE_SAMPLE_MS {
                record.duration_ms
            } else {
                0
            };
            if window > 0 {
                record.tok_per_s = Some(record.completion_tokens as f64 / (window as f64 / 1000.0));
            }
        }

        let mut inner = self.lock();
        record.seq = inner.next_seq;
        inner.next_seq += 1;
        inner.requests_total += 1;
        inner.output.add(at, record.completion_tokens);
        inner.input.add(at, record.prompt_tokens);
        inner.records.push_back(Entry {
            record: record.clone(),
            at,
        });
        while inner.records.len() > self.capacity {
            inner.records.pop_front();
        }
        drop(inner);

        // A full subscriber queue only costs that reader a dropped frame; it will
        // heal on its next ./logs?cursor= poll.
        let _ = self.tx.send(record);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<RequestRecord> {
        self.tx.subscribe()
    }

    /// Everything after cursor (exclusive), oldest first, plus the new cursor.
    pub fn snapshot(&self, cursor: u64, limit: usize) -> (u64, Vec<RequestRecord>) {
        let inner = self.lock();
        let records: Vec<RequestRecord> = inner
            .records
            .iter()
            .filter(|e| e.record.seq > cursor)
            .take(limit.clamp(1, 2000))
            .map(|e| e.record.clone())
            .collect();
        let cursor = records.last().map(|r| r.seq).unwrap_or(cursor);
        (cursor, records)
    }

    pub fn stats(&self) -> StatsSnapshot {
        let now = Instant::now();
        let mut inner = self.lock();
        let buffered = inner.records.len();
        let output_tok_s = inner.output.rate(now);
        let input_tok_s = inner.input.rate(now);

        let mut window_requests = 0usize;
        let mut window_errors = 0usize;
        let (mut ttft_sum, mut ttft_n) = (0u64, 0u64);
        let (mut dur_sum, mut dur_n) = (0u64, 0u64);
        let (mut estimated, mut counted) = (0usize, 0usize);
        for entry in inner.records.iter().rev() {
            let record = &entry.record;
            if now.duration_since(entry.at) <= RATE_WINDOW {
                window_requests += 1;
                if record.status >= 400 {
                    window_errors += 1;
                }
            }
            if let Some(ttft) = record.ttft_ms {
                ttft_sum = ttft_sum.saturating_add(ttft);
                ttft_n += 1;
            }
            if record.duration_ms > 0 {
                dur_sum = dur_sum.saturating_add(record.duration_ms);
                dur_n += 1;
            }
            if counted < 200 {
                counted += 1;
                if record.tokens_estimated {
                    estimated += 1;
                }
            }
        }

        StatsSnapshot {
            inflight: inner.inflight,
            uptime_s: now.duration_since(self.started_at).as_secs_f64(),
            requests_total: inner.requests_total,
            output_tok_s,
            input_tok_s,
            window_s: RATE_WINDOW.as_secs_f64(),
            requests_window: window_requests,
            errors_window: window_errors,
            avg_ttft_ms: (ttft_n > 0).then(|| ttft_sum as f64 / ttft_n as f64),
            avg_duration_ms: (dur_n > 0).then(|| dur_sum as f64 / dur_n as f64),
            tokens_estimated_share: if counted > 0 {
                estimated as f64 / counted as f64
            } else {
                0.0
            },
            price_in_per_mtok: self.price_in,
            price_out_per_mtok: self.price_out,
            capacity: self.capacity,
            buffered,
            started_at_ms: self.started_at_ms,
        }
    }
}

/// The one store per process. Installed by server::startup and read by the
/// routers - which receive nothing but Option<&HeaderMap> - and by the ingest
/// middleware, so no state has to be threaded through every router constructor.
static STORE: OnceLock<Arc<RequestLogStore>> = OnceLock::new();

/// Header the ingest middleware injects so routers can find their ingest handle.
pub const REQUEST_ID_HEADER: &str = "x-lmr-request-id";

impl RequestLogStore {
    /// Install the process-wide store; the first call wins.
    pub fn install(store: Arc<RequestLogStore>) -> Arc<RequestLogStore> {
        STORE.get_or_init(|| store).clone()
    }

    pub fn current() -> Option<&'static Arc<RequestLogStore>> {
        STORE.get()
    }
}

/// Resolve the in-flight ingest for a request from the injected id header.
/// Returns None for requests the log does not track.
pub fn ingest_from_headers(headers: Option<&HeaderMap>) -> Option<Ingest> {
    let store = RequestLogStore::current()?;
    let raw = headers?.get(REQUEST_ID_HEADER)?.to_str().ok()?;
    store.ingest_for(raw)
}

/// Keeps the concurrency counter honest: dropped exactly once on every exit path,
/// including client disconnects that never produce a response.
pub struct InflightGuard<'a> {
    store: &'a RequestLogStore,
}

impl Drop for InflightGuard<'_> {
    fn drop(&mut self) {
        let mut inner = self.store.lock();
        inner.inflight = inner.inflight.saturating_sub(1);
    }
}

/// Owned concurrency counter (see RequestLogStore::begin_handle).
pub struct InflightHandle {
    store: Option<Arc<RequestLogStore>>,
}

impl Drop for InflightHandle {
    fn drop(&mut self) {
        if let Some(store) = self.store.take() {
            let mut inner = store.lock();
            inner.inflight = inner.inflight.saturating_sub(1);
        }
    }
}

/// A tracked request that has not finished yet: the ingest handle for the
/// routers, the concurrency guard, and the once-flag for the log row.
///
/// Drop closes the row as a 499, so an exit path that never produced a complete
/// response still shows up in the Logs table.
pub struct PendingRequest {
    pub ingest: Ingest,
    /// Concurrency guard: never read, its whole job is running Drop.
    #[allow(dead_code)]
    inflight: InflightHandle,
    finished: AtomicBool,
}

impl PendingRequest {
    /// Close the row (idempotent) with the final status and error snippet.
    pub fn finalize(&mut self, status: u16, error: Option<String>) {
        if self.finished.swap(true, Ordering::AcqRel) {
            return;
        }
        let store = self.ingest.store.clone();
        store.forget(&self.ingest.id);
        self.ingest.finish_with_status(status, error);
    }
}

impl Drop for PendingRequest {
    fn drop(&mut self) {
        self.finalize(
            499,
            Some("client disconnected before the response completed".to_string()),
        );
    }
}

/// Static request metadata captured by the ingest middleware before the router
/// runs, so even a rejected request gets a row.
pub struct RequestHead {
    pub id: Arc<str>,
    pub ts_ms: u64,
    pub started: Instant,
    pub method: String,
    pub path: String,
    pub endpoint: String,
}

/// The request body belongs to the axum handler by the time the router runs, so
/// the middleware cannot read it. Each router reports the cheap fields we need
/// for a row (model, effort, session, stream) through Ingest::note_request.
pub fn head_fields_from_body(body: &Value) -> (Option<String>, Option<String>, Option<String>) {
    let model = body
        .get("model")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let effort = body
        .get("reasoning_effort")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let session = session_key(body);
    (model, effort, session)
}

/// Per-request mutable state, shared between the ingest middleware (which counts
/// concurrency and finalizes the row) and the routers (which fill in tokens as
/// the response streams).
#[derive(Clone)]
pub struct Ingest {
    pub id: Arc<str>,
    pub ts_ms: u64,
    pub started: Instant,
    pub method: String,
    pub path: String,
    pub endpoint: String,
    pub(crate) store: Arc<RequestLogStore>,
    state: Arc<Mutex<IngestState>>,
    /// Head fields the router fills once it has the typed request (model, the
    /// effort it finally forwarded, the session fingerprint, stream or not).
    head: Arc<Mutex<IngestHead>>,
}

#[derive(Default)]
struct IngestHead {
    stream: bool,
    model: Option<String>,
    session: Option<String>,
    effort: Option<String>,
}

#[derive(Default)]
struct IngestState {
    model: Option<String>,
    provider: Option<String>,
    worker: Option<String>,
    effort: Option<String>,
    route_type: Option<String>,
    selected: Option<String>,
    candidates: Vec<String>,
    first_token_at: Option<Instant>,
    usage: Option<(u64, u64, u64, u64)>,
    text: String,
    finalized: bool,
}

impl Ingest {
    pub(crate) fn new(store: Arc<RequestLogStore>, head: RequestHead) -> Self {
        Self {
            id: head.id,
            ts_ms: head.ts_ms,
            started: head.started,
            method: head.method,
            path: head.path,
            endpoint: head.endpoint,
            store,
            state: Arc::new(Mutex::new(IngestState::default())),
            head: Arc::new(Mutex::new(IngestHead::default())),
        }
    }

    pub fn is_streaming(&self) -> bool {
        self.head().stream
    }

    fn head(&self) -> std::sync::MutexGuard<'_, IngestHead> {
        self.head.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The router reports what it actually forwarded: the model, the effort value
    /// after any rewriting, the session key and whether this became a stream.
    /// First writer wins so a retry does not overwrite the original request.
    pub fn note_request(&self, model: &str, effort: Option<&str>, session: Option<String>, stream: bool) {
        let mut hd = self.head();
        if hd.model.is_none() {
            hd.model = Some(model.to_string());
        }
        if hd.effort.is_none() {
            hd.effort = effort.map(|s| s.to_string());
        }
        if hd.session.is_none() {
            hd.session = session;
        }
        hd.stream = hd.stream || stream;
    }

    fn state(&self) -> std::sync::MutexGuard<'_, IngestState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Model / provider / effort as finally chosen by the router (after rewrites).
    pub fn set_decision(&self, model: &str, provider: Option<&str>, effort: Option<&str>) {
        let mut st = self.state();
        st.model = Some(model.to_string());
        st.provider = provider.map(|s| s.to_string());
        st.effort = effort.map(|s| s.to_string()).or(st.effort.take());
    }

    pub fn set_selected_worker(&self, url: &str) {
        let mut st = self.state();
        let compact = compact_url(url);
        st.worker = Some(compact.clone());
        st.selected = Some(compact);
    }

    pub fn set_candidates(&self, candidates: &[String]) {
        let mut st = self.state();
        if st.candidates.is_empty() {
            st.candidates = candidates.to_vec();
        }
    }

    pub fn set_route_type(&self, route_type: &str) {
        self.state().route_type = Some(route_type.to_string());
    }

    /// Feed one raw SSE chunk: TTFT on the first token, usage when present.
    pub fn observe_stream_chunk(&self, buf: &[u8]) {
        let infos = scan_chunk(buf);
        if infos.is_empty() {
            return;
        }
        let mut st = self.state();
        for info in infos {
            match info {
                ChunkInfo::FirstToken(text) => {
                    if st.first_token_at.is_none() {
                        st.first_token_at = Some(Instant::now());
                    }
                    // Keep the streamed text (bounded) so a usage-less upstream
                    // still gets an estimated completion count at finalize.
                    if !text.is_empty() && st.text.len() < 2_000_000 {
                        st.text.push_str(&text);
                    }
                }
                ChunkInfo::Usage(p, c, comp, r) => {
                    let prev = st.usage.take().unwrap_or((0, 0, 0, 0));
                    st.usage = Some((p.max(prev.0), c.max(prev.1), comp.max(prev.2), r.max(prev.3)));
                }
                ChunkInfo::Done => {}
            }
        }
    }

    /// Accumulate streamed or in-band text so we can still report tokens when the
    /// upstream never sends usage.
    pub fn observe_text(&self, text: &str) {
        if text.is_empty() {
            return;
        }
        let mut st = self.state();
        if st.text.len() < 2_000_000 {
            st.text.push_str(text);
        }
    }

    pub fn observe_usage(&self, prompt: u64, cached: u64, completion: u64, reasoning: u64) {
        let mut st = self.state();
        let prev = st.usage.take().unwrap_or((0, 0, 0, 0));
        st.usage = Some((
            prompt.max(prev.0),
            cached.max(prev.1),
            completion.max(prev.2),
            reasoning.max(prev.3),
        ));
    }

    /// Non-streaming success: read the usage object and the message text out of
    /// the (small, already buffered) response body.
    pub fn observe_json_body(&self, body: &[u8]) {
        let Ok(value) = serde_json::from_slice::<Value>(body) else {
            return;
        };
        if let Some(usage) = value.get("usage").filter(|v| !v.is_null()) {
            let prompt =
                json_u64(usage.get("prompt_tokens")).max(json_u64(usage.get("input_tokens")));
            let completion =
                json_u64(usage.get("completion_tokens")).max(json_u64(usage.get("output_tokens")));
            let cached = usage
                .get("prompt_tokens_details")
                .and_then(|d| d.get("cached_tokens"))
                .map(|v| json_u64(Some(v)))
                .unwrap_or_else(|| json_u64(usage.get("cached_tokens")));
            let reasoning = usage
                .get("completion_tokens_details")
                .and_then(|d| d.get("reasoning_tokens"))
                .map(|v| json_u64(Some(v)))
                .unwrap_or_else(|| json_u64(usage.get("reasoning_tokens")));
            self.observe_usage(prompt, cached, completion, reasoning);
        }
        if let Some(choices) = value.get("choices").and_then(|v| v.as_array()) {
            for choice in choices {
                let message = choice.get("message");
                let content = message
                    .and_then(|m| m.get("content"))
                    .and_then(|v| v.as_str())
                    .or_else(|| choice.get("text").and_then(|v| v.as_str()))
                    .unwrap_or("");
                self.observe_text(content);
                if let Some(reasoning) = message
                    .and_then(|m| m.get("reasoning_content"))
                    .and_then(|v| v.as_str())
                {
                    self.observe_text(reasoning);
                }
            }
        }
    }

    /// Close the row. Called by the ingest middleware once the response body has
    /// been fully written (or dropped), and by the router for pre-send failures.
    pub fn finish_with_status(&self, status: u16, error: Option<String>) {
        let mut st = match self.state.try_lock() {
            Ok(st) => st,
            // Somebody else is mid-update; skipping here costs at most one row and
            // can never produce a wrong one.
            Err(_) => return,
        };
        if st.finalized {
            return;
        }
        st.finalized = true;

        let hd = self.head();
        let stream = hd.stream;
        let requested_model = hd.model.clone();
        let requested_effort = hd.effort.clone();
        let session = hd.session.clone();
        drop(hd);

        let elapsed = self.started.elapsed();
        let ttft_ms = st
            .first_token_at
            .map(|t| t.duration_since(self.started).as_millis() as u64);
        let (prompt, cached, completion, reasoning, estimated) = match st.usage {
            Some((p, c, comp, r)) => (p, c, comp, r, false),
            None => {
                // Only a stream that carried text can be estimated; a JSON body
                // would have been parsed by observe_json_body instead.
                let est = if stream {
                    estimate_tokens(&st.text)
                } else {
                    0
                };
                (0, 0, est, 0, est > 0)
            }
        };

        let record = RequestRecord {
            seq: 0,
            id: self.id.to_string(),
            ts_ms: self.ts_ms,
            method: self.method.clone(),
            path: self.path.clone(),
            endpoint: self.endpoint.clone(),
            status,
            stream,
            model: st.model.clone().or_else(|| requested_model.clone()),
            requested_model,
            provider: st.provider.clone(),
            worker: st.worker.clone(),
            requested_effort,
            effort: st.effort.clone(),
            route_type: st.route_type.clone(),
            selected: st.selected.clone(),
            candidates: st.candidates.clone(),
            session,
            duration_ms: elapsed.as_millis() as u64,
            ttft_ms,
            prompt_tokens: prompt,
            cached_tokens: cached,
            completion_tokens: completion,
            reasoning_tokens: reasoning,
            tokens_estimated: estimated,
            tok_per_s: None,
            error,
        };
        self.store.publish(record, Instant::now());
    }
}

/// Cost estimate for the ~$ column; None renders as 「无法估算」 in the UI.
pub fn cost_estimate(stats: &StatsSnapshot, prompt: u64, completion: u64) -> Option<f64> {
    let (Some(in_price), Some(out_price)) = (stats.price_in_per_mtok, stats.price_out_per_mtok)
    else {
        return None;
    };
    Some(prompt as f64 * in_price / 1e6 + completion as f64 * out_price / 1e6)
}
