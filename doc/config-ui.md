# llm-router Config 页 / runtime-config：热改思考强度与上下文上限

对应实现：`gateway/src/runtime_config.rs`（策略与进程内存储）、`gateway/src/server.rs`
（`/_ui/config*` 路由与 `/_ui/props` 的 n_ctx 改写）、`ui/config.html`（前端单文件页面）、
`watcher/README.md`（模型改名控制面）。

## 1. 功能概述

- **热改不重启**：所有配置都在进程内的 `RuntimeConfigStore`（`OnceCell` 单例 + `RwLock`）：
  - **默认思考强度** `default_effort`：请求未带 `reasoning_effort` 时由网关注入；
  - **effort 映射** `effort_map`：命中 `from` 的请求把 `reasoning_effort` 改写为 `to`（如 `high→xhigh`）；
  - **按模型上下文上限** `model_ctx` 与 **按模型强制强度** `model_effort`：两个按模型名建键的全局表（legacy，
    保留 env 基线与旧 API 兼容；card 会覆盖/清除它们，见下）；
  - **每模型配置卡** `model_configs`：每个模型一张 `ModelConfig` 卡，含四个字段——
    `ctx`（上下文上限）、`default_effort`（该模型默认强度）、`effort_map`（模型专属强度映射）、
    `modalities`（能力白名单），语义见 §3。卡是当前主要配置面。
  Config 页/API 改动立即生效，不需要重启。
- **模型改名另走一路**：`original:new` 改名不归 runtime-config 管，Config 页把请求原样转发给
  llm-watcher 的 `/model-map` 控制面；watcher 把表存进 ledger，下一轮 reconcile 把它拥有的
  worker 按新名删除重注册（见 §6）。网关只代理，不存改名表。
- **页面入口**：`/_ui/config.html`（`ui/` 整目录随镜像 `COPY ui/ /usr/local/share/llama-ui`）。
  `ui/logs-inject.js` 在官方 webui 左侧导航注入「Config」按钮（与 Logs 同一脚本、同一套
  MutationObserver 防丢机制）；页面在 `/_ui/config.html`，因此页内 `./config` 即 `/_ui/config`。
  表单视图四个区：思考强度（全局默认档 + 映射行编辑）、模型配置（每模型一张卡，见 §5）、
  模型名映射、环境默认值（只读折叠）；顶部另有「表单 / JSON」双视图切换（见 §5）。
  顶部状态条显示 watcher 未配置（灰）/在线（绿）/不可达（红）。

## 2. 环境变量基线与优先级

| 环境变量 | 含义 | 示例 |
|---|---|---|
| `LMR_DEFAULT_EFFORT` | 请求未带 `reasoning_effort` 时注入的默认档位 | `high` |
| `LMR_EFFORT_MAP` | effort 改写表，`from:to` 条目，逗号/分号/换行分隔 | `high:xhigh,low:medium` |
| `LMR_MODEL_CTX` | 按模型上下文上限，`model:tokens` 条目，分隔同上 | `qwen3-32b:32768,glm:8192` |
| `LMR_MODEL_EFFORT` | 按模型**强制**指定思考强度（legacy 表；Config 页保存卡片的 default_effort 会清掉同名条目） | `qwen3-32b:high` |
| `LMR_MODEL_EFFORT_MAP` | 按模型思考强度映射，`model:from>to` 条目，多条用逗号/分号/换行分隔 | `qwen3-32b:high>xhigh,gpt:minimal>low` |
| `LMR_MODEL_MODALITIES` | 按模型能力白名单，`model:text+image`（级别 `text`、`image`、`video`、`audio`，`text` 恒含），分隔同上 | `qwen3:text+image,other:text` |
| `LMR_WATCHER_URL` | llm-watcher 控制面 base URL（metrics 端口），改名走它 | `http://127.0.0.1:9912` |

