// Terrain shading shared by the 3D draped mesh and the 2D hillshade/slope overlay.
//
// Heights live in an R32Float texture array (not filterable), so bilinear filtering
// is done by hand with textureLoad. Heights are in real metres; scene positions are
// Mercator metres with z scaled by the Mercator factor k so relief isn't flattened.

struct Globals {
    view_proj: mat4x4<f32>,   // eye-relative, reversed-Z (3D only)
    viewport: vec4<f32>,      // width px, height px, k, eye_z (scene metres)
    shade: vec4<f32>,         // hill strength, slope alpha, sun azimuth (rad), sun altitude (rad)
    flags: vec4<f32>,         // hill on, slope on, slope preset (0 degrees, 1 fire %), exaggeration
    fog: vec4<f32>,           // fog start, fog end (scene metres), unused, unused
    sky: vec4<f32>,           // sky/fog colour
    extra: vec4<f32>,         // overlay opacity, unused, unused, unused
};

@group(0) @binding(0) var<uniform> g: Globals;
@group(0) @binding(1) var imagery: texture_2d_array<f32>;
@group(0) @binding(2) var samp: sampler;
@group(0) @binding(3) var heights: texture_2d_array<f32>;

const TEXELS: f32 = 255.0;

fn load_h(layer: i32, x: i32, y: i32) -> f32 {
    return textureLoad(heights, vec2<i32>(clamp(x, 0, 255), clamp(y, 0, 255)), layer, 0).r;
}

// Bilinear height in metres; uv spans the whole tile, mapped onto texel centres 0..255
// so adjacent tiles agree on shared edges.
fn sample_h(layer: i32, uv: vec2<f32>) -> f32 {
    if (layer < 0) {
        return 0.0;
    }
    let p = clamp(uv, vec2<f32>(0.0), vec2<f32>(1.0)) * TEXELS;
    let i = vec2<i32>(floor(p));
    let f = p - floor(p);
    let a = load_h(layer, i.x, i.y);
    let b = load_h(layer, i.x + 1, i.y);
    let c = load_h(layer, i.x, i.y + 1);
    let d = load_h(layer, i.x + 1, i.y + 1);
    return mix(mix(a, b, f.x), mix(c, d, f.x), f.y);
}

// Upward-pointing surface normal (x east, y north, z up) from central differences.
fn normal_at(layer: i32, uv: vec2<f32>, texel_m: f32, exag: f32) -> vec3<f32> {
    let e = 1.0 / TEXELS;
    let dzdx = (sample_h(layer, uv + vec2<f32>(e, 0.0)) - sample_h(layer, uv - vec2<f32>(e, 0.0))) / (2.0 * texel_m);
    // Image rows run north to south, so +v is south.
    let dzdy = -(sample_h(layer, uv + vec2<f32>(0.0, e)) - sample_h(layer, uv - vec2<f32>(0.0, e))) / (2.0 * texel_m);
    return normalize(vec3<f32>(-dzdx * exag, -dzdy * exag, 1.0));
}

// Lambert light relative to a flat surface: 1.0 on flat ground, <1 shadowed, >1 sunlit.
fn hill_factor(n: vec3<f32>) -> f32 {
    let az = g.shade.z;
    let alt = g.shade.w;
    let l = vec3<f32>(sin(az) * cos(alt), cos(az) * cos(alt), sin(alt));
    return max(dot(n, l), 0.0) / sin(alt);
}

// Slope class colour (rgb) and presence (a). Preset 0 = degrees (avalanche-style
// classes), preset 1 = percent slope classes useful for fire behaviour.
fn slope_class(n: vec3<f32>) -> vec4<f32> {
    let deg = degrees(acos(clamp(n.z, 0.0, 1.0)));
    var t = array<f32, 4>(27.0, 30.0, 35.0, 40.0);
    if (g.flags.z > 0.5) {
        // 20%, 40%, 60%, 80% grade
        t = array<f32, 4>(11.31, 21.80, 30.96, 38.66);
    }
    if (deg < t[0]) { return vec4<f32>(0.0); }
    if (deg < t[1]) { return vec4<f32>(1.0, 0.92, 0.1, 1.0); }
    if (deg < t[2]) { return vec4<f32>(1.0, 0.55, 0.05, 1.0); }
    if (deg < t[3]) { return vec4<f32>(0.9, 0.1, 0.1, 1.0); }
    return vec4<f32>(0.55, 0.1, 0.75, 1.0);
}

// ---------------------------------------------------------------- 3D terrain

struct GridIn {
    @location(0) grid: vec3<f32>,        // u, v (0..1, v=0 north), skirt flag
    @location(1) tile: vec4<f32>,        // SW corner rel to eye (x, y), size, texel_m
    @location(2) img: vec4<f32>,         // u0, v0, scale, layer (<0 = none)
    @location(3) hgt: vec4<f32>,         // u0, v0, scale, layer (<0 = none)
};

struct TerrainOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) img_uv: vec2<f32>,
    @location(1) h_uv: vec2<f32>,
    @location(2) @interpolate(flat) img_layer: i32,
    @location(3) @interpolate(flat) h_layer: i32,
    @location(4) @interpolate(flat) texel_m: f32,
    @location(5) dist: f32,
};

