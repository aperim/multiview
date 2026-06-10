// Unit tests for the pure form-state <-> config-body mapping of the Sources /
// Outputs / Overlays management forms. The body shapes asserted here mirror the
// Rust config schema EXACTLY (crates/multiview-config/src/schema.rs: `Source`,
// `Output`, `Overlay`; audio.rs `OutputAudio`; placement.rs `DevicePin`) — the
// control plane is gaining 422 typed validation of these shapes (ADR-W015), so
// a wrong body is a rejected body. No DOM, no React.
import { describe, expect, it } from 'vitest';

import {
  emptyOutputForm,
  emptyOverlayForm,
  emptySourceForm,
  isValidUrl,
  OUTPUT_RUNNABLE,
  outputFormFromRecord,
  outputFormToBody,
  overlayFormFromRecord,
  overlayFormToBody,
  sourceFormFromRecord,
  sourceFormToBody,
  validateOutputForm,
  validateOverlayForm,
  validateSourceForm,
  withOverlayKind,
  withSourceKind,
} from './forms';
import type { OutputFormState, OverlayFormState, SourceFormState } from './forms';

function sourceForm(over: Partial<SourceFormState> = {}): SourceFormState {
  return { ...emptySourceForm(), id: 'cam1', name: 'Cam 1', ...over };
}

function outputForm(over: Partial<OutputFormState> = {}): OutputFormState {
  return { ...emptyOutputForm(), id: 'out1', name: 'Out 1', ...over };
}

function overlayForm(over: Partial<OverlayFormState> = {}): OverlayFormState {
  return { ...emptyOverlayForm(), id: 'ov1', name: 'Overlay 1', ...over };
}

describe('isValidUrl', () => {
  it('accepts the kind scheme and rejects others', () => {
    expect(isValidUrl('rtsp://cam.example/stream', ['rtsp'])).toBe(true);
    expect(isValidUrl('http://cam.example/stream', ['rtsp'])).toBe(false);
    expect(isValidUrl('srt://relay.example:7001', ['srt'])).toBe(true);
    expect(isValidUrl('rtmp://ingest.example/app/key', ['rtmp', 'rtmps'])).toBe(true);
    expect(isValidUrl('https://example.com/x.m3u8', ['http', 'https'])).toBe(true);
  });

  it('accepts bracketed IPv6 literal hosts', () => {
    expect(isValidUrl('rtsp://[2001:db8::1]:8554/cam', ['rtsp'])).toBe(true);
    expect(isValidUrl('srt://[2001:db8::2]:7001', ['srt'])).toBe(true);
    expect(isValidUrl('https://[2001:db8::3]/playlist.m3u8', ['http', 'https'])).toBe(true);
  });

  it('rejects an unbracketed IPv6 literal (the port would be ambiguous)', () => {
    expect(isValidUrl('rtsp://2001:db8::1/cam', ['rtsp'])).toBe(false);
  });

  it('rejects garbage and scheme-less strings', () => {
    expect(isValidUrl('not a url', ['rtsp'])).toBe(false);
    expect(isValidUrl('', ['rtsp'])).toBe(false);
    expect(isValidUrl('cam.example/stream', ['rtsp'])).toBe(false);
  });
});

