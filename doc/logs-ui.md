# llm-router 内存请求日志 + 实时吞吐统计 + /_ui/ Logs 页面

对应实现：`gateway/src/observability/request_log.rs`（后端）、`ui/logs.html` + `ui/logs-inject.js`（前端）、
`watcher/patch_ui_logs.sh`（webui 注入补丁）。

## 1. 功能概述

llm-router 把官方 llama.cpp webui 静态挂在 `/_ui/`（`--ui-dir` / env `SMG_UI_DIR`，见 doc/webui.md）。
在此之上网关额外维护一份**纯内存**的请求日志与实时吞吐统计，Logs 页面把它可视化：

- **Logs 菜单入口**：`logs-inject.js` 在官方 webui 左侧导航注入一个「Logs」按钮（与 New chat /
  Settings 同一套类名，折叠态 36x36 圆形图标一致），点击跳转 `/_ui/logs.html`。导航容器由 Svelte
  持有 children 引用，展开/折叠侧栏会重渲染并清掉注入节点，因此脚本用 MutationObserver 防抖补回，
  并用 `data-lmr-inject` 属性判重，避免注入自触发死循环；任何异常静默失败，不影响聊天页。
- **滚动请求日志**：每个进入网关的请求（含被拒绝的、客户端中途断开的）一行，**最新在最上面**：
  新行插入表头下方（prepend），页面默认停在顶部即最新行。用户向下滚离顶部即视为离开实时区，
  自动暂停跟随并显示「回到最新」悬浮条（文案「有新日志」，离开期间新增行数内部累计、
  回顶时清零），点击回顶恢复跟随。支持子串
  筛选（模型 / 提供方 / 请求 ID / 会话 / 状态码 / 路由类型 / 推理强度）、「清空」（只清前端视图，
  后端环形缓冲不动）、逐行「查看详情」（基本信息 / 路由决策 / 性能三段，含候选 worker 打勾、
  复制请求 ID、按会话筛选）。
- **三个头部指标（10 秒滑窗）**：
  - **当前并发**（`inflight`）：正在网关内部处理、尚未写出完毕的请求数，由 `InflightGuard` 在
    drop 时精确递减，客户端断连也会计数归位；
  - **输出 tok/s**（`output_tok_s`）：已完成请求的 `completion_tokens` 在最近 10 秒滑窗内的聚合速率；
  - **输入 tok/s**（`input_tok_s`）：同理按 `prompt_tokens` 聚合。
  滑窗未满时按实际存活时长（下限 250ms）折算，冷启动网关不会把速率低估一个数量级。
  摘要条还展示窗口请求数/错误数、平均 TTFT、运行时长、环形缓冲水位（buffered/capacity）。
- **数据来源双通道**：页面每 1s 轮询 `GET ./logs?cursor=<seq>` 增量（权威、断线可恢复），同时挂
  `GET ./logs/stream` SSE 低延迟追加；两路按 `record.seq` 去重，流式路径只 prepend `<tr>`
  到表头下方（不整表重建）。
  fetch 失败按指数退避放慢轮询节拍（上限 30s），不打断已渲染列表。前端最多保留 2000 行，
  超出时从 DOM 尾部（最旧行）往前裁剪（展开的详情行随所属行一并清掉）。

## 2. 数据接口（前端契约）

三个接口都挂在 `/_ui/` 前缀下，字段一律 snake_case——`RequestRecord` 的 serde 字段名就是与
`ui/logs.html` 的线上传输协议，改动必须两端同步。

### GET /_ui/logs?cursor=&limit=

返回 `{ "cursor": <新的最大 seq>, "capacity": <环形缓冲容量>, "requests": [RequestRecord...] }`。
语义：返回 `seq > cursor` 的记录，按 seq 升序，最多 `limit` 条（服务端 clamp 到 1..2000；
前端固定用 `cursor=<已消费最大seq>&limit=1000`）。`cursor` 为已消费的最大 seq（exclusive 游标）。

### GET /_ui/stats

返回 `StatsSnapshot`（下表），供摘要条渲染。与 logs 同节拍轮询。

### GET /_ui/logs/stream

SSE：每 finalize 一条请求就推一帧 `data: {RequestRecord JSON}`。服务端是
`tokio::sync::broadcast`（队列 512）扇出：订阅者队列满只丢该读者一帧，下次 `./logs?cursor=` 轮询自愈。
断线由 EventSource 自动重连；浏览器不支持 EventSource 时页面退化为纯轮询。

