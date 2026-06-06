# Dante audio in/out — research brief

**Status:** research complete (2026-06-06). Drives [ADR-T010](../decisions/ADR-T010.md).
**Question:** how should Multiview support **Dante** (Audinate) audio ingest and egress?

> **Honesty split.** Everything about **AES67 / SMPTE ST 2110-30**, clocking, ports,
> discovery, and the Dante↔AES67 relationship below is from Audinate's own
> documentation + the AES67 standard and is treated as **confirmed**. The
> **native-Dante on-wire framing** is only known from independent
> **reverse-engineering** (RIT security blog, Wikipedia) — it is **inferred /
> unofficial**, Audinate publishes no on-wire spec. Audinate's **SDK licensing
> terms were not independently pinned** and need legal verification before any
> native-Dante commitment.

---

## 0. Headline

**Implement Dante interop via AES67 / SMPTE ST 2110-30 — the open standard — not
native Dante.** AES67 is Audinate's *own documented, licence-free* bridge to Dante
networks; it is pure RFC/AES/SMPTE (RTP + SDP + SAP + PTPv2), implementable with
**zero Audinate IP**, and Multiview **already owns the hard part** (the ST 2110-30 /
AES67 L16/L24 PCM depacketizer + RTP parser + PTP servo). Native Dante is a closed,
patent/trademark-protected, SDK-licensed protocol that collides head-on with
Multiview's open-standards-only + LGPL-clean-default rule, so it is offered **only**
as an off-by-default, never-vendored, operator-licence-attested feature mirroring NDI
([ADR-0008](../decisions/ADR-0008.md)) — and only if a paid Audinate OEM relationship
is ever in scope.

---

## 1. Native Dante is closed and licence-encumbered

- **Not RTP.** Native Dante audio is a **proprietary UDP framing** — raw interleaved
  PCM behind a small proprietary header (reverse-engineered: a 2ch/24-bit/48 kHz
  datagram measured 9-byte header + 360-byte PCM on UDP 14341; header = channel count
  + 8-byte frame-start timestamp). Full RTP appears **only** in Dante's AES67/ST 2110-30
  mode. *(INFERRED — RIT blog / Wikipedia; verify against a real capture before relying
  on it.)*
- **Ports:** native multicast audio UDP 4321 (239.255/16); unicast audio UDP
  14336–14591; PTP 319/320; mDNS/DNS-SD 5353; ConMon control 8700–8708; AES67 RTP 5004.
  *(Confirmed — Audinate/Shure.)*
- **Flows:** unicast ≤4 ch; multicast device-dependent (16/32/64 ch, smaller packets =
  lower latency; pre-v4.2 firmware rejects multicast >8 ch). Flow counts are
  **hardware-limited** (Brooklyn II 32 Tx/32 Rx; Ultimo 2/2). *(Confirmed.)*
- **Control = "subscription"** over the **proprietary ConMon protocol** (Dante
  Controller / DDM / licensed API). The closed control + subscription protocol is the
  hard part for any independent implementation. *(Confirmed.)*
- **Clocking:** native Dante syncs to one PTP **Leader** on **PTPv1 (IEEE 1588-2002)**
  by default; PTPv2 only when AES67/ST 2110-30 is enabled or for DDM boundary clocks.
  *(Confirmed.)*
- **Licensing reality:** every native path (Dante SDK / DAL / Dante Embedded Platform /
  Brooklyn/Ultimo HW / DVS / Via) is **proprietary Audinate IP under OEM/NDA + per-device
  or per-channel royalties**. Dante Virtual Soundcard is a **paid end-user product
  ($49.99 / DVS Pro subscription), not an embeddable SDK**. There is **no open or
  royalty-free native-Dante path**, and reverse-engineered implementations are not
  shippable for an open product. *(Confirmed at the level of "paid + closed"; exact SDK
  terms NOT pinned — legal must verify.)*

→ **Native Dante can only ever be an off-by-default, never-vendored, runtime-SDK-gated,
operator-licence-attested feature** (the NDI posture). It must never contaminate the
LGPL-clean default build.

## 2. AES67 / ST 2110-30 is the open interop path Audinate itself documents

