#!/usr/bin/env bash
# Build the browser bundle: the Dioxus WASM application and its JS glue, which
# the server serves from WEBUI_DIST, plus the Tailwind CSS the server embeds.
#
# Needs the Dioxus CLI (DX, version 0.7.10) and the standalone Tailwind CSS binary
# (TAILWIND, version 4.1.18), plus Rust:
#
#   DX=/path/to/dx TAILWIND=/path/to/tailwindcss bash scripts/build-webui.sh [release|debug]
#   TAILWIND=/path/to/tailwindcss bash scripts/build-webui.sh css > src/webui/assets/tailwind.css
#
# release|debug  Regenerate the CSS, then build the bundle into $WEBUI_DIST
#                (default <checkout>/dist/webui): index.html, which the server reads
#                only to find the module script (it is never served as a page), and
#                assets/*.js|*.wasm. Build the server after this: it embeds the CSS.
# css            Print the CSS Tailwind generates from the views to stdout (progress
#                goes to stderr), changing nothing. Redirect it into
#                src/webui/assets/tailwind.css to refresh the committed copy.
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$ROOT"

MODE=${1:-release}
case $MODE in
    release | debug | css) ;;
    *) echo "usage: $0 [release|debug|css]" >&2; exit 2 ;;
esac

DX=${DX:-$(command -v dx || true)}
TAILWIND=${TAILWIND:-$(command -v tailwindcss || true)}
TARGET_DIR=${CARGO_TARGET_DIR:-$ROOT/target}
OUT=${WEBUI_DIST:-$ROOT/dist/webui}
CSS=src/webui/assets/tailwind.css

[[ -x $TAILWIND ]] || { echo "Tailwind not found: set TAILWIND to the standalone 4.1.18 binary" >&2; exit 1; }
tailwind_help=$("$TAILWIND" --help 2>&1 || true)
[[ $tailwind_help == *v4.1.18* ]] || { echo "Tailwind 4.1.18 is required" >&2; exit 1; }

css_tmp=$(mktemp)
trap 'rm -f "$css_tmp"' EXIT
"$TAILWIND" -i src/webui/input.css -o "$css_tmp" --minify >&2

if [[ $MODE == css ]]; then
    cat "$css_tmp"
    exit 0
fi

[[ -x $DX ]] || { echo "Dioxus CLI not found: set DX (version 0.7.10 is required)" >&2; exit 1; }
[[ $OUT == /* && $OUT != / ]] || { echo "WEBUI_DIST must be an absolute directory, not /" >&2; exit 1; }
PUBLIC=$(realpath -m -- "$TARGET_DIR/dx/codeyard/$MODE/web/public")
OUT=$(realpath -m -- "$OUT")
[[ $OUT != / && ( $OUT == "$PUBLIC" || ( $PUBLIC != "$OUT/"* && $OUT != "$PUBLIC/"* ) ) ]] || {
    echo "WEBUI_DIST must not overlap the Dioxus output directory unless identical" >&2
    exit 1
}
dx_version=$("$DX" --version)
[[ $dx_version == *0.7.10* ]] || { echo "Dioxus CLI 0.7.10 is required, found: $dx_version" >&2; exit 1; }

echo "== CSS"
if [[ $(sha256sum <"$css_tmp") == "$(sha256sum <"$CSS")" ]]; then
    echo "$CSS is up to date"
else
    cp "$css_tmp" "$CSS"
    echo "$CSS differed from what Tailwind generates and was rewritten here; refresh the committed copy"
fi

echo "== browser dependency graph"
# The browser build must not pull in the server crates (database, git, SSH, S3).
cargo tree --locked -p codeyard --target wasm32-unknown-unknown --no-default-features \
    --features web --edges normal --prefix none --format '{p}' | python3 scripts/check-web-dependencies.py

echo "== dx build ($MODE)"
# dx keeps hashed files of earlier builds in its output; start from nothing.
rm -rf "$PUBLIC"
args=(build --platform web --package codeyard --bin codeyard --no-default-features --features web
    --cargo-args=--locked --force-sequential)
if [[ $MODE == release ]]; then
    args+=(--release --debug-symbols false)
fi
"$DX" "${args[@]}"
[[ -f $PUBLIC/index.html ]] || { echo "dx produced no $PUBLIC/index.html" >&2; exit 1; }

echo "== publish to $OUT"
if [[ $OUT != "$PUBLIC" ]]; then
    rm -rf "$OUT"
    mkdir -p "$(dirname "$OUT")"
    cp -R "$PUBLIC" "$OUT"
fi
find "$OUT" -type f -printf '%10s  %P\n' | sort -k2
