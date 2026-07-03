#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=common.sh
source "$SCRIPT_DIR/common.sh"

WORKSPACE=${GITHUB_WORKSPACE:?GITHUB_WORKSPACE is required}
ACTION_ROOT=${EDGEZERO_ACTION_ROOT:?EDGEZERO_ACTION_ROOT is required}
WORKING_DIRECTORY=${INPUT_WORKING_DIRECTORY:-.}
MANIFEST=${INPUT_MANIFEST:-}
RUST_TOOLCHAIN_INPUT=${INPUT_RUST_TOOLCHAIN:-auto}
CACHE=${INPUT_CACHE:-false}
ACTION_REF=${EDGEZERO_ACTION_REF:-}

WORKSPACE_REAL=$(canonical_path "$WORKSPACE")
APP_DIR=$(canonical_path "$WORKSPACE/$WORKING_DIRECTORY")
is_under "$WORKSPACE_REAL" "$APP_DIR" || fail "input 'working-directory' must resolve inside github.workspace"
[[ -d "$APP_DIR" ]] || fail "working-directory '$WORKING_DIRECTORY' does not exist or is not a directory"
APP_REL=$(relative_to "$WORKSPACE_REAL" "$APP_DIR")

if [[ -n "$MANIFEST" ]]; then
  MANIFEST_PATH=$(canonical_path "$APP_DIR/$MANIFEST")
  is_under "$WORKSPACE_REAL" "$MANIFEST_PATH" || fail "input 'manifest' must resolve inside github.workspace"
  [[ -f "$MANIFEST_PATH" ]] || fail "manifest '$APP_REL/$MANIFEST' does not exist or is not a regular file"
  MANIFEST_REL=$(relative_to "$WORKSPACE_REAL" "$MANIFEST_PATH")
else
  MANIFEST_PATH=""
  MANIFEST_REL="EdgeZero default discovery"
fi

APP_GIT_ROOT=$(git -C "$APP_DIR" rev-parse --show-toplevel 2>/dev/null || true)
[[ -n "$APP_GIT_ROOT" ]] || fail "working-directory '$APP_REL' is not inside a Git repository"
APP_GIT_ROOT=$(canonical_path "$APP_GIT_ROOT")
is_under "$WORKSPACE_REAL" "$APP_GIT_ROOT" || fail "application Git root must resolve inside github.workspace"
SOURCE_REVISION=$(git -C "$APP_GIT_ROOT" rev-parse HEAD)
if ! git -C "$APP_GIT_ROOT" diff --quiet --ignore-submodules -- || \
   ! git -C "$APP_GIT_ROOT" diff --cached --quiet --ignore-submodules -- || \
   [[ -n "$(git -C "$APP_GIT_ROOT" ls-files --others --exclude-standard)" ]]; then
  fail "deployments require committed source; working tree for '$APP_REL' is dirty"
fi

resolve_rust_toolchain() {
  if [[ "$RUST_TOOLCHAIN_INPUT" != "auto" ]]; then
    [[ -n "$RUST_TOOLCHAIN_INPUT" ]] || fail "input 'rust-toolchain' cannot be empty"
    printf '%s\n' "$RUST_TOOLCHAIN_INPUT"
    return
  fi

  python3 - "$APP_DIR" "$APP_GIT_ROOT" "$ACTION_ROOT" <<'PY'
import os, re, sys
app, git_root, action_root = map(os.path.realpath, sys.argv[1:4])

def parents(start, stop):
    cur = start
    stop = os.path.realpath(stop)
    while True:
        yield cur
        if cur == stop:
            break
        nxt = os.path.dirname(cur)
        if nxt == cur:
            break
        cur = nxt

def parse_rust_toolchain(path):
    raw = open(path, encoding='utf-8').read().strip()
    if not raw:
        raise SystemExit(f"::error::malformed Rust toolchain file: {path}")
    return raw.splitlines()[0].strip()

def parse_rust_toolchain_toml(path):
    raw = open(path, encoding='utf-8').read()
    m = re.search(r'(?m)^\s*channel\s*=\s*["\']([^"\']+)["\']\s*$', raw)
    if not m:
        raise SystemExit(f"::error::malformed Rust toolchain TOML file: {path}")
    return m.group(1)

def parse_tool_versions(path):
    with open(path, encoding='utf-8') as f:
        for line in f:
            parts = line.split()
            if parts and parts[0] == 'rust':
                if len(parts) < 2 or not parts[1].strip():
                    raise SystemExit(f"::error::malformed .tool-versions rust entry: {path}")
                return parts[1]
    return None

for directory in parents(app, git_root):
    toml = os.path.join(directory, 'rust-toolchain.toml')
    plain = os.path.join(directory, 'rust-toolchain')
    tools = os.path.join(directory, '.tool-versions')
    if os.path.exists(toml):
        print(parse_rust_toolchain_toml(toml))
        sys.exit(0)
    if os.path.exists(plain):
        print(parse_rust_toolchain(plain))
        sys.exit(0)
    if os.path.exists(tools):
        value = parse_tool_versions(tools)
        if value:
            print(value)
            sys.exit(0)

fallback = os.path.join(action_root, '.tool-versions')
value = parse_tool_versions(fallback) if os.path.exists(fallback) else None
if value:
    print(value)
    sys.exit(0)
raise SystemExit('::error::could not resolve Rust toolchain; checked rust-toolchain.toml, rust-toolchain, and .tool-versions; set input rust-toolchain explicitly')
PY
}

