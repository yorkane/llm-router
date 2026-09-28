# llm-router watcher 健康检查与剔除逻辑审查报告

> 2026-09-28 只读审查，仓库 /home/aigc/ChatGPT/llm-router。
> watcher 主体单文件 `watcher/llm_watcher.py`（1468 行），router 网关在 `gateway/`（Rust，下文 gw/ 前缀指 gateway/ 下路径）。

## 1. 健康检查的分层与参数

watcher 侧实际是**三层**，外加 router 自带一层（第 5 节详述）。注意：「docker 容器状态扫描」实际**只是 running 过滤**，不读容器的 health 状态。

| 层 | 位置 | 机制 | 间隔 | 超时 | 并发 | 失败阈值（默认值所在行） |
|---|---|---|---|---|---|---|
| A. 发现/准入探测 | `probe_worker` llm_watcher.py:356，每轮 reconcile 调 `probe_all` :707 | 对每个候选依次 GET `/v1/models` + `/server_info` + `/get_server_info` + `/props` + `/metrics` + `/health`，判定「是不是 OpenAI worker」 | 每 `--interval` 15s 全量一轮 :603（`run()` 循环 :1148） | 3.0s/请求 :604（deploy compose 覆盖为 4s，deploy/docker-compose.yml:66） | ThreadPoolExecutor 16 :605、:727 | 无阈值——单轮不达标就不进 desired 集；「消失」判定靠 `missing_since` 宽限期 |
| B. activity probe（推理活性） | `activity_probe` :426，判定 `_note_activity` :879，执行 `_check_activity` :830 | POST `/v1/chat/completions`，`max_tokens=1, temperature=0`，只判池内已注册 worker | 每 worker 60s :621，首轮按 `port % 97` 错开 :860-861 | 15s :622 | 16 :873 | 连续 dead **3** 次 :623；slow（超时）需 3×3=**9** 次 :624，need 计算在 :898 |
| C. docker 容器扫描 | `docker_candidates` :266 | GET unix-socket `/containers/json?filters={"status":["running"]}` :275-277，只取**运行中**容器的发布端口作为候选；容器消失 ⇒ 该 URL 不再被发现 ⇒ 走宽限期剔除 | 同 A（每 15s） | unix socket 5.0s :125、:174 | 单请求 | 消失满 `--remove-grace` **300s** 才删 :617，判定在 :804-815 |

补充：层 A 里 `/health` 的角色是**准入闸门**（决定注册时是否带 `disable_health_check`，:1070），不是持续性存活探测；真正的持续存活判定是 B 层 + router 自带层。

## 2. 判定分类是否严谨

分类集中在 `activity_probe` 的返回（:446-465）：

- **refused / reset / 不可达** → `OSError` → `"dead"` :463
- **5xx** → `"dead"` :454、:456
- **timeout**（`socket.timeout` 或 URLError 包裹的 timeout）→ `"slow"` :458、:461
- **404** → `"unknown"` :456，在 `_note_activity` 中**永不记罚**，只每小时打一条 skip 日志（经 `_note_skip` :345）
- **其他 4xx（含 400）→ `"alive"`** :456

**400 的处理核实**：`exc.code >= 500` 才 dead，`== 404` 才 unknown，其余一律 alive——400 确实**不计为不健康**，而且比"忽略"更强：`"alive"` 会**清零**已累积的 strike 并清掉 `unresponsive_since`（:883-889）。这符合"400 是引擎能正常工作、只是拒绝了参数"的语义，但有个副作用：一个正在死掉的 worker 若恰好因模型 id 改名开始回 400，其 strike 也会被反复清零（见第 7 节建议 6）。

**404 不计失败的 rationale**（docstring :426-443）：404 只说明「引擎没有 chat 端点（如 embedding-only 服务）或拒绝这个 model id」，进程本身在应答，HTTP 层判定它活着，宁可不判。设计上有代价——见第 4 节，model-map 改名后 404 可能变成**永久漏杀通道**。

## 3. 误杀风险与保护链路量化

**场景一：worker 在加载大模型（进程/容器活着，chat 要几分钟才好）。**