describe('sourceFormToBody', () => {
  it('builds an rtsp body with id + display_name + url', () => {
    const body = sourceFormToBody(
      sourceForm({ kind: 'rtsp', url: 'rtsp://cam.example/live' }),
    );
    expect(body).toEqual({
      id: 'cam1',
      display_name: 'Cam 1',
      kind: 'rtsp',
      url: 'rtsp://cam.example/live',
    });
  });

  it('carries RtspOptions under the rtsp key when a transport is chosen', () => {
    const body = sourceFormToBody(
      sourceForm({ kind: 'rtsp', url: 'rtsp://cam.example/live', rtspTransport: 'tcp' }),
    );
    expect(body.rtsp).toEqual({ transport: 'tcp' });
  });

  it('omits the rtsp options block when transport is the default', () => {
    const body = sourceFormToBody(
      sourceForm({ kind: 'rtsp', url: 'rtsp://cam.example/live' }),
    );
    expect(body).not.toHaveProperty('rtsp');
  });

  it('builds each url-locator kind with its tag', () => {
    for (const kind of ['hls', 'youtube', 'ts', 'srt', 'rtmp'] as const) {
      const body = sourceFormToBody(sourceForm({ kind, url: 'srt://relay.example:7001' }));
      expect(body.kind).toBe(kind);
      expect(body.url).toBe('srt://relay.example:7001');
    }
  });

  it('builds ndi by source name and file by path', () => {
    expect(sourceFormToBody(sourceForm({ kind: 'ndi', ndiName: 'STUDIO (CAM 1)' }))).toMatchObject({
      kind: 'ndi',
      name: 'STUDIO (CAM 1)',
    });
    expect(sourceFormToBody(sourceForm({ kind: 'file', path: '/media/clip.mp4' }))).toMatchObject({
      kind: 'file',
      path: '/media/clip.mp4',
    });
  });

  it('builds the synthetic kinds (bars carries only its tag)', () => {
    const bars = sourceFormToBody(sourceForm({ kind: 'bars' }));
    expect(bars).toEqual({ id: 'cam1', display_name: 'Cam 1', kind: 'bars' });

    const solid = sourceFormToBody(sourceForm({ kind: 'solid', color: '#101014' }));
    expect(solid).toMatchObject({ kind: 'solid', color: '#101014' });

    const clock = sourceFormToBody(
      sourceForm({ kind: 'clock', clockFace: 'digital', clockTwelveHour: true, clockTzMinutes: '600' }),
    );
    expect(clock).toMatchObject({
      kind: 'clock',
      face: 'digital',
      twelve_hour: true,
      tz_offset_minutes: 600,
    });
  });

  it('carries the advanced blocks exactly per schema when set', () => {
    const body = sourceFormToBody(
      sourceForm({
        kind: 'hls',
        url: 'https://example.com/x.m3u8',
        authSecretRef: 'op://Servers/cam/credentials',
        colorOverrideEnabled: true,
        colorPrimaries: 'bt709',
        colorTransfer: 'auto',
        colorMatrix: 'auto',
        colorRange: 'limited',
        captionsMode: 'teletext_page',
        captionsPage: '801',
        gpuPinEnabled: true,
        gpuPinVendor: 'nvidia',
        gpuPinStableId: 'GPU-uuid-1',
        wallclock: 'use',
      }),
    );
    expect(body.auth).toEqual({ secret_ref: 'op://Servers/cam/credentials' });
    expect(body.color_override).toEqual({
      primaries: 'bt709',
      transfer: 'auto',
      matrix: 'auto',
      range: 'limited',
    });
    expect(body.captions).toEqual({ mode: 'teletext_page', page: 801 });
    expect(body.gpu_pin).toEqual({ vendor: 'nvidia', stable_id: 'GPU-uuid-1' });
    expect(body.wallclock).toEqual({ use: 'use' });
  });

  it('maps every caption mode onto its tagged payload', () => {
    const base = sourceForm({ kind: 'hls', url: 'https://example.com/x.m3u8' });
    expect(sourceFormToBody({ ...base, captionsMode: 'auto' }).captions).toEqual({ mode: 'auto' });
    expect(sourceFormToBody({ ...base, captionsMode: 'off' }).captions).toEqual({ mode: 'off' });
    expect(
      sourceFormToBody({ ...base, captionsMode: 'track', captionsTrack: 'eng' }).captions,
    ).toEqual({ mode: 'track', id: 'eng' });
    expect(
      sourceFormToBody({ ...base, captionsMode: 'embedded_cc', captionsField: 'cc1' }).captions,
    ).toEqual({ mode: 'embedded_cc', field: 'cc1' });
    expect(
      sourceFormToBody({ ...base, captionsMode: 'sidecar', captionsPath: '/subs/a.vtt' }).captions,
    ).toEqual({ mode: 'sidecar', path: '/subs/a.vtt' });
  });

  it('omits every optional block when unset (absent ≠ default-valued)', () => {
    const body = sourceFormToBody(sourceForm({ kind: 'rtsp', url: 'rtsp://h/x' }));
    for (const key of ['auth', 'color_override', 'captions', 'gpu_pin', 'wallclock']) {
      expect(body).not.toHaveProperty(key);
    }
  });

  it('preserves unknown-but-valid extra body fields across an edit round-trip', () => {
    const record = {
      id: 'cam1',
      name: 'Cam 1',
      body: {
        id: 'cam1',
        kind: 'rtsp',
        url: 'rtsp://h/x',
        future_field: { keep: true },
      },
    };
    const form = sourceFormFromRecord(record);
    const body = sourceFormToBody(form);
    expect(body.future_field).toEqual({ keep: true });
  });
});

