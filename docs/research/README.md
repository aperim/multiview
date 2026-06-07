# Research & Design Briefs

Deep, verification-hardened design records that back the Multiview implementation. Each was produced by a multi-agent research workflow that fanned out, adversarially verified its load-bearing claims, and synthesized an authoritative brief. Canonical naming is in [../architecture](../architecture/); decisions in [../decisions](../decisions/).

- [Core Engine Architecture](core-engine.md) — Core Engine
- [Bulletproof Output, Resilience & A/V](resilience-and-av.md) — Resilience & A/V
- [Efficiency on Commodity Hardware](efficiency.md) — Efficiency (the "why"; the per-stage budget + remediation backlog live in [`../architecture/efficiency-budget.md`](../architecture/efficiency-budget.md))
- [GPU/CPU Monitoring & Multi-GPU Work Placement](gpu-monitoring-and-scheduling.md) — Efficiency
- [GPU Work-Placement Decision Engine](gpu-placement-engine.md) — Efficiency
- [Color Management](color-management.md) — Color
- [Streaming Robustness Runbook](streaming-gotchas.md) — Streaming/Timing
- [Timing Architecture](timing-architecture.md) — Streaming/Timing (pacing + reference-lock + frame-sync + wall-clock + timecode)
- [Input Timing & Frame-Sync](input-timing-and-sync.md) — Streaming/Timing (best-effort PTS normalisation + wall-clock pacer + sample-at-tick)
- [AES67 / ST 2110-30 audio I/O delivery](aes67-delivery.md) — IO/Transport (open-interop-first AES67 send+receive: L16/L24 PCM/RTP, PTP media-clock reference (never a pacer, inv #1) + boundary resampler, SDP+SAP discovery (NMOS optional), bounded fail-honest receive; receive ~80% reuses the existing st2110/v30 depacketizer; drives ADR-0033)
- [Dante audio in/out](dante-audio.md) — Streaming/Timing (AES67/ST 2110-30 open interop vs licence-gated native Dante; drives ADR-T010)
- [NDI + NDI|HX integration](ndi-integration.md) — IO/Transport (own-the-FFI-binding over the NewTek/Vizrt C SDK: NV12-native send, transparent HX decode, Advanced-SDK HX send; + open libndi+SpeedHQ receive path)
- [Preview Subsystem](preview-subsystem.md) — Preview
- [Realtime / Eventing API](realtime-api.md) — Realtime API
- [Management Capability Matrix](management-capability-matrix.md) — Management
- [Web App + API Stack](web-api-stack.md) — Web/API Stack
- [Dev Container Design](devcontainer-design.md) — Dev Container
- [ACME/TLS (DNS-01 only)](acme-tls.md) — Web/API Stack (automatic TLS for the control plane: instant-acme + rustls, pluggable `DnsProvider` trait with Cloudflare first; drives ADR-0029)
- [Multiple active programs](multi-program.md) — Core Engine (N concurrent output programs: multiview / **guarded passthrough** (bulletproof: packet-copy + pre-baked slate-splice on loss, #1-preserving) / transcode under one `ProgramSet`; per-program clocks + decode-once-use-many + robustness ladder + admission control; drives ADR-0030)
- [FFmpeg build & sourcing strategy](ffmpeg-strategy.md) — Licensing/Build (build our OWN pinned FFmpeg, LGPL-clean + GPL variant, reproducible multi-arch; reject jellyfin/PPA; reduce-reliance roadmap; drives ADR-0031)
- [HLS + LL-HLS delivery](hls-delivery.md) — Streaming/Output (tier the serving boundary: static-frontable master/segments/init for any CDN/nginx/traefik vs Multiview's own async blocking-reload origin for the live LL-HLS playlist + byte-range parts; CMAF/fMP4 default; rolling-playlist + DVR + atomic-publish + grace-period pruning foundation; configurable locations/`base_url`; Cache-Control/CORS contract + reference fronting config; HLS-0..14 backlog; drives ADR-0032)