解析规则（`runtime_config.rs`）：档位 trim + 小写后必须落在 `EFFORT_LEVELS`
`none minimal low medium high xhigh max ultra`（8 档）内；`null`、`default`、空串一律视为
「不设置」；`LMR_MODEL_CTX` 的 token 数必须解析为正整数，0 或空模型名整条丢弃；
`LMR_MODEL_EFFORT_MAP` 里 from/to 任一不是合法档位、`LMR_MODEL_MODALITIES` 里的非法级别
整条/逐项丢弃（`text` 恒被补上）；`LMR_WATCHER_URL` 去首尾空白与尾部 `/`，空串视为未配置。

`LMR_MODEL_EFFORT_MAP` / `LMR_MODEL_MODALITIES` 装进 store 后成为每模型卡的
`effort_map` / `modalities` 字段（见 §3）。env 里的 per-model 条目目前只有这四个：
`LMR_MODEL_CTX`（model:tokens）、`LMR_MODEL_EFFORT`（model:effort）、
`LMR_MODEL_EFFORT_MAP`（model:from>to）、`LMR_MODEL_MODALITIES`（model:cap+cap）；
不存在其它写法（尤其没有 `model:from:to` 形式的 effort 映射）。

**优先级与持久化语义：env 是启动基线**，进程启动时装入一次 store；之后所有 UI/API 改动
**只改内存、不落盘，重启后以 env 为准**。GET `/_ui/config` 的 `env_defaults` 字段回显启动时的
env 快照（Config 页只读面板），方便对照「当前值 vs 启动值」的漂移。唯一持久化的是模型改名
（存在 watcher ledger，见 §6）。

## 3. API 契约

成功响应（除 model-map）都是改完后的**最新完整 GET 结构**，前端整页替换刷新；
校验失败一律 **400 + `{"error": "..."}`**（error 原文直接展示在 toast 里）。

### GET /_ui/config

```json
{
  "default_effort": "high",
  "effort_map":  [{"from": "high", "to": "xhigh"}],
  "model_ctx":   [{"model": "qwen3-32b", "ctx": 32768}],
  "model_effort": [{"model": "glm-4", "effort": "none"}],
  "model_configs": [
    {"model": "qwen3-32b", "ctx": 32768, "default_effort": null,
     "effort_map": [{"from": "high", "to": "xhigh"}], "modalities": ["text", "image"]}
  ],
  "models": [
    {"model": "qwen3-32b", "registered": true,
     "sources": ["http://127.0.0.1:8800", "http://217.t:8200"],
     "ctx": 32768, "default_effort": null,
     "effort_map": [{"from": "high", "to": "xhigh"}], "modalities": ["text", "image"]}
  ],
  "env_defaults": {"default_effort": "high", "effort_map": [...], "model_ctx": [...],
                   "model_effort": [...], "model_configs": [...]},
  "watcher": {"url": "http://127.0.0.1:9912", "reachable": true, "model_map": {"a.gguf": "a"}}
}
```

- `model_configs`：每模型配置卡（`ModelConfig`，见下），`ctx`/`default_effort`/
  `modalities` 为 `null` 表示「跟随后端/全局」，`effort_map` 恒为数组（可为空）。
- `models`：worker 注册的模型 ∪ 配置卡的并集（按模型名排序）。每个条目字段：
  `model` / `registered`（是否被某个 worker 注册）/ `sources`（承接该模型的 worker URL，
  响应里为完整 URL，前端卡片的来源列压缩成 `:port`/host 显示，未注册时为空数组）/
  `ctx` / `default_effort` / `effort_map` /
  `modalities`（后四个取自配置卡，无卡时为 null/空数组）。**同名模型跨 provider 合并为一张卡**，
  `sources` 列出所有来源——这是有意设计，跨 provider 同名共享一张卡。
- `env_defaults`：启动时 env 快照（含 `model_configs`），供只读面板对照漂移。
- `watcher.model_map` 来自实时拉取 watcher（3s 超时），失败时 `reachable:false`、`model_map:null`。

### 每模型配置卡 ModelConfig 语义

