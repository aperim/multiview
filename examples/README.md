# Example configurations

Ready-to-adapt Mosaic configs. They conform to the schema documented in
[`../docs/templates/layout-and-config.md`](../docs/templates/layout-and-config.md) (canvas →
layout → cells → overlays → outputs, config-as-code). `fps` is **always** a rational string
(never a float); the canvas working color space is the single source of truth for the pipeline.

| File | Layout | Sources | Notes |
|------|--------|---------|-------|
| [`2x2.toml`](2x2.toml) | 2×2 grid | 4 × built-in `test` | Self-contained; no network needed. |
| [`3x3.toml`](3x3.toml) | 3×3 grid | 9 × built-in `test` | Density demo. |
| [`1plus5.toml`](1plus5.toml) | 1 large + 5 small | 6 × built-in `test` | Asymmetric grid via `grid-template-areas`. |
| [`pip.toml`](pip.toml) | Picture-in-picture | 2 × built-in `test` | Absolute normalized rect overlap. |
| [`public-streams-2x2.toml`](public-streams-2x2.toml) | 2×2 grid | Real public streams | Mixed fps/codec/**color** torture test — see below. |

The `test` source is the built-in synthetic pattern, so the grid/PiP examples run with no external
dependencies. Replace a source's `kind`/`url` to point at real `rtsp`/`hls`/`ts`/`srt`/`ndi` inputs.

## `public-streams-2x2.toml` — the real-world mix

Uses the project demo streams from
[`../docs/reference/example-streams.md`](../docs/reference/example-streams.md). Between them they
span **25 / 29.97 / 50 / 60 fps**, **H.264 + HEVC**, and **untagged vs BT.709 vs BT.601-mixed**
color — exactly the heterogeneity the engine must normalize onto one canvas. Several streams are
**geo-restricted** (AU/US) and may be unavailable; for reproducible/offline runs prefer the `test`
sources or the synthetic recipes in the example-streams doc.

```sh
# (once implemented) validate, then run:
mosaic validate examples/public-streams-2x2.toml
mosaic run      examples/public-streams-2x2.toml
```