@vertex
fn vs_terrain(in: GridIn) -> TerrainOut {
    var o: TerrainOut;
    let uv = in.grid.xy;
    let h_uv = in.hgt.xy + uv * in.hgt.z;
    let h = sample_h(i32(in.hgt.w), h_uv);
    let px = in.tile.x + uv.x * in.tile.z;
    let py = in.tile.y + (1.0 - uv.y) * in.tile.z;
    // Skirts hang below the edge so LOD seams never show sky through cracks.
    let drop = in.grid.z * in.tile.z * 0.03;
    let pz = h * g.viewport.z * g.flags.w - g.viewport.w - drop;
    o.clip = g.view_proj * vec4<f32>(px, py, pz, 1.0);
    o.img_uv = in.img.xy + uv * in.img.z;
    o.h_uv = h_uv;
    o.img_layer = i32(in.img.w);
    o.h_layer = i32(in.hgt.w);
    o.texel_m = in.tile.w;
    o.dist = length(vec3<f32>(px, py, pz));
    return o;
}

// Slope classes then hillshade, applied to a draped colour.
fn shade_rgb(rgb_in: vec3<f32>, h_layer: i32, h_uv: vec2<f32>, texel_m: f32) -> vec3<f32> {
    var rgb = rgb_in;
    if (h_layer >= 0) {
        // Slope classes describe the real ground; only the lighting follows the exaggeration.
        if (g.flags.y > 0.5) {
            let s = slope_class(normal_at(h_layer, h_uv, texel_m, 1.0));
            rgb = mix(rgb, s.rgb, s.a * g.shade.y);
        }
        if (g.flags.x > 0.5) {
            let lum = clamp(hill_factor(normal_at(h_layer, h_uv, texel_m, g.flags.w)), 0.35, 1.25);
            rgb = rgb * mix(1.0, lum, g.shade.x);
        }
    }
    return rgb;
}

@fragment
fn fs_terrain(in: TerrainOut) -> @location(0) vec4<f32> {
    var rgb = vec3<f32>(0.62, 0.64, 0.60);
    if (in.img_layer >= 0) {
        rgb = textureSample(imagery, samp, in.img_uv, in.img_layer).rgb;
    }
    rgb = shade_rgb(rgb, in.h_layer, in.h_uv, in.texel_m);
    let fog = clamp((in.dist - g.fog.x) / max(g.fog.y - g.fog.x, 1.0), 0.0, 1.0);
    return vec4<f32>(mix(rgb, g.sky.rgb, fog * fog), 1.0);
}

// Overlay layers: same mesh and depth as the base pass, blended on top with opacity.
@fragment
fn fs_overlay(in: TerrainOut) -> @location(0) vec4<f32> {
    if (in.img_layer < 0) {
        discard;
    }
    let t = textureSample(imagery, samp, in.img_uv, in.img_layer);
    let rgb = shade_rgb(t.rgb, in.h_layer, in.h_uv, in.texel_m);
    let fog = clamp((in.dist - g.fog.x) / max(g.fog.y - g.fog.x, 1.0), 0.0, 1.0);
    return vec4<f32>(mix(rgb, g.sky.rgb, fog * fog), t.a * g.extra.x);
}

// ---------------------------------------------------------------- 2D overlay

struct ShadeIn {
    @location(0) pos: vec2<f32>,    // screen px
    @location(1) h_uv: vec2<f32>,   // position inside the resolved height tile (0..1)
    @location(2) layer: f32,
    @location(3) texel_m: f32,
};

struct ShadeOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) h_uv: vec2<f32>,
    @location(1) @interpolate(flat) layer: i32,
    @location(2) @interpolate(flat) texel_m: f32,
};

@vertex
fn vs_shade(in: ShadeIn) -> ShadeOut {
    var o: ShadeOut;
    let ndc = vec2<f32>(in.pos.x / g.viewport.x * 2.0 - 1.0, 1.0 - in.pos.y / g.viewport.y * 2.0);
    o.clip = vec4<f32>(ndc, 0.0, 1.0);
    o.h_uv = in.h_uv;
    o.layer = i32(in.layer);
    o.texel_m = in.texel_m;
    return o;
}

// Non-premultiplied output for standard alpha blending: slope colour first, hillshade on top.
@fragment
fn fs_shade(in: ShadeOut) -> @location(0) vec4<f32> {
    var c = vec3<f32>(0.0);
    var a = 0.0;
    if (g.flags.y > 0.5) {
        let s = slope_class(normal_at(in.layer, in.h_uv, in.texel_m, 1.0));
        c = s.rgb;
        a = s.a * g.shade.y;
    }
    if (g.flags.x > 0.5) {
        let f = clamp(hill_factor(normal_at(in.layer, in.h_uv, in.texel_m, g.flags.w)), 0.35, 1.25);
        var hc = vec3<f32>(0.0);
        var ha = 0.0;
        if (f < 1.0) {
            ha = (1.0 - f) * g.shade.x;
        } else {
            hc = vec3<f32>(1.0);
            ha = (f - 1.0) * g.shade.x * 0.6;
        }
        let out_a = ha + a * (1.0 - ha);
        if (out_a > 0.0001) {
            c = (hc * ha + c * a * (1.0 - ha)) / out_a;
        }
        a = out_a;
    }
    return vec4<f32>(c, a);
}