- 若 worker **尚未注册进池**（加载未完成，router 的 AddWorker job 还在 processing）：activity probe 只判池内 worker（`_check_activity` 遍历 `actual`，:830），**不会误杀**。但 `add_confirm_timeout` 默认只有 **180s** :629：加载超过 3 分钟，`_reap_pending` :1012-1031 会删掉 parked 的 AddWorker job 并在下一轮重新排队——加载期间形成约每 3 分钟一次的 add/reap 循环。自愈的 churn，不是正确性 bug，但日志会很吵。
- 若 worker **已在池中且容器被重启**（换权重等）：chat probe 立即 connection refused → 每次 `"dead"`，need=3，间隔 60s → **约 3 分钟**（首个 strike 还要等首轮错开延迟，最坏 ~4 分钟）就会触发剔除。此时唯一的救它是 `keep_last_grace`：如果它是该 model 最后一个健康 worker，保护 **1800s** :619，判定在 :914-927。**量化结论：多副本模型中，单副本加载超过 ~3 分钟即被误摘**；剔除后有 add-fail 退避 `min(900, 60×2^(n-1))` :963（首次 60s），加载完成后自动回池。这是全链路最大的误杀窗口，router 侧的 1800s 注册保护管不到「重新加载」，因为 worker 早已在池里。

**场景二：worker 被长请求占满，/health 或 chat 变慢。**

- chat probe 15s 超时 → `"slow"`，need = 3×3 = **9 次** → 约 **9~10 分钟**才剔（:895-906 的注释明确写了这是为了 262K 大上下文深排队的 worker）。
- 但走**发现层**的慢是另一条更短的链：`/v1/models` 在 3s（compose 里 4s）内不应答 ⇒ 该轮不在 desired ⇒ `missing_since` 累计；只要**连续消失满 300s** 且不是最后健康 worker 就删（:804-815）。**量化结论：高负载下 `/v1/models` 持续 3s 超时约 5 分钟，活跃副本也会被摘**（单副本靠 1800s keep-last 保护）。
- 保护链路汇总（默认值）：strike 连续计数 3（dead）/9（slow）→ 每 worker 60s 一次 → 最后健康 worker 额外 1800s 宽限 → 删除前 `router.health()` 不通过则整轮跳过 activity 且**保留 strike**（:834-838，防止 router 重启把全池误杀）→ `--allow-remove false` 只告警不删（:908-912）→ strike 状态**纯内存**，watcher 重启清零（保守方向：不在旧证据上删除，:653-656 注释）→ 恢复即清零并打 "generates again after N failed probe(s)"（:884-885）。

## 4. 漏杀风险（进程活着但推理已死）

| 故障形态 | probe 表现 | 能否抓到 | 时延 |
|---|---|---|---|
| GPU 卡死，请求挂起 | 15s 超时 → slow | 能 | 9×60s ≈ **9 分钟** |
| GPU 错误，引擎快速回 500 | dead | 能 | 3×60s ≈ **3 分钟** |
| 僵尸：/health 200 但 chat 5xx | dead | 能（这正是 activity probe 存在的理由，:426-432） | ~3 分钟 |
| KV cache 死锁，/v1/models 仍 200 | chat 挂起 → slow | 能 | ~9 分钟 |
| 引擎回 **200 + 错误 JSON body**（未用 HTTP 状态码表达错误） | `resp.status < 500` → alive | **不能**，永久漏杀 | — |
| **model-map 改名后** probe 用公共 id 打 worker | llama.cpp 无视 model 名 → 照常 alive；vLLM/sglang 对未知 model 可能回 **404 → unknown → 永不判罚** | **不能**，僵尸模型永久挂在 /v1/models 下 | — |
| worker 无 chat 端点（纯 embedding） | 404 → unknown | 设计上故意不判（该场景本身无法用 chat 验证） | — |

第 6 行具体机制：`_check_activity` 取 `item["model_id"]`（router 里注册的**公共 id**，:852）作为 chat 请求的 `model` 字段；而 `model_map` 改名后（`_model_name` :1035），worker 自己只认原始 id，probe 拿改名后的 id 去打——llama.cpp 接受任意 model 名所以没事，但 vLLM/sglang 会拒绝，若表现为 404 则落入 unknown 永免区。

