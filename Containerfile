# Packaging only: nothing is compiled in this build. The server binary and the
# browser bundle are built outside the image (scripts/release.sh). Before building
# the image, place the release in dist/release/:
#
#   dist/release/codeyard            the server binary
#   dist/release/webui/              the browser bundle (output of scripts/build-webui.sh)
#   dist/release/runtime-nix.tar.gz  the /nix/store files the binary needs at run time;
#                                    an empty archive when it needs none
#   dist/release/SHA256SUMS          sha256sum lines for the binary, runtime-nix.tar.gz and
#                                    every file under webui/
#
# The image has no root compilation, SSH credentials, source snapshot or toolchain.
FROM docker.io/library/debian:trixie-slim AS package

ARG RELEASE_DIR=dist/release
COPY ${RELEASE_DIR}/ /opt/codeyard-release/
# Checksums bind the binary, bundle and runtime closure. The closure holds only
# immutable /nix/store runtime dependencies (possibly empty for a portable binary),
# never build homes, toolchains or secrets.
RUN cd /opt/codeyard-release \
    && sha256sum -c SHA256SUMS \
    && mkdir -p /runtime/nix/store /runtime/usr/local/bin /runtime/app/webui \
    && tar -xzf runtime-nix.tar.gz -C /runtime \
    && install -m 0755 codeyard /runtime/usr/local/bin/codeyard \
    && cp -R webui /runtime/app/webui/dist

FROM docker.io/library/debian:trixie-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates curl git libssl3 tini \
    && rm -rf /var/lib/apt/lists/*

COPY --from=package /runtime/ /
# Archive pin for v0.2.89 (published 2026-06-01), taken from the release's
# checksums.txt and matched against the downloaded archive. Versions before
# 0.2.86 have CVE-2026-34041 and CVE-2026-34042. To change the version, change the
# URL and the hash together; do not trust a checksum downloaded beside mutable bytes.
RUN curl -fsSL "https://github.com/nektos/act/releases/download/v0.2.89/act_Linux_x86_64.tar.gz" -o /tmp/codeyard-act.tar.gz \
    && printf '%s\n' '0191d6f1f3b716b5c55820032605d05fc3c1cdbf581ebeff655019e5dd1524c0  /tmp/codeyard-act.tar.gz' | sha256sum -c - \
    && tar -xzf /tmp/codeyard-act.tar.gz -C /usr/local/bin act \
    && chown 0:0 /usr/local/bin/act \
    && chmod 0755 /usr/local/bin/act \
    && rm /tmp/codeyard-act.tar.gz

WORKDIR /app
# Database: DATABASE_URL (PostgreSQL) or, unset, SQLite at $GITSERV_STATE_DIR/gitserv.db.
# Uploads: $GITSERV_STATE_DIR/objects, or the S3 bucket when S3_* is set.
# GITSERV_STATE_DIR=/data places gitserv.db directly in /data. The default is /data/state.
ENV RUST_LOG=info \
    GITSERV_STATE_DIR=/data/state \
    REPOS_PATH=/data/repos \
    HTTP_ADDR=0.0.0.0:3000 \
    SSH_ADDR=0.0.0.0:2222 \
    HOST_KEY_PATH=/data/host_key \
    WEBUI_DIST=/app/webui/dist

# Trusted workflows ONLY: mounting a Podman socket is host-user execution, not
# an untrusted workflow sandbox.
VOLUME ["/data"]
EXPOSE 3000 2222 7444/udp 7445/tcp
ENTRYPOINT ["/usr/bin/tini", "--"]
CMD ["codeyard", "serve"]
