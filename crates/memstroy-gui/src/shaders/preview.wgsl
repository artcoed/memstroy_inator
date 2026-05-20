// Preview compositor shaders for memstroy_generator.
// Fullscreen quad vertex shader + fragment shaders for blit and chroma key.

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

// Fullscreen triangle strip (4 vertices → quad covering NDC [-1,1])
@vertex
fn vs_main(@builtin(vertex_index) idx: u32) -> VertexOutput {
    var out: VertexOutput;
    // Positions for a fullscreen quad as triangle strip
    let x = f32(i32(idx & 1u)) * 2.0 - 1.0;
    let y = f32(i32(idx >> 1u)) * 2.0 - 1.0;
    out.position = vec4<f32>(x, y, 0.0, 1.0);
    // UV: [0,0] top-left to [1,1] bottom-right
    out.uv = vec2<f32>((x + 1.0) * 0.5, (1.0 - y) * 0.5);
    return out;
}

// Texture + sampler bindings
@group(0) @binding(0) var t_source: texture_2d<f32>;
@group(0) @binding(1) var s_source: sampler;

// Simple blit with alpha
@fragment
fn fs_blit(in: VertexOutput) -> @location(0) vec4<f32> {
    return textureSample(t_source, s_source, in.uv);
}

// Chroma key fragment shader.
// Removes green-screen pixels based on HSV distance from a key colour.
// The key colour is hardcoded as standard Mellstroy green (0, 177, 64) / 255.
// TODO: Pass key_color, similarity, blend as uniforms for configurability.
@fragment
fn fs_chromakey(in: VertexOutput) -> @location(0) vec4<f32> {
    let texel = textureSample(t_source, s_source, in.uv);
    let rgb = texel.rgb;

    // Key colour (normalised): Mellstroy green
    let key = vec3<f32>(0.0, 0.694, 0.251);

    // Convert to a simple colour-distance metric
    let diff = rgb - key;
    let dist = length(diff);

    // Similarity threshold (lower = more aggressive keying)
    let similarity = 0.35;
    let blend_range = 0.15;

    var alpha = 1.0;
    if dist < similarity {
        alpha = 0.0;
    } else if dist < similarity + blend_range {
        alpha = (dist - similarity) / blend_range;
    }

    // Spill suppression: reduce green toward avg of R and B
    var corrected = rgb;
    if alpha > 0.0 {
        let avg_rb = (rgb.r + rgb.b) * 0.5;
        let spill_amount = max(0.0, rgb.g - avg_rb) * 0.5;
        corrected.g = rgb.g - spill_amount;
    }

    return vec4<f32>(corrected, texel.a * alpha);
}