| 字段 | 语义 |
|---|---|
| `ctx` | 该模型的上下文上限（token）。钳制 `max_tokens` / `max_completion_tokens`（见 §4 生效链路）；取值优先于 legacy `model_ctx` 表（`ctx_cap` 先查卡再查旧表）。 |
| `default_effort` | 该模型的默认思考强度：请求未带强度、卡内映射未命中、或请求强度非法时使用。 |
| `effort_map` | 该模型专属的强度映射（`requested → replacement`），在**全局** `effort_map` 之前检查。 |
| `modalities` | 能力白名单（`text`/`image`/`video`/`audio` 子集，`text` 恒含）。影响 `/_ui/props` 的 `modalities` 探测：显式卡覆盖 worker 自报——取消勾选「图片」后 webui 客户端就不再附加图片，而不是把图发给读不懂的模型。 |

### 强度解析顺序（`request_effort_for`）

对每个请求，生效强度按以下优先级确定（高 → 低）：

1. **legacy 强制项** `model_effort`（env `LMR_MODEL_EFFORT`）：该模型存在强制项时直接覆盖一切；
   Config 页保存卡的 `default_effort` 会清掉同名强制项，所以卡保存后它就是单一事实源；
2. **模型卡**：卡内映射命中 → 用映射值；卡有映射或卡默认、但请求强度未命中/非法 → 用卡默认
   （卡有映射而无卡默认时，未命中回落请求原档位）；卡映射为空且无卡默认 → 回落全局；
3. **全局 `effort_map`**：命中请求强度则改写；
4. **全局 `default_effort`**：请求未带强度时注入；两者皆无则字段原样不动。

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

 这是 legacy 端点：卡片的 `ctx` 字段优先级更高（`ctx_cap` 先查卡再查旧表），
 编辑卡片 ctx 会清掉旧表里的同名条目。

### POST /_ui/config/model

```bash
curl -s -X POST http://127.0.0.1:8800/_ui/config/model \
  -d '{"model":"qwen3-32b","ctx":32768,"default_effort":"high","modalities":["text","image"]}'
curl -s -X POST http://127.0.0.1:8800/_ui/config/model -d '{"model":"qwen3-32b","remove":true}'
```

单卡补丁，成功响应为**含 `models` 的最新完整 GET 结构**。字段语义：

- **缺省 = 不动该字段**；`null` = 清除（ctx 上限解除 / default_effort 回落全局 /
  modalities 回到自动探测）；
- `effort_map` 为完整列表（整体替换）；`to` 空串 = 删除该行；`modalities` 为空数组
  = 「只保留 text」（显式），与 `null` 的「自动探测」不同；
- `remove: true` = 删除整条卡，**连带清掉旧 `model_ctx` / `model_effort` 里同名条目**；
- 编辑 `default_effort` 会清掉旧 `model_effort` 的同名强制项、编辑 `ctx` 会清掉旧
  `model_ctx` 的同名项——保存过的卡就是该模型的单一事实源；
- `model` 必填非空；校验失败 400 + `{"error": "..."}`。

### POST /_ui/config/apply

```bash
curl -s -X POST http://127.0.0.1:8800/_ui/config/apply \
  -d '{"default_effort":"high","effort_map":[{"from":"high","to":"xhigh"}],
       "model_ctx":[{"model":"qwen3-32b","ctx":32768}],"model_effort":[],
       "model_configs":[{"model":"qwen3-32b","default_effort":null,
                         "effort_map":[],"modalities":["text","image"]}],
       "model_map":{"a.gguf":"a"}}'
```

**整文档替换**（JSON 视图「校验并应用」走这里）：五个可编辑段（`default_effort` /
`effort_map` / `model_ctx` / `model_effort` / `model_configs`）从 payload 重建，
**缺省字段 = 清空**，因此提交的 JSON 是完整事实源而不是补丁。validate-then-write：
先整体校验进一个 detached 配置，任一处非法（非法档位、ctx 非正、条目缺 model 等）
全部拒绝 400，不会半提交。

