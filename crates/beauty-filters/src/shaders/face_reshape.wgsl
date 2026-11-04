struct Uniforms {
    aspect_ratio: f32,
    thin_face_delta: f32,
    big_eye_delta: f32,
    has_face: u32,
};

struct VertexOutput {
    @builtin(position) pos: vec4f,
    @location(0) uv: vec2f,
};

@group(0) @binding(0) var<uniform> params: Uniforms;
@group(0) @binding(1) var samp: sampler;
@group(0) @binding(2) var tex: texture_2d<f32>;
@group(0) @binding(3) var<storage, read> face_points: array<f32, 212>;

fn curveWarp(textureCoord: vec2f, originPosition: vec2f, targetPosition: vec2f, delta: f32) -> vec2f {
    let direction = (targetPosition - originPosition) * delta;
    let radius = distance(
        vec2f(targetPosition.x, targetPosition.y / params.aspect_ratio),
        vec2f(originPosition.x, originPosition.y / params.aspect_ratio)
    );
    let ratio = distance(
        vec2f(textureCoord.x, textureCoord.y / params.aspect_ratio),
        vec2f(originPosition.x, originPosition.y / params.aspect_ratio)
    ) / radius;
    let clamped_ratio = clamp(1.0 - ratio, 0.0, 1.0);
    let offset = direction * clamped_ratio;
    return textureCoord - offset;
}

fn thinFace(currentCoordinate: vec2f) -> vec2f {
    var coord = currentCoordinate;
    let faceIndexs = array<vec2f, 9>(
        vec2f(3., 44.), vec2f(29., 44.), vec2f(7., 45.),
        vec2f(25., 45.), vec2f(10., 46.), vec2f(22., 46.),
        vec2f(14., 49.), vec2f(18., 49.), vec2f(16., 49.),
    );
    for (var i = 0; i < 9; i++) {
        let originIndex = i32(faceIndexs[i].x);
        let targetIndex = i32(faceIndexs[i].y);
        let originPoint = vec2f(face_points[u32(originIndex * 2)], face_points[u32(originIndex * 2 + 1)]);
        let targetPoint = vec2f(face_points[u32(targetIndex * 2)], face_points[u32(targetIndex * 2 + 1)]);
        coord = curveWarp(coord, originPoint, targetPoint, params.thin_face_delta);
    }
    return coord;
}

fn enlargeEye(textureCoord: vec2f, originPosition: vec2f, radius: f32, delta: f32) -> vec2f {
    var coord = textureCoord;
    let weight = distance(
        vec2f(coord.x, coord.y / params.aspect_ratio),
        vec2f(originPosition.x, originPosition.y / params.aspect_ratio)
    ) / radius;
    let w = clamp(1.0 - (1.0 - weight * weight) * delta, 0.0, 1.0);
    coord = originPosition + (coord - originPosition) * w;
    return coord;
}

fn bigEye(currentCoordinate: vec2f) -> vec2f {
    var coord = currentCoordinate;
    let faceIndexs = array<vec2f, 2>(
        vec2f(74., 72.), vec2f(77., 75.),
    );
    for (var i = 0; i < 2; i++) {
        let originIndex = i32(faceIndexs[i].x);
        let targetIndex = i32(faceIndexs[i].y);
        let originPoint = vec2f(face_points[u32(originIndex * 2)], face_points[u32(originIndex * 2 + 1)]);
        let targetPoint = vec2f(face_points[u32(targetIndex * 2)], face_points[u32(targetIndex * 2 + 1)]);
        let radius = distance(
            vec2f(targetPoint.x, targetPoint.y / params.aspect_ratio),
            vec2f(originPoint.x, originPoint.y / params.aspect_ratio)
        ) * 5.0;
        coord = enlargeEye(coord, originPoint, radius, params.big_eye_delta);
    }
    return coord;
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
    return out;
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4f {
    var positionToUse = in.uv;
    if (params.has_face == 1u) {
        positionToUse = thinFace(positionToUse);
        positionToUse = bigEye(positionToUse);
    }
    return textureSample(tex, samp, positionToUse);
}