### RequestRecord 字段（每条日志行）

| 字段 | 类型 | 含义 |
|---|---|---|
| `seq` | u64 | 进程内单调递增序号（1 起），前端去重与 cursor 的基准；重启归零 |
| `id` | string | 请求 ID，形如 `lmr-8f3c1a2b4d5e6f70`（与 opencodex 的 `ocx-` 同形态，便于粘进笔记） |
| `ts_ms` | u64 | 请求进入网关的 Unix 毫秒时间戳 |
| `method` / `path` / `endpoint` | string | HTTP 方法、原始路径、归一化的端点类别 |
| `status` | u16 | 响应状态码；客户端中途断开记 **499**（error 附注 client disconnected 说明） |
| `stream` | bool | 是否流式（SSE）请求 |
| `model` | string? | 路由改写后最终生效的模型 |
| `requested_model` | string? | 请求体里原本要求的模型（两者不同则详情里标注「请求 xxx」） |
| `provider` | string? | 提供方（引擎/集群标签） |
| `worker` | string? | 实际承接的 worker，压缩成 `host:port`（去掉 scheme，列宽有限） |
| `requested_effort` / `effort` | string? | 请求的 / 生效的 reasoning_effort；不同则 UI 显示 `high → xhigh` |
| `route_type` | string? | 路由决策类型 |
| `selected` | string? | 最终选中的 worker（详情候选列表里打 ✓） |
| `candidates` | string[] | 候选 worker 列表（只记首次） |
| `session` | string? | 会话指纹（sha256 hex）：优先 `prompt_cache_key`/`user`/`conversation`/`session_id`，否则取首条消息 role+content 的哈希（messages≥2 时），让多轮对话在筛选里聚成一组 |
| `duration_ms` | u64 | 全请求耗时；UI 中 ≥3000ms 加粗 |
| `ttft_ms` | u64? | 首个携带实际 token 的 SSE chunk 的时刻（非流式为 null） |
| `prompt_tokens` | u64 | 输入 token 数 |
| `cached_tokens` | u64 | 前缀缓存命中 token（usage 的 `prompt_tokens_details.cached_tokens`） |
| `completion_tokens` | u64 | 输出 token 数（无 usage 时为估算值，见 §4） |
| `reasoning_tokens` | u64 | 思考 token（`completion_tokens_details.reasoning_tokens`） |
| `tokens_estimated` | bool | token 数由文本长度估算而非上游 usage（见 §4），UI 在 Token 数与 tok/s 前标灰色 `≈` |
| `tok_per_s` | f64? | 整请求吞吐 = completion_tokens / duration；finalize 时后端补齐 |
| `error` | string? | 错误摘要：响应体压成单行、截断 300 字符 |

### StatsSnapshot 字段（/_ui/stats）

| 字段 | 类型 | 含义 |
|---|---|---|
| `inflight` | usize | 当前并发（网关侧正在处理、尚未写出完毕的请求数） |
| `uptime_s` | f64 | 进程运行秒数 |
| `requests_total` | u64 | 进程生命周期内完成的请求总数（不随缓冲淘汰回退） |
| `output_tok_s` | f64 | 10s 滑窗聚合输出 token/s |
| `input_tok_s` | f64 | 10s 滑窗聚合输入 token/s |
| `window_s` | f64 | 滑窗宽度（固定 10） |
| `requests_window` | usize | 窗口内完成请求数 |
| `errors_window` | usize | 窗口内 status≥400 数 |
| `avg_ttft_ms` | f64? | 缓冲内全部带 TTFT 请求的均值（无则 null） |
| `avg_duration_ms` | f64? | 缓冲内请求平均耗时 |
| `tokens_estimated_share` | f64 | 最近 200 条里估算值的占比（0..1），提示数据可信度 |
| `price_in_per_mtok` | f64? | 输入单价（每百万 token），来自 env，未配置为 null |
| `price_out_per_mtok` | f64? | 输出单价（每百万 token），来自 env，未配置为 null |
| `capacity` | usize | 环形缓冲容量 |
| `buffered` | usize | 当前已缓冲条数（UI 水位显示 buffered/capacity） |
| `started_at_ms` | u64 | 进程启动时间戳——**UI 用它检测网关重启**，见 §3 |

