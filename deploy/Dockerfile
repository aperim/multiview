# syntax=docker/dockerfile:1
#
# Multiview runtime image (LGPL-clean default build).
#
# Multi-stage:
#   * ffmpeg-build — Debian trixie; builds OUR OWN pinned FFmpeg 8.1.1 from a
#                GPG- AND SHA-256-verified source tarball with the exact ADR-0031
#                LGPL-clean allowlist (VAAPI + Intel QSV via libvpl/oneVPL on
#                amd64; NO GPL/nonfree). Installs to /opt/multiview-ffmpeg
#                (libavcodec.so.62). Replaces the Debian-trixie apt libav*.
#   * web      — Node 22, builds the React SPA to `web/dist` (npm ci + npm run
#                build). MUST run before the rust build: the `web` feature embeds
#                `web/dist` into the binary at compile time (rust_embed).
#   * builder  — Debian trixie + Rust, links against OUR FFmpeg 8.1.1 dev headers
#                + .so (via PKG_CONFIG_PATH) + VAAPI userspace, builds the
#                `multiview` binary with `--features ffmpeg,linux-vaapi,web` (the
#                `web` feature pulls multiview-control/embed-web, embedding the
#                SPA so the web UI + REST/WS API + OpenAPI/Scalar docs serve from
#                the binary on the [control] listener).
#   * runtime  — debian:trixie-slim with OUR FFmpeg 8.1.1 runtime sonames
#                (libavcodec.so.62, …) + VAAPI userspace (Intel/AMD), a non-root
#                user, and the binary.
#
# FFmpeg provenance (ADR-0031 FF-0/FF-0b): we build FFmpeg 8.1.1 ourselves from
# the pinned ffmpeg.org tarball, verified by BOTH the FFmpeg release GPG key
# (FCF986EA15E6E293A5644F10B4322F04D67658D8) AND a pinned SHA-256, under our
# `--disable-everything` allowlist — replacing the opaque apt libav* (provenance
# + soname we did not own). The default build is genuinely LGPL-clean: we
# DELIBERATELY do NOT enable `gpl-codecs` (x264/x265) here. A separate `-gpl`
# image tag (built with `--build-arg FF_LICENSE=gpl --build-arg
# CARGO_FEATURES=ffmpeg,linux-vaapi,web,gpl-codecs`) is the opt-in GPL variant.
#
# 8.1.1 → libavcodec.so.62 / libavutil.so.60 / libavformat.so.62, matching
# `ffmpeg-sys-next 8.1.0` (build.rs `ffmpeg_8_1` → libavcodec 62), the version
# the workspace lockfile resolves — so the binary links .so.62.
#
# Base-image pinning: tags are floating minor lines (`rust:1-trixie`,
# `debian:trixie`, `debian:trixie-slim`). For a reproducible release pin each by
# digest, e.g.
#   FROM debian:trixie@sha256:<digest> AS ffmpeg-build
#   FROM rust:1-trixie@sha256:<digest> AS builder
#   FROM debian:trixie-slim@sha256:<digest> AS runtime
# (Resolve the current digest with `docker buildx imagetools inspect <ref>` and
# commit it; CI provenance/SBOM record the resolved digest regardless.)

# -----------------------------------------------------------------------------
# Stage 0: ffmpeg-build — our own pinned, GPG+SHA-256-verified FFmpeg 8.1.1
# (VAAPI/QSV profile). deploy/ffmpeg-build.sh owns the audited configure flag
# list (LGPL-clean by default). Output: /opt/multiview-ffmpeg.
# -----------------------------------------------------------------------------
FROM debian:trixie AS ffmpeg-build

ENV DEBIAN_FRONTEND=noninteractive

# FFmpeg source pin. FFMPEG_SHA256 is the SHA-256 of ffmpeg-8.1.1.tar.xz from
# ffmpeg.org/releases, verified end-to-end (the GPG good-signature + this digest
# both checked against the real tarball at author time). The build ALSO verifies
# the detached GPG .asc against FFMPEG_GPG_KEY; both must pass.
ARG FFMPEG_VERSION=8.1.1
ARG FFMPEG_SHA256=b6863adde98898f42602017462871b5f6333e65aec803fdd7a6308639c52edf3
ARG FFMPEG_GPG_KEY=FCF986EA15E6E293A5644F10B4322F04D67658D8
# Licensing profile: lgpl (default) | gpl. gpl adds x264/x265 and tags `-gpl`.
ARG FF_LICENSE=lgpl
# Buildx supplies TARGETARCH (amd64|arm64); QSV/libvpl is gated amd64-only.
ARG TARGETARCH