describe('sourceFormFromRecord', () => {
  it('prefills the kind payload and the advanced blocks', () => {
    const form = sourceFormFromRecord({
      id: 'cam1',
      name: 'Cam 1',
      body: {
        id: 'cam1',
        kind: 'rtsp',
        url: 'rtsp://h/x',
        rtsp: { transport: 'udp' },
        auth: { secret_ref: 'op://v/i/f' },
        captions: { mode: 'track', id: 'eng' },
        gpu_pin: { vendor: 'amd', stable_id: '0000:03:00.0' },
        wallclock: { use: 'discard' },
        color_override: { primaries: 'bt2020', transfer: 'auto', matrix: 'auto', range: 'auto' },
      },
    });
    expect(form.kind).toBe('rtsp');
    expect(form.url).toBe('rtsp://h/x');
    expect(form.rtspTransport).toBe('udp');
    expect(form.authSecretRef).toBe('op://v/i/f');
    expect(form.captionsMode).toBe('track');
    expect(form.captionsTrack).toBe('eng');
    expect(form.gpuPinEnabled).toBe(true);
    expect(form.gpuPinVendor).toBe('amd');
    expect(form.gpuPinStableId).toBe('0000:03:00.0');
    expect(form.wallclock).toBe('discard');
    expect(form.colorOverrideEnabled).toBe(true);
    expect(form.colorPrimaries).toBe('bt2020');
  });

  it('parses the youtube kind', () => {
    const form = sourceFormFromRecord({
      id: 'yt',
      name: 'YT',
      body: { id: 'yt', kind: 'youtube', url: 'https://www.youtube.com/watch?v=abc' },
    });
    expect(form.kind).toBe('youtube');
    expect(form.url).toBe('https://www.youtube.com/watch?v=abc');
  });
});

describe('withSourceKind', () => {
  it('resets the kind payload so stale fields never leak into the body', () => {
    const rtsp = sourceForm({ kind: 'rtsp', url: 'rtsp://h/x', rtspTransport: 'tcp' });
    const solid = withSourceKind(rtsp, 'solid');
    expect(solid.kind).toBe('solid');
    const body = sourceFormToBody(solid);
    expect(body).not.toHaveProperty('url');
    expect(body).not.toHaveProperty('rtsp');
    expect(typeof body.color).toBe('string');
  });
});

describe('validateSourceForm', () => {
  it('requires id on create and name always', () => {
    expect(validateSourceForm(sourceForm({ id: '' }), true).id).toBe('required');
    expect(validateSourceForm(sourceForm({ id: '' }), false).id).toBeUndefined();
    expect(validateSourceForm(sourceForm({ name: '' }), true).name).toBe('required');
  });

  it('validates the locator per kind scheme', () => {
    expect(
      validateSourceForm(sourceForm({ kind: 'rtsp', url: 'http://h/x' }), true).url,
    ).toBe('scheme-rtsp');
    expect(
      validateSourceForm(sourceForm({ kind: 'srt', url: 'rtsp://h/x' }), true).url,
    ).toBe('scheme-srt');
    expect(
      validateSourceForm(sourceForm({ kind: 'hls', url: 'rtsp://h/x' }), true).url,
    ).toBe('scheme-http');
    expect(
      validateSourceForm(sourceForm({ kind: 'youtube', url: 'ftp://h/x' }), true).url,
    ).toBe('scheme-http');
    expect(
      validateSourceForm(sourceForm({ kind: 'rtmp', url: 'srt://h:1' }), true).url,
    ).toBe('scheme-rtmp');
    expect(
      validateSourceForm(sourceForm({ kind: 'ts', url: 'not a url' }), true).url,
    ).toBe('url-invalid');
    expect(
      validateSourceForm(
        sourceForm({ kind: 'rtsp', url: 'rtsp://[2001:db8::1]:8554/cam' }),
        true,
      ).url,
    ).toBeUndefined();
  });

  it('requires the ndi name / file path', () => {
    expect(validateSourceForm(sourceForm({ kind: 'ndi' }), true).ndiName).toBe('required');
    expect(validateSourceForm(sourceForm({ kind: 'file' }), true).path).toBe('required');
  });

  it('checks the solid colour and the clock tz range', () => {
    expect(
      validateSourceForm(sourceForm({ kind: 'solid', color: 'red' }), true).color,
    ).toBe('hex-color');
    expect(
      validateSourceForm(sourceForm({ kind: 'clock', clockTzMinutes: '900' }), true)
        .clockTzMinutes,
    ).toBe('int-range');
    expect(
      validateSourceForm(sourceForm({ kind: 'clock', clockTzMinutes: '600' }), true)
        .clockTzMinutes,
    ).toBeUndefined();
  });

  it('checks the caption mode parameters', () => {
    expect(
      validateSourceForm(
        sourceForm({ kind: 'bars', captionsMode: 'teletext_page', captionsPage: '42' }),
        true,
      ).captionsPage,
    ).toBe('int-range');
    expect(
      validateSourceForm(
        sourceForm({ kind: 'bars', captionsMode: 'track', captionsTrack: '' }),
        true,
      ).captionsTrack,
    ).toBe('required');
  });

  it('requires a stable id when a GPU pin is enabled', () => {
    expect(
      validateSourceForm(
        sourceForm({ kind: 'bars', gpuPinEnabled: true, gpuPinStableId: '  ' }),
        true,
      ).gpuPinStableId,
    ).toBe('required');
  });

  it('passes a complete, correct form', () => {
    expect(
      validateSourceForm(sourceForm({ kind: 'rtsp', url: 'rtsp://h/x' }), true),
    ).toEqual({});
  });
});

