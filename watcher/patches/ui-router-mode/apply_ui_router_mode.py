#!/usr/bin/env python3
"""Reapply the ui-router-mode (webui model picker, role:"router") patch onto gateway/.

upstream_sync.sh swaps gateway/ wholesale with the upstream tree, so this runs
right after apply_runtime_config.py: two of the five server.rs edits anchor on
lines that only exist once the runtime-config patch has wrapped the /props
bodies in ui_props_with_ctx, and the route hunk anchors on ui_api_routes()
which the ui patch creates. Idempotent (skips when ui_router_mode already
exists in server.rs) and loud: any missing or ambiguous anchor aborts the
sync, so a broken patch fails CI instead of silently shipping a router whose
webui picker points at dead endpoints.

All snippet files live next to this script and were extracted byte-exactly
from the patched tree; update them together with any manual change to the
router-mode code in gateway/src/server.rs.
"""
import os
import sys

gw, pd = sys.argv[1], sys.argv[2]
SV = gw + "/src/server.rs"


def read(p):
    return open(p, encoding="utf-8").read()


def die(msg):
    sys.exit("[ui-router-mode-patch] " + msg)


def snip(name):
    p = pd + "/" + name
    if not os.path.isfile(p):
        die("snippet missing: " + p)
    return read(p)


router_mode_fns = snip("router_mode_fns.rs")
router_mode_handlers = snip("router_mode_handlers.rs")
routes_models = snip("routes_models.rs")

t = read(SV)
if "fn ui_router_mode(" in t:
    print("[ui-router-mode-patch] already applied; nothing to do")
    sys.exit(0)


def swap(t, a, b, path, name):
    if t.count(a) != 1:
        die("anchor %s not unique (%dx) in %s" % (name, t.count(a), path))
    return t.replace(a, b)


# 1) the two helper fns (ui_router_mode / ui_props_with_role), inserted ahead
#    of the runtime-config ctx-cap fn doc comment
t = swap(
    t,
    "/// Report the configured context cap instead of the worker's raw n_ctx when the\n",
    router_mode_fns
    + "/// Report the configured context cap instead of the worker's raw n_ctx when the\n",
    SV,
    "router-mode-fns",
)

# 2) /props: force role:"router" on the worker-answered body (the line only
#    exists in this shape after the runtime-config patch added the ctx wrap)
t = swap(
    t,
    "                            ui_props_with_thinking(value),\n",
    "                            ui_props_with_role(ui_props_with_thinking(value)),\n",
    SV,
    "props-call-site",
)

# 3) /props: same wrap on the local fallback json! body
t = swap(
    t,
    "        ui_props_with_thinking(json!({\n"
    '            "model_path": model_path,\n'
    '            "model_alias": null,\n'
    '            "webui_version": "llm-router",\n'
    "        })),\n",
    "        ui_props_with_role(ui_props_with_thinking(json!({\n"
    '            "model_path": model_path,\n'
    '            "model_alias": null,\n'
    '            "webui_version": "llm-router",\n'
    "        }))),\n",
    SV,
    "props-fallback",
)

# 4) the four router-mode model handlers (ui_models / ui_model_load /
#    ui_model_unload / ui_models_sse), inserted ahead of the Logs route doc
t = swap(
    t,
    "/// Public (no auth) routes for the Logs page. The page itself is a static asset\n",
    router_mode_handlers
    + "/// Public (no auth) routes for the Logs page. The page itself is a static asset\n",
    SV,
    "router-mode-handlers",
)

# 5) ui_api_routes (created by the ui patch): swap the /_ui/v1/models handler
#    and add the /_ui/models/{load,unload,sse} aliases
t = swap(
    t,
    '        .route("/_ui/v1/models", get(v1_models))\n',
    routes_models,
    SV,
    "models-routes",
)

open(SV, "w", encoding="utf-8").write(t)
print("[ui-router-mode-patch] server.rs: role:\"router\" /props + 4 router-mode model routes")
