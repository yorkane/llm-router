# llama.cpp WebUI（静态资源）

来源：llama.cpp 官方发布产物 `llama-b11215-ui.tar.gz`
（https://github.com/ggml-org/llama.cpp/releases/tag/b11215 ，2026-09-27）。

本目录内容 = 官方 UI 包原样解包，仅做一处本地补丁（见下方"本地补丁"）。

## 用途

router 以 `--ui-dir <此目录>`（或环境变量 `SMG_UI_DIR`）启动后，在
`/_ui/` 路径提供 llama.cpp 官方 chat 界面，直接复用 router 已注册的
worker/模型做测试；根路径 API 行为不受影响。

## 本地补丁（必须随每次升级重做）

官方 bundle 的 chat/props 等请求是相对路径（`./v1/chat/completions` 等），
挂在 `/_ui/` 前缀下会解析成 `/_ui/v1/...` 而 404。因此构建/打包时需执行：

```bash
bash ../watcher/patch_ui.sh ui    # 或镜像构建时由 Dockerfile 调用
```

补丁内容：将 bundle 中的
`"./v1/chat/completions"`、`"./v1/chat/completions/control"`、
`"./v1/stream"`、`"./v1/streams/lookup"`、`"./props"` 全部改为
`"/_ui/v1/..."` / `"/_ui/props"` 绝对路径；router 侧对这几个路径做了
转发到对应 API handler 的别名路由。

## 升级方法

```bash
V=b<新号>
curl -L -o /data/tmp/$V-ui.tar.gz https://github.com/ggml-org/llama.cpp/releases/download/$V/llama-$V-ui.tar.gz
mkdir -p /data/tmp/uix/$V && tar -xf /data/tmp/$V-ui.tar.gz -C /data/tmp/uix/$V
rsync -a --delete /data/tmp/uix/$V/ ui/   # 注意会删掉本 README，先备份或事后补回
git diff ui/ | head                        # 确认仅版本差异后重跑 patch_ui.sh
```