实现细节：流式响应对每个网络 chunk 跑一次 `scan_chunk`，先用 `contains("data:")` 做子串门禁再上
serde，保证热路径便宜；识别 OpenAI delta、llama.cpp `{"content":...,"stop":...}`、`[DONE]`、
`finish_reason`、usage（`prompt_tokens`/`input_tokens` 等别名取 max，多次 usage 单调取最大）。
record 一次定稿、之后绝不改动，SSE 与 JSON 快照读的是同一条记录。

## 3. 不落盘：固定容量环形缓冲

- 没有任何磁盘/持久化：一个 `VecDeque` 环形缓冲（默认 **1000** 条，代码下限 16）加两个滑窗事件队列，
  全部在同一把小锁后面。容量可由 env `LMR_REQUEST_LOG_CAPACITY` 调整（deploy 时在 compose 的
  environment 里给 llm-router 容器设置即可）。
- **进程重启即清空**，`seq` 从 1 重新开始。UI 每轮轮询比对 `stats.started_at_ms`：数值一变即判定
  网关重启——旧 cursor 与旧 SSE 连接全部作废（旧 seq 会与新缓冲撞号），前端清空列表、重置 cursor、
  断开重建 EventSource，并用 `cursor=0` 补拉一次新缓冲。
- 「清空」按钮只清前端视图：后端环形缓冲与 cursor 不动，因此不会把旧数据重复拉回来。
- 传输层设计原因：axum 的 Body 是类型擦除的，下游 router 无法把数据挂回给上游 middleware 读。
  因此 ingest middleware 递给 router 一个 `Ingest` 句柄（按请求 ID 匹配），router 往里填路由决策与
  token 计数，middleware 在响应体写完（或被 drop）后 `finish_with_status` 定稿这一行。
  客户端断开走 `record_aborted`（499 行）。被限流/鉴权拒绝的请求同样有行，因为静态字段在进 router 前就已捕获。

## 4. token 估算

上游不发 usage 时（llama.cpp 不显式要求 `stream_options` 就没有 usage），网关按**已流出的输出
文本长度**估算 completion_tokens：

```
tokens ≈ ascii字符数 / 4 + 宽字符(非ASCII)数 / 1.4
```

拉丁文本约 4 字符/token、CJK 约 1.4，分开计数避免中文严重低估。此时 `tokens_estimated=true`，
UI 在该行的 Token 数、tok/s 前加灰色 `≈` 标记，详情里注明「token 数由输出文本长度估算
（上游未返回 usage）」，且 `/_ui/stats` 的 `tokens_estimated_share` 会抬高作为整体提示。
文本累计上限 2MB；只有真正流过文本的流式请求才估算，非流式 JSON 走 usage 解析
（`observe_json_body` 同时抽 `choices[].message.content` 与 `reasoning_content` 备用）。
prompt_tokens 无 usage 时不估算（保持 0），因为输入未经过网关的流式通道。

## 5. 费用列 ~$

- 单价来自两个 env（每百万 token 的价格，网关启动时读取）：
  - `LMR_PRICE_IN_PER_MTOK` —— 输入价；
  - `LMR_PRICE_OUT_PER_MTOK` —— 输出价。
- 单行费用 = `prompt_tokens × 输入价 / 1e6 + completion_tokens × 输出价 / 1e6`，显示 4 位小数。
  这是按标价估算，不是实际扣费（表头 title 已注明）。
- **任一价格未配置 → 整列显示灰色「无法估算」**，不猜价。价格从缺到有时（罕见路径）前端整表重建一次刷新该列。
- 有 token 数的行才出数，其余显示「-」。

## 6. webui 升级流程

`ui/` 是预压缩 bundle（llama.cpp 官方发布包 `llama-b11215-ui.tar.gz` 解包），index.html 与
`_app/` 产物都是 SvelteKit "auto generated, do not edit" 的构建输出。用新版 `*-ui.tar.gz` 覆盖 `ui/` 后，
按顺序重跑三个幂等补丁脚本（任何一个 anchor 失配都会 assert 报错退出，不会静默跳过）：

