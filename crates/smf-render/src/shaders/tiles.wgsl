// Draws raster tiles as screen-space quads. The CPU projects tile corners in f64
// and sends pixel positions, so the GPU never sees huge Mercator coordinates.

// Padded to 16 bytes: WebGL-based browsers require uniform structs to be 16-byte multiples.
struct Globals {
    size: vec2<f32>,
    pad: vec2<f32>,
};

@group(0) @binding(0) var<uniform> globals: Globals;
@group(0) @binding(1) var tiles: texture_2d_array<f32>;
@group(0) @binding(2) var samp: sampler;

struct VsIn {
    @location(0) pos: vec2<f32>,
    @location(1) uv: vec2<f32>,
    @location(2) layer: u32,
    @location(3) alpha: f32,
};

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) @interpolate(flat) layer: u32,
    @location(2) alpha: f32,
};

@vertex
fn vs_main(in: VsIn) -> VsOut {
    var out: VsOut;
    let ndc = vec2<f32>(in.pos.x / globals.size.x * 2.0 - 1.0, 1.0 - in.pos.y / globals.size.y * 2.0);
    out.clip = vec4<f32>(ndc, 0.0, 1.0);
    out.uv = in.uv;
    out.layer = in.layer;
    out.alpha = in.alpha;
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    var c = textureSample(tiles, samp, in.uv, in.layer);
    c.a = c.a * in.alpha;
    return c;
}