- Dante's **AES67 mode** transmits/receives AES67 multicast RTP to/from non-Dante
  devices; **ST 2110-30 mode** (firmware v4.2+) adds SAP/SDP advertisement + configurable
  PTPv2 domains. This is the *only* license-free way to exchange audio with Dante. *(Confirmed.)*
- **Dante-AES67 interop budget (the format clamp Multiview must hit):** **L24, 48 kHz**
  (96 kHz on supporting gear), **1 ms packet time** (48 samples/ch/pkt), **≤8 channels per
  AES67 flow**, **multicast in 239.x/16**, **PTPv2 fixed domain 0**, discovery via **SAP/SDP**.
  (ST 2110-30 mode unlocks 125 µs profiles + up to 64 ch.) *(Confirmed.)*
- **Limitations to design around:** Dante↔Dante always uses native transport (AES67 is
  non-Dante-only); per-device enablement; multicast-only/non-redundant; Dante Controller
  does **not** route AES67 flows — discovery/subscription is via **SAP/SDP** (older Dante
  needs DDM to proxy SDP→SAP). *(Confirmed.)*

## 3. What Multiview already has, and the exact gap

**Already implemented as pure, tested, open-standard Rust** (no Audinate IP):
- The RTP fixed-header parser + the **ST 2110-30 / AES67 L16/L24 → i32 PCM depacketizer**
  (`crates/multiview-input/src/st2110/v30.rs:111–168`, property-tested in
  `crates/multiview-input/tests/st2110_depacketize.rs`). **The wire-payload path already
  works** — it only lacks the *signalling* that fills in the channel/depth format.
- The **PTP servo** (`multiview-engine`, `ptp` feature). The gap is **configuration** of
  the PTP profile/domain, not the servo.
- A real **SDP media-section parser** in `crates/multiview-input/src/webrtc/sdp.rs:386`
  (`parse_rtpmap`, `RtpMap`) — reusable *patterns*, but WebRTC-shaped
  (`UDP/TLS/RTP/SAVPF` + ICE/DTLS), whereas AES67 is plain `RTP/AVP` over UDP multicast.

**Missing for actual AES67/Dante in/out:**
1. **Audio `FrameProducer`** wiring the `v30` depacketizer into the ingest pump (today
   `St2110Producer` is video-shaped).
2. **SAP** session-announce/listen (RFC 2974, UDP 9875, group 224.2.127.254) — **zero**
   `sap`/`9875`/`announce` hits in `crates/` today.
3. An **AES67-audio SDP** parse/generate (RFC 4566/8866: `m=audio <port> RTP/AVP <pt>`,
   `a=rtpmap:<pt> L24/48000/<ch>`) — the existing parse is video-centric.
4. An **AES67/ST 2110-30 RTP transmit (sender)** path — there is **none** today.
5. A config `SourceKind` (AES67/Dante-in) + `Output` (AES67/Dante-out).
6. PTP profile/domain config surfacing.

Note: AES67/Dante define **no** ST 2022-7 redundancy, so the existing dual-path receiver
is unused for plain Dante AES67 — single-path `RtpReceiver` is the substrate.

## 4. Architecture fit (invariants #1/#3/#10)