```bash
cd <仓库>
bash watcher/patch_ui.sh          # 1. 相对 API 路径改写为 /_ui/... 绝对前缀
bash watcher/patch_ui_effort.sh   # 2. thinking effort 选择器补丁 + sw.js precache revision 固定
bash watcher/patch_ui_logs.sh     # 3. 向 index.html 注入 logs-inject.js 的 script 标签（defer）
node --check _app/immutable/entry/app.*.js   # 校验 bundle 语法未被补丁破坏
```

`patch_ui_logs.sh` 的机制：在 index.html 最后一个 `</body>` 行前插入一行 tab 缩进的
script 标签（引入 ./logs-inject.js，defer）；已存在 `logs-inject.js` 引用则跳过（幂等）。
`logs.html` 和 `logs-inject.js` 本身**不是**构建产物、也不在 `ui/sw.js` 的 precache 列表里，
升级时只要不整目录删除就自然存活——真正会被覆盖的只有 index.html 里的注入行。

**CI 守卫**：`bash watcher/patch_ui_logs.sh --check`（可带 ui_dir 参数，默认 `<repo>/ui`）只校验不修改，
注入行缺失时退出码 1。把它放进 CI/升级脚本尾部，可保证 Logs 入口不会在某次 webui 升级后悄悄消失。
若上游改版导致导航类名变化，logs-inject.js 的 `NAV_SELECTORS` 首条是实测锚点，并有退化策略
（从 New chat / Settings 按钮向上找同类容器），注入失败也只是入口缺失、不影响聊天页。

## 7. 与 Prometheus 的关系

两套遥测互补，不互相替代：

- `--prometheus-port 29000` 暴露的 `/metrics` 是面向采集系统的长期时序（直方图、计数器），
  进 Prometheus/Grafana 做历史曲线与告警；
- `/_ui/stats` 是面向 UI 的**轻量实时快照**：一次锁内读就返回全部摘要字段，专为 1s 轮询设计，
  只有 10 秒滑窗视角，无历史。排查「此刻这台 router 上发生了什么」用 Logs 页更直接——
  它还能看到 Prometheus 没有的逐请求路由决策（selected/candidates/effort 改写/会话指纹）。
- watcher 另有自己的 `:9912/metrics`（增删/发现 gauge），与此无关。

## 8. 内存上界估算

总量 ≈ `capacity × 平均 record 大小`，两个滑窗事件队列只是 (Instant,u64) 元组，量级可忽略。

一条 `RequestRecord` 的骨架：23 个字段里占大头的是几个 String——method/path/endpoint
（几十字节）、id（20B）、session（64B hex）、model/provider/worker/selected（各几十字节）、
candidates 数组（每候选一个 URL 字符串，通常几条到十几条）。按经验值一条 **0.5–2KB**，极端
（长路径、多候选、错误摘要 300B）按 4KB 计。默认 capacity=1000 时：

```
1000 × 2KB ≈ 2MB，上界 1000 × 4KB ≈ 4MB
```

对一个转发超长上下文请求的网关（单请求 body 就可能上百 MB）来说，这个常驻量完全无感。
安全的原因有三：容量是**硬编码上界**且环形缓冲覆盖最旧条目（内存随时间不增长）；record 定稿后不再
改动，clone 进 SSE 扇出后原对象即可释放；估算用的文本累积有 2MB 硬顶、且只活在单请求的 `Ingest`
里，请求结束随 Arc 析构。把 `LMR_REQUEST_LOG_CAPACITY` 调到 10000 也只是 ~20–40MB 量级。

## 部署速记

`deploy/docker-compose.yml` 里 llm-router 以 host 网络跑在 :8800，镜像内置 webui 于
`/usr/local/share/llama-ui`（CMD 已带 `--ui-dir`，compose 部署免费获得 /_ui/）。
访问入口：`http://<host>:8800/_ui/`（聊天）与 `http://<host>:8800/_ui/logs.html`（Logs）。
如需成本列与自定义容量，在 llm-router 服务加 environment：

```yaml
    environment:
      LMR_PRICE_IN_PER_MTOK: "0.4"
      LMR_PRICE_OUT_PER_MTOK: "1.6"
      LMR_REQUEST_LOG_CAPACITY: "2000"
```

注意 http 明文访问时 `navigator.serviceWorker` 不可用，sw.js 旧缓存不会挡住新加的
logs.html / logs-inject.js；若将来改 https 或 localhost 访问，改完 index.html 需同步重算
`ui/sw.js` 里对应条目的 revision。
