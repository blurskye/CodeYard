#!/usr/bin/env bash
# Exercise publication failure and retry using owned fake build tools, without compiling.
set -euo pipefail
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
fixture=$(mktemp -d)
trap 'rm -rf -- "$fixture"' EXIT
mkdir -p "$fixture/scripts" "$fixture/tools" "$fixture/webui/src/assets"
cp "$ROOT/scripts/build-webui.sh" "$ROOT/scripts/check-web-dependencies.py" "$fixture/scripts/"
printf 'fixture css\n' >"$fixture/webui/src/assets/tailwind.css"
cat >"$fixture/tools/tailwind" <<'TAILWIND'
#!/usr/bin/env bash
set -euo pipefail
if [[ $1 == --help ]]; then echo 'tailwindcss v4.1.18'; exit; fi
while (($#)); do
    if [[ $1 == -o ]]; then printf 'fixture css\n' >"$2"; exit; fi
    shift
done
exit 2
TAILWIND
cat >"$fixture/tools/cargo" <<'CARGO'
#!/usr/bin/env bash
set -euo pipefail
[[ $1 == tree && " $* " == *' -p codeyard '* ]] || exit 2
printf '%s\n' 'codeyard v0.1.5' 'codeyard-webui v0.1.5' 'dioxus-web v0.7.10'
[[ ${BAD_GRAPH:-0} != 1 ]] || echo 'sqlx v0.8.6'
exit 0
CARGO
cat >"$fixture/tools/dx" <<'DX'
#!/usr/bin/env bash
set -euo pipefail
if [[ $1 == --version ]]; then echo 'dioxus 0.7.10'; exit; fi
[[ $1 == build ]] || exit 2
public=$CARGO_TARGET_DIR/dx/codeyard/debug/web/public
mkdir -p "$public/assets"
printf '%s\n' "$BUNDLE_VERSION" >"$public/index.html"
printf '%s\n' "$BUNDLE_VERSION" >"$public/assets/$BUNDLE_VERSION.data"
[[ ${FAIL_BUILD:-0} != 1 ]] || exit 7
DX
chmod +x "$fixture/tools/"*
export PATH="$fixture/tools:$PATH" DX="$fixture/tools/dx" TAILWIND="$fixture/tools/tailwind"
export WEBUI_DIST="$fixture/published" CARGO_TARGET_DIR="$fixture/target" BUNDLE_VERSION=first
build() { bash "$fixture/scripts/build-webui.sh" debug >"$fixture/build.log" 2>&1; }
fails() {
    if build; then cat "$fixture/build.log" >&2; echo 'Build unexpectedly succeeded' >&2; exit 1; fi
}
# An unrelated output directory remains untouched.
mkdir "$WEBUI_DIST"
printf 'preserve\n' >"$WEBUI_DIST/keep"
fails
grep -q 'without an owned bundle marker' "$fixture/build.log"
[[ $(cat "$WEBUI_DIST/keep") == preserve ]]
rm -rf -- "$WEBUI_DIST"
# Partial CLI output is a build cache, not a published bundle.
FAIL_BUILD=1 fails
[[ ! -e $WEBUI_DIST ]]
build
[[ -f $WEBUI_DIST/.codeyard-webui-bundle && $(cat "$WEBUI_DIST/index.html") == first ]]
BUNDLE_VERSION=second FAIL_BUILD=1 fails
[[ $(cat "$WEBUI_DIST/index.html") == first && ! -e $WEBUI_DIST/assets/second.data ]]
BAD_GRAPH=1 fails
grep -q 'Browser dependency graph imports native authority: sqlx' "$fixture/build.log"
[[ $(cat "$WEBUI_DIST/index.html") == first ]]
BUNDLE_VERSION=second build
[[ $(cat "$WEBUI_DIST/index.html") == second ]]
[[ -f $WEBUI_DIST/assets/first.data && -f $WEBUI_DIST/assets/second.data ]]
[[ -z $(find "$fixture" -maxdepth 1 -type d -name '.webui.*' -print) ]]
echo 'Bundle publication preserves unknown data, retries failed builds and retains old assets.'
