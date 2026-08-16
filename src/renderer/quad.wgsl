// Same binding layout as equirect shader so we can reuse bind groups.
// @group(0) = camera/quad transform matrix (perspective for 360°, aspect-fit
//             scale for 2D letterboxing)
// @group(1) = texture + sampler
@group(0) @binding(0) var<uniform> view_proj: mat4x4<f32>;
@group(1) @binding(0) var y_texture: texture_2d<f32>;
@group(1) @binding(1) var uv_texture: texture_2d<f32>;
@group(1) @binding(2) var video_sampler: sampler;

struct VertexInput {
    @location(0) position: vec2<f32>,
    @location(1) uv: vec2<f32>,
}

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
}

@vertex
fn vs_main(input: VertexInput) -> VertexOutput {
    var out: VertexOutput;
    out.position = view_proj * vec4(input.position, 0.0, 1.0);
    out.uv = input.uv;
    return out;
}

@fragment
fn fs_main(input: VertexOutput) -> @location(0) vec4<f32> {
    let y = textureSample(y_texture, video_sampler, input.uv).r;
    let uv = textureSample(uv_texture, video_sampler, input.uv).rg;
    // NV12 → RGB, BT.709 limited range (same matrix as FFmpeg's swscale).
    let yv = (y - 16.0 / 255.0) * (255.0 / 219.0);
    let u = (uv.r - 128.0 / 255.0) * (255.0 / 224.0);
    let v = (uv.g - 128.0 / 255.0) * (255.0 / 224.0);
    let r = clamp(yv + 1.5748 * v, 0.0, 1.0);
    let g = clamp(yv - 0.1873 * u - 0.4681 * v, 0.0, 1.0);
    let b = clamp(yv + 1.8556 * u, 0.0, 1.0);
    return vec4<f32>(r, g, b, 1.0);
}
