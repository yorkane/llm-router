#!/usr/bin/env bash
# watcher/patch_ui_effort.sh - extend the llama.cpp webui thinking-effort picker with
# xhigh / max / ultra so the router can pass those efforts through. Idempotent; run
# after watcher/patch_ui.sh whenever the ui/ bundle is refreshed from upstream.
#
# The stock bundle enum es has {default, off, low, medium, high, max}; max already
# exists, so this adds xhigh and ultra to the enum, the DSt options array, and the
# yIe budget map (budget is a UI-side display hint only; the router forwards
# reasoning_effort verbatim and the model/backend decides).
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
UI_DIR="${1:-$ROOT/ui}"
BUNDLE="$(ls "$UI_DIR"/_app/immutable/bundle.*.js 2>/dev/null | head -n1)"
[ -n "$BUNDLE" ] || { echo "patch_ui_effort: bundle not found under $UI_DIR" >&2; exit 1; }

python3 - "$BUNDLE" <<'PYEOF'
import sys
path = sys.argv[1]
t = open(path).read()
changed = 0

# 1) enum: add XHIGH and ULTRA
enum_old = 't.MAX="max",t.MEDIUM="medium",t.OFF="off",t))(es||{})'
enum_new = 't.MAX="max",t.MEDIUM="medium",t.OFF="off",t.XHIGH="xhigh",t.ULTRA="ultra",t))(es||{})'
if 'XHIGH="xhigh"' not in t:
    assert enum_old in t, "effort enum anchor missing (bundle changed upstream?)"
    t = t.replace(enum_old, enum_new, 1)
    changed += 1

# 2) options array: insert XHigh and Ultra before the Max entry
opts_old = '{hasInfo:!0,label:"Max",value:es.MAX}'
opts_new = '{label:"XHigh",value:es.XHIGH},{hasInfo:!0,label:"Max",value:es.MAX},{label:"Ultra",value:es.ULTRA}'
if 'value:es.XHIGH' not in t:
    assert opts_old in t, "effort options anchor missing (bundle changed upstream?)"
    t = t.replace(opts_old, opts_new, 1)
    changed += 1

# 3) budget map: give the new values a display hint (token budgets)
bud_old = '[es.MAX]:-1'
bud_new = '[es.MAX]:-1,[es.XHIGH]:16384,[es.ULTRA]:32768'
if 'es.XHIGH]:' not in t:
    assert bud_old in t, "effort budget anchor missing (bundle changed upstream?)"
    t = t.replace(bud_old, bud_new, 1)
    changed += 1

# 4) enum: add NONE (null-equivalent effort)
na = 't.MEDIUM="medium",t.OFF="off",t.XHIGH="xhigh"'
nb = 't.MEDIUM="medium",t.NONE="none",t.OFF="off",t.XHIGH="xhigh"'
if 't.NONE="none"' not in t:
    assert na in t, "none enum anchor missing"
    t = t.replace(na, nb, 1)
    changed += 1

# 5) options: None entry between Off and Low
na2 = '{label:"Off",value:es.OFF},{label:"Low",value:es.LOW}'
nb2 = '{label:"Off",value:es.OFF},{label:"None",value:es.NONE},{label:"Low",value:es.LOW}'
if 'value:es.NONE' not in t:
    assert na2 in t, "none options anchor missing"
    t = t.replace(na2, nb2, 1)
    changed += 1

# 6) budget map: none -> 0
na3 = '[es.XHIGH]:16384'
nb3 = '[es.NONE]:0,[es.XHIGH]:16384'
if 'es.NONE]:' not in t:
    assert na3 in t, "none budget anchor missing"
    t = t.replace(na3, nb3, 1)
    changed += 1

if changed:
    open(path, "w").write(t)
    print(f"patch_ui_effort: {changed} hunks applied")
else:
    print("patch_ui_effort: already applied")
PYEOF
echo "patch_ui_effort: done ($BUNDLE)"