## 5. 与 router 自带健康检查 / 熔断的关系

**router 侧**（gateway 独立核实）：

- 健康循环 `start_health_checker`（gw/src/core/worker_registry.rs:646-691），默认 interval 60s / timeout 5s / fail 3 / success 2（gw/src/main.rs:411-427；deploy compose 把 interval 设为 30s）。连续失败 ≥3 才 `set_healthy(false)`（gw/src/core/worker.rs:770-776），成功 ≥2 恢复。
- 熔断器（gw/src/core/circuit_breaker.rs:247-255）：**连续**失败 ≥10 开（`--cb-failure-threshold` 10，main.rs:389），open 60s 后 half-open，连续成功 ≥3 关闭；计数依据是**真实请求**结果（4xx 除 408/429 算成功，gw/src/routers/http/router.rs:735-739）。open 期间该 worker 被 policy 过滤，全模型候选空时 503（gw/src/routers/openai/router.rs:315-318）。
- worker 注册窗口 `worker_startup_timeout_secs`=1800s（main.rs:221-222）只管 AddWorker job 轮询时长，**不是**启动期 unhealthy 豁免：worker 一旦创建默认以 unhealthy 出生（gw/src/core/steps/worker/local/create_worker.rs:336-340），靠健康循环转回。

**两层叠加的行为**：

1. **不冲突但职责不同**：router 层只"摘流量不摘人"（unhealthy 可逆）；watcher 层"摘人"（DELETE worker，发现后自动加回）。两者判定完全独立——watcher 的 probe 直连 worker，**不经过 router**，所以既不会污染 router 的 CB 计数，CB open 也不会让 watcher 误杀。
2. **watcher 传的 per-worker 健康参数是死配置**：`_add()` 里随 POST /workers 发的 `health_check_interval_secs/timeout_secs/failure_threshold/success_threshold`（llm_watcher.py:1062-1065）在 router 的 add 路径**全部被忽略**——创建时唯一采纳请求值的是 `disable_health_check`，且是「全局 OR per-worker」（gw/src/core/steps/worker/local/create_worker.rs:273-283）。per-worker 值只有走 `update_worker_properties` 才生效（gw/src/core/steps/worker/local/update_worker_properties.rs:66-85）。也就是说 compose 里的 `--health-check-interval-secs 30` 才是全局唯一节奏。
3. **CB 与 unhealthy 叠加的用户可见行为**：routing 选择统一过滤 `is_healthy() && can_execute()`（gw/src/policies/mod.rs:136-142）。一个真实流量失败 10 次的健康 worker 会被 CB open 60s——期间用户侧 503 或全落其他副本，而 watcher 的 60s activity probe 可能仍判它 alive，两层状态短暂背离（CB 恢复后无残留）。
4. 一个有意的交互：`_is_last_for_model` 用 router 的 `is_healthy` 判"最后健康 worker"（llm_watcher.py:1138-1146）——router 先把另一副本标 unhealthy 会**升级**本 worker 的 keep-last 保护，延长僵尸注册存活时间。

## 6. metrics 与可观测性

watcher 暴露 10 个指标（llm_watcher.py:1189-1198）：`reconciles_total`、`adds_total`、`removes_total`、`discovered_workers`、`owned_workers`、`protected_workers`、`activity_unresponsive`（**当轮**失败 probe 数，每轮重置于 :746）、`activity_removes_total`、`model_map_entries`、`router_reachable`。

能否回答「什么时候摘了谁、为什么」：**Prometheus 回答不了**。`removes_total` 没有 worker/reason 标签，无 last-removal 时间戳，无 per-worker strike gauge；只能靠日志事后定位——日志本身是够的：每次记罚打 `did not answer a chat probe (slow|dead, n/need)`（:904），剔除打 `REMOVED <url> (model 'X'): <N> chat probes in a row went unanswered (last: …)` 且含 keep-last 原因（:964-967），恢复打 "generates again after N failed probe(s)"。另一个语义瑕疵：`router_reachable` 由 `stats["last_error"]` 决定，而 DELETE 失败也会写 `last_error`（:940）——router 明明在线，一次删除失败就会把该指标打成 0。router 侧则有 `smg_worker_health`、`smg_worker_health_checks_total`、`smg_worker_cb_state`、`smg_worker_cb_transitions_total` 等（gw/src/observability/metrics.rs:229-267），worker 状态翻转只体现在指标、无 info 日志。

