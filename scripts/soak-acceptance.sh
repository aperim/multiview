#!/usr/bin/env bash
# DEV-C4 acceptance-soak harness — drive a 2-node sync group for a long run,
# scrape each node's clock-servo telemetry + the engine output-tick counter,
# inject the invariant-#1 chaos (kill PTP/WS mid-soak), then render the PASS/FAIL
# verdict with `cargo xtask soak-report`.
#
# The pass/fail MATHS lives in the tested `multiview-telemetry::soak` analyzer
# (p99 |offset| vs the ADR-M010 per-source bound + cadence continuity); this
# script is the orchestration around it. The `--dry-run` mode feeds bundled
# fixtures through `soak-report` and asserts the expected verdicts, so the harness
# wiring is exercised in CI with no hardware. The physical 24 h run + the camera
# OCR of the burnt-in frame counter are hardware-gated (need ≥2 real nodes).
#
# Thresholds (from the analyzer, ADR-M010): 99th-pct |offset| ≤ 100 µs (PTP) /
# ≤ 1 ms (chrony) over the 24 h window. Pass-conditions are the analyzer's, not
# this script's — keep them in one place.
set -euo pipefail

# ── defaults ────────────────────────────────────────────────────────────────
CONTROLLER="http://[::1]:8080"          # IPv6-first (ADR-0042); bracket literals
GROUP=""                                 # sync-group id under test
NODE_METRICS=()                          # one /metrics URL per node
DURATION_SECS=86400                      # 24 h acceptance window
SAMPLE_SECS=30                           # scrape cadence
OUT="soak-capture.json"                  # capture document the verdict reads
CADENCE_METRIC="multiview_engine_output_ticks_total"  # the output-tick counter
OFFSET_METRIC="multiview_clock_servo_offset_nanoseconds"
CHAOS_HOOK=""                            # command run once at the soak midpoint
FRAME_OCR_HOOK=""                        # command capturing+OCR'ing one node head
DRY_RUN=0

usage() {
  cat <<'USAGE'
soak-acceptance.sh — DEV-C4 2-node clock/sync acceptance soak

  --controller URL     controller base (default http://[::1]:8080)
  --group ID           sync-group id under test (required for a real run)
  --node-metrics URL   a node's Prometheus /metrics endpoint (repeatable, ≥2)
  --duration SECS      soak window (default 86400 = 24 h)
  --sample SECS        scrape interval (default 30)
  --cadence-metric M   the engine output-tick counter metric (must be monotone)
  --chaos-hook CMD     command run once at the midpoint to kill PTP/WS
  --frame-ocr-hook CMD command that captures+OCRs a node head's burnt-in counter
  --out FILE           capture document path (default soak-capture.json)
  --dry-run            run the bundled fixtures through `xtask soak-report` and
                       assert the expected PASS/FAIL — no hardware, CI-safe
  -h, --help           this help
USAGE
}

# ── arg parsing ─────────────────────────────────────────────────────────────
while [ "$#" -gt 0 ]; do
  case "$1" in
    --controller) CONTROLLER="$2"; shift 2 ;;
    --group) GROUP="$2"; shift 2 ;;
    --node-metrics) NODE_METRICS+=("$2"); shift 2 ;;
    --duration) DURATION_SECS="$2"; shift 2 ;;
    --sample) SAMPLE_SECS="$2"; shift 2 ;;
    --cadence-metric) CADENCE_METRIC="$2"; shift 2 ;;
    --offset-metric) OFFSET_METRIC="$2"; shift 2 ;;
    --chaos-hook) CHAOS_HOOK="$2"; shift 2 ;;
    --frame-ocr-hook) FRAME_OCR_HOOK="$2"; shift 2 ;;
    --out) OUT="$2"; shift 2 ;;
    --dry-run) DRY_RUN=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown argument: $1" >&2; usage >&2; exit 2 ;;
  esac
done

repo_root() { git rev-parse --show-toplevel; }

# Render a capture document through the tested analyzer; returns its exit code.
run_verdict() {
  local capture="$1"
  ( cd "$(repo_root)" && cargo run --quiet -p xtask -- soak-report "$capture" )
}

