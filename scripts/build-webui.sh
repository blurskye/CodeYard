#!/usr/bin/env bash
# Build the browser bundle: the Dioxus WASM application and its JS glue, which
# the server serves from WEBUI_DIST, plus the Tailwind CSS the server embeds.
#
# Needs the Dioxus CLI (DX, version 0.7.10) and the standalone Tailwind CSS binary
# (TAILWIND, version 4.1.18), plus Rust:
#
#   DX=/path/to/dx TAILWIND=/path/to/tailwindcss bash scripts/build-webui.sh [release|debug]
#   TAILWIND=/path/to/tailwindcss bash scripts/build-webui.sh css > webui/src/assets/tailwind.css
#
# release|debug  Check that the committed CSS is what Tailwind generates, then build the
#                bundle into $WEBUI_DIST (default <checkout>/dist/webui): index.html,
#                which the server reads only to find the module script (it is never
#                served as a page), and assets/*.js|*.wasm. A stale CSS file stops the
#                build; the server embeds that file, so refresh and commit it first.
# check          Check committed CSS without building the browser bundle.
# css            Print the CSS Tailwind generates from the views to stdout (progress
#                goes to stderr), changing nothing. Redirect it into
#                webui/src/assets/tailwind.css to refresh the committed copy.
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$ROOT"

MODE=${1:-release}
case $MODE in
    release | debug | css | check) ;;
    *) echo "usage: $0 [release|debug|css|check]" >&2; exit 2 ;;
esac

DX=${DX:-$(command -v dx || true)}
TAILWIND=${TAILWIND:-$(command -v tailwindcss || true)}
TARGET_DIR=${CARGO_TARGET_DIR:-$ROOT/target}
OUT=${WEBUI_DIST:-$ROOT/dist/webui}
CSS=webui/src/assets/tailwind.css

[[ -x $TAILWIND ]] || { echo "Tailwind not found: set TAILWIND to the standalone 4.1.18 binary" >&2; exit 1; }
tailwind_help=$("$TAILWIND" --help 2>&1 || true)
[[ $tailwind_help == *v4.1.18* ]] || { echo "Tailwind 4.1.18 is required" >&2; exit 1; }

css_tmp=$(mktemp)
staging=
dx_pid=
cleanup() {
    local status=$?
    trap - EXIT
    if [[ -n $dx_pid ]]; then
        kill -TERM "$dx_pid" 2>/dev/null || true
        wait "$dx_pid" 2>/dev/null || true
    fi
    [[ -z $staging ]] || rm -rf -- "$staging"
    rm -f "$css_tmp"
    exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
"$TAILWIND" -i webui/src/input.css -o "$css_tmp" --minify >&2

if [[ $MODE == css ]]; then
    cat "$css_tmp"
    exit 0
fi

if [[ $(sha256sum <"$css_tmp") != "$(sha256sum <"$CSS")" ]]; then
    echo "$CSS is stale; refresh it with: bash scripts/build-webui.sh css > $CSS" >&2
    exit 1
fi
[[ $MODE != check ]] || exit 0
[[ -x $DX ]] || { echo "Dioxus CLI not found: set DX (version 0.7.10 is required)" >&2; exit 1; }
[[ $OUT == /* && $OUT != / ]] || { echo "WEBUI_DIST must be an absolute directory, not /" >&2; exit 1; }
DX_PUBLIC=$(realpath -m -- "$TARGET_DIR/dx/codeyard/$MODE/web/public")
OUT=$(realpath -m -- "$OUT")
[[ $OUT != "$ROOT" && $ROOT != "$OUT/"* && $OUT != / ]] || {
    echo "Bundle output must not be the checkout or its ancestor" >&2
    exit 1
}
[[ $DX_PUBLIC != "$OUT" && $DX_PUBLIC != "$OUT/"* && $OUT != "$DX_PUBLIC/"* ]] || {
    echo "WEBUI_DIST must not overlap the Dioxus build cache" >&2
    exit 1
}
# Only replace directories this script has published, never arbitrary existing data.
require_bundle() {
    local directory=$1 marker=$1/.codeyard-webui-bundle
    [[ -e $directory || -L $directory ]] || return 0
    [[ -d $directory && ! -L $directory && -O $directory &&
       -f $marker && ! -L $marker && -O $marker ]] || {
        echo "Refusing to replace an existing directory without an owned bundle marker: $directory" >&2
        exit 1
    }
}
# The CLI owns its build cache, including partial output after a failed build.
# Lock it while building and copying; only the published output needs our marker.
lock_fds=()
while IFS= read -r directory; do
    mkdir -p "$(dirname "$directory")"
    exec {lock_fd}>"$directory.lock"
    flock "$lock_fd"
    lock_fds+=("$lock_fd")
done < <(printf '%s\n' "$DX_PUBLIC" "$OUT" | LC_ALL=C sort -u)
require_bundle "$OUT"
dx_version=$("$DX" --version)
[[ $dx_version == *0.7.10* ]] || { echo "Dioxus CLI 0.7.10 is required, found: $dx_version" >&2; exit 1; }


echo "== browser dependency graph"
# The browser build must not pull in the server crates (database, git, SSH, S3).
cargo tree --locked -p codeyard --target wasm32-unknown-unknown --no-default-features \
    --features web --edges normal --prefix none --format '{p}' | python3 scripts/check-web-dependencies.py

# Debug DWARF makes the development WASM exceed the server asset size limit.
# Keep both profiles servable; the dev profile remains unoptimized.
echo "== dx build ($MODE)"
staging=$(mktemp -d "$(dirname "$OUT")/.webui.XXXXXX")
PUBLIC=$staging/bundle
args=(build --platform web --package codeyard --bin codeyard --no-default-features --features web
    --cargo-args=--locked --force-sequential --debug-symbols false)
if [[ $MODE == release ]]; then
    args+=(--release)
fi
"$DX" "${args[@]}" &
dx_pid=$!
if wait "$dx_pid"; then
    dx_pid=
else
    status=$?
    dx_pid=
    exit "$status"
fi
[[ -f $DX_PUBLIC/index.html ]] || { echo "dx did not produce $DX_PUBLIC/index.html" >&2; exit 1; }
mkdir "$PUBLIC"
cp -R "$DX_PUBLIC/." "$PUBLIC/"
# Retain immutable hashed assets: a serving process may still have cached the old loader.
# Publish assets first and replace only the index atomically, never move into a directory.
touch "$PUBLIC/.codeyard-webui-bundle"
if [[ ! -e $OUT && ! -L $OUT ]]; then
    # The first publication installs its ownership marker with the directory.
    mv -T --no-clobber "$PUBLIC" "$OUT"
    if [[ ! -e $PUBLIC ]]; then
        find "$OUT" -type f -printf '%10s  %P\n' | sort -k2
        exit 0
    fi
fi
require_bundle "$OUT"
cp -R "$PUBLIC/." "$OUT/" --no-clobber
index=$(mktemp "$OUT/.index.XXXXXX")
cp "$PUBLIC/index.html" "$index"
mv -T -- "$index" "$OUT/index.html"
find "$OUT" -type f -printf '%10s  %P\n' | sort -k2
