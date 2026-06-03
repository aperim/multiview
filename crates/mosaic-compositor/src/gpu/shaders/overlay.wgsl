// Overlay sub-pass — blend batched overlay primitives premultiplied-source-over
// the existing Rgba16Float LINEAR canvas, between the composite and encode
// passes (ADR-0016 §4.1, invariants #5 + #8). NO extra full-canvas READBACK to
// the CPU (T10): it samples the composite canvas and writes the blended result
// to the canvas the encode pass already reads — all on-GPU, no host transfer.
// (A read-write storage texture in `rgba16float` is not portable in WebGPU
// core, so the pass reads an input view and writes a separate output storage
// texture; the GPU compositor aliases this with the encode-pass input so no
// extra copy is introduced.)
//
// One batched compute pass over the canvas (cost ~constant in tile count, T5):
// each invocation is one canvas pixel; it folds every primitive that covers it,
// back-to-front, with the same `over` math as crate::blend::over and the CPU
// reference (crate::overlay::subpass). Colors are LINEAR + premultiplied at
// blend time (swash coverage is straight). Glyph coverage is sampled from the
// persistent atlas (R8 straight coverage); primitives are evaluated analytically
// from a small storage buffer — no per-frame bitmap.
//
// `common.wgsl` is prepended at load time (mat3_apply, transfer fns, quantize).

// Primitive kinds (mirror crate::overlay::gpu_subpass::PrimitiveKind).
const KIND_GLYPH: u32 = 0u;
const KIND_RECT: u32 = 1u;

struct OverlayPrim {
    // x: kind. y: corner_radius (px, rects). z: atlas x (texels, glyphs).
    // w: atlas y (texels, glyphs).
    kind_meta: vec4<u32>,
    // x,y: dest top-left on canvas (px). z,w: box width,height (px).
    rect: vec4<i32>,
    // Straight LINEAR RGBA color (premultiplied in-shader by coverage).
    color: vec4<f32>,
};

struct OverlayUniforms {
    // x,y: canvas width,height (px). z: primitive count. w: padding.
    canvas: vec4<u32>,
};

@group(0) @binding(0) var<uniform> ov: OverlayUniforms;
@group(0) @binding(1) var<storage, read> prims: array<OverlayPrim>;
// Persistent glyph atlas: R8 straight coverage (premultiplied at blend time).
@group(0) @binding(2) var atlas: texture_2d<f32>;
// The composite-output linear canvas (sampled input).
@group(0) @binding(3) var canvas_in: texture_2d<f32>;
// The blended linear canvas the encode pass reads (write-only storage).
@group(0) @binding(4) var canvas_out: texture_storage_2d<rgba16float, write>;

// Closed-form rounded-rect coverage, identical to the CPU reference
// (crate::overlay::subpass::rect_coverage): 0 radius => 1.0 everywhere; corner
// pixels get a 1px linear antialias falloff across the arc.
fn rect_coverage(width: f32, height: f32, col: f32, row: f32, radius: f32) -> f32 {
    if radius <= 0.0 {
        return 1.0;
    }
    let px = col + 0.5;
    let py = row + 0.5;
    let dx = max(max(radius - px, px - (width - radius)), 0.0);
    let dy = max(max(radius - py, py - (height - radius)), 0.0);
    if dx <= 0.0 || dy <= 0.0 {
        return 1.0;
    }
    let dist = sqrt(dx * dx + dy * dy);
    return clamp(radius - dist + 0.5, 0.0, 1.0);
}

@compute @workgroup_size(8, 8, 1)
fn overlay_main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let cw = ov.canvas.x;
    let ch = ov.canvas.y;
    if gid.x >= cw || gid.y >= ch {
        return;
    }
    let coord = vec2<i32>(i32(gid.x), i32(gid.y));

    // The composite pass stores STRAIGHT linear RGBA; premultiply for the
    // source-over fold, then un-premultiply on store so the encode pass sees the
    // same straight-alpha layout it already expects.
    let straight_in = textureLoad(canvas_in, coord, 0);
    var acc = vec4<f32>(straight_in.rgb * straight_in.a, straight_in.a);

    let count = ov.canvas.z;
    for (var i: u32 = 0u; i < count; i = i + 1u) {
        let p = prims[i];
        let dx = p.rect.x;
        let dy = p.rect.y;
        let pw = p.rect.z;
        let ph = p.rect.w;
        let col = coord.x - dx;
        let row = coord.y - dy;
        if col < 0 || row < 0 || col >= pw || row >= ph {
            continue;
        }

        var coverage = 0.0;
        if p.kind_meta.x == KIND_GLYPH {
            // Sample straight coverage from the persistent atlas (R8).
            let ax = i32(p.kind_meta.z) + col;
            let ay = i32(p.kind_meta.w) + row;
            coverage = textureLoad(atlas, vec2<i32>(ax, ay), 0).r;
        } else {
            // Analytic (rounded) rectangle — meters, markers, tally, chrome.
            let radius = f32(p.kind_meta.y);
            coverage = rect_coverage(f32(pw), f32(ph), f32(col), f32(row), radius);
        }
        if coverage <= 0.0 {
            continue;
        }

        // Premultiplied-linear source-over (same as crate::blend::over).
        let a = clamp(p.color.a * coverage, 0.0, 1.0);
        let src = vec4<f32>(p.color.rgb * a, a);
        let inv = 1.0 - src.a;
        acc = src + acc * inv;
    }

    // Store straight linear RGBA again (encode reads straight; invariant #8).
    var straight_out = vec4<f32>(0.0, 0.0, 0.0, 0.0);
    if acc.a != 0.0 {
        straight_out = vec4<f32>(acc.rgb / acc.a, acc.a);
    }
    textureStore(canvas_out, coord, straight_out);
}
