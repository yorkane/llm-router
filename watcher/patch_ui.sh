#!/usr/bin/env bash
# watcher/patch_ui.sh — rewrite the llama.cpp webui bundle relative API paths so they
# work under the router /_ui/ mount. Idempotent.
#
# The official UI bundle fetches ./v1/chat/completions, ./props etc. relative to the
# page URL. Under /_ui/ those resolve to /_ui/v1/... which the static ServeDir would
# 404, so we pin them to absolute /_ui/... paths; the router aliases those back onto
# its regular API handlers (ui_routes in server.rs).
#
# Usage: bash watcher/patch_ui.sh [ui_dir]   (default: <repo>/ui)
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
UI_DIR="${1:-$ROOT/ui}"
BUNDLE="$(ls "$UI_DIR"/_app/immutable/bundle.*.js 2>/dev/null | head -n1)"
[ -n "$BUNDLE" ] || { echo "patch_ui: bundle not found under $UI_DIR" >&2; exit 1; }

python3 - "$BUNDLE" <<'PYEOF'
import re, sys
path = sys.argv[1]
t = open(path).read()
# rewrite every quoted relative API literal ("./v1/..." or './props') to an
# absolute /_ui/... path; quote-agnostic, idempotent
pat = re.compile(r'(["\'])(\./(?:v1/[A-Za-z/_]*|props))\1')
hits = pat.findall(t)
if hits:
    t = pat.sub(lambda m: m.group(1) + "/_ui" + m.group(2)[1:] + m.group(1), t)
    open(path, "w").write(t)
    for p in sorted({p for _, p in hits}):
        print(f"patch_ui: {p} -> /_ui{p[1:]}")
if re.search(r'(["\'])(\./(?:v1/|props))\1', open(path).read()):
    print("patch_ui: relative path still present", file=sys.stderr)
    sys.exit(1)
PYEOF
echo "patch_ui: done ($BUNDLE)"
