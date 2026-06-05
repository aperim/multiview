# syntax=docker/dockerfile:1
#
# Multiview runtime image (LGPL-clean default build).
#
# Multi-stage:
#   * web      — Node 22, builds the React SPA to `web/dist` (npm ci + npm run
#                build). MUST run before the rust build: the `web` feature embeds
#                `web/dist` into the binary at compile time (rust_embed).
#   * builder  — Debian trixie + Rust, libav* -dev headers + VAAPI userspace,
#                builds the `multiview` binary with
#                `--features ffmpeg,linux-vaapi,web` (the `web` feature pulls
#                multiview-control/embed-web, embedding the SPA so the web UI +
#                REST/WS API + OpenAPI/Scalar docs serve from the binary on the
#                [control] listener).
#   * runtime  — debian:trixie-slim with the MATCHING libav* runtime sonames
#                + VAAPI userspace (Intel/AMD), a non-root user, and the binary.
#
# Why trixie: Debian trixie ships FFmpeg 7.1.x, i.e. libavcodec.so.61 etc. — the
# soname family this build links. (bookworm ships FFmpeg 5.1, too old; Ubuntu
# 24.04 apt ships 6.1.) The dev-container Dockerfile relies on the same fact.
#
# LGPL-clean: we build `ffmpeg,linux-vaapi,web` and DELIBERATELY do NOT enable
# `gpl-codecs` (x264/x265), which would relicense the whole build GPL. The `web`
# feature only embeds the SPA + serves the API (no GPL deps). A separate `-gpl`
# image tag (built with
# `--build-arg CARGO_FEATURES=ffmpeg,linux-vaapi,web,gpl-codecs`) is the opt-in
# GPL variant.
#
# Base-image pinning: tags are floating minor lines (`rust:1-trixie`,
# `debian:trixie-slim`). For a reproducible release pin each by digest, e.g.
#   FROM rust:1-trixie@sha256:<digest>
#   FROM debian:trixie-slim@sha256:<digest>
# (Resolve the current digest with `docker buildx imagetools inspect <ref>` and
# commit it; CI provenance/SBOM record the resolved digest regardless.)

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
# Stage 2: builder
# -----------------------------------------------------------------------------
FROM rust:1-trixie AS builder

ENV DEBIAN_FRONTEND=noninteractive

# Build-time feature set. Default is the LGPL-clean `ffmpeg,linux-vaapi,web`
# (`web` embeds the SPA + serves the control API; no GPL deps). Pass
# `--build-arg CARGO_FEATURES=ffmpeg,linux-vaapi,web,gpl-codecs` to produce the
# GPL `-gpl` image variant.
ARG CARGO_FEATURES=ffmpeg,linux-vaapi,web

# Native build deps:
#   * pkg-config + clang/libclang  — FFI bindgen + locating system libav*.
#   * libav*-dev                   — the FFmpeg 7.x headers + .so symlinks.
#   * libva-dev                    — VAAPI (Intel/AMD) for the `linux-vaapi` path.
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      pkg-config clang llvm-dev libclang-dev nasm \
      libavcodec-dev libavformat-dev libavutil-dev libavfilter-dev \
      libavdevice-dev libswscale-dev libswresample-dev \
      libva-dev \
 && rm -rf /var/lib/apt/lists/*

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
# Stage 3: runtime
# -----------------------------------------------------------------------------
FROM debian:trixie-slim AS runtime

ENV DEBIAN_FRONTEND=noninteractive

# Matching FFmpeg 7.x runtime sonames + VAAPI userspace + Intel/AMD drivers.
# Soname families are pinned to the trixie FFmpeg 7.1 line:
#   libavcodec61 libavformat61 libavutil59 libavfilter10 libavdevice61
#   libswscale8 libswresample5
# intel-media-va-driver is x86_64-only (no arm64 package); install it only on
# amd64 so multi-arch builds don't break on arm64.
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      ca-certificates \
      libavcodec61 libavformat61 libavutil59 libavfilter10 \
      libavdevice61 libswscale8 libswresample5 \
      libva2 mesa-va-drivers \
 && if [ "$(dpkg --print-architecture)" = "amd64" ]; then \
      apt-get install -y --no-install-recommends intel-media-va-driver; \
    fi \
 && rm -rf /var/lib/apt/lists/*

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
