#!/usr/bin/env python3
"""Reapply the runtime-config (/_ui/ Config page) patch onto gateway/.

upstream_sync.sh swaps gateway/ wholesale with the upstream tree, so this runs
right after apply_request_log.py: the route_chat policy hunk anchors on the
request-log decision block and the /_ui route merge anchors on ui_logs_routes(),
both of which that patch creates. Idempotent (skips when runtime_config is
already referenced in routers/openai/router.rs) and loud: any missing or
ambiguous anchor aborts the sync, so a broken patch fails CI instead of
silently shipping a router without the Config page.

All snippet files live next to this script and were extracted byte-exactly
from the patched tree; update them together with any manual change to the
runtime config code in gateway/src/.
"""
import os
import shutil
import sys

gw, pd = sys.argv[1], sys.argv[2]
src = gw + "/src"
LIB = src + "/lib.rs"
SV = src + "/server.rs"
OA = src + "/routers/openai/router.rs"
HR = src + "/routers/http/router.rs"


def read(p):
    return open(p, encoding="utf-8").read()


def die(msg):
    sys.exit("[runtime-config-patch] " + msg)


def snip(name):
    p = pd + "/" + name
    if not os.path.isfile(p):
        die("snippet missing: " + p)
    return read(p)


route_chat_policy = snip("route_chat_policy.rs")
route_responses_policy = snip("route_responses_policy.rs")
props_with_ctx_fn = snip("props_with_ctx_fn.rs")
config_handlers = snip("config_handlers.rs")

if "runtime_config" in read(OA):
    print("[runtime-config-patch] already applied; nothing to do")
    sys.exit(0)


def swap(t, a, b, path, name):
    if t.count(a) != 1:
        die("anchor %s not unique (%dx) in %s" % (name, t.count(a), path))
    return t.replace(a, b)


# 1) module declaration + the config store itself
t = read(LIB)
t = swap(
    t,
    "pub mod service_discovery;\n",
    "pub mod service_discovery;\npub mod runtime_config;\n",
    LIB,
    "lib-mod",
)
open(LIB, "w", encoding="utf-8").write(t)
shutil.copy2(pd + "/runtime_config.rs", src + "/runtime_config.rs")
print("[runtime-config-patch] lib.rs + src/runtime_config.rs")

# 2) openai router: effort/ctx policy ahead of the request-log decision
#    (route_chat) and ahead of the provider lookup (route_responses)
t = read(OA)
t = swap(
    t,
    "        if let Some(ingest) = crate::observability::request_log::ingest_from_headers(headers) {",
    route_chat_policy
    + "        if let Some(ingest) = crate::observability::request_log::ingest_from_headers(headers) {",
    OA,
    "route-chat-policy",
)
t = swap(
    t,
    "        let provider = self.get_provider_arc_for_worker(worker.as_ref(), model_id);\n"
    "        if let Err(e) = provider.transform_request(&mut payload, Endpoint::Responses) {",
    route_responses_policy
    + "        let provider = self.get_provider_arc_for_worker(worker.as_ref(), model_id);\n"
    "        if let Err(e) = provider.transform_request(&mut payload, Endpoint::Responses) {",
    OA,
    "route-responses-policy",
)
open(OA, "w", encoding="utf-8").write(t)
print("[runtime-config-patch] openai/router.rs: effort/ctx policy in chat+responses")

# 3) http router: the typed payload is serialized once (by the request-log
#    ingest block), run through the same policy for the OpenAI-style endpoints,
#    and what goes downstream is that payload instead of the raw typed_req
t = read(HR)
t = swap(
    t,
    "            .send_typed_request(headers, typed_req, route, &worker, is_stream, load_guard)\n",
    "            .send_typed_request(headers, &payload, route, &worker, is_stream, load_guard)\n",
    HR,
    "http-send-payload",
)
open(HR, "w", encoding="utf-8").write(t)
print("[runtime-config-patch] http/router.rs: send the policy-rewritten payload")

# 4) server.rs: ctx-aware /props, Config-page handlers, route merge
t = read(SV)
t = swap(
    t,
    "async fn ui_props(state: &Arc<AppState>, wanted: Option<String>) -> Response {\n",
    props_with_ctx_fn
    + "async fn ui_props(state: &Arc<AppState>, wanted: Option<String>) -> Response {\n"
    "    let cap_model = wanted.clone();\n",
    SV,
    "ui-props-ctx",
)
t = swap(
    t,
    "                        return Json(ui_props_with_thinking(value)).into_response();\n",
    "                        return Json(ui_props_with_ctx(\n"
    "                            ui_props_with_thinking(value),\n"
    "                            cap_model.as_deref(),\n"
    "                        ))\n"
    "                        .into_response();\n",
    SV,
    "props-call-site",
)
t = swap(
    t,
    "    Json(ui_props_with_thinking(json!({\n"
    '        "model_path": model_path,\n'
    '        "model_alias": null,\n'
    '        "webui_version": "llm-router",\n'
    "    })))\n",
    "    Json(ui_props_with_ctx(\n"
    "        ui_props_with_thinking(json!({\n"
    '            "model_path": model_path,\n'
    '            "model_alias": null,\n'
    '            "webui_version": "llm-router",\n'
    "        })),\n"
    "        cap_model.as_deref(),\n"
    "    ))\n",
    SV,
    "props-fallback",
)
t = swap(
    t,
    "async fn liveness() -> Response {",
    config_handlers + "async fn liveness() -> Response {",
    SV,
    "config-handlers",
)
t = swap(
    t,
    "        .merge(ui_logs_routes())\n",
    "        .merge(ui_logs_routes())\n        .merge(ui_config_routes())\n",
    SV,
    "merge-config",
)
open(SV, "w", encoding="utf-8").write(t)
print("[runtime-config-patch] server.rs: ui_props_with_ctx + /_ui/config routes")