`model_map` 段是可选附加项：从 body 剥离后原样转发 watcher（对象或 `orig:new,...` 字符串
均可），让一次粘贴也能落地改名。转发**失败不回滚配置**——响应仍是 200 的完整文档，
但带 `"warning"` 字段（`model_map not applied: ...`；watcher 未配置时为
`watcher not configured; model_map not applied`），前端以 toast 提示；成功时响应附加
`watcher_model_map` 为 watcher 返回体。

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

effort 重写（`apply_effort_policy`）：按 §3「强度解析顺序」取出生效档位——legacy
`model_effort` 强制项 > 模型卡（卡内映射 → 卡默认）> 全局 `effort_map` → 全局
`default_effort`；取到就写回 `reasoning_effort`，什么都取不到则字段原样不动。

上下文钳制（`apply_ctx_cap`）：cap 取 `ctx_cap(model)`（卡片 ctx 优先，其次 legacy
`model_ctx` 表）。对 `max_tokens` 与 `max_completion_tokens` 两个字段，
**缺失或大于 cap 都写入 cap**——即设置上限后，不带 `max_tokens` 的请求也会被注入
`max_tokens=cap`；没有配置 cap 的模型完全不动。

能力探测（`ui_props_with_modalities`）：`/_ui/props` 按模型的 `modalities` 卡改写
`modalities` 字段（`image`/`video` → `vision`，`audio` 直映）；卡未配置时保持 worker
自报，worker 连 `modalities` 都不报（vLLM/SGLang 没有 /props）时默认广播 `vision:true`——
让 webui 不再静默吞掉图片附件，纯文本模型在引擎侧显式报错。取消勾选「图片」则反过来
生效：UI 客户端直接不再附加图片。

配套一致性：

- `/_ui/props` 的 `ui_props_with_ctx` 把该模型 props 的 `n_ctx` 改写为 cap，并把
  `n_ctx_train` 抬到 `max(原值, cap)`，webui 上下文滑块的上界与请求钳制一致；能力
  字段同路由改写（见上）；
- Logs 页每行记录 `requested_effort` / `effort` 两个字段，改写发生时显示 `high → xhigh`
  （见 [logs-ui.md](logs-ui.md)）。

## 5. Config 页 UI

顶部「表单 / JSON」分段按钮切换两个视图，共享同一份 state（最近一次服务器响应）；
写操作成功后用响应体整体重建 state，未保存的本地编辑按草稿保留。

**表单视图**

- **思考强度（全局）**：默认档下拉（含「不注入」）+ 全局映射行编辑（整体替换语义）。
- **模型配置**：每个模型一张卡（来自 GET 的 `models` 数组，已注册模型排前、按名排序）：
  - **上下文长度**：数字输入，留空 = 不限制（跟随上游）；
  - **默认思考强度**：下拉 = 跟随全局 / 不注入 (null) / 8 个档位；
  - **思考强度映射**：该模型专属的 `from → to` 行编辑（整体替换语义）；
  - **能力启用**：三态——「自动探测（跟随上游）」勾选时手动项禁用；取消自动后图片/
    视频/音频可手动勾选（文本恒开且不可取消）。
  卡头显示模型名、「已注册 / 自定义」徽标、来源端口列表（`host:port` 压缩显示）与
  「未保存」脏标；卡脚「保存此模型」一次提交全部字段，「删除此模型配置」走
  `remove: true`（confirm 后删卡并连带清旧表）。
- **「+ 添加自定义模型」**：按名建一张本地草稿卡（`registered:false`），保存该模型前
  不会写后端；模型名下拉来自 `GET ./v1/models`（拿不到时退化为纯手工输入）。
- **模型名映射** / **环境默认值（只读）**：行为不变（见 §2 / §6）。
- 后端旧到 GET 响应没有 `models` 字段时，卡片区显示升级提示（请升级 llm-router），
  其余区域照常工作。