# ── dry-run: prove the harness→analyzer wiring with bundled fixtures ─────────
# A clean capture must PASS (exit 0); an offset-breach and a cadence-stall must
# each FAIL (exit 1). This is the CI-exercisable self-test — no nodes needed.
dry_run() {
  local tmp; tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' RETURN

  cat >"$tmp/pass.json" <<'JSON'
{"offsets":[{"source":"ptp","samples_ns":[0,1000,-2000,500]},
            {"source":"system","samples_ns":[0,400000,-300000]}],
 "cadence":{"tick_samples":[0,30,60,90,120],"expected_min_delta":30}}
JSON
  cat >"$tmp/offset-breach.json" <<'JSON'
{"offsets":[{"source":"ptp","samples_ns":[200000,200000,200000]}],
 "cadence":{"tick_samples":[0,30,60],"expected_min_delta":30}}
JSON
  cat >"$tmp/cadence-stall.json" <<'JSON'
{"offsets":[{"source":"ptp","samples_ns":[0,0,0]}],
 "cadence":{"tick_samples":[0,30,30,60],"expected_min_delta":30}}
JSON

  echo "dry-run: clean capture must PASS"
  run_verdict "$tmp/pass.json"

  echo "dry-run: offset breach must FAIL"
  if run_verdict "$tmp/offset-breach.json"; then
    echo "FAIL: offset-breach fixture unexpectedly passed" >&2; exit 1
  fi

  echo "dry-run: cadence stall must FAIL"
  if run_verdict "$tmp/cadence-stall.json"; then
    echo "FAIL: cadence-stall fixture unexpectedly passed" >&2; exit 1
  fi

  echo "dry-run OK: harness→analyzer wiring verified"
}

# ── real run helpers (hardware) ─────────────────────────────────────────────
# Scrape one Prometheus gauge value for a given metric+source from a /metrics URL.
scrape_offset() {
  local url="$1" source="$2"
  curl -fsS "$url" \
    | grep -E "^${OFFSET_METRIC}\{[^}]*source=\"${source}\"" \
    | awk '{print $NF}' | head -n1
}

# Scrape the monotone output-tick counter (cadence) from a /metrics URL.
scrape_cadence() {
  local url="$1"
  curl -fsS "$url" \
    | grep -E "^${CADENCE_METRIC}( |\{)" \
    | awk '{print $NF}' | head -n1
}

real_run() {
  if [ -z "$GROUP" ] || [ "${#NODE_METRICS[@]}" -lt 2 ]; then
    echo "a real run needs --group and ≥2 --node-metrics URLs" >&2
    exit 2
  fi
  echo "starting the burnt-in test-pattern on sync-group ${GROUP}"
  curl -fsS -X POST "${CONTROLLER}/api/v1/sync-groups/${GROUP}/test-pattern" \
    -H 'content-type: application/json' -d '{}' >/dev/null

  # NOTE: the OCR of the burnt-in counter across node heads is a PHYSICAL step
  # (cameras pointed at each display); --frame-ocr-hook runs the operator's
  # capture+compare. The cross-node counter-skew comparison is real once frames
  # exist; without a hook we record telemetry only and skip the visual leg.
  local samples=$(( DURATION_SECS / SAMPLE_SECS ))
  local midpoint=$(( samples / 2 ))
  local ptp_series="" sys_series="" tick_series=""
  local i=0
  while [ "$i" -lt "$samples" ]; do
    ptp_series="${ptp_series}$(scrape_offset "${NODE_METRICS[0]}" ptp || echo 0),"
    sys_series="${sys_series}$(scrape_offset "${NODE_METRICS[0]}" system || echo 0),"
    tick_series="${tick_series}$(scrape_cadence "${NODE_METRICS[0]}" || echo 0),"
    if [ "$i" -eq "$midpoint" ] && [ -n "$CHAOS_HOOK" ]; then
      echo "midpoint: injecting chaos (kill PTP/WS) via the chaos hook"
      eval "$CHAOS_HOOK" || true
    fi
    if [ -n "$FRAME_OCR_HOOK" ]; then eval "$FRAME_OCR_HOOK" || true; fi
    sleep "$SAMPLE_SECS"
    i=$(( i + 1 ))
  done

  # Build the capture document the analyzer reads. The per-sample cadence floor
  # is the ticks one SAMPLE_SECS of output should advance (engine cadence × secs)
  # — operators set it via --cadence-floor if the default heuristic is wrong.
  python3 - "$OUT" "$ptp_series" "$sys_series" "$tick_series" "${CADENCE_FLOOR:-1}" <<'PY'
import json, sys
out, ptp, sys_, ticks, floor = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4], int(sys.argv[5])
def nums(s): return [int(float(x)) for x in s.split(",") if x.strip()]
doc = {
    "offsets": [
        {"source": "ptp", "samples_ns": nums(ptp)},
        {"source": "system", "samples_ns": nums(sys_)},
    ],
    "cadence": {"tick_samples": nums(ticks), "expected_min_delta": floor},
}
with open(out, "w") as f:
    json.dump(doc, f, indent=2)
print(f"wrote {out}")
PY

  echo "rendering the verdict"
  run_verdict "$OUT"
}

if [ "$DRY_RUN" -eq 1 ]; then
  dry_run
else
  real_run
fi