describe('outputFormToBody', () => {
  it('builds an rtsp_server body (mount + codec + optional latency profile)', () => {
    const body = outputFormToBody(
      outputForm({ kind: 'rtsp', mount: '/multiview', codec: 'h264', latencyProfile: 'low_latency' }),
    );
    expect(body).toEqual({
      kind: 'rtsp_server',
      id: 'out1',
      mount: '/multiview',
      codec: 'h264',
      latency_profile: 'low_latency',
    });
  });

  it('builds an ll_hls body with part/segment/gop in ms', () => {
    const body = outputFormToBody(
      outputForm({
        kind: 'll-hls',
        path: '/hls/multiview',
        codec: 'h264',
        partTargetMs: '200',
        segmentMs: '2000',
        gopMs: '1000',
      }),
    );
    expect(body).toEqual({
      kind: 'll_hls',
      id: 'out1',
      path: '/hls/multiview',
      codec: 'h264',
      part_target_ms: 200,
      segment_ms: 2000,
      gop_ms: 1000,
    });
  });

  it('omits the optional durations when blank', () => {
    const body = outputFormToBody(
      outputForm({ kind: 'hls', path: '/hls/multiview', codec: 'h264' }),
    );
    expect(body).toEqual({ kind: 'hls', id: 'out1', path: '/hls/multiview', codec: 'h264' });
  });

  it('builds hls with segment_ms, ndi by name (no codec), rtmp/srt by url', () => {
    expect(
      outputFormToBody(outputForm({ kind: 'hls', path: '/hls/m', codec: 'hevc', segmentMs: '4000' })),
    ).toMatchObject({ kind: 'hls', segment_ms: 4000 });
    const ndi = outputFormToBody(outputForm({ kind: 'ndi', ndiName: 'Multiview PGM' }));
    expect(ndi).toEqual({ kind: 'ndi', id: 'out1', name: 'Multiview PGM' });
    expect(
      outputFormToBody(outputForm({ kind: 'rtmp', url: 'rtmp://i.example/app/key', codec: 'h264' })),
    ).toEqual({ kind: 'rtmp', id: 'out1', url: 'rtmp://i.example/app/key', codec: 'h264' });
    expect(
      outputFormToBody(outputForm({ kind: 'srt', url: 'srt://[2001:db8::1]:7001', codec: 'h264' })),
    ).toEqual({ kind: 'srt', id: 'out1', url: 'srt://[2001:db8::1]:7001', codec: 'h264' });
  });

  it('carries the audio selection and gpu pin per schema when set', () => {
    const program = outputFormToBody(
      outputForm({ kind: 'hls', path: '/h', codec: 'h264', audioMode: 'program' }),
    );
    expect(program.audio).toEqual({ mode: 'program', tracks: [] });

    const tracks = outputFormToBody(
      outputForm({
        kind: 'hls',
        path: '/h',
        codec: 'h264',
        audioMode: 'tracks',
        audioTracks: 'prog, commentary',
        gpuPinEnabled: true,
        gpuPinVendor: 'intel',
        gpuPinStableId: '0000:00:02.0',
      }),
    );
    expect(tracks.audio).toEqual({ mode: 'tracks', tracks: ['prog', 'commentary'] });
    expect(tracks.gpu_pin).toEqual({ vendor: 'intel', stable_id: '0000:00:02.0' });
  });
});

