struct Uniforms {
    texel_width_offset: f32,
    texel_height_offset: f32,
    _pad: vec2f,
};

struct VertexOutput {
    @builtin(position) pos: vec4f,
    @location(0) blurCoordinates_0: vec2f,
    @location(1) blurCoordinates_1: vec2f,
    @location(2) blurCoordinates_2: vec2f,
    @location(3) blurCoordinates_3: vec2f,
    @location(4) blurCoordinates_4: vec2f,
    @location(5) blurCoordinates_5: vec2f,
    @location(6) blurCoordinates_6: vec2f,
    @location(7) blurCoordinates_7: vec2f,
    @location(8) blurCoordinates_8: vec2f,
    @location(9) blurCoordinates_9: vec2f,
    @location(10) blurCoordinates_10: vec2f,
    @location(11) blurCoordinates_11: vec2f,
    @location(12) blurCoordinates_12: vec2f,
    @location(13) blurCoordinates_13: vec2f,
};

@group(0) @binding(0) var<uniform> params: Uniforms;
@group(0) @binding(1) var samp: sampler;
@group(0) @binding(2) var tex: texture_2d<f32>;

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

    let uv = uvs[vid];
    let singleStepOffset = vec2f(params.texel_width_offset, params.texel_height_offset);

    var out: VertexOutput;
    out.pos = vec4f(positions[vid], 0.0, 1.0);
    out.blurCoordinates_0 = uv;
    out.blurCoordinates_1 = uv + singleStepOffset * 1.5;
    out.blurCoordinates_2 = uv - singleStepOffset * 1.5;
    out.blurCoordinates_3 = uv + singleStepOffset * 3.5;
    out.blurCoordinates_4 = uv - singleStepOffset * 3.5;
    out.blurCoordinates_5 = uv + singleStepOffset * 5.5;
    out.blurCoordinates_6 = uv - singleStepOffset * 5.5;
    out.blurCoordinates_7 = uv + singleStepOffset * 7.5;
    out.blurCoordinates_8 = uv - singleStepOffset * 7.5;
    out.blurCoordinates_9 = uv + singleStepOffset * 9.5;
    out.blurCoordinates_10 = uv - singleStepOffset * 9.5;
    out.blurCoordinates_11 = uv + singleStepOffset * 11.5;
    out.blurCoordinates_12 = uv - singleStepOffset * 11.5;
    out.blurCoordinates_13 = uv + singleStepOffset * 13.5;
    return out;
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4f {
    let boxWeight = 1.0 / 29.0;
    var sum = textureSample(tex, samp, in.blurCoordinates_0) * boxWeight;
    sum += textureSample(tex, samp, in.blurCoordinates_1) * (boxWeight * 2.0);
    sum += textureSample(tex, samp, in.blurCoordinates_2) * (boxWeight * 2.0);
    sum += textureSample(tex, samp, in.blurCoordinates_3) * (boxWeight * 2.0);
    sum += textureSample(tex, samp, in.blurCoordinates_4) * (boxWeight * 2.0);
    sum += textureSample(tex, samp, in.blurCoordinates_5) * (boxWeight * 2.0);
    sum += textureSample(tex, samp, in.blurCoordinates_6) * (boxWeight * 2.0);
    sum += textureSample(tex, samp, in.blurCoordinates_7) * (boxWeight * 2.0);
    sum += textureSample(tex, samp, in.blurCoordinates_8) * (boxWeight * 2.0);
    sum += textureSample(tex, samp, in.blurCoordinates_9) * (boxWeight * 2.0);
    sum += textureSample(tex, samp, in.blurCoordinates_10) * (boxWeight * 2.0);
    sum += textureSample(tex, samp, in.blurCoordinates_11) * (boxWeight * 2.0);
    sum += textureSample(tex, samp, in.blurCoordinates_12) * (boxWeight * 2.0);
    sum += textureSample(tex, samp, in.blurCoordinates_13) * (boxWeight * 2.0);
    return sum;
}