# Build toolchain + the LGPL-clean external libs our allowlist enables
# (libopus, libsrt, gnutls/nettle, libdrm) + VAAPI (libva) and Intel QSV
# (libvpl) userspace. libvpl-dev is amd64-only; install it only there so the
# arm64 leg builds (the script also gates --enable-libvpl on TARGETARCH=amd64).
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      ca-certificates curl gnupg \
      build-essential clang llvm pkg-config nasm yasm make \
      libopus-dev libsrt-openssl-dev libgnutls28-dev libdrm-dev libva-dev \
 && if [ "$(dpkg --print-architecture)" = "amd64" ]; then \
      apt-get install -y --no-install-recommends libvpl-dev; \
    fi \
 && rm -rf /var/lib/apt/lists/*

COPY deploy/ffmpeg-build.sh /usr/local/bin/ffmpeg-build.sh
RUN chmod 0755 /usr/local/bin/ffmpeg-build.sh \
 && FF_HWACCEL=vaapi \
    FFMPEG_VERSION="${FFMPEG_VERSION}" \
    FFMPEG_SHA256="${FFMPEG_SHA256}" \
    FFMPEG_GPG_KEY="${FFMPEG_GPG_KEY}" \
    FF_LICENSE="${FF_LICENSE}" \
    TARGETARCH="${TARGETARCH:-amd64}" \
    FF_PREFIX=/opt/multiview-ffmpeg \
    /usr/local/bin/ffmpeg-build.sh

# -----------------------------------------------------------------------------
# Stage 1: web — build the React SPA to web/dist (Node 22, matches the
# dev-container). The `web` cargo feature embeds web/dist into the binary at
# compile time, so this stage MUST complete before the rust build. `npm ci`
# installs from the committed web/package-lock.json; `npm run build` runs
# `lingui compile && tsc --noEmit && vite build` and writes to web/dist.
# -----------------------------------------------------------------------------
FROM node:22-trixie-slim AS web

WORKDIR /web

# Install from the lockfile first (cache-friendly), then build the SPA.
COPY web/package.json web/package-lock.json ./
RUN --mount=type=cache,target=/root/.npm \
    npm ci
COPY web/ ./
RUN npm run build

# -----------------------------------------------------------------------------
# Stage 2: builder — links against OUR FFmpeg 8.1.1 (libavcodec.so.62)
# -----------------------------------------------------------------------------
FROM rust:1-trixie AS builder

ENV DEBIAN_FRONTEND=noninteractive

# Build-time feature set. Default is the LGPL-clean `ffmpeg,linux-vaapi,web`
# (`web` embeds the SPA + serves the control API; no GPL deps). Pass
# `--build-arg CARGO_FEATURES=ffmpeg,linux-vaapi,web,gpl-codecs` (with
# `--build-arg FF_LICENSE=gpl`) to produce the GPL `-gpl` image variant.
ARG CARGO_FEATURES=ffmpeg,linux-vaapi,web

# Native build deps:
#   * pkg-config + clang/libclang  — FFI bindgen + locating our libav* via .pc.
#   * libva-dev                    — VAAPI (Intel/AMD) for the `linux-vaapi` path.
# The libav* themselves come from our ffmpeg-build stage below — NOT from apt
# (no libav*-dev here; that opaque apt source is exactly what we replaced).
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      pkg-config clang llvm-dev libclang-dev nasm \
      libva-dev \
 && rm -rf /var/lib/apt/lists/*

# Our pinned FFmpeg 8.1.1 (headers + .so + .pc). Point pkg-config + the loader at
# it so `ffmpeg-sys-next` discovers FFmpeg 8.1.1 (detects ffmpeg_8_1 → libavcodec
# 62) and the build links .so.62.
COPY --from=ffmpeg-build /opt/multiview-ffmpeg /opt/multiview-ffmpeg
ENV PKG_CONFIG_PATH=/opt/multiview-ffmpeg/lib/pkgconfig \
    LD_LIBRARY_PATH=/opt/multiview-ffmpeg/lib

WORKDIR /build

# Copy the whole workspace. (A leaner copy would stage Cargo.{toml,lock} first
# for dependency caching, but the BuildKit cargo cache mounts below give most of
# that benefit without fragile manifest juggling across 16 crates.)
COPY . .

# Bring in the SPA built by the `web` stage. The `web` feature's rust_embed
# folder is `web/dist` (resolved relative to multiview-control at compile time),
# so it must exist before `cargo build`. This COPY overwrites any web/dist that
# rode in with `COPY . .`, guaranteeing a fresh, in-image build.
COPY --from=web /web/dist ./web/dist

# Build the release binary with BuildKit cache mounts for the cargo registry,
# git db, and the target dir so incremental rebuilds reuse compiled deps.
# `--locked` enforces the committed Cargo.lock (reproducible).
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/build/target \
    cargo build --release --locked -p multiview-cli --features "${CARGO_FEATURES}" \
 && cp /build/target/release/multiview /usr/local/bin/multiview \
 && /usr/local/bin/multiview --version

# -----------------------------------------------------------------------------
# Stage 3: runtime — debian:trixie-slim + OUR FFmpeg 8.1.1 sonames + VAAPI
# -----------------------------------------------------------------------------
FROM debian:trixie-slim AS runtime

ENV DEBIAN_FRONTEND=noninteractive

# Runtime deps for OUR FFmpeg 8.1.1 shared libs + VAAPI userspace + Intel/AMD
# drivers. We build FFmpeg --enable-shared against libopus/libsrt/gnutls/libdrm
# (+ libvpl on amd64), so the runtime needs those shared libs present (the loader
# resolves them when the binary pulls in libavcodec.so.62 et al). libva2 +
# mesa-va-drivers provide VAAPI; intel-media-va-driver is x86_64-only (no arm64
# package), installed only on amd64; libvpl2 (oneVPL runtime) likewise amd64-only.
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      ca-certificates \
      libopus0 libsrt1.5-openssl libgnutls30t64 libdrm2 \
      libva2 mesa-va-drivers \
 && if [ "$(dpkg --print-architecture)" = "amd64" ]; then \
      apt-get install -y --no-install-recommends intel-media-va-driver libvpl2; \
    fi \
 && rm -rf /var/lib/apt/lists/*

# Our FFmpeg 8.1.1 shared libraries (libavcodec.so.62, libavutil.so.60,
# libavformat.so.62, libavfilter.so.11, libswscale.so.9, libswresample.so.6, and
# their symlinks). Install into /usr/local/lib and ldconfig so the binary finds
# them with NO LD_LIBRARY_PATH override. Assert the 8.1.1 soname (.so.62, NOT
# .61) so a version/layout drift fails the BUILD, not the deploy.
COPY --from=ffmpeg-build /opt/multiview-ffmpeg/lib /usr/local/lib
RUN ldconfig \
 && test -e /usr/local/lib/libavcodec.so.62 \
 && test -e /usr/local/lib/libavutil.so.60

# Non-root runtime user (fixed uid/gid 10001). The render group is added at
# `docker run` time (e.g. compose `group_add`) for VAAPI /dev/dri access.
RUN groupadd --gid 10001 multiview \
 && useradd --uid 10001 --gid 10001 --create-home --shell /usr/sbin/nologin multiview \
 && mkdir -p /etc/multiview /var/lib/multiview/hls \
 && chown -R 10001:10001 /etc/multiview /var/lib/multiview

COPY --from=builder /usr/local/bin/multiview /usr/local/bin/multiview

USER 10001:10001
WORKDIR /var/lib/multiview

# The control/serving HTTP surface: web UI at `/`, REST under `/api/v1`,
# WebSocket at `/api/v1/ws`, SSE at `/api/v1/events`, OpenAPI at
# `/api/v1/openapi.json`, and Scalar docs at `/docs`. `multiview run` binds this
# when the config has a [control] section (the example config sets
# `listen = "0.0.0.0:8080"`). The SPA serves because this image is built with the
# `web` feature (see the builder stage); set MULTIVIEW_CONTROL_TOKEN to fix the
# admin bearer secret (`admin.<secret>`), else one is generated + logged once.
EXPOSE 8080

# `multiview` is config-driven: `run <CONFIG>` (TOML). Override CMD to pass a
# config path; the default points at the in-image example config volume mount.
ENTRYPOINT ["/usr/local/bin/multiview"]
CMD ["run", "/etc/multiview/multiview.toml"]
