# llm-watcher

Keeps local inference services (vLLM / sglang / llama.cpp / anything OpenAI-compatible)
inside an [llm-router](../README.md) pool, so starting or killing a model no longer means
editing `--worker-urls` and restarting the router.

Single file, standard library only: no pip, no CUDA, no image of its own.

## How it works

Every `--interval` seconds it reconciles two sets.

```
 discover                                  reconcile                              llm-router
 ---------                     ------------------------------------          ----------------
 docker ps     ---\          desired = discovered - protected               POST /workers    -> AddWorker job
 /proc/net/tcp  >-- probe -> strict: /v1/models must answer OpenAI JSON      (202 = queued, then confirmed
 --target      ---/          remove  = owned - desired, after a grace period  against GET /workers)
                                                                     DELETE /workers/{id} -> RemoveWorker job
```

Discovery never trusts a port. A candidate becomes a worker only when `GET /v1/models`
answers with real OpenAI JSON (`data[].id`), which is what keeps node_exporter, nginx 404
pages and every other HTML-speaking service out of the pool. The engine (sglang / vllm /
llama.cpp) is sniffed from `/get_server_info`, `/props` and `/metrics` for labels only,
never as a routing decision.

## What makes it safe against a live router

| Guard | Behaviour |
|---|---|
| No self-loop | The router itself serves `/v1/models`. It is rejected by its `/server_info` fingerprint and by an own-port check. Anything advertising more than `--max-models` (default 8) models is treated as an aggregator or proxy and skipped. |
| Protects your config | On first contact, every worker already in the pool (from `--worker-urls` or added by hand) is snapshotted as `protected` and never deleted. |
| Only deletes what it owns | Removals come from a local ledger of workers this daemon added itself. |
| Grace period | A worker is removed only after being undiscovered for `--remove-grace` (default 300s). Short restarts stay the router's job: it marks the worker unhealthy and keeps the slot. |
| Never empties a model | The last worker of a model is kept and warned about instead of deleted (`--no-keep-last` overrides). |
| Releases stuck adds | A `202` only means "queued". An AddWorker job parked forever on a dead URL also squats that URL (every retry says `already exists`), so the watcher deletes it and re-adds on the next pass. |
| Survives a router restart | Recorded worker ids are refreshed from `GET /workers`, otherwise a later `DELETE` would 404. |
| Warns on mixed models | A single-router `smg` ignores the requested model when choosing a worker (measured: 10/10 requests naming the local model were served by a remote one, both returning 200). The daemon cannot fix that, so it logs one warning pointing at `--enable-igw` as soon as a second model appears. |
| Blind by design | It never reads model files or GPU state, and never starts or stops a service. |

## Run it

Dry-run first: it prints what it would change and touches nothing.

```bash
python3 watcher/llm_watcher.py --router http://127.0.0.1:8800 --dry-run --once -v
```

Then either in a container -- the recommended shape when the router is itself a container:
[../deploy/docker-compose.yml](../deploy/docker-compose.yml) runs router and watcher as one
project, so they start together and the watcher tracks the whole host (remote instances go in
`LLM_WATCHER_TARGETS`):

```bash
docker compose -f deploy/docker-compose.yml up -d
docker logs -f llm-watcher
curl -s localhost:9912/metrics | grep llm_watcher
```

or as a systemd unit on a bare host ([deploy/llm-watcher.service](deploy/llm-watcher.service)):

```bash
sudo mkdir -p /var/lib/llm-watcher
sudo cp watcher/deploy/llm-watcher.service /etc/systemd/system/
sudo systemctl daemon-reload && sudo systemctl enable --now llm-watcher
journalctl -u llm-watcher -f
```

or in a container. Keep `--network host`, otherwise `/proc/net/tcp` shows the container
namespace and discovery finds nothing:

```bash
docker run -d --name llm-watcher --network host --restart unless-stopped \
  -v /var/run/docker.sock:/var/run/docker.sock:ro \
  -v /home/aigc/ChatGPT/llm-router/watcher:/opt/watcher:ro \
  -v /var/lib/llm-watcher:/var/lib/llm-watcher \
  python:3.12-alpine python3 /opt/watcher/llm_watcher.py \
    --router http://127.0.0.1:8800 --state-dir /var/lib/llm-watcher --metrics-port 9912
```

Pull `python:3.12-alpine` through the ACR mirror instead of docker.io (see skill: acr-wasu).

## Flags that matter

