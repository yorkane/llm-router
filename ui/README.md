# llama.cpp webui bundle

来自 llama.cpp 官方发布包 `llama-b11215-ui.tar.gz`，解包后就地打两处补丁：

1. `watcher/patch_ui.sh` —— 把相对 API 路径（./v1/... ./props）改写成绝对 /_ui/... 前缀，
   让静态 ServeDir 下的页面能命中 router 的 /_ui 别名路由。
2. `watcher/patch_ui_effort.sh` —— 给思考强度（thinking effort）下拉扩展取值：
   在官方 default/off/low/medium/high/max 之外补 none / xhigh / ultra（枚举 + 选项数组 + 预算表三处），
   使 router 能把这些 effort 透传给后端。router 侧 `clean_ui_effort` 会把空串/null 归一为缺省。

升级方法：用新版 *-ui.tar.gz 覆盖 ui/ 后，依次重跑：

```bash
bash ../watcher/patch_ui.sh ui
bash ../watcher/patch_ui_effort.sh ui
node --check _app/immutable/bundle.*.js   # 校验补丁未破坏语法
```

两个脚本都幂等；若上游改了混淆变量名导致 anchor 失配，脚本会 assert 失败（而非静默跳过）。
