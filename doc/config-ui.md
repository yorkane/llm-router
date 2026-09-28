# llm-router Config 页 / runtime-config：热改思考强度与上下文上限

对应实现：`gateway/src/runtime_config.rs`（策略与进程内存储）、`gateway/src/server.rs`
（`/_ui/config*` 路由与 `/_ui/props` 的 n_ctx 改写）、`ui/config.html`（前端单文件页面）、
`watcher/README.md`（模型改名控制面）。

## 1. 功能概述

- **热改不重启**：三件事都存在进程内的 `RuntimeConfigStore`（`OnceCell` 单例 + `RwLock`）：
  - **默认思考强度** `default_effort`：请求未带 `reasoning_effort` 时由网关注入；
  - **effort 映射** `effort_map`：命中 `from` 的请求把 `reasoning_effort` 改写为 `to`（如 `high→xhigh`）；
  - **按模型上下文上限** `model_ctx`：钳制该模型请求里的 `max_tokens` / `max_completion_tokens`。
  Config 页/API 改动立即生效，不需要重启。
- **模型改名另走一路**：`original:new` 改名不归 runtime-config 管，Config 页把请求原样转发给
  llm-watcher 的 `/model-map` 控制面；watcher 把表存进 ledger，下一轮 reconcile 把它拥有的
  worker 按新名删除重注册（见 §5）。网关只代理，不存改名表。
- **页面入口**：`/_ui/config.html`（`ui/` 整目录随镜像 `COPY ui/ /usr/local/share/llama-ui`）。
  `ui/logs-inject.js` 在官方 webui 左侧导航注入「Config」按钮（与 Logs 同一脚本、同一套
  MutationObserver 防丢机制）；页面在 `/_ui/config.html`，因此页内 `./config` 即 `/_ui/config`。
  页面四张卡片：思考强度（默认档 + 映射行编辑）、上下文上限、模型名映射、环境默认值（只读折叠）；
  顶部状态条显示 watcher 未配置（灰）/在线（绿）/不可达（红）。

## 2. 环境变量基线与优先级

| 环境变量 | 含义 | 示例 |
|---|---|---|
| `LMR_DEFAULT_EFFORT` | 请求未带 `reasoning_effort` 时注入的默认档位 | `high` |
| `LMR_EFFORT_MAP` | effort 改写表，`from:to` 条目，逗号/分号/换行分隔 | `high:xhigh,low:medium` |
| `LMR_MODEL_CTX` | 按模型上下文上限，`model:tokens` 条目，分隔同上 | `qwen3-32b:32768,glm:8192` |
| `LMR_WATCHER_URL` | llm-watcher 控制面 base URL（metrics 端口），改名走它 | `http://127.0.0.1:9912` |

解析规则（`runtime_config.rs`）：档位 trim + 小写后必须落在 `EFFORT_LEVELS`
`none minimal low medium high xhigh max ultra`（8 档）内；`null`、`default`、空串一律视为
「不设置」；`LMR_MODEL_CTX` 的 token 数必须解析为正整数，0 或空模型名整条丢弃；
`LMR_WATCHER_URL` 去首尾空白与尾部 `/`，空串视为未配置。

**优先级与持久化语义：env 是启动基线**，进程启动时装入一次 store；之后所有 UI/API 改动
**只改内存、不落盘，重启后以 env 为准**。GET `/_ui/config` 的 `env_defaults` 字段回显启动时的
env 快照（Config 页只读面板），方便对照「当前值 vs 启动值」的漂移。唯一持久化的是模型改名
（存在 watcher ledger，见 §5）。

## 3. API 契约

成功响应（除 model-map）都是改完后的**最新完整 GET 结构**，前端整页替换刷新；
校验失败一律 **400 + `{"error": "..."}`**（error 原文直接展示在 toast 里）。

### GET /_ui/config

```json
{
  "default_effort": "high",
  "effort_map":  [{"from": "high", "to": "xhigh"}],
  "model_ctx":   [{"model": "qwen3-32b", "ctx": 32768}],
  "env_defaults": {"default_effort": null, "effort_map": [], "model_ctx": []},
  "watcher": {"url": "http://127.0.0.1:9912", "reachable": true, "model_map": {"a.gguf": "a"}}
}
```

`watcher.model_map` 来自实时拉取 watcher（3s 超时），失败时 `reachable:false`、`model_map:null`。

### POST /_ui/config/effort（POST /_ui/config 等价）

```bash
curl -s -X POST http://127.0.0.1:8800/_ui/config/effort \
  -H 'Content-Type: application/json' \
  -d '{"default_effort":"high","effort_map":[{"from":"high","to":"xhigh"}]}'
```

- `default_effort`：字符串或 `null`（`null`/空串 = 恢复「跟随后端」，不注入）；非法档位 400。
- `effort_map`：`[{from,to}]`，**提交的是完整列表（整体替换，不是增量）**；`to` 空串 = 删除该映射；
  `from`/`to` 任一不是合法档位 400。

