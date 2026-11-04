struct Uniforms {
    intensity: f32,
    blend_mode: i32,
    _pad: vec2f,
};

struct VertexOutput {
    @builtin(position) pos: vec4f,
    @location(0) uv: vec2f,
    @location(1) uv2: vec2f,
};

@group(0) @binding(0) var<uniform> params: Uniforms;
@group(0) @binding(1) var samp: sampler;
@group(0) @binding(2) var tex0: texture_2d<f32>;  // background (original image)
@group(0) @binding(3) var tex1: texture_2d<f32>;  // makeup texture (mouth/blusher)

fn blendHardLight(base: f32, blend: f32) -> f32 {
    return select(
        1.0 - 2.0 * (1.0 - base) * (1.0 - blend),
        2.0 * base * blend,
        blend < 0.5
    );
}

fn blendHardLight3(base: vec3f, blend: vec3f) -> vec3f {
    return vec3f(
        blendHardLight(base.r, blend.r),
        blendHardLight(base.g, blend.g),
        blendHardLight(base.b, blend.b),
    );
}

fn blendOverlay(base: f32, blend: f32) -> f32 {
    return select(
        1.0 - 2.0 * (1.0 - base) * (1.0 - blend),
        2.0 * base * blend,
        base < 0.5
    );
}

fn blendOverlay3(base: vec3f, blend: vec3f) -> vec3f {
    return vec3f(
        blendOverlay(base.r, blend.r),
        blendOverlay(base.g, blend.g),
        blendOverlay(base.b, blend.b),
    );
}

fn blendFunc(base: vec3f, blend: vec3f) -> vec3f {
    if (params.blend_mode == 0) {
        return blend;
    } else if (params.blend_mode == 15) {
        return base * blend;  // Multiply
    } else if (params.blend_mode == 17) {
        return blendOverlay3(base, blend);
    } else if (params.blend_mode == 22) {
        return blendHardLight3(base, blend);
    }
    return blend;
}

@vertex
fn vs_main(@builtin(vertex_index) vid: u32) -> VertexOutput {
    var positions = array<vec2f, 4>(
        vec2f(-1.0, -1.0), vec2f( 1.0, -1.0),
        vec2f(-1.0,  1.0), vec2f( 1.0,  1.0),
    );
    var uvs = array<vec2f, 4>(
        vec2f(0.0, 1.0), vec2f(1.0, 1.0),
        vec2f(0.0, 0.0), vec2f(1.0, 0.0),
    );
    var out: VertexOutput;
    out.pos = vec4f(positions[vid], 0.0, 1.0);
    out.uv = uvs[vid];
    out.uv2 = positions[vid] * 0.5 + 0.5;
    return out;
}

// Vertex shader for indexed face mesh rendering — reads pos/uv from vertex buffer
@vertex
fn vs_mesh(@location(0) pos: vec2f, @location(1) uv: vec2f) -> VertexOutput {
    var out: VertexOutput;
    out.pos = vec4f(pos, 0.0, 1.0);
    out.uv = uv;
    // NDC X → texture U: [-1,1] → [0,1]
    // NDC Y → texture V: [-1,1] → [1,0] (flip: NDC Y-up, texture V-down)
    out.uv2 = vec2f(pos.x * 0.5 + 0.5, 0.5 - pos.y * 0.5);
    return out;
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4f {
    var fgColor = textureSample(tex1, samp, in.uv);
    fgColor = fgColor * params.intensity;
    let bgColor = textureSample(tex0, samp, in.uv2);

    if (fgColor.a == 0.0) {
        return bgColor;
    }

    let blended = blendFunc(bgColor.rgb, clamp(fgColor.rgb * (1.0 / fgColor.a), vec3f(0.0), vec3f(1.0)));
    return vec4f(bgColor.rgb * (1.0 - fgColor.a) + blended * fgColor.a, 1.0);
}