describe('outputFormFromRecord', () => {
  it('round-trips an ll_hls record including the advanced fields', () => {
    const form = outputFormFromRecord({
      id: 'llh',
      name: 'LL-HLS',
      body: {
        kind: 'll_hls',
        id: 'llh',
        path: '/hls/m',
        codec: 'hevc',
        part_target_ms: 250,
        segment_ms: 2000,
        gop_ms: 1000,
        audio: { mode: 'tracks', tracks: ['prog'] },
        gpu_pin: { vendor: 'nvidia', stable_id: 'GPU-1' },
      },
    });
    expect(form.kind).toBe('ll-hls');
    expect(form.path).toBe('/hls/m');
    expect(form.codec).toBe('hevc');
    expect(form.partTargetMs).toBe('250');
    expect(form.segmentMs).toBe('2000');
    expect(form.gopMs).toBe('1000');
    expect(form.audioMode).toBe('tracks');
    expect(form.audioTracks).toBe('prog');
    expect(form.gpuPinEnabled).toBe(true);
    expect(outputFormToBody(form)).toMatchObject({
      kind: 'll_hls',
      part_target_ms: 250,
      audio: { mode: 'tracks', tracks: ['prog'] },
    });
  });
});

describe('validateOutputForm', () => {
  it('requires the per-kind destination', () => {
    expect(validateOutputForm(outputForm({ kind: 'rtsp', codec: 'h264' }), true).mount).toBe(
      'required',
    );
    expect(
      validateOutputForm(outputForm({ kind: 'rtsp', mount: 'multiview', codec: 'h264' }), true)
        .mount,
    ).toBe('mount-slash');
    expect(validateOutputForm(outputForm({ kind: 'hls', codec: 'h264' }), true).path).toBe(
      'required',
    );
    expect(validateOutputForm(outputForm({ kind: 'ndi' }), true).ndiName).toBe('required');
    expect(
      validateOutputForm(outputForm({ kind: 'rtmp', url: 'srt://h:1', codec: 'h264' }), true).url,
    ).toBe('scheme-rtmp');
    expect(
      validateOutputForm(outputForm({ kind: 'srt', url: 'rtmp://h/a', codec: 'h264' }), true).url,
    ).toBe('scheme-srt');
  });

  it('requires a codec on the codec-bearing kinds only', () => {
    expect(
      validateOutputForm(outputForm({ kind: 'hls', path: '/h', codec: '' }), true).codec,
    ).toBe('required');
    expect(
      validateOutputForm(outputForm({ kind: 'ndi', ndiName: 'X', codec: '' }), true).codec,
    ).toBeUndefined();
  });

  it('rejects non-positive-integer durations and a trackless tracks mode', () => {
    expect(
      validateOutputForm(
        outputForm({ kind: 'll-hls', path: '/h', codec: 'h264', partTargetMs: '-2' }),
        true,
      ).partTargetMs,
    ).toBe('positive-int');
    expect(
      validateOutputForm(
        outputForm({ kind: 'hls', path: '/h', codec: 'h264', segmentMs: 'abc' }),
        true,
      ).segmentMs,
    ).toBe('positive-int');
    expect(
      validateOutputForm(
        outputForm({ kind: 'hls', path: '/h', codec: 'h264', audioMode: 'tracks', audioTracks: ' ' }),
        true,
      ).audioTracks,
    ).toBe('tracks-required');
  });
});

describe('OUTPUT_RUNNABLE', () => {
  it('mirrors build_outputs in multiview-cli pipeline.rs', () => {
    // hls / ll_hls / rtmp / srt are runnable today; rtsp_server and ndi are
    // accepted by config but warned + skipped by build_outputs.
    expect(OUTPUT_RUNNABLE.hls).toBe(true);
    expect(OUTPUT_RUNNABLE['ll-hls']).toBe(true);
    expect(OUTPUT_RUNNABLE.rtmp).toBe(true);
    expect(OUTPUT_RUNNABLE.srt).toBe(true);
    expect(OUTPUT_RUNNABLE.rtsp).toBe(false);
    expect(OUTPUT_RUNNABLE.ndi).toBe(false);
  });
});

