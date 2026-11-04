struct Uniforms {
    width_offset: f32,
    height_offset: f32,
    sharpen: f32,
    blur_alpha: f32,
    whiten: f32,
    _pad1: f32,
    _pad2: f32,
    _pad3: f32,
};

struct VertexOutput {
    @builtin(position) pos: vec4f,
    @location(0) uv: vec2f,
    @location(1) textureShift_1: vec4f,
    @location(2) textureShift_2: vec4f,
    @location(3) textureShift_3: vec4f,
    @location(4) textureShift_4: vec4f,
};

@group(0) @binding(0) var<uniform> params: Uniforms;
@group(0) @binding(1) var samp: sampler;
@group(0) @binding(2) var tex0: texture_2d<f32>;  // original
@group(0) @binding(3) var tex1: texture_2d<f32>;  // mean (blurred)
@group(0) @binding(4) var tex2: texture_2d<f32>;  // variance (high pass)
@group(0) @binding(5) var lookup_gray: texture_2d<f32>;
@group(0) @binding(6) var lookup_origin: texture_2d<f32>;
@group(0) @binding(7) var lookup_skin: texture_2d<f32>;
@group(0) @binding(8) var lookup_custom: texture_2d<f32>;

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
    let wo = params.width_offset;
    let ho = params.height_offset;

    var out: VertexOutput;
    out.pos = vec4f(positions[vid], 0.0, 1.0);
    out.uv = uv;
    out.textureShift_1 = vec4f(uv + vec2f(-wo, 0.0), uv + vec2f(wo, 0.0));
    out.textureShift_2 = vec4f(uv + vec2f(0.0, -ho), uv + vec2f(0.0, ho));
    out.textureShift_3 = vec4f(uv + vec2f(wo, ho), uv + vec2f(-wo, -ho));
    out.textureShift_4 = vec4f(uv + vec2f(-wo, ho), uv + vec2f(wo, -ho));
    return out;
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4f {
    let iColor = textureSample(tex0, samp, in.uv);
    let meanColor = textureSample(tex1, samp, in.uv);
    let varColor = textureSample(tex2, samp, in.uv);

    const levelRangeInv = 1.02657;
    const levelBlack = 0.0258820;
    const alpha = 0.7;

    var color = iColor.rgb;

    if (params.blur_alpha >= 0.0) {
        let theta = 0.1;
        let p = clamp((min(iColor.r, meanColor.r - 0.1) - 0.2) * 4.0, 0.0, 1.0);
        let meanVar = (varColor.r + varColor.g + varColor.b) / 3.0;
        var kMin = (1.0 - meanVar / (meanVar + theta)) * p * params.blur_alpha;
        kMin = clamp(kMin, 0.0, 1.0);
        var resultColor = mix(iColor.rgb, meanColor.rgb, kMin);

        // High-pass sharpening
        var sum = 0.25 * iColor.rgb;
        sum += 0.125 * textureSample(tex0, samp, in.textureShift_1.xy).rgb;
        sum += 0.125 * textureSample(tex0, samp, in.textureShift_1.zw).rgb;
        sum += 0.125 * textureSample(tex0, samp, in.textureShift_2.xy).rgb;
        sum += 0.125 * textureSample(tex0, samp, in.textureShift_2.zw).rgb;
        sum += 0.0625 * textureSample(tex0, samp, in.textureShift_3.xy).rgb;
        sum += 0.0625 * textureSample(tex0, samp, in.textureShift_3.zw).rgb;
        sum += 0.0625 * textureSample(tex0, samp, in.textureShift_4.xy).rgb;
        sum += 0.0625 * textureSample(tex0, samp, in.textureShift_4.zw).rgb;

        let hPass = iColor.rgb - sum;
        color = resultColor + params.sharpen * hPass * 2.0;
    }

    if (params.whiten > 0.0) {
        let colorEPM = color;
        var c = clamp((colorEPM - vec3f(levelBlack)) * levelRangeInv, vec3f(0.0), vec3f(1.0));

        // Stage 1: lookup_gray
        var texel = vec3f(
            textureSample(lookup_gray, samp, vec2f(c.r, 0.5)).r,
            textureSample(lookup_gray, samp, vec2f(c.g, 0.5)).g,
            textureSample(lookup_gray, samp, vec2f(c.b, 0.5)).b,
        );
        texel = mix(c, texel, 0.5);
        texel = mix(colorEPM, texel, alpha);

        // Stage 2: lookup_origin
        texel = clamp(texel, vec3f(0.0), vec3f(1.0));
        var blueColor = texel.b * 15.0;
        var quad1_y = floor(floor(blueColor) * 0.25);
        var quad1_x = floor(blueColor) - (quad1_y * 4.0);
        var quad2_y = floor(ceil(blueColor) * 0.25);
        var quad2_x = ceil(blueColor) - (quad2_y * 4.0);
        var texPos2 = texel.rg * 0.234375 + 0.0078125;
        var texPos1 = vec2f(quad1_x * 0.25, quad1_y * 0.25) + texPos2;
        var texPos2b = vec2f(quad2_x * 0.25, quad2_y * 0.25) + texPos2;
        let newColor1Origin = textureSample(lookup_origin, samp, texPos1).rgb;
        let newColor2Origin = textureSample(lookup_origin, samp, texPos2b).rgb;
        let colorOrigin = mix(newColor1Origin, newColor2Origin, fract(blueColor));
        texel = mix(colorOrigin, c, alpha);

        // Stage 3: lookup_skin
        texel = clamp(texel, vec3f(0.0), vec3f(1.0));
        blueColor = texel.b * 15.0;
        quad1_y = floor(floor(blueColor) * 0.25);
        quad1_x = floor(blueColor) - (quad1_y * 4.0);
        quad2_y = floor(ceil(blueColor) * 0.25);
        quad2_x = ceil(blueColor) - (quad2_y * 4.0);
        texPos2 = texel.rg * 0.234375 + 0.0078125;
        texPos1 = vec2f(quad1_x * 0.25, quad1_y * 0.25) + texPos2;
        texPos2b = vec2f(quad2_x * 0.25, quad2_y * 0.25) + texPos2;
        let newColor1 = textureSample(lookup_skin, samp, texPos1).rgb;
        let newColor2 = textureSample(lookup_skin, samp, texPos2b).rgb;
        color = mix(newColor1, newColor2, fract(blueColor));
        color = clamp(color, vec3f(0.0), vec3f(1.0));

        // Stage 4: lookup_custom (512x512 with 8x8 grid)
        let blueColor_custom = color.b * 63.0;
        var quad1c_y = floor(floor(blueColor_custom) / 8.0);
        var quad1c_x = floor(blueColor_custom) - (quad1c_y * 8.0);
        var quad2c_y = floor(ceil(blueColor_custom) / 8.0);
        var quad2c_x = ceil(blueColor_custom) - (quad2c_y * 8.0);
        var texPos1c = vec2f(
            (quad1c_x * 1.0 / 8.0) + 0.5 / 512.0 + ((1.0 / 8.0 - 1.0 / 512.0) * color.r),
            (quad1c_y * 1.0 / 8.0) + 0.5 / 512.0 + ((1.0 / 8.0 - 1.0 / 512.0) * color.g),
        );
        var texPos2c = vec2f(
            (quad2c_x * 1.0 / 8.0) + 0.5 / 512.0 + ((1.0 / 8.0 - 1.0 / 512.0) * color.r),
            (quad2c_y * 1.0 / 8.0) + 0.5 / 512.0 + ((1.0 / 8.0 - 1.0 / 512.0) * color.g),
        );
        let newColor1c = textureSample(lookup_custom, samp, texPos1c).rgb;
        let newColor2c = textureSample(lookup_custom, samp, texPos2c).rgb;
        let color_custom = mix(newColor1c, newColor2c, fract(blueColor_custom));
        color = mix(color, color_custom, params.whiten);
    }

    return vec4f(color, iColor.a);
}
