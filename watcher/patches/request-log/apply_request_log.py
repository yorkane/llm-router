#!/usr/bin/env python3
"""Reapply the request-log (/_ui/ Logs page) patch onto gateway/.

upstream_sync.sh swaps gateway/ wholesale with the upstream tree, so this runs
right after apply_ui.py. Idempotent (skips when RequestLogLayer is already in
middleware.rs) and loud: any missing or ambiguous anchor aborts the sync, so a
broken patch fails CI instead of silently shipping a router without the Logs
page.

All snippet files live next to this script and were extracted byte-exactly from
the patched tree; update them together with any manual change to the request-log
code in gateway/src/.
"""
import os
import shutil
import sys

gw, pd = sys.argv[1], sys.argv[2]
src = gw + "/src"
MW = src + "/middleware.rs"
SV = src + "/server.rs"
OB = src + "/observability/mod.rs"
OA = src + "/routers/openai/router.rs"
HR = src + "/routers/http/router.rs"

BAR = "// " + "=" * 76 + "\n"


def read(p):
    return open(p, encoding="utf-8").read()


def die(msg):
    sys.exit("[request-log-patch] " + msg)


def snip(name):
    p = pd + "/" + name
    if not os.path.isfile(p):
        die("snippet missing: " + p)
    return read(p)


middleware_block = snip("middleware_block.rs")
handlers = snip("handlers.rs")
store_install = snip("snip_store_install.rs")
oa_note = snip("snip_oa_note.rs")
oa_decision = snip("snip_oa_decision.rs")
oa_observe = snip("snip_oa_observe.rs")
hr_ingest = snip("snip_hr_ingest.rs")

if "RequestLogLayer" in read(MW):
    print("[request-log-patch] already applied; nothing to do")
    sys.exit(0)


def swap(t, a, b, path, name):
    if t.count(a) != 1:
        die("anchor %s not unique (%dx) in %s" % (name, t.count(a), path))
    return t.replace(a, b)


# 1) observability: new module + file
shutil.copy2(pd + "/request_log.rs", src + "/observability/request_log.rs")
t = read(OB)
t = swap(t, "pub mod otel_trace;\n", "pub mod otel_trace;\npub mod request_log;\n", OB, "otel-trace")
open(OB, "w", encoding="utf-8").write(t)
print("[request-log-patch] observability/request_log.rs + mod.rs")

# 2) middleware.rs: import + the RequestLogLayer/TrackedBody block
t = read(MW)
t = swap(
    t,
    "use crate::{\n",
    "use crate::{\n    observability::request_log::PendingRequest,\n",
    MW,
    "import",
)
t = swap(
    t,
    BAR + "// HTTP Metrics Layer (Layer 1: SMG metrics)",
    middleware_block + BAR + "// HTTP Metrics Layer (Layer 1: SMG metrics)",
    MW,
    "metrics-banner",
)
open(MW, "w", encoding="utf-8").write(t)
print("[request-log-patch] middleware.rs: RequestLogLayer + TrackedBody")

# 3) server.rs: handlers + route merge + layer + store install in startup()
t = read(SV)
t = swap(t, "async fn liveness() -> Response {", handlers + "async fn liveness() -> Response {", SV, "handlers")
t = swap(
    t,
    ".merge(ui_api_routes(auth_config.clone()))",
    ".merge(ui_api_routes(auth_config.clone()))\n        .merge(ui_logs_routes())",
    SV,
    "merge-logs",
)
t = swap(
    t,
    ".layer(middleware::RequestIdLayer::new(request_id_headers))",
    ".layer(middleware::RequestIdLayer::new(request_id_headers))\n        .layer(middleware::RequestLogLayer)",
    SV,
    "layer-logs",
)
t = swap(
    t,
    "        metrics::start_prometheus(prometheus_config.clone());\n    }\n\n",
    "        metrics::start_prometheus(prometheus_config.clone());\n    }\n\n" + store_install,
    SV,
    "store-install",
)
open(SV, "w", encoding="utf-8").write(t)
print("[request-log-patch] server.rs: ui_logs_routes + RequestLogLayer + store install")

# 4) openai router: note_request / decision / observe hunks
t = read(OA)
t = swap(
    t,
    "        let streaming = body.stream;\n\n",
    "        let streaming = body.stream;\n\n" + oa_note,
    OA,
    "oa-note",
)
t = swap(
    t,
    "        let provider = self.get_provider_arc_for_worker(worker.as_ref(), model_id);\n"
    "        if let Err(e) = provider.transform_request(&mut payload, Endpoint::Chat) {",
    oa_decision + "        let provider = self.get_provider_arc_for_worker(worker.as_ref(), model_id);\n"
    "        if let Err(e) = provider.transform_request(&mut payload, Endpoint::Chat) {",
    OA,
    "oa-decision",
)
t = swap(
    t,
    "                                if status.is_success() {\n"
    "                                    worker.circuit_breaker().record_success();\n"
    "                                }\n",
    "                                if status.is_success() {\n"
    "                                    worker.circuit_breaker().record_success();\n"
    "                                }\n" + oa_observe,
    OA,
    "oa-observe",
)
open(OA, "w", encoding="utf-8").write(t)
print("[request-log-patch] openai/router.rs: 3 ingest hunks")

# 5) http router: ingest after the routing decision
t = read(HR)
t = swap(
    t,
    '            None => self.policy_registry.get_default_policy(),\n        };\n\n'
    '        let load_guard = ["cache_aware", "manual"]',
    '            None => self.policy_registry.get_default_policy(),\n        };\n\n'
    + hr_ingest + '        let load_guard = ["cache_aware", "manual"]',
    HR,
    "hr-ingest",
)
open(HR, "w", encoding="utf-8").write(t)
print("[request-log-patch] http/router.rs: ingest hunk")

