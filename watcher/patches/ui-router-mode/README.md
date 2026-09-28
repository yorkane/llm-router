# ui-router-mode patch（webui router 模式模型选择器 · role:"router"）

上游同步（`watcher/upstream_sync.sh`）会整棵替换 `gateway/`，本目录的补丁负责把
「webui 多模型选择器 / router 模式」特性原样重放到新的上游树上。写法与
`watcher/patches/runtime-config/` 一致：片段文件是从打过补丁的工作区**逐字节
抽取**的，脚本只做锚点插入/单行改写；锚点不唯一或缺失一律报错退出非 0
（宁可 CI 红，也不能悄悄上线一个 picker 指向死端点的网关）。

## 这块补丁做什么

gateway 顶替 llama.cpp 直接服务 webui 时，默认 /props 会原样透传 worker 的
role（通常是 "model"），UI 因此只显示单模型视图。本补丁让 /props 强制返回
`role:"router"`，UI 头部切换为**多模型选择器**；选择器的 列表 / load / sse /
unload 请求经 SvelteKit base（`/_ui`）落到下面四个端点：

- `/_ui/v1/models`（`ui_models`）：聚合 `worker_registry` 全部 worker 的模型，
  去重后返回 `{"object":"list","data":[{"id","object","created","owned_by",
  "status":{"value":"loaded"}}]}`。每个模型都由已注册 worker 常驻服务，
  所以一律标记 loaded。
- `/_ui/models/load`（`ui_model_load`）：picker 只是切换器不是加载器，
  直接回 `{"success":true}`。
- `/_ui/models/unload`（`ui_model_unload`）：400 + 中文错误
  「模型由 router 后的实例常驻提供，聊天界面不能卸载；要摘除请在 watcher /
  服务侧操作」——摘除实例是 watcher 的职责。
- `/_ui/models/sse`（`ui_models_sse`）：pending 流 + 30s keepalive（"ping"），
  永远不发数据帧，只为阻止 UI 每秒重试。

开关：env `LMR_UI_ROUTER_MODE`，默认**开**。设为 `false` / `0` / `off`（大小写
不敏感）即回到单模型展示，适合 UI 期望自己加载 GGUF 的纯 llama.cpp 实例。

## 文件

| 文件 | 落点 |
| --- | --- |
| `router_mode_fns.rs` | `server.rs`，插在 `ui_props_with_ctx` 的文档注释之前（runtime-config 产物） |
| `router_mode_handlers.rs` | `server.rs`，插在 Logs 路由的文档注释之前（request-log 产物） |
| `routes_models.rs` | `server.rs` `ui_api_routes()` 内，替换 `/v1/models` 一行并新增 3 条路由（ui 产物） |
| `apply_ui_router_mode.py` | 重放脚本（含 /props 两处 role 包裹的锚点改写） |
| `README.md` | 本文件 |

## 如何重放

    python3 watcher/patches/ui-router-mode/apply_ui_router_mode.py gateway watcher/patches/ui-router-mode

幂等：`server.rs` 出现 `fn ui_router_mode(` 即 exit 0。

## 依赖顺序（不能颠倒）

    ui -> request-log -> runtime-config -> ui-router-mode

- ui 补丁产生 `ui_api_routes()`（routes 锚点的宿主）。
- request-log 产生 Logs 路由块（handlers 插入锚点）。
- runtime-config 产生 `ui_props_with_ctx(...)` 包裹：本补丁 /props 两处
  （worker 应答分支 + fallback json! 分支）的锚点都是**包裹之后**的行形，
  必须在它之后跑。
- 中间态可编译性无特殊要求，构建只发生在四个补丁全部落位之后。

## 锚点清单（均要求恰好命中 1 次）

1. `/// Report the configured context cap instead of the worker's raw n_ctx when the`（runtime-config 的 `ui_props_with_ctx` 文档首行）→ 前面插 `router_mode_fns.rs`。
2. 28 空格缩进的 `ui_props_with_thinking(value),`（/props worker 应答分支，runtime-config 包裹后的形态）→ 包一层 `ui_props_with_role(...)`。
3. 五行 fallback 块（`        ui_props_with_thinking(json!({` 起、`"webui_version": "llm-router",` + `        })),` 止，runtime-config 包裹后的形态）→ 同法包裹并改闭合括号 `}))),`。
4. `/// Public (no auth) routes for the Logs page. The page itself is a static asset`（request-log 产物）→ 前面插 `router_mode_handlers.rs`。
5. `        "/_ui/v1/models", get(v1_models))` 整行（ui 补丁 routes.snippet 产物）→ 换成 7 行新路由块（`routes_models.rs`，含 `get(ui_models)` 与 load/unload/sse 三条新路由）。

## 回退语义（LMR_UI_ROUTER_MODE=false）

`ui_router_mode()` 返回 false 时 `ui_props_with_role` 不再改写 role，/props
原样透传 worker 的角色（llama.cpp 即 "model"），UI 回到单模型/自加载视图；
四个 model 端点仍然注册（无人调用），无其他副作用。该开关在每次 /props
请求时读取，改 env 需重启进程才生效（读的是进程 env，不做热加载）。

## 校验

基线取 router-mode 之前的 gateway 树（补丁链的输入态），四个补丁按序重放后
必须与工作树逐字节一致：

    WORK=/data/tmp/pcv2/gateway   # 由 git archive ae9efd7 gateway 还原（router-mode 提交前）
    python3 watcher/patches/ui/apply_ui.py "$WORK" watcher/patches/ui
    python3 watcher/patches/request-log/apply_request_log.py "$WORK" watcher/patches/request-log
    python3 watcher/patches/runtime-config/apply_runtime_config.py "$WORK" watcher/patches/runtime-config
    python3 watcher/patches/ui-router-mode/apply_ui_router_mode.py "$WORK" watcher/patches/ui-router-mode
    diff -r -x target -x LICENSE "$WORK" gateway    # 必须为空（LICENSE 是符号链接，排除）

注：router-mode 改动已随 `feat(ui): webui model switcher via forced router mode`
进入 HEAD，所以整链的干净验证基线是 `ae9efd7`（router-mode 之前）而非 HEAD；
若以 `deee9b4`（裸上游 v0.5.20 同步）为基线，ui 层会幂等跳过、链上其余三层
照常落位。

编译由 upstream_sync.sh / CI 的 `cargo build --profile ci --bin smg` 负责。
