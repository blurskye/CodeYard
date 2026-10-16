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

if [[ -n $(git status --porcelain) && ${ALLOW_DIRTY:-} != 1 ]]; then
    echo "The working tree has uncommitted changes; commit them or set ALLOW_DIRTY=1." >&2
    exit 1
fi

echo "== Build dist/release"
rm -rf dist/release
mkdir -p dist/release
WEBUI_DIST="$PWD/dist/release/webui" bash scripts/build-webui.sh release
cargo build --release --locked --bin codeyard
install -m 0755 "${CARGO_TARGET_DIR:-target}/release/codeyard" dist/release/codeyard
# The image expects this archive. It is empty: no extra runtime files are shipped.
tar -czf dist/release/runtime-nix.tar.gz --files-from /dev/null
(
    cd dist/release
    {
        sha256sum codeyard runtime-nix.tar.gz
        find webui -type f | LC_ALL=C sort | xargs sha256sum
    } > SHA256SUMS
)

echo "== Build the image $IMAGE:$TAG"
podman build -f Containerfile \
    --label "org.opencontainers.image.revision=$(git rev-parse HEAD)" \
    -t "$IMAGE:$TAG" -t "$IMAGE:latest" .
podman image inspect "$IMAGE:$TAG" --format 'built {{.Id}} ({{.Size}} bytes)'
