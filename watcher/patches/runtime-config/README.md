 # runtime-config patch（/_ui/ Config 页 · 热更新 effort / ctx 上限 / 模型改名代理）

上游同步（`watcher/upstream_sync.sh`）会整棵替换 `gateway/`，本目录的补丁负责把
「运行时配置 / Config UI」特性原样重放到新的上游树上。写法与
`watcher/patches/request-log/` 一致：片段文件是从打过补丁的工作区**逐字节抽取**
的，脚本只做锚点插入/单行改写；锚点不唯一或缺失一律报错退出非 0（宁可 CI 红，
也不能悄悄上线一个配置静默失效的网关）。

## 这块补丁做什么

- 新增 `src/runtime_config.rs`：进程级 `RuntimeConfigStore`（默认 effort、effort
  改写表、每模型 ctx 上限；env 基线 `LMR_DEFAULT_EFFORT` / `LMR_EFFORT_MAP` /
  `LMR_MODEL_CTX` / `LMR_WATCHER_URL`）+ `apply_effort_policy` /
  `apply_ctx_cap` 请求改写函数 + 模型改名向 llm-watcher 的代理。
- `lib.rs`：`pub mod runtime_config;`。
- `routers/openai/router.rs`：route_chat 在 request-log decision 块**之前**插入
  effort/ctx 策略（decision 里的 `set_decision` effort 即来自这里的
  `effective_effort`）；route_responses 在 provider 查找前插入同款策略。
- `routers/http/router.rs`：/generate 之外的 OpenAI 风格端点走同一套策略
  （策略行随 request-log 的 `snip_hr_ingest.rs` 片段落位），并把下游发送从
  `typed_req` 改为策略改写后的 `&payload`（本补丁对 http router 的唯一改写点）。
- `server.rs`：`ui_props_with_ctx`（/props 的 n_ctx 按配置钳制）+ 4 个
  /_ui/config handlers + `.merge(ui_config_routes())`。

## 文件

| 文件 | 落点 |
| --- | --- |
| `runtime_config.rs` | `gateway/src/runtime_config.rs`（原样复制） |
| `route_chat_policy.rs` | `routers/openai/router.rs` route_chat，插在 request-log decision 块之前 |
| `route_responses_policy.rs` | `routers/openai/router.rs` route_responses，插在 provider 查找之前 |
| `props_with_ctx_fn.rs` | `server.rs`，插在 `async fn ui_props` 之前 |
| `config_handlers.rs` | `server.rs`，插在 `async fn liveness()` 之前 |
| `apply_runtime_config.py` | 重放脚本（含 lib.rs / http router / props 调用点的锚点改写） |
| `README.md` | 本文件 |

## 如何重放

    python3 watcher/patches/runtime-config/apply_runtime_config.py gateway watcher/patches/runtime-config

幂等：`routers/openai/router.rs` 里已出现 `runtime_config` 即 exit 0。

## 依赖顺序（不能颠倒）

    ui -> request-log -> runtime-config

- ui 补丁在最前（本仓 HEAD 已内置 ui 特性时它幂等跳过）。
- request-log 必须先跑：route_chat 策略块锚在其 decision 块上、
  `.merge(ui_config_routes())` 锚在其 `.merge(ui_logs_routes())` 上、http router
  的 payload 序列化也由它的 ingest 片段带进来。
- request-log 单独跑完的中间态**不保证可编译**（decision/ingest 片段引用了
  `effective_effort` / `payload`），runtime-config 紧随其后补齐；构建只发生在
  三个补丁全部落位之后，这是有意设计。

## 上游改到哪些文件最容易失败

锚点全部要求**恰好命中 1 次**：

1. `lib.rs` 的 `pub mod service_discovery;`。
2. `routers/openai/router.rs` 的 request-log decision 块首行（8 空格缩进的
   `ingest_from_headers(headers) {`）、`let provider = ...` +
   `Endpoint::Responses` 两行。
3. `routers/http/router.rs` 的 `.send_typed_request(headers, typed_req, ...)` 整行。
4. `server.rs` 的 `async fn ui_props(state: ...) -> Response {` 签名、
   `return Json(ui_props_with_thinking(value)).into_response();`、fallback 的
   `Json(ui_props_with_thinking(json!({...})))` 五行块、`async fn liveness()`、
   `.merge(ui_logs_routes())`（由 request-log 产生）。

报错后的修复流程与 request-log 相同：手动把工作区改回期望形态（或按新代码更新
对应片段），重跑脚本，再用下面的校验确认与工作区一致。

## 校验

    WORK=/data/tmp/pcv/gateway   # 由 git archive HEAD gateway 还原
    python3 watcher/patches/ui/apply_ui.py "$WORK" watcher/patches/ui
    python3 watcher/patches/request-log/apply_request_log.py "$WORK" watcher/patches/request-log
    python3 watcher/patches/runtime-config/apply_runtime_config.py "$WORK" watcher/patches/runtime-config
    diff -r -x target -x LICENSE "$WORK" gateway    # 必须为空

编译由 upstream_sync.sh / CI 的 `cargo build --profile ci --bin smg` 负责。
