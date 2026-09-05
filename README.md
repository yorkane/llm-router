# llm-router

Standalone LLM reverse proxy with **cache-aware routing**. A trimmed build of
[sgl-project/sglang](https://github.com/sgl-project/sglang)'s model gateway (`smg`), packaged as a tiny
CUDA-free / Python-free container (~170MB) that only routes traffic to existing
**vLLM** or **llama.cpp** (OpenAI-compatible) instances and maximizes their prefix-cache hit rate.

It does **not** load models or do inference locally. Point it at your workers and it keeps
conversations/session prefixes sticky to the same worker, spreading new conversations evenly.

> ⚠️ Gotcha learned the hard way: with `--backend openai` the policy engine is bypassed
> (it just picks `min_by_key(load)`, which degenerates to one worker at low concurrency).
> For cache-aware routing use `--backend sglang` (default in this image), which works fine
> against plain OpenAI-compatible workers too.

## Quick start

```bash
docker run -d --name llm-router --network host --restart unless-stopped \
  ghcr.io/yorkane/llm-router:latest \
  --worker-urls http://10.0.0.1:8000 http://10.0.0.2:8000 http://10.0.0.3:8000 \
  --cache-threshold 0.2 --balance-abs-threshold 32 --balance-rel-threshold 1.5 \
  --eviction-interval 120 --max-tree-size 50000 \
  --health-check-interval-secs 30 --request-timeout-secs 1800 --log-level info
```

Then send traffic to `http://<host>:8801/v1/chat/completions` (OpenAI-compatible, streaming included).
`--network host` is convenient on a single machine; otherwise publish ports `8801` (and `29001` for Prometheus).

## Key knobs

| Flag | Meaning |
|---|---|
| `--policy cache_aware` | prefix-cache-aware routing (default) |
| `--cache-threshold` | affinity threshold; lower = stickier sessions (default 0.5, try 0.2) |
| `--balance-abs-threshold` / `--balance-rel-threshold` | load-imbalance escape hatch: when one worker lags, affinity is temporarily suspended and requests spill to idle workers |
| `--max-tree-size` / `--eviction-interval` | routing-tree capacity / cleanup cadence |
| `--health-check-endpoint` | per-worker health endpoint (llama.cpp: `/health`) |
| `--prometheus-port` | metrics endpoint |

Full flag list: `docker run --rm --entrypoint smg ghcr.io/yorkane/llm-router:latest launch --help`.

## How it works

`cache_aware` keeps a radix tree of request prefixes per (pool, model):

- prefix match rate > `cache-threshold` → route to the worker that served the prefix (KV/prefix cache hit);
- otherwise → pick the least-loaded worker, ties broken randomly;
- when load skew exceeds the balance thresholds → shortest-queue routing until rebalanced.

## Layout

- `gateway/` — Rust source (from sglang `sgl-model-gateway` @ v0.5.18, patched: python bindings removed, `smg` bin only)
- `harmony/` — vendored [openai/harmony](https://github.com/openai/harmony) v0.0.4 (path dependency, so the build needs no GitHub access)
- `Dockerfile` — runtime image (`ubuntu:24.04` + `libssl3`), copies the CI-built binary
- `.github/workflows/build.yml` — builds `smg` with the `ci` profile and publishes `ghcr.io/yorkane/llm-router:latest`

### Rebuilding locally

```bash
cd gateway && cargo build --profile ci --bin smg   # needs rust 1.90, libssl-dev, cmake
docker build --build-arg BIN=target/ci/smg -t llm-router:dev .
```

The binary dynamically links `libssl.so.3`/`libgcc_s` (glibc 2.39 baseline → ubuntu:24.04).

## Credits / license

Derived from [sgl-project/sglang](https://github.com/sgl-project/sglang) (`sgl-model-gateway`, Apache-2.0)
and vendors [openai/harmony](https://github.com/openai/harmony) v0.0.4 (MIT). See `gateway/LICENSE` and `harmony/LICENSE`.

**Not official sglang software.** Fixes here (session stickiness tuning, packaging) are ours; the routing algorithm is upstream's.