| Flag | Use |
|---|---|
| `--router` | Router base URL. Control-plane auth, if the router has one, comes from `--router-api-key` / `ROUTER_API_KEY` (env). |
| `--target` | Register a URL discovery cannot see (a service on another host). Repeatable. |
| `--exclude` | Regex of URLs never to touch. Repeatable. |
| `--allow-port` / `--deny-port` | Narrow the scan, e.g. `--allow-port 8000-8020,11434`. |
| `--require-health` | Refuse services with no 2xx `/health` (llama.cpp has one, some wrappers do not). |
| `--max-models` | Ceiling for "this looks like a proxy, not a worker"; 0 disables it. |
| `--remove-grace` / `--allow-remove false` | How eagerly dead services are dropped. |
| `--worker-api-key` | Key the router should present to the workers; it does not inherit its own. |
| `--fix-model-drift` | Recycle a worker whose served model id changed on the same URL (a weight swap). Off by default: it only warns. |

`--once` runs one pass and exits, which is what cron or CI wants.

## Environment variables

Every flag above also reads an environment variable, which is how a container gets
configured. Prefix them with `LLM_WATCHER_` (the bare name works too, for the flags people
already write in compose files). Precedence: flag > `LLM_WATCHER_X` > plain `X` > built-in.

| Variable | Meaning |
|---|---|
| `LLM_WATCHER_ROUTER` (or `ROUTER_URL`) | Router base URL |
| `LLM_WATCHER_ROUTER_API_KEY` | Bearer key for the control plane |
| `LLM_WATCHER_TARGETS` | Remote/pinned worker URLs, comma, space or newline separated |
| `LLM_WATCHER_WORKER_API_KEY` | Key the router presents to the workers |
| `LLM_WATCHER_STATE_DIR` | Ledger directory, put it on a volume |
| `LLM_WATCHER_INTERVAL` / `_PROBE_TIMEOUT` / `_WORKERS` | Loop cadence and probe tuning |
| `LLM_WATCHER_DOCKER` / `_PROC_SCAN` / `_CONTAINER_IPS` | Turn a discovery source off with `false` |
| `LLM_WATCHER_ALLOW_PORT` / `_DENY_PORT` | Restrict which ports get probed |
| `LLM_WATCHER_REQUIRE_HEALTH` / `_MAX_MODELS` | Tighten what counts as a worker |
| `LLM_WATCHER_ALLOW_REMOVE` / `_REMOVE_GRACE` / `_KEEP_LAST` | Removal behaviour |
| `LLM_WATCHER_SHORT_MODEL_NAMES` | Register `/models/foo.gguf` as `foo` (llama.cpp reports the full path) |
| `LLM_WATCHER_METRICS_PORT` | Prometheus port, `0` disables it |

A blank or unset value never overrides a default, and a value that is not a number is
logged and ignored rather than crashing the daemon.

## State and metrics

The ledger is `--state-dir/ledger.json` (`protected`, `owned`, `missing_since`). Deleting it
is safe but forgets the protection snapshot, so only do that against an empty pool. With
`--metrics-port` a small Prometheus endpoint exposes `llm_watcher_adds_total`,
`removes_total`, `discovered_workers`, `owned_workers`, `protected_workers` and
`router_reachable`.

## Tests

```bash
python3 -m unittest discover -s watcher -p 'test_*.py' -v
```

44 tests, no network and no real router: fakes stand in for both an inference server and
the router, covering the guards above (self-loop, HTML services, protection snapshot,
grace period, last worker, stuck adds, router restart).

## Limitations

- One model id per worker URL: a server hosting several models is registered under its
  first id, because the router keys its model index by URL. Run one model per instance, or
  give each model its own URL, if all of them must be addressable.
- DP-aware workers (`--dp-aware`) are not expanded here; register those URLs explicitly.
- Prefill/decode (PD) workers are out of scope: only `regular` workers are registered.
- A remote `--target` is not immortal: it is probed like anything else, so an instance that
  stays unreachable is removed after `--remove-grace`. Usually what you want, but a flaky
  remote host can therefore leave the pool; raise the grace or use `--no-allow-remove`.
- `smg` in single-router mode does not route by model, so a heterogeneous pool is only
  correct with `--enable-igw`. Verified both ways against a live router: without IGW the
  explicit `model` was ignored; with IGW `qwen` reached the local worker and `ornith` the
  remote one every time, and an omitted `model` was rejected rather than guessed.
