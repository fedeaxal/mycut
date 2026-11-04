struct Uniforms {
    delta: f32,
    _pad1: f32,
    _pad2: f32,
    _pad3: f32,
};

struct VertexOutput {
    @builtin(position) pos: vec4f,
    @location(0) uv: vec2f,
};

@group(0) @binding(0) var<uniform> params: Uniforms;
@group(0) @binding(1) var samp: sampler;
@group(0) @binding(2) var tex0: texture_2d<f32>;
@group(0) @binding(3) var tex1: texture_2d<f32>;

@vertex
fn vs_main(@builtin(vertex_index) vid: u32) -> VertexOutput {
    var positions = array<vec2f, 4>(
        vec2f(-1.0, -1.0),
        vec2f( 1.0, -1.0),
        vec2f(-1.0,  1.0),
        vec2f( 1.0,  1.0),
    );
    var uvs = array<vec2f, 4>(
        vec2f(0.0, 1.0),
        vec2f(1.0, 1.0),
        vec2f(0.0, 0.0),
        vec2f(1.0, 0.0),
    );
    var out: VertexOutput;
    out.pos = vec4f(positions[vid], 0.0, 1.0);
    out.uv = uvs[vid];
    return out;
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4f {
    let iColor = textureSample(tex0, samp, in.uv).rgb;
    let meanColor = textureSample(tex1, samp, in.uv).rgb;
    var diffColor = (iColor - meanColor) * params.delta;
    diffColor = min(diffColor * diffColor, vec3f(1.0));
    return vec4f(diffColor, 1.0);
}
