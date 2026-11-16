struct Vertex {
    @location(0) pos: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) uv: vec2<f32>,
    @location(3) color: vec4<f32>,
}

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) normal: vec3<f32>,
    @location(1) uv: vec2<f32>,
    @location(2) color: vec4<f32>,
}

struct Uniforms {
    view_proj: mat4x4<f32>,
    model: mat4x4<f32>,
    normal_mat: mat4x4<f32>,
    base_color: vec4<f32>,
    // x metallic, y roughness, z mode, w has_texture
    misc: vec4<f32>,
    // x alpha mode (0 opaque, 1 mask, 2 blend), y cutoff, z unlit
    extra: vec4<f32>,
}

@group(0) @binding(0) var<uniform> u: Uniforms;
@group(0) @binding(1) var tex: texture_2d<f32>;
@group(0) @binding(2) var samp: sampler;

struct CompactIn {
    @location(0) pos: vec3<f32>,
    @location(1) normal: vec4<f32>,
}

@vertex
fn vs_compact(v: CompactIn) -> VsOut {
    var o: VsOut;
    let world = u.model * vec4<f32>(v.pos, 1.0);
    o.clip = u.view_proj * world;
    o.normal = (u.normal_mat * vec4<f32>(v.normal.xyz, 0.0)).xyz;
    o.uv = vec2<f32>(0.0, 0.0);
    o.color = vec4<f32>(1.0, 1.0, 1.0, 1.0);
    return o;
}

@vertex
fn vs_main(v: Vertex) -> VsOut {
    var o: VsOut;
    let world = u.model * vec4<f32>(v.pos, 1.0);
    o.clip = u.view_proj * world;
    let n = (u.normal_mat * vec4<f32>(v.normal, 0.0)).xyz;
    o.normal = n;
    o.uv = v.uv;
    o.color = v.color;
    return o;
}

@fragment
fn fs_main(i: VsOut) -> @location(0) vec4<f32> {
    let mode = u32(u.misc.z + 0.5);
    var raw_n = i.normal;
    if (dot(raw_n, raw_n) < 1e-6) {
        raw_n = vec3<f32>(0.0, 1.0, 0.0);
    }
    let n = normalize(raw_n);
    if (mode == 2u) {
        return vec4<f32>(n * 0.5 + vec3<f32>(0.5), 1.0);
    }
    if (mode == 3u) {
        let cell = floor(i.uv * 8.0);
        let parity = (cell.x + cell.y) - 2.0 * floor((cell.x + cell.y) * 0.5);
        let col = mix(vec3<f32>(0.86, 0.82, 0.74), vec3<f32>(0.22, 0.26, 0.32), parity);
        return vec4<f32>(col, 1.0);
    }
    if (mode == 4u) {
        return vec4<f32>(i.color.rgb, 1.0);
    }
    if (mode == 5u) {
        return vec4<f32>(i.color.rgb, 1.0);
    }
    var nrm = n;
    if (dot(nrm, nrm) < 1e-6) {
        nrm = vec3<f32>(0.0, 1.0, 0.0);
    }
    nrm = normalize(nrm);
    var albedo = u.base_color.rgb;
    var alpha = u.base_color.a;
    if (u.misc.w > 0.5) {
        let sample = textureSample(tex, samp, i.uv);
        // A zero factor with a texture is how some exporters say "the texture is the color".
        if (dot(albedo, albedo) < 1e-6) {
            albedo = sample.rgb;
        } else {
            albedo = albedo * sample.rgb;
        }
        alpha = alpha * sample.a;
    }
    let alpha_mode = u32(u.extra.x + 0.5);
    if (alpha_mode == 1u && alpha < u.extra.y) {
        discard;
    }
    if (alpha_mode == 2u && alpha < 0.02) {
        discard;
    }
    if (u.extra.z > 0.5) {
        return vec4<f32>(albedo, alpha);
    }
    let key = normalize(vec3<f32>(0.35, 0.85, 0.40));
    let fill = normalize(vec3<f32>(-0.65, 0.25, -0.35));
    let ndl = clamp(dot(nrm, key), 0.0, 1.0);
    let ndf = clamp(dot(nrm, fill), 0.0, 1.0);
    var lit = albedo * (0.20 + 0.80 * ndl) + albedo * 0.18 * ndf;
    if (mode == 1u) {
        lit = mix(lit, vec3<f32>(0.05, 0.05, 0.06), 0.72);
    }
    return vec4<f32>(lit, alpha);
}
