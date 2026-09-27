#!/usr/bin/env python3
"""Reapply the llama.cpp webui (--ui-dir /_ui/) patch onto gateway/.

upstream_sync.sh swaps gateway/ wholesale with the upstream tree, so this runs
right after the swap. Idempotent (skips when ui_dir is already present) and
loud: any moved anchor aborts the sync, so a broken patch fails CI instead of
silently shipping a router without the webui.

Snippets (handlers / routes / nest block) live next to this file and were
extracted from the patched tree; update them together with any manual change
to the UI code in gateway/src/server.rs.
"""
import sys

gw, pd = sys.argv[1], sys.argv[2]
ct, mn, sv = gw + "/Cargo.toml", gw + "/src/main.rs", gw + "/src/server.rs"

handlers = open(pd + "/handlers.snippet").read()
routes = open(pd + "/routes.snippet").read()
nest = open(pd + "/nest.snippet").read()


def die(msg):
    sys.exit("[ui-patch] " + msg)


if "ui_dir" in open(mn).read():
    print("[ui-patch] already applied; nothing to do")
    sys.exit(0)

if "pub mesh_server_config: Option<MeshServerConfig>," not in open(sv).read():
    die("gateway tree looks newer than the ui snippets; update watcher/patches/ui first")

# 1) tower-http fs feature (ServeDir)
t = open(ct).read()
tail = t.split("tower-http", 1)[-1][:400]
if '"fs"]' not in tail:
    a = '"request-id", "util"]'
    if a not in t:
        die("tower-http feature anchor missing in Cargo.toml")
    open(ct, "w").write(t.replace(a, '"request-id", "util", "fs"]', 1))
    print("[ui-patch] Cargo.toml: tower-http fs feature")


def swap(t, a, b, path, name):
    if t.count(a) != 1:
        die("anchor %s not unique (%dx) in %s" % (name, t.count(a), path))
    return t.replace(a, b)


# 2) main.rs: --ui-dir clap arg + ServerConfig init
t = open(mn).read()
arg = (
    "    /// Serve the bundled llama.cpp chat webui under /_ui/ from this\n"
    "    /// directory (env: SMG_UI_DIR). The directory is the unpacked\n"
    "    /// llama.cpp *-ui.tar.gz payload; see ui/README.md.\n"
    '    #[arg(long, env = "SMG_UI_DIR", help_heading = "WebUI")]\n'
    "    ui_dir: Option<String>,\n\n"
)
a1 = "    /// Enable IGW (Inference Gateway) mode for multi-model support"
a2 = "            control_plane_auth,\n            mesh_server_config,\n        }"
t = swap(t, a1, arg + a1, mn, "igw-doc")
t = swap(t, a2, "            control_plane_auth,\n            mesh_server_config,\n            ui_dir: self.ui_dir.clone(),\n        }", mn, "cfg-init")
open(mn, "w").write(t)
print("[ui-patch] main.rs: --ui-dir arg")

# 3) server.rs
t = open(sv).read()
t = swap(t, "use std::{\n", "use std::{\n    collections::HashMap,\n", sv, "std-import")
t = swap(t, "    routing::{delete, get, post},", "    routing::{any, delete, get, post},", sv, "routing-import")
t = swap(t, "        worker::WorkerType,", "        worker::{ConnectionMode, Worker, WorkerType},", sv, "worker-import")
t = swap(t, "async fn liveness() -> Response {", handlers + "async fn liveness() -> Response {", sv, "handlers")
t = swap(t, "pub fn build_app(", routes + "pub fn build_app(", sv, "routes-fn")
t = swap(
    t,
    "    pub mesh_server_config: Option<MeshServerConfig>,\n}",
    "    pub mesh_server_config: Option<MeshServerConfig>,\n"
    "    /// Directory holding the unpacked llama.cpp webui assets; when set, they\n"
    "    /// are served under /_ui/ together with the API aliases the UI needs.\n"
    "    pub ui_dir: Option<String>,\n}",
    sv,
    "config-field",
)
t = swap(t, "        .merge(public_routes)", "        .merge(public_routes)\n        .merge(ui_api_routes(auth_config.clone()))", sv, "merge")
t = swap(t, "    // TcpListener::bind accepts", nest + "\n    // TcpListener::bind accepts", sv, "nest")
open(sv, "w").write(t)
print("[ui-patch] server.rs: ui_props + ui_api_routes + ui_dir + /_ui nest")

