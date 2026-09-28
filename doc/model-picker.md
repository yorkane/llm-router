# WebUI 模型切换（router 模式）

> 2026-09-28。让 /_ui/ 聊天界面像单机 llama.cpp 一样列出并切换 router 后面的所有模型，用于快速对比测试。

## 背景

llama-ui 的模型选择器只在 /props 返回 `role:"router"` 时出现（`isRouterMode`）。本仓库的网关虽然本质是 router，但 /props 会代理到某个 llama.cpp worker，拿到的是 `role:"model"`，导致界面只显示单模型、没有切换入口。

## 实现（gateway/src/server.rs）

不改 llama-ui 的 bundle，全部在网关侧补齐协议：

1. **强制 router 角色**：`ui_props_with_role()` 在 `_ui_props` 链中把 /props 的 `role` 覆盖为 `router`（worker 代理分支与兜底分支都覆盖）。
2. **模型清单** `GET /_ui/v1/models`：聚合注册表中所有 worker 的模型并去重，每项返回 `status:{value:"loaded"}`，界面显示为已加载（绿点）。
3. **加载** `POST /_ui/models/load`：恒返 `{"success":true}`。模型由各实例常驻提供，切到清单内的模型时 UI 直接选用，不发 load；对外部未加载模型的 load 也只是空确认，实际请求仍由 router 按模型名路由。
4. **卸载** `POST /_ui/models/unload`：返回 400 + 中文错误「模型由 router 后的实例常驻提供，聊天界面不能卸载；要摘除请在 watcher / 服务侧操作」。生命周期归 watcher 管。
5. **SSE** `GET /_ui/models/sse`：长连接 + 30s keepalive ping，不主动推送（模型清单变化由 watcher→router 注册表决定，UI 重新打开选择器即可见）。

以上四个路由都在 `ui_api_routes()` 内，与其余 /_ui/ API 同一层鉴权。

URL 拼接之所以无需改 bundle：SvelteKit base 在运行时取 `location.pathname` 推导（index.html 的 `__sveltekit_*.base`），从 /_ui/ 进入后所有 `fetch("/v1/models")`、`"/models/load"` 等相对 base 自动变成 `/_ui/...`；service worker 的 root-anchored 路由不拦截 /_ui/。

## 环境变量

| 变量 | 默认 | 说明 |
|---|---|---|
| `LMR_UI_ROUTER_MODE` | 开（未设置即开） | 设为 `false`/`0`/`off` 关闭：/props 不再强制 role=router，模型下拉退化为单 worker 的 /v1/models 直代理。多实例共UI/排查场景用。 |

## 行为与限制

- 切换模型后界面 pill 与后续请求的 `model` 字段即变，router 按 cache_aware 策略路由到对应 worker；Logs 里能看到 worker 列随之变化。
- 每模型的 /props（`/_ui/props?model=<id>`）沿用原过滤逻辑，thinking 下拉、上下文上限等仍按该模型的 worker 报告 + LMR_MODEL_CTX 覆盖。
- 清单只含「已注册且健康」的模型；worker 被熔断摘除后稍等一轮 registry 刷新即从列表消失。
- 卸载按钮报错是设计如此，不是 bug。
