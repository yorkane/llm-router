#!/usr/bin/env bash
# watcher/patch_ui_effort.sh - make the llama.cpp webui thinking-effort picker usable
# behind the router: widen the effort list (none / minimal / xhigh / max / ultra),
# keep the control visible when the backend is not llama.cpp, and make the picker send
# a top-level reasoning_effort instead of chat_template_kwargs.enable_thinking.
# Idempotent; run after watcher/patch_ui.sh whenever the ui/ bundle is refreshed.
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

# 2) options array: insert XHigh and Ultra before the Max entry, and spell out that
# Default is the "send nothing" (null) option so it is not mistaken for a real effort
# level - the router/llama.cpp default is the model's own, not a forced one.
opts_old = '{hasInfo:!0,label:"Max",value:es.MAX}'
opts_new = '{label:"XHigh",value:es.XHIGH},{hasInfo:!0,label:"Max",value:es.MAX},{label:"Ultra",value:es.ULTRA}'
if 'value:es.XHIGH' not in t:
    assert opts_old in t, "effort options anchor missing (bundle changed upstream?)"
    t = t.replace(opts_old, opts_new, 1)
    changed += 1

if 'label:"Default (null)"' not in t:
    a = '{label:"Default",value:es.DEFAULT}'
    assert a in t, "default option anchor missing"
    t = t.replace(a, '{label:"Default (null)",value:es.DEFAULT}', 1)
    changed += 1

# 3) budget map: give the new values a display hint (token budgets)
bud_old = '[es.MAX]:-1'
bud_new = '[es.MAX]:-1,[es.XHIGH]:16384,[es.ULTRA]:32768'
if 'es.XHIGH]:' not in t:
    assert bud_old in t, "effort budget anchor missing (bundle changed upstream?)"
    t = t.replace(bud_old, bud_new, 1)
    changed += 1

# 4) enum: add NONE. Guard must be effort-enum-scoped: the bundle also defines an
# unrelated reasoning_format enum with t.NONE="none", and a bare 't.NONE="none"' probe
# matches that one instead, silently skipping THIS hunk while hunks 5/6 still add
# es.NONE references - leaving es.NONE undefined, so the "None" row renders blank and
# selecting it sends no effort at all. Anchor on the MEDIUM/OFF pair, which only the
# effort enum has.
# Guard must stay idempotent once hunk 7 has inserted MINIMAL in front of NONE.
na = 't.MEDIUM="medium",t.OFF="off",t.XHIGH="xhigh"'
nb = 't.MEDIUM="medium",t.NONE="none",t.OFF="off",t.XHIGH="xhigh"'
none_done = ('t.MEDIUM="medium",t.NONE="none"' in t) or ('t.MINIMAL="minimal",t.NONE="none"' in t)
if not none_done:
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

if 't.MEDIUM="medium",t.MINIMAL="minimal"' not in t:
    na = 't.MEDIUM="medium",t.NONE="none",t.OFF="off"'
    nb = 't.MEDIUM="medium",t.MINIMAL="minimal",t.NONE="none",t.OFF="off"'
    assert na in t, "minimal enum anchor missing (hunk 4 must run first)"
    t = t.replace(na, nb, 1)
    changed += 1

# 7) options: Minimal between None and Low
if '{label:"Minimal"' not in t:
    na = '{label:"None",value:es.NONE},{label:"Low",value:es.LOW}'
    nb = '{label:"None",value:es.NONE},{label:"Minimal",value:es.MINIMAL},{label:"Low",value:es.LOW}'
    if '[label:"Minimal"' not in t:
        assert na in t, "minimal options anchor missing"
        t = t.replace(na, nb, 1)
        changed += 1

# 8) budget map: minimal -> 256
na = '[es.NONE]:0,'
nb = '[es.MINIMAL]:256,[es.NONE]:0,'
if '[es.MINIMAL]:' not in t:
    assert na in t, "minimal budget anchor missing"
    t = t.replace(na, nb, 1)
    changed += 1

# (no hunk 9 on purpose) The picker is gated on /props advertising a thinking-capable
# chat_template. That capability question is answered router-side instead: see
# ui_props_with_thinking() in watcher/patches/ui/handlers.snippet. Do not special-case
# router mode here - llama-ui only enters router mode when /props says role:"router",
# and that mode additionally drives /models/sse plus model load/unload controls the
# router does not own, which leaves the UI listing "No models available".

# 10) request body: the picker must drive reasoning_effort, not chat_template_kwargs.
# Upstream only derives thinking_budget_tokens + chat_template_kwargs.enable_thinking,
# and every backend behind our routers rejects chat_template_kwargs with
# 400 chat_template_option_not_supported, so selecting any effort would fail.
# default -> send nothing (backend decides); off/none -> reasoning_effort "none";
# any other level -> forwarded verbatim. thinking_budget_tokens stays as-is: it is a
# native llama.cpp knob and ignored harmlessly elsewhere (verified 200 on ninfer).
na = 'm!==void 0&&(ue.chat_template_kwargs={...ue.chat_template_kwargs??{},enable_thinking:m})'
nb = 'O!==void 0&&O!==es.DEFAULT&&(O===es.OFF||O===es.NONE?ue.reasoning_effort="none":ue.reasoning_effort=O)'
if 'enable_thinking:m' in t:
    assert na in t, "effort request-body anchor missing (bundle changed upstream?)"
    t = t.replace(na, nb, 1)
    changed += 1

if changed:
    open(path, "w").write(t)
    print(f"patch_ui_effort: {changed} hunks applied")
else:
    print("patch_ui_effort: already applied")

# 11) sw.js: give the patched bundle an explicit precache revision, computed from the
# FINAL bytes (hence after the write above). Upstream ships that entry with
# revision:null, so the URL is its own version key: after a redeploy the service worker
# keeps serving the previously cached bundle and the picker silently stays on the old
# code. A content md5 makes workbox refetch.
import hashlib, os
sw_dir = path[: path.rindex("/_app/")]
sw = sw_dir + "/sw.js"
rel = os.path.relpath(path, sw_dir)
digest = hashlib.md5(open(path, "rb").read()).hexdigest()
entry = '{url:"%s",revision:null}' % rel
pinned = '{url:"%s",revision:"%s"}' % (rel, digest)
s = open(sw).read()
if entry in s:
    open(sw, "w").write(s.replace(entry, pinned, 1))
    print("patch_ui_effort: pinned %s revision %s in sw.js" % (rel, digest))
elif pinned not in s:
    print("patch_ui_effort: WARNING sw.js precache entry for %s not found; revision not pinned" % rel)
PYEOF
echo "patch_ui_effort: done ($BUNDLE)"
