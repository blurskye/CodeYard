#!/usr/bin/env bash
# Build the server binary and browser bundle into dist/release/, then package the server image.
#
#   scripts/release.sh [tag]      tag defaults to the version in Cargo.toml
#
# Runs on this machine and needs Rust, the Dioxus CLI (DX) and the standalone Tailwind
# CSS binary (TAILWIND); see scripts/build-webui.sh. The release is built from the
# committed source: commit first, or set ALLOW_DIRTY=1 to build the working tree anyway.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

IMAGE=${IMAGE:-localhost/codeyard-backend}
TAG=${1:-$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)}
if [[ $TAG == latest ]]; then
    echo "Give a version tag; latest is tagged after the image has started." >&2
    exit 1
fi

if git rev-parse --git-dir >/dev/null 2>&1; then
    revision=$(git rev-parse --verify HEAD) || exit 1
    source_status=$(git status --porcelain --untracked-files=all) || exit 1
    dirty=0
    [[ -z $source_status ]] || dirty=1
else
    revision=${CODEYARD_SOURCE_REVISION:-}
    dirty=${CODEYARD_SOURCE_DIRTY:-}
fi
[[ $revision =~ ^([0-9a-f]{40}|[0-9a-f]{64})$ && $dirty =~ ^[01]$ ]] || {
    echo "Release requires Git metadata or valid CODEYARD_SOURCE_REVISION and CODEYARD_SOURCE_DIRTY from scripts/vps." >&2
    exit 1
}
if [[ $dirty == 1 && ${ALLOW_DIRTY:-} != 1 ]]; then
    echo "The working tree has uncommitted changes; commit them or set ALLOW_DIRTY=1." >&2
    exit 1
fi

echo "== Build dist/release"
rm -rf dist/release
mkdir -p dist/release
WEBUI_DIST="$PWD/dist/release/webui" bash scripts/build-webui.sh release
cargo build --release --locked --bin codeyard --message-format=json >dist/release/cargo-artifacts.json
executable=$(python3 -I -c '
import json, sys
paths = [m["executable"] for line in sys.stdin if (m := json.loads(line)).get("reason") == "compiler-artifact"
         and m["target"]["name"] == "codeyard" and "bin" in m["target"]["kind"] and m.get("executable")]
if len(paths) != 1:
    sys.exit("Expected one codeyard executable from Cargo")
print(paths[0])
' <dist/release/cargo-artifacts.json)
rm dist/release/cargo-artifacts.json
install -m 0755 "$executable" dist/release/codeyard
# A binary built in a Nix shell loads its interpreter and libraries from /nix/store, and the
# image has none of those paths. Ship their closure in the archive the image unpacks. A binary
# that names no store path gets an empty archive.
store_roots=$(python3 -I scripts/elf-store-paths.py dist/release/codeyard)
store_paths=$PWD/dist/store-paths.txt
: >"$store_paths"
if [[ -n $store_roots ]]; then
    if ! command -v nix-store >/dev/null; then
        echo "codeyard names these /nix/store paths, but nix-store is not on PATH to find their closure:" >&2
        echo "$store_roots" >&2
        exit 1
    fi
    printf '%s\n' "$store_roots" | xargs nix-store -qR | sort -u | sed 's|^/||' >"$store_paths"
fi
tar -czf dist/release/runtime-nix.tar.gz --owner=0 --group=0 --numeric-owner \
    -C / --files-from="$store_paths"
(
    cd dist/release
    {
        sha256sum codeyard runtime-nix.tar.gz
        find webui -type f | LC_ALL=C sort | xargs sha256sum
    } > SHA256SUMS
)

echo "== Build the image $IMAGE:$TAG"
podman build -f Containerfile \
    --label "org.opencontainers.image.revision=$revision" \
    -t "$IMAGE:$TAG" .
# The binary has to start inside the image. A /nix/store path or library that the image lacks
# fails here, before the image is tagged latest.
podman run --rm "$IMAGE:$TAG" codeyard --version >/dev/null || {
    echo "codeyard does not run in $IMAGE:$TAG; see the error above. A file it needs may be missing from the image." >&2
    exit 1
}
podman tag "$IMAGE:$TAG" "$IMAGE:latest"
podman image inspect "$IMAGE:$TAG" --format 'built {{.Id}} ({{.Size}} bytes)'