## 7. 改进建议（按改动小/收益大排序）

1. **activity probe 改用 worker 自报的 model id**（堵 404 永久漏杀洞）。`_check_activity` llm_watcher.py:852：注册时把 `info.models[0]`（原始 id）存进 ledger，probe 用原始 id 而非 router 公共 id。改 2 处数据结构 + 1 行取值。**收益最大**：vLLM/sglang + model-map 组合下僵尸模型永不可剔除的问题直接消失。
2. **区分「进程已死」与「引擎 5xx」**（缩小长模型加载误杀窗）。`activity_probe` :463 把 connection refused 和 5xx 都归为 dead；让 5xx 单独一个 verdict（如 `"err"`），在 `_note_activity` :898 对 `"err"` 用 slow_factor（或单独 factor 2）。多副本加载 >3 分钟被误摘的主场景即被堵住，代价只是真正坏掉的引擎多等几个周期。
3. **把 activity strike 持久化进 ledger**。`self._acts` 是纯内存（:653-656），watcher 每重启一次僵尸的 strike 清零一次——崩溃循环的 watcher 会让僵尸 worker 无限续命。序列化 `n` 进 ledger.json（`Ledger.save/_load` :559-587）即可。
4. **删掉 add 时发送的死配置**（`_add` llm_watcher.py:1062-1065），或注册确认后补一次 `update_worker_properties` 让 15s/3×/3× 真正落到 worker 上。当前状态是「看起来配了、实际没配」，排查时极易误导。
5. **metrics 加 per-worker 维度**（`start_metrics` llm_watcher.py:1161）：`llm_watcher_activity_strikes{url,result}` gauge + `llm_watcher_last_remove_timestamp{url,reason}`，让「何时摘了谁、为什么」不再只能翻日志。顺手把 `router_reachable` 与 `last_error` 解耦（DELETE 失败不算 router 不可达）。
6. **404 永免区加兜底**：`_note_activity` 的 unknown 分支（:889-891）连续 N 次后主动重探一次 `/v1/models`，若 model id 变了则触发已有的 drift 告警（`_check_drift` :1116）——同时缓解「400/404 反复清零 strike」的副作用（400→alive 清零逻辑 :883-889 可改为"不清零、只不加分"）。
7. **发现层消失判加容忍**：`reconcile` 的 missing 分支（:815-818）目前是「单轮未出现即开始计时」，对 3s 超时的 busy worker 偏激进；改成连续 2~3 轮未出现才置 `missing_since`，可消除高负载下 5 分钟误摘链（第 3 节场景二后半）。
8. **`add_confirm_timeout` 与模型加载时长对齐**：默认 180s :629 小于典型大模型加载时间，加载期间每 ~3 分钟一次 add/reap 循环。flag 已存在，部署侧把 `LLM_WATCHER_ADD_CONFIRM_TIMEOUT` 提到 1800+ 即可，代码零改动。
9. **docker 扫描利用 Health 字段**（可选）：`docker_candidates` :266 只过滤 `status=running`，`/containers/json` 本身已返回 `Health.Status`，对声明了 healthcheck 的容器可提前拿到 unhealthy 信号、缩短 300s 宽限期；对未声明 healthcheck 的容器无影响。

**总体评价**：这套设计里最值钱的三个决策都验证成立——400 确实不算不健康（且会清 strike）；slow/dead 双阈值（3 vs 9 次）让 262K 长上下文 worker 有 9 分钟预算；keep-last + 1800s grace 防单副本误杀。真正需要动手的是 404 永免区遇上 model-map 的漏杀组合（建议 1）、加载期 5xx 与进程死亡的混淆（建议 2），以及 watcher 重启丢 strike（建议 3）——三者都是小改动。

> 附：gateway(Rust 侧) 行号由子代理独立核实，以 2026-09-28 工作区为准。
