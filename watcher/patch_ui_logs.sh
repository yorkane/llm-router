#!/usr/bin/env bash
# watcher/patch_ui_logs.sh — inject the Logs/Config entry <script> tag into the llama.cpp webui index.html.
# Same style / same idempotency contract as watcher/patch_ui.sh.
#
# Single-script design: logs-inject.js injects BOTH nav buttons (Logs -> logs.html,
# Config -> config.html), so index.html keeps exactly one injected line and --check
# keeps validating just that one reference.
#
# ui/index.html is a SvelteKit build artifact ("auto generated, do not edit"). Upstream is not
# rebuilt here, so exactly one tag is added right before </body> (measured anchor: the last
# "</div>" line immediately followed by the "</body>" line, tab indented). A llama.cpp upgrade
# regenerates index.html and drops the tag — re-run this script. logs.html, config.html and
# logs-inject.js survive an upgrade on their own: they are not build output and are not in
# sw.js precache.
#
# Usage:
#   bash watcher/patch_ui_logs.sh [ui_dir]            # default: <repo>/ui
#   bash watcher/patch_ui_logs.sh --check [ui_dir]    # verify only; exit 1 when the tag is absent
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CHECK=0
if [ "${1:-}" = "--check" ]; then CHECK=1; shift; fi
UI_DIR="${1:-$ROOT/ui}"
INDEX="$UI_DIR/index.html"

[ -f "$INDEX" ] || { echo "patch_ui_logs: $INDEX not found" >&2; exit 1; }

# 幂等：已存在注入行（或任何等价引用）直接跳过
if grep -qF 'logs-inject.js' "$INDEX"; then
  echo "patch_ui_logs: already present ($INDEX)"
  exit 0
fi

if [ "$CHECK" = 1 ]; then
  echo "patch_ui_logs: tag missing in $INDEX (run: bash watcher/patch_ui_logs.sh)" >&2
  exit 1
fi

# 锚点：文件里最后一个 </body>，在其所在行行首插入一行（tab 缩进与实测推荐形态一致）。
# 锚点缺失必须报错退出非 0：静默跳过会让 Logs 入口悄悄消失，事后极难发现。
python3 - "$INDEX" <<"PYEOF"
import sys
path = sys.argv[1]
t = open(path, "r", encoding="utf-8").read()
tag = '<script src="./logs-inject.js" defer></script>'

body = t.rfind("</body>")
if body < 0:
    sys.stderr.write("patch_ui_logs: anchor </body> not found in %s\n" % path)
    sys.exit(1)
nl = t.rfind("\n", 0, body)             # </body> 所在行的行首（换行符位置）
if nl < 0:
    sys.stderr.write("patch_ui_logs: cannot locate line start before </body>\n")
    sys.exit(1)
# 校验 </div> 紧邻在 </body> 之前（实测锚点形态），只允许空白差异
middle = t[nl + 1:body]
if middle.strip() not in ("", "</div>") and not t[:nl + 1].rstrip().endswith("</div>"):
    sys.stderr.write("patch_ui_logs: unexpected structure before </body>: %r\n" % middle[:40])
    sys.exit(1)

out = t[:nl + 1] + "\t\t" + tag + "\n" + t[nl + 1:]
open(path, "w", encoding="utf-8").write(out)
print("patch_ui_logs: injected before </body>")
PYEOF

grep -qF 'logs-inject.js' "$INDEX" || { echo "patch_ui_logs: injection did not land" >&2; exit 1; }
echo "patch_ui_logs: done ($INDEX)"