- **Ingest** = an AES67/Dante source feeding a per-source `AudioStore` (sampled, never
  pacing — **#1**); PTP only disciplines the *reference* clock used to map the media
  timestamp, it never gates frame emission (**#3**: output cadence stays the tick
  counter; "reference, not pacer").
- **Egress** = a program-bus sink (the `AUD-3`/`AUD-4` bus) that stamps the AES67 RTP
  media clock from the shared PTP reference (RTP ts = samples since the PTP epoch) so
  receiver jitter-buffers align — while the program's own output tick stays the pacer.
- **Isolation (#10):** the AoIP send/recv runs off the engine hot loop on its own
  thread with bounded drop-oldest buffers; a stalled NIC/peer can never back-pressure
  the engine. (Same posture as preview/control.)
- Audio routing maps to the `multiview-audio` capability matrix (NDI/Dante = channel-map,
  per ADR-R005).

## 5. Test strategy without proprietary hardware

- **AES67 RTP loopback on one host** (sender + receiver on `lo` / a local multicast
  group) = the core wire-contract proof, gated `#[ignore]`/feature. Proves the AES67
  contract, **not** Dante-branded behaviour.
- **CRITICAL caveat — Dante Virtual Soundcard is NOT AES67-capable** (DVS 4.3.x is
  explicitly not AES67). **DVS cannot be the AES67 interop target.** Real Dante-over-AES67
  interop requires **AES67-capable Dante hardware + SAP**.
- On the test box (Linux, sudo): a real **non-loopback** AES67 counterpart is achievable
  with **`linuxptp` (`ptp4l`/`phc2sys`)** as a software grandmaster (media-profile sync
  intervals + DSCP/EF) + **FFmpeg** sending/receiving AES67 RTP via SDP. (`aes67-linux-daemon`
  exists but is GPL + needs the RAVENNA ALSA kernel module — avoid in the default path.)
- **Native-Dante** live tests would need the **licensed Audinate runtime** installed on
  the runner — exactly the gated posture of the NDI live binding.

## 6. Recommendation + slice plan

**Build AES67/ST 2110-30 audio I/O (open) → it *is* Dante interop.** Treat native Dante
as a separate, licence-gated, optional feature only if a paid OEM relationship lands.

- **AES67-1** `L` — Audio `FrameProducer` over the `v30` depacketizer → `AudioStore`
  (Dante/AES67 **IN**); single-path `RtpReceiver`. *deps: IN-2, AUD-2.*
- **AES67-2 (SDP)** `M` — AES67-audio SDP parse + generate (RFC 4566/8866); fill
  channels/depth/ptime. *deps: AES67-1.*
- **AES67-3 (SAP)** `M` — SAP announce + listen (RFC 2974, UDP 9875) for discovery in
  both directions; DDM-proxy note for legacy Dante. *deps: AES67-2.*
- **AES67-4** `L` — AES67/ST 2110-30 RTP **transmit** from the program bus (L24/48k/1ms,
  239.x/16); PTP-referenced media clock; bounded/off-hot-path (**#1/#10**). *deps: AES67-2,
  AUD-3/4.*
- **AES67-5** `M` — PTP profile/domain config surfacing (AES67 media profile / ST 2059,
  domain 0) on the existing servo; config `SourceKind`/`Output`; gated loopback interop
  test + chaos-gate #10. *deps: AES67-3, AES67-4, `ptp`._
- **DANTE-1..5** `XL/M/L` (NATIVE, deferred/optional, mirror NDI) — `multiview-dante-sys`
  runtime-load leaf (dlopen the operator-installed Audinate DAL/API dylib; C ABI from
  documented headers; sole `allow(unsafe_code)`) → `DanteLicense` typestate gate +
  `[system.dante] accept_license` + attribution → Dante source → `AudioStore` → Dante
  sink from the bus → UI attribution + capability report + SDK-equipped CI lane. **Only
  with a paid Audinate licence.**

---

## Citations (selected; full set in the research transcript)

- AES67 standard summary — <https://en.wikipedia.org/wiki/AES67>
- Audinate AES67 config / RTP config / creating AES67-ST2110-30 flows —
  <https://dev.audinate.com/GA/dante-controller/userguide/webhelp/content/aes67_config.htm>
- Audinate clock synchronization (PTPv1/v2, leader election) —
  <https://dev.audinate.com/GA/dante-controller/userguide/webhelp/content/clock_synchronization.htm>
- Audinate latency model —
  <https://dev.audinate.com/GA/dante-controller/userguide/webhelp/content/latency.htm>
- DDM AES67 vs SMPTE domains —
  <https://dev.audinate.com/GA/ddm/userguide/1.1/webhelp/content/appendix/aes67_and_smpte_domains.htm>
- Network ports — <https://www.audinate.com/learning/faqs/which-network-ports-does-dante-use>
- DVS not AES67-capable — <https://www.mdw.ac.at/aesr-lab/docs/A-System/Networked-audio/Dante-AES67-interoperability/>
- Native-transport reverse-engineering (INFERRED) —
  <https://ritcsec.wordpress.com/2020/04/28/reverse-engineering-and-vulnerability-assessment-of-the-dante-protocol/>
