# llama.cpp webui integrated at /_ui/

llm-router can serve the official llama.cpp chat webui (llama-b11215-ui.tar.gz) under
/_ui/, so any model behind the router can be chat-tested from the browser without
touching the original /v1 entrypoints or needing a llama.cpp --path proxy.

## Enable

- Binary flag: --ui-dir <dir> (env SMG_UI_DIR). The Docker image ships the bundle at
  /usr/local/share/llama-ui and enables it in the CMD, so compose deployments get it free.
- URL: http://ROUTER:8800/_ui/

## How it works

- ui/ holds the official bundle with a path patch (watcher/patch_ui.sh re-applies it on
  upgrade): bundle API calls rewritten to /_ui/v1/chat/completions, /_ui/v1/... and
  /_ui/props. See ui/README.md for the upgrade procedure.
- gateway/src/server.rs adds ui_api_routes(): /_ui/v1/chat/completions,
  /_ui/v1/completions and /_ui/v1/models reuse the normal handlers (auth, limits,
  metrics, IGW routing included); /_ui/props is proxied to the upstream worker with a
  3s timeout and synthesized from router state when the worker does not answer, so
  vLLM/SGLang workers still list correctly; /_ui/slots, /_ui/tools and
  /_ui/v1/streams/lookup return empty JSON; control/stream answer 501.
- Static assets are served by tower-http ServeDir nested at /_ui. Root / is unchanged.
- watcher/patches/ui/ re-applies the Rust hunks after each upstream sync
  (upstream_sync.sh runs apply_ui.py right after the rsync swap; anchors are loud).

## Verification (2026-09-27, local)

Built with cargo build --profile ci --bin smg, run on :8810 against a fake llama.cpp
worker on :8300 (fake_llama_worker.py providing /v1/models and /props):

- GET /_ui/ -> 200 index.html; /_ui/_app/immutable/bundle.*.js -> 200
- GET /_ui/v1/models -> fake-llama-model listed
- GET /_ui/props?model=... -> proxied model_path=/models/fake-llama-model.gguf
- POST /_ui/v1/chat/completions -> hello from fake worker
- POST /_ui/v1/chat/completions/control -> 501; GET /v1/models -> 200; GET / -> 404
- playwright: page renders, model chip shows, real chat round-trip OK
  (screenshot: doc/webui-chat.png), no console errors after the /_ui/tools fix.
