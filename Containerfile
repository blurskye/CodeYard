# Packaging only: nothing is compiled in this build. The server binary and the
# browser bundle are built outside the image (scripts/release.sh). Before building
# the image, place the release in dist/release/:
#
#   dist/release/codeyard            the server binary
#   dist/release/codeyard-actions    the in-house workflow runner
#   dist/release/act                 pinned act with job-scoped environment values
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
    && install -m 0755 codeyard-actions /runtime/usr/local/bin/codeyard-actions \
    && install -m 0755 act /runtime/usr/local/bin/act \
    && cp -R webui /runtime/app/webui/dist

FROM docker.io/library/debian:trixie-slim

# The workflow runner starts job containers with the `podman` command, which reaches the host
# daemon through the mounted socket. Backups run pg_dump and pg_restore, which must be at least
# the server's major version (compose.yaml runs PostgreSQL 18), so they come from the PostgreSQL
# project's repository.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates curl git libssl3 podman-remote tini \
    && install -d /usr/share/postgresql-common/pgdg \
    && curl -fsSo /usr/share/postgresql-common/pgdg/apt.postgresql.org.asc \
        https://www.postgresql.org/media/keys/ACCC4CF8.asc \
    && echo "deb [signed-by=/usr/share/postgresql-common/pgdg/apt.postgresql.org.asc] https://apt.postgresql.org/pub/repos/apt trixie-pgdg main" \
        > /etc/apt/sources.list.d/pgdg.list \
    && apt-get update \
    && apt-get install -y --no-install-recommends postgresql-client-18 \
    && ln -s /usr/bin/podman-remote /usr/local/bin/podman \
    && rm -rf /var/lib/apt/lists/*

COPY --from=package /runtime/ /

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
