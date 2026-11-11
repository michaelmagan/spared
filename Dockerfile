# syntax=docker/dockerfile:1
# spared server image: the host binary plus three example apps, deployed on first start.
#
# Build (repository root as context):
#   docker build -t spared:local .
# Run:
#   docker run -d --name spared --read-only --tmpfs /tmp:rw,noexec,nosuid,size=64m --user 10001:10001 \
#     --cap-drop=ALL --security-opt=no-new-privileges --pids-limit=256 --memory=512m \
#     -p 127.0.0.1:8080:8080 -v spared-data:/data spared:local
#   curl http://notes.localhost:8080/        # or http://localhost:8080/_/notes/
# The entrypoint turns on path routing (/_/<app>/) for these bundled examples; it puts every app
# on one browser origin. Pass -e SPARED_PATH_ROUTING=0 to turn it off (path routing is off by default elsewhere).
#
# Stage 1 needs network (crates.io, GitHub for wasi-sdk) and about 8 GB of disk; the first build
# compiles the host and SQLite for wasm and takes several minutes. Debian bookworm in both
# stages keeps the glibc the same. Base tags are pinned, not digests (accepted: a digest pin needs a
# bump process); bump the tags together. The wasi-sdk download is checked against
# scripts/wasi-sdk-sha256.txt.

FROM rust:1.99.0-bookworm AS build
ARG WASI_SDK_VERSION=25
COPY scripts/wasi-sdk-sha256.txt /tmp/wasi-sdk-sha256.txt
RUN rustup target add wasm32-wasip2 \
 && arch="$(dpkg --print-architecture)" \
 && case "$arch" in amd64) sdk=x86_64-linux ;; arm64) sdk=arm64-linux ;; *) echo "unsupported arch $arch" >&2; exit 1 ;; esac \
 && file="wasi-sdk-${WASI_SDK_VERSION}.0-${sdk}.tar.gz" \
 && curl --proto '=https' --tlsv1.2 -fsSL -o "/tmp/$file" "https://github.com/WebAssembly/wasi-sdk/releases/download/wasi-sdk-${WASI_SDK_VERSION}/$file" \
 && (cd /tmp && grep "  $file\$" wasi-sdk-sha256.txt | sha256sum -c -) \
 && mkdir -p /opt/wasi-sdk \
 && tar -xz -C /opt/wasi-sdk --strip-components=1 -f "/tmp/$file" \
 && rm "/tmp/$file"
ENV WASI_SDK_PATH=/opt/wasi-sdk
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
# the host embeds the wasi-sdk checksums for `spared setup` (crates/spared/src/cli.rs)
COPY scripts/wasi-sdk-sha256.txt ./scripts/wasi-sdk-sha256.txt
# ...and the guest C flags, which scripts/build-examples.sh also reads
COPY scripts/guest-cflags.txt ./scripts/guest-cflags.txt
# The build context has no .git, so the commit `spared --version` reports comes from this
# build argument (docker build --build-arg SPARED_GIT_SHA=$(git rev-parse HEAD)); empty is fine.
ARG SPARED_GIT_SHA=
RUN SPARED_GIT_SHA="$SPARED_GIT_SHA" cargo build --release --locked -p spared
COPY apps/shared-list ./apps/shared-list
COPY apps/demo-spared ./apps/demo-spared
COPY apps/demo-shared ./apps/demo-shared
COPY examples ./examples
COPY scripts/build-examples.sh ./scripts/build-examples.sh
RUN sh scripts/build-examples.sh

FROM debian:bookworm-20261104-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 10001 --user-group --no-create-home spared \
 && mkdir /data /examples \
 && chown spared:spared /data
COPY --from=build /src/target/release/spared /usr/local/bin/spared
# The image holds the binary and example components built with SQLite and wasi-libc.
COPY LICENSE NOTICE THIRD-PARTY-NOTICES /usr/share/doc/spared/
COPY --from=build /src/examples/dist /examples
COPY deploy/entrypoint.sh /usr/local/bin/spared-entrypoint
VOLUME /data
EXPOSE 8080
# The entrypoint execs the server, so it is PID 1 and gets this signal; it shuts down gracefully
# within about 4 s (docker stop waits 10 s by default).
STOPSIGNAL SIGTERM
ENTRYPOINT ["/usr/local/bin/spared-entrypoint"]