**JSON 视图**

- 编辑的是**可写子集**：`default_effort` / `effort_map` / `model_ctx` / `model_effort` /
  `model_configs` / `model_map`（派生字段 `watcher` / `env_defaults` / `models` 不出现）。
  内容等价于容器环境变量，可导出后粘进 compose 固化。
- 「校验并应用」走 `POST ./config/apply`：前端先本地 `JSON.parse`，**解析失败不发请求**
  （错误信息带行列号）；提交时只透传六个可写字段（缺省 = 整体替换语义下视为清空）。
- 应用失败（400/网络错误）保留草稿文本，可在原编辑内容上继续修改重试；成功则丢弃草稿
  以响应重建，表单视图同步刷新；响应带 `warning`（如 model_map 未转发成功）时 toast 提示。
- 「复制」复制当前编辑文本；「重新载入」用服务器状态覆盖草稿；切回表单视图时若有未
  应用的 JSON 编辑会先 confirm（确定 = 丢弃编辑）。

## 6. 模型改名（watcher 代理）

改名由 llm-watcher 落地：POST `/model-map` 的表存进 watcher ledger（**重启存活**），
watcher 下一轮 reconcile 把它拥有的 worker 删除并按新名重注册；protected worker 不动；
in-flight 请求不重放。优先级：watcher `POST /model-map` > `--model-map` 启动参数 > env。

Config 页行为：watcher 未配置或不可达时，改名卡片的输入框和按钮禁用并给出提示；
应用成功后页面自动重拉整页（改名会连带改变 `/v1/models` 列表）。

直连 watcher 本身的同名控制面（`GET`/`POST /model-map` 的 4 种 body 形态、返回体、生效时机与历史坑的自查/自清命令）见 `watcher/README.md` 的 `Renaming model ids (model map)` 一节。

## 7. 已知限制

- `/generate` 端点不改写 effort、也不钳制上下文；
- **UI 配置不持久化**：重启后回落 env，只有模型改名（watcher ledger）留存；
- **legacy 表与卡的并存**：`model_effort` / `model_ctx` 旧表仍在（env 基线 + 旧 API
  兼容），但卡片字段优先（强度见 §3 顺序、ctx 见 `ctx_cap`）；保存卡的 `default_effort` /
  `ctx` 会自动清掉旧表同名条目，`remove: true` 一并清除。
- 无鉴权：与 `/_ui` 其它路由同一信任假设（可信局域网部署；聊天 API 别名同样无鉴权），
  不要把 :8800 暴露到公网；
- 覆盖面：openai router 的 legacy `/v1/completions` 本身未实现（Router trait 默认 501），
  该端点的改写只在 http router 模型上成立；openai router 覆盖 chat + responses；
- grpc/harmony 路由与 mesh 路由未接入 runtime-config（全仓库调用点仅 openai/router.rs
  与 http/router.rs）；
- 实现细节：env `LMR_EFFORT_MAP` 的 `to` 值不做档位校验（trim 后原样保存），而 API 路径
  会校验——写 env 时自行保证是合法档位。

## 8. 部署速记

`deploy/docker-compose.yml` 的 llm-router 服务（host 网络 :8800）加 environment 即可；
watcher 与 router 同项目同 host 网络，控制面就是 watcher 的 metrics 端口：

```yaml
    environment:
      LMR_DEFAULT_EFFORT: "high"
      LMR_EFFORT_MAP: "high:xhigh,low:medium"
      LMR_MODEL_CTX: "qwen3-32b:32768"
      LMR_MODEL_EFFORT_MAP: "qwen3-32b:high>xhigh"
      LMR_MODEL_MODALITIES: "qwen3-32b:text+image"
      LMR_WATCHER_URL: "http://127.0.0.1:9912"
```

env 只兜底「重启后的默认值」；日常调整全部可以在 Config 页完成。改动的逐请求效果
（requested vs effective）到 Logs 页核对。
