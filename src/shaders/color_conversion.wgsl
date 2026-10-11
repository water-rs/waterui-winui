// One colour-space conversion pass between the compositor's scRGB surfaces
// and the effect's premultiplied linear Display P3 working space. The
// direction lives entirely in the primaries matrix in `conversion`, so both
// passes share this shader and pipeline.

struct ConversionUniform {
    column0: vec4<f32>,
    column1: vec4<f32>,
    column2: vec4<f32>,
}

@group(0) @binding(0) var input_texture: texture_2d<f32>;
@group(0) @binding(1) var<uniform> conversion: ConversionUniform;

@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32) -> @builtin(position) vec4<f32> {
    let positions = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>(3.0, -1.0),
        vec2<f32>(-1.0, 3.0),
    );
    return vec4<f32>(positions[vertex_index], 0.0, 1.0);
}

@fragment
fn fs_main(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let source = textureLoad(input_texture, vec2<i32>(position.xy), 0);
    let matrix = mat3x3<f32>(
        conversion.column0.xyz,
        conversion.column1.xyz,
        conversion.column2.xyz,
    );
    return vec4<f32>(matrix * source.rgb, source.a);
}
