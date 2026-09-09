#!/usr/bin/env bash
# watcher/upstream_sync.sh — sync the vendored sgl-model-gateway from upstream.
#
#   1. reads the last synced upstream tag from gateway/.upstream-ref
#   2. picks the newest upstream v0.x.y tag strictly newer (or UPSTREAM_REF forces one)
#   3. sparse-clones only sgl-model-gateway at that tag
#   4. copies it over gateway/, reapplying the local patches: rust-only workspace,
#      harmony as a path dep, and the [profile.ci] build profile
#   5. verifies with cargo build --profile ci --bin smg (SKIP_BUILD=1 to skip)
#   6. commits, tags upstream-<date>-<ref>, pushes -> triggers build-and-publish (ghcr)
#
# Local dry run: SKIP_BUILD=1 SKIP_PUSH=1 bash watcher/upstream_sync.sh
# CI:            bash watcher/upstream_sync.sh   (UPSTREAM_REF optional)
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
GATEWAY="$ROOT/gateway"
UPSTREAM_REPO="${UPSTREAM_REPO:-https://github.com/sgl-project/sglang.git}"
UPSTREAM_DIR="sgl-model-gateway"
REF_FILE="$GATEWAY/.upstream-ref"
SKIP_BUILD="${SKIP_BUILD:-0}"
SKIP_PUSH="${SKIP_PUSH:-0}"

log() { printf "[upstream-sync] %s\n" "$*"; }

CURRENT_TAG="$(cat "$REF_FILE" 2>/dev/null || echo v0.5.18)"
log "last synced upstream tag: $CURRENT_TAG"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

# --- pick the target tag ---
if [ -n "${UPSTREAM_REF:-}" ]; then
  TARGET_REF="$UPSTREAM_REF"
  log "forced target: $TARGET_REF"
else
  git ls-remote --tags "$UPSTREAM_REPO" 'v0.*' 2>/dev/null |
    sed 's#.*refs/tags/##' | grep -v '\^{}$' | sort -V > "$WORK/tags"
  [ -s "$WORK/tags" ] || { echo "could not list upstream tags" >&2; exit 1; }
  { cat "$WORK/tags"; echo "$CURRENT_TAG"; } | sort -V > "$WORK/all"
  TARGET_REF="$(grep -A1 -xF "$CURRENT_TAG" "$WORK/all" | tail -n1)"
  if [ -z "$TARGET_REF" ]; then
    log "no upstream tag newer than $CURRENT_TAG; nothing to do"
    exit 0
  fi
  log "target tag: $TARGET_REF"
fi

# --- sparse clone of just the gateway directory at that tag ---
git clone --quiet --filter=blob:none --no-checkout --depth 1 "$UPSTREAM_REPO" "$WORK/up"
git -C "$WORK/up" sparse-checkout set "$UPSTREAM_DIR"
git -C "$WORK/up" fetch --depth 1 origin "refs/tags/$TARGET_REF:refs/tags/$TARGET_REF"
git -C "$WORK/up" checkout -q "$TARGET_REF"

STAGE="$WORK/stage"
mkdir -p "$STAGE"
cp -a "$WORK/up/$UPSTREAM_DIR/." "$STAGE/"
rm -rf "$STAGE/bindings/python" "$STAGE/.cargo"

# --- reapply local patches on top of the upstream tree ---
python3 - "$STAGE/Cargo.toml" <<'PYEOF'
import re, sys
p = sys.argv[1]
t = open(p).read()
orig = t
# 1) rust-only workspace
t = re.sub(r"(?ms)^\[workspace\](.*?)(?=^\[|\Z)", "[workspace]\nmembers = []\n\n", t, count=1)
# 2) vendored harmony as a path dep
t = re.sub(r'openai-harmony = \{ git = "https://github\.com/openai/harmony", tag = "v[^"]+" \}',
          'openai-harmony = { path = "../harmony" }', t)
# 3) ci profile used by .github/workflows/build.yml
if "[profile.ci]" not in t:
    t += ("\n[profile.ci]\ninherits = \"release\"\nopt-level = 2\n"
          "lto = \"thin\"\ncodegen-units = 16\nstrip = true\n")
if t == orig:
    print("[upstream-sync] WARNING: no local patch matched, check Cargo.toml", file=sys.stderr)
open(p, "w").write(t)
PYEOF

# keep the existing lockfile; cargo refreshes it if deps changed
[ -f "$GATEWAY/Cargo.lock" ] && cp "$GATEWAY/Cargo.lock" "$STAGE/Cargo.lock"

# --- apply into the repo ---
rsync -a --delete --exclude=.upstream-ref "$STAGE/" "$GATEWAY/"
echo "$TARGET_REF" > "$REF_FILE"
log "gateway/ updated to upstream $TARGET_REF"

# --- verify it still builds ---
if [ "$SKIP_BUILD" != "1" ]; then
  log "verifying: cargo build --profile ci --bin smg"
  (cd "$GATEWAY" && cargo build --profile ci --bin smg)
else
  log "SKIP_BUILD=1, skipping cargo build"
fi

if git -C "$ROOT" diff --quiet -- gateway; then
  log "no changes; nothing to commit"
  exit 0
fi

# --- commit, tag, push (the push triggers build-and-publish -> ghcr image) ---
git -C "$ROOT" var -l 2>/dev/null | grep -q '^user\.name=' || git -C "$ROOT" config user.name  "upstream-sync-bot"
git -C "$ROOT" var -l 2>/dev/null | grep -q '^user\.email=' || git -C "$ROOT" config user.email "noreply@github.com"
git -C "$ROOT" add gateway
git -C "$ROOT" commit -m "chore(gateway): sync sgl-model-gateway to upstream $TARGET_REF" \
  -m "Vendored tree refreshed from $UPSTREAM_REPO; local patches reapplied (rust-only workspace, harmony path dep, ci profile)."

BRANCH="$(git -C "$ROOT" branch --show-current || echo main)"
TAG="upstream-$(date -u +%Y%m%d)-$TARGET_REF"
if [ "$SKIP_PUSH" != "1" ]; then
  git -C "$ROOT" push origin "HEAD:$BRANCH"
  git -C "$ROOT" tag -f "$TAG"
  git -C "$ROOT" push origin "$TAG"
  log "pushed $BRANCH + tag $TAG; build-and-publish will build and push the ghcr image"
else
  log "SKIP_PUSH=1; commit left on $BRANCH (would tag $TAG)"
fi
