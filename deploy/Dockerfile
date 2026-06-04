# syntax=docker/dockerfile:1
#
# Multiview runtime image (LGPL-clean default build).
#
# Multi-stage:
#   * builder  — Debian trixie + Rust, libav* -dev headers + VAAPI userspace,
#                builds the `multiview` binary with `--features ffmpeg,linux-vaapi`.
#   * runtime  — debian:trixie-slim with the MATCHING libav* runtime sonames
#                + VAAPI userspace (Intel/AMD), a non-root user, and the binary.
#
# Why trixie: Debian trixie ships FFmpeg 7.1.x, i.e. libavcodec.so.61 etc. — the
# soname family this build links. (bookworm ships FFmpeg 5.1, too old; Ubuntu
# 24.04 apt ships 6.1.) The dev-container Dockerfile relies on the same fact.
#
# LGPL-clean: we build `ffmpeg,linux-vaapi` and DELIBERATELY do NOT enable
# `gpl-codecs` (x264/x265), which would relicense the whole build GPL. A separate
# `-gpl` image tag (built with `--build-arg CARGO_FEATURES=ffmpeg,linux-vaapi,gpl-codecs`)
# is the opt-in GPL variant.
#
# Base-image pinning: tags are floating minor lines (`rust:1-trixie`,
# `debian:trixie-slim`). For a reproducible release pin each by digest, e.g.
#   FROM rust:1-trixie@sha256:<digest>
#   FROM debian:trixie-slim@sha256:<digest>
# (Resolve the current digest with `docker buildx imagetools inspect <ref>` and
# commit it; CI provenance/SBOM record the resolved digest regardless.)

# -----------------------------------------------------------------------------
# Stage 1: builder
# -----------------------------------------------------------------------------
FROM rust:1-trixie AS builder

ENV DEBIAN_FRONTEND=noninteractive

# Build-time feature set. Default is the LGPL-clean `ffmpeg,linux-vaapi`. Pass
# `--build-arg CARGO_FEATURES=ffmpeg,linux-vaapi,gpl-codecs` to produce the GPL
# `-gpl` image variant.
ARG CARGO_FEATURES=ffmpeg,linux-vaapi

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
# Stage 2: runtime
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

# The control/serving HTTP surface (web UI + OpenAPI + /api/v1 + health) is
# intended to bind :8080 once the binary wires the control plane (see the
# dev-container forwardPorts + docs/architecture/conventions.md §6). Documented
# here for operators; the current scaffold does not yet bind a listener.
EXPOSE 8080

# `multiview` is config-driven: `run <CONFIG>` (TOML). Override CMD to pass a
# config path; the default points at the in-image example config volume mount.
ENTRYPOINT ["/usr/local/bin/multiview"]
CMD ["run", "/etc/multiview/multiview.toml"]