RUST_TOOLCHAIN=$(resolve_rust_toolchain)
ACTION_RUST_TOOLCHAIN=$(read_tool_version "$ACTION_ROOT/.tool-versions" rust || true)
[[ -n "$ACTION_RUST_TOOLCHAIN" ]] || fail "EdgeZero repository .tool-versions must contain a rust entry"

LOCKFILE="$APP_GIT_ROOT/Cargo.lock"
if [[ "$CACHE" == "true" && ! -f "$LOCKFILE" ]]; then
  fail "cache is enabled but Cargo.lock was not found at application Git root; exact-key caching requires Cargo.lock"
fi
LOCK_HASH="none"
if [[ -f "$LOCKFILE" ]]; then
  LOCK_HASH=$(python3 - "$LOCKFILE" <<'PY'
import hashlib, sys
h = hashlib.sha256()
with open(sys.argv[1], 'rb') as f:
    for chunk in iter(lambda: f.read(1024 * 1024), b''):
        h.update(chunk)
print(h.hexdigest())
PY
)
fi

if EDGEZERO_REVISION=$(git -C "$ACTION_ROOT" rev-parse HEAD 2>/dev/null); then
  :
elif [[ "$ACTION_REF" =~ ^[0-9a-fA-F]{40}$ ]]; then
  EDGEZERO_REVISION=$ACTION_REF
else
  fail "could not resolve immutable EdgeZero action revision; pin the action with a full commit SHA"
fi
CACHE_KEY="edgezero-deploy-${RUNNER_OS:-Linux}-${RUNNER_ARCH:-X64}-$(sanitize_ref "$RUST_TOOLCHAIN")-wasm32-wasip1-${EDGEZERO_REVISION}-${SOURCE_REVISION}-${LOCK_HASH}"
TARGET_DIR="$APP_GIT_ROOT/target"

effective_build_mode() {
  case "${INPUT_BUILD_MODE:-auto}" in
    auto) printf 'never\n' ;;
    always) printf 'always\n' ;;
    never) printf 'never\n' ;;
    *) fail "input 'build-mode' must be one of: auto, always, never" ;;
  esac
}
EFFECTIVE_BUILD_MODE=$(effective_build_mode)

append_output working-directory "$APP_DIR"
append_output working-directory-relative "$APP_REL"
append_output manifest "$MANIFEST_PATH"
append_output manifest-summary "$MANIFEST_REL"
append_output app-git-root "$APP_GIT_ROOT"
append_output source-revision "$SOURCE_REVISION"
append_output rust-toolchain "$RUST_TOOLCHAIN"
append_output action-rust-toolchain "$ACTION_RUST_TOOLCHAIN"
append_output edgezero-revision "$EDGEZERO_REVISION"
append_output effective-build-mode "$EFFECTIVE_BUILD_MODE"
append_output cache-key "$CACHE_KEY"
append_output cache-path "$TARGET_DIR"