describe('overlayFormToBody', () => {
  it('builds a clock overlay with its consumed params (face / tz / placement)', () => {
    const body = overlayFormToBody(
      overlayForm({
        kind: 'clock',
        target: 'canvas',
        z: '100',
        clockFace: 'analog',
        clockTzMinutes: '600',
        clockX: '1800',
        clockY: '1000',
        clockRadius: '120',
      }),
    );
    expect(body).toEqual({
      id: 'ov1',
      kind: 'clock',
      target: 'canvas',
      z: 100,
      face: 'analog',
      tz_minutes: 600,
      x: 1800,
      y: 1000,
      radius: 120,
    });
  });

  it('omits unset optional clock params', () => {
    const body = overlayFormToBody(
      overlayForm({ kind: 'clock', target: 'canvas', z: '10', clockFace: 'digital' }),
    );
    expect(body).toEqual({ id: 'ov1', kind: 'clock', target: 'canvas', z: 10, face: 'digital' });
  });

  it('builds a tally_border overlay (width_px / color / binding)', () => {
    const body = overlayFormToBody(
      overlayForm({
        kind: 'tally_border',
        target: 'cell_big',
        z: '50',
        tallyWidthPx: '6',
        tallyColor: '#FF0000',
        tallyBinding: 'tally://cell_big',
      }),
    );
    expect(body).toEqual({
      id: 'ov1',
      kind: 'tally_border',
      target: 'cell_big',
      z: 50,
      width_px: 6,
      color: '#FF0000',
      binding: 'tally://cell_big',
    });
  });

  it('builds label / image / subtitle with no invented params', () => {
    for (const kind of ['label', 'image', 'subtitle'] as const) {
      const body = overlayFormToBody(overlayForm({ kind, target: 'canvas', z: '1' }));
      expect(body).toEqual({ id: 'ov1', kind, target: 'canvas', z: 1 });
    }
  });

  it('preserves verbatim params it does not render when the kind is unchanged', () => {
    const record = {
      id: 'ov_clock',
      name: 'Clock',
      body: {
        id: 'ov_clock',
        kind: 'clock',
        target: 'canvas',
        z: 100,
        face: 'analog',
        format: '%H:%M:%S',
        anchor: 'bottom_right',
        offset: { x: -20, y: -16 },
      },
    };
    const form = overlayFormFromRecord(record);
    const body = overlayFormToBody(form);
    expect(body.format).toBe('%H:%M:%S');
    expect(body.anchor).toBe('bottom_right');
    expect(body.offset).toEqual({ x: -20, y: -16 });
  });

  it('drops the previous kind params when the kind switches', () => {
    const record = {
      id: 'ov_clock',
      name: 'Clock',
      body: { id: 'ov_clock', kind: 'clock', target: 'canvas', z: 100, face: 'analog', format: '%H' },
    };
    const switched = withOverlayKind(overlayFormFromRecord(record), 'label');
    const body = overlayFormToBody(switched);
    expect(body).toEqual({ id: 'ov_clock', kind: 'label', target: 'canvas', z: 100 });
  });
});

describe('validateOverlayForm', () => {
  it('requires id (create) / name / target and an integer z', () => {
    expect(validateOverlayForm(overlayForm({ id: '' }), true).id).toBe('required');
    expect(validateOverlayForm(overlayForm({ target: '' }), true).target).toBe('required');
    expect(validateOverlayForm(overlayForm({ z: 'x' }), true).z).toBe('int');
  });

  it('checks the tally border params', () => {
    expect(
      validateOverlayForm(
        overlayForm({ kind: 'tally_border', tallyWidthPx: '0' }),
        true,
      ).tallyWidthPx,
    ).toBe('positive-int');
    expect(
      validateOverlayForm(
        overlayForm({ kind: 'tally_border', tallyWidthPx: '4', tallyColor: 'red' }),
        true,
      ).tallyColor,
    ).toBe('hex-color');
  });

  it('checks the clock numeric params', () => {
    expect(
      validateOverlayForm(overlayForm({ kind: 'clock', clockTzMinutes: '1.5' }), true)
        .clockTzMinutes,
    ).toBe('int');
    expect(
      validateOverlayForm(overlayForm({ kind: 'clock', clockRadius: '-3' }), true).clockRadius,
    ).toBe('positive-int');
  });
});
