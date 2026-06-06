# ADR-T010 — Dante audio via AES67 / ST 2110-30 (open); native Dante is licence-gated

**Status:** Accepted (2026-06-06)
**Area:** streaming/timing (T) — audio-over-IP transport
**Drivers:** operator requirement for Dante audio in/out; the open-standards-only +
LGPL-clean-default mandate (CLAUDE.md, [conventions §7](../architecture/conventions.md)).
**Research:** [dante-audio](../research/dante-audio.md).
**Related:** [ADR-0008](ADR-0008.md) (NDI runtime-load/licence gate — the template),
[ADR-T003](ADR-T003.md) (unified timing), the ST 2110 + PTP work (IN-1/IN-2),
[ADR-R005](ADR-R005.md) (audio mix/route + capability matrix).

## Context

Dante (Audinate) is the dominant pro-AV audio-over-IP system; operators need Multiview
to send and receive Dante audio. Native Dante is a **closed, proprietary protocol**:
audio is a non-RTP proprietary UDP framing, control/subscription is the closed ConMon
protocol, clocking defaults to PTPv1, and **every native implementation path requires a
licensed Audinate SDK/IP** (DAL / Dante Embedded Platform / Brooklyn/Ultimo HW; DVS/Via
are paid end-user products, not embeddable SDKs) under OEM/NDA + royalties. Audinate
publishes **no** on-wire spec; the only public knowledge is reverse-engineering. This
collides with our open-standards-only + LGPL-clean-default rule.

Crucially, Audinate **documents AES67 / SMPTE ST 2110-30 as the licence-free interop
bridge** to Dante networks, and Multiview **already implements the load-bearing part**:
the ST 2110-30 / AES67 L16/L24 PCM depacketizer + RTP parser (`multiview-input/src/st2110/v30.rs`,
property-tested) and a PTP servo. AES67 is pure RFC/AES/SMPTE (RTP, SDP, SAP, PTPv2).

## Decision

1. **Primary path: implement AES67 / SMPTE ST 2110-30 audio I/O** from the open specs,
   with **zero Audinate IP**, reusing the existing ST 2110-30 depacketizer + PTP servo.
   Dante devices interoperate through their AES67 mode. Interop budget: **L24, 48 kHz,
   1 ms ptime, ≤8 ch/flow, multicast 239.x/16, PTPv2 domain 0, SAP/SDP discovery**. This
   is `AES67-1..5` in the plan.
2. **Native Dante is offered ONLY as an off-by-default, never-vendored,
   runtime-SDK-gated, operator-licence-attested feature** mirroring [ADR-0008](ADR-0008.md)
   exactly (a `multiview-dante-sys` dlopen leaf isolating the unsafe FFI; a `DanteLicense`
   typestate gate; trademark attribution). It is built **only if** a paid Audinate OEM
   relationship is explicitly in scope, and it **never** enters the default build. This
   is `DANTE-1..5`.
3. **Timing:** be a PTPv2 follower on an AES67/ST 2059 grandmaster and use it as the
   **media-clock reference**, never as a pacer — the output tick stays the sole pacer
   (invariant #1; ADR-T003). The AoIP send/recv runs off the engine hot loop with
   bounded drop-oldest buffers (invariant #10).

## Consequences

- We get Dante audio in/out **without licensing Audinate**, on the open standard the
  rest of the IP-video plant already speaks (ST 2110-30 / ST 2059), reusing tested code.
- New open work: an audio `FrameProducer` over `v30`, an AES67-audio SDP parse/generate,
  a SAP announce/listen layer, an AES67/ST 2110-30 **RTP transmit** path, and PTP
  profile/domain config — none requiring proprietary IP.
- Limits inherited from Dante's AES67 mode (per-device enablement, multicast-only,
  non-redundant, ≤8 ch/flow, no Dante-Controller routing → SAP/SDP discovery, older gear
  needs DDM to proxy SDP→SAP) are documented and surfaced honestly, not hidden.
- **Test honesty:** Dante Virtual Soundcard is **not** AES67-capable, so it cannot be the
  interop target. CI proves the AES67 wire contract via a gated loopback; real
  Dante-over-AES67 needs AES67-capable hardware + SAP; native-Dante live tests need the
  licensed Audinate runtime — all gated, none in shared CI.
- Default build stays LGPL-clean and proprietary-free.

## Alternatives rejected

- **Native Dante in the default build** — impossible without licensed Audinate IP;
  reverse-engineered implementations are not shippable for an open product. Rejected.
- **Native Dante as the primary path (licensed)** — would gate the common case behind a
  paid SDK and contaminate the build posture; AES67 already covers Dante interop. Kept
  only as an optional gated feature.
