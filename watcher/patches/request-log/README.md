# request-log patch（/_ui/ Logs 页 · 内存请求日志）

上游同步（`watcher/upstream_sync.sh`）会整棵替换 `gateway/`，本目录的补丁负责把
「请求日志 / Logs UI」特性原样重放到新的上游树上。写法与 `watcher/patches/ui/` 一致：
片段文件是从打过补丁的工作区**逐字节抽取**的，脚本只做锚点插入，锚点不唯一或缺失一律
报错退出非 0（宁可 CI 红，也不能悄悄上线一个没有 Logs 页的网关）。

## 这块补丁做什么

- 新增 `src/observability/request_log.rs`：内存环形缓冲（不落盘），TTFT / token 速率 /
  路由决策 / 候选 worker 等字段；容量用 `LMR_REQUEST_LOG_CAPACITY` 调，0 关闭。
- `src/middleware.rs`：`RequestLogLayer` + `TrackedBody`，每个推理请求记一行，
  并把 request-id 以请求头形式传给 router（router 只拿得到 `Option<&HeaderMap>`）。
- `server.rs`：`/_ui/logs`、`/_ui/stats`、`/_ui/logs/stream`（SSE）三个公开路由 +
  `startup()` 里安装全局 store。
- `routers/openai/router.rs`（3 处）、`routers/http/router.rs`（1 处）：
  路由侧 ingest（模型 / effort / provider / 选中 worker / 候选集 / 响应 usage）。

## 文件

| 文件 | 落点 |
| --- | --- |
| `request_log.rs` | `gateway/src/observability/request_log.rs`（原样复制） |
| `middleware_block.rs` | `middleware.rs` 里 RequestLogLayer..TrackedBody 整段，插在「HTTP Metrics Layer」横幅之前 |
| `handlers.rs` | `server.rs` 中 `async fn liveness()` 之前 |
| `snip_store_install.rs` | `server.rs` startup() 里 prometheus 块之后 |
| `snip_oa_note.rs` / `snip_oa_decision.rs` / `snip_oa_observe.rs` | `routers/openai/router.rs` 3 处 |
| `snip_hr_ingest.rs` | `routers/http/router.rs` 1 处 |
| `apply_request_log.py` | 重放脚本 |
| `README.md` | 本文件 |

## 如何重放

    python3 watcher/patches/request-log/apply_request_log.py gateway watcher/patches/request-log

幂等：`gateway/src/middleware.rs` 里已出现 `RequestLogLayer` 即 exit 0。
依赖 ui 补丁先跑（`.merge(ui_api_routes(...))` 锚点由它产生），且必须先于
runtime-config 补丁（后者的策略块与 `merge(ui_config_routes())` 都锚在本补丁
产物上），upstream_sync.sh 里的三行顺序不能颠倒。

## 上游改到哪些文件最容易失败

锚点全部要求**恰好命中 1 次**，上游一旦动到这些位置就会大声报错：

1. `middleware.rs` 顶部 `use crate::{` 块、以及「HTTP Metrics Layer (Layer 1: SMG
   metrics)」注释横幅（上游若重排并发中间件 / metrics 层顺序即失配）。
2. `observability/mod.rs` 的 `pub mod otel_trace;`。
3. `server.rs` 的 `async fn liveness() -> Response {`、`.merge(ui_api_routes(...))`、
   `.layer(middleware::RequestIdLayer::new(request_id_headers))`、
   `metrics::start_prometheus(prometheus_config.clone());` 所在 if 块。
4. `routers/openai/router.rs` 的 `let streaming = body.stream;`、
   `get_provider_arc_for_worker(...)` + `transform_request` 两行、非流式的
   `circuit_breaker().record_success()` 块。
5. `routers/http/router.rs` `route_typed_request_once` 里 policy 求得之后的
   `let load_guard = ["cache_aware", "manual"]`。

报错后的修复流程：手动把工作区改回期望形态（或按上游新代码更新对应 snip 片段），
再重跑脚本，最后用下面的校验确认与工作区一致。

## 校验

重放进临时副本后与真实工作区对比，应完全一致且第二遍幂等：

    WORK=$(mktemp -d /data/tmp/rl-XXXX)
    rsync -a --exclude target gateway/ "$WORK/gateway/"
    for f in src/middleware.rs src/observability/mod.rs src/server.rs \
             src/routers/openai/router.rs src/routers/http/router.rs; do
      git show "HEAD:gateway/$f" > "$WORK/gateway/$f"
    done
    rm "$WORK/gateway/src/observability/request_log.rs"
    python3 watcher/patches/request-log/apply_request_log.py "$WORK/gateway" watcher/patches/request-log
    diff -r -x target "$WORK/gateway" gateway    # 必须为空
    python3 watcher/patches/request-log/apply_request_log.py "$WORK/gateway" watcher/patches/request-log  # exit 0

编译由 upstream_sync.sh / CI 的 `cargo build --profile ci --bin smg` 负责。
UI 侧 index.html 的 Logs 注入行由 `watcher/patch_ui_logs.sh --check` 在 build.yml 里把关。