### POST /_ui/config/ctx

```bash
curl -s -X POST http://127.0.0.1:8800/_ui/config/ctx -d '{"model":"qwen3-32b","ctx":32768}'
curl -s -X POST http://127.0.0.1:8800/_ui/config/ctx -d '{"model":"qwen3-32b","ctx":null}'  # 清除
```

- `model` 必填非空；`ctx` 为正整数或 `null`。**`ctx` 缺省等价 `null`，同样表示清除**；
  `ctx:0` 400（`ctx must be greater than zero`）。

### POST /_ui/config/model-map

```bash
curl -s -X POST http://127.0.0.1:8800/_ui/config/model-map -d '{"map":"qwen3-32b:qwen3-32b-prod"}'
```

body 原样转发给 watcher；watcher 校验失败/不可达时把它的 error 包进响应回传。
- **503**：网关未配置 `LMR_WATCHER_URL`（`watcher not configured (set LMR_WATCHER_URL)`）；
- **502**：watcher 不可达或拒绝了请求（`watcher said <status>: <detail>`）。
成功响应是 watcher 的返回体加上 `"ok": true`。

## 4. 生效链路

两个 router 在选完 worker、把请求序列化成 JSON payload 后套用策略（先 effort 后 ctx）：

- **openai router**：`route_chat`（`/v1/chat/completions`）与 `route_responses`（`/v1/responses`）
  调用 `apply_effort_policy` + `apply_ctx_cap`；
- **http router**：所有走 `route_typed_request_once` 的端点（`/v1/chat/completions`、
  `/v1/completions`、`/v1/responses` 等）同样生效；**原生 `/generate` 显式跳过两者**
  （它用的是不同的采样字段，不做改写）。

effort 重写规则（`apply_effort_policy`）：请求带 `reasoning_effort`（trim 后非空）→ 查
`effort_map`，命中则替换为 `to`，未命中原样放行；没带 → 注入 `default_effort`；两者皆无 →
字段原样不动。映射匹配是**精确匹配**请求里的原始字符串（仅 trim，区分大小写）。

上下文钳制（`apply_ctx_cap`）：对 `max_tokens` 与 `max_completion_tokens` 两个字段，
**缺失或大于 cap 都写入 cap**——即设置上限后，不带 `max_tokens` 的请求也会被注入
`max_tokens=cap`；没有配置 cap 的模型完全不动。

配套一致性：

- `/_ui/props` 的 `ui_props_with_ctx` 把该模型 props 的 `n_ctx` 改写为 cap，并把
  `n_ctx_train` 抬到 `max(原值, cap)`，webui 上下文滑块的上界与请求钳制一致；
- Logs 页每行记录 `requested_effort` / `effort` 两个字段，改写发生时显示 `high → xhigh`
  （见 [logs-ui.md](logs-ui.md)）。

## 5. 模型改名（watcher 代理）

改名由 llm-watcher 落地：POST `/model-map` 的表存进 watcher ledger（**重启存活**），
watcher 下一轮 reconcile 把它拥有的 worker 删除并按新名重注册；protected worker 不动；
in-flight 请求不重放。优先级：watcher `POST /model-map` > `--model-map` 启动参数 > env。

Config 页行为：watcher 未配置或不可达时，改名卡片的输入框和按钮禁用并给出提示；
应用成功后页面自动重拉整页（改名会连带改变 `/v1/models` 列表）。

## 6. 已知限制

- `/generate` 端点不改写 effort、也不钳制上下文；
- **UI 配置不持久化**：重启后回落 env，只有模型改名（watcher ledger）留存；
- 无鉴权：与 `/_ui` 其它路由同一信任假设（可信局域网部署；聊天 API 别名同样无鉴权），
  不要把 :8800 暴露到公网；
- 覆盖面：openai router 的 legacy `/v1/completions` 本身未实现（Router trait 默认 501），
  该端点的改写只在 http router 模型上成立；openai router 覆盖 chat + responses；
- grpc/harmony 路由与 mesh 路由未接入 runtime-config（全仓库调用点仅 openai/router.rs
  与 http/router.rs）；
- 实现细节：env `LMR_EFFORT_MAP` 的 `to` 值不做档位校验（trim 后原样保存），而 API 路径
  会校验——写 env 时自行保证是合法档位。

## 7. 部署速记

`deploy/docker-compose.yml` 的 llm-router 服务（host 网络 :8800）加 environment 即可；
watcher 与 router 同项目同 host 网络，控制面就是 watcher 的 metrics 端口：

```yaml
    environment:
      LMR_DEFAULT_EFFORT: "high"
      LMR_EFFORT_MAP: "high:xhigh,low:medium"
      LMR_MODEL_CTX: "qwen3-32b:32768"
      LMR_WATCHER_URL: "http://127.0.0.1:9912"
```

env 只兜底「重启后的默认值」；日常调整全部可以在 Config 页完成。改动的逐请求效果
（requested vs effective）到 Logs 页核对。
