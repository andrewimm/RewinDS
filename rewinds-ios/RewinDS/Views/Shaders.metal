#include <metal_stdlib>
using namespace metal;

// A fullscreen textured quad that blits one emulator screen (RGBA8) to the drawable.
// The SwiftUI view is constrained to each screen's native aspect ratio, so no
// letterboxing is needed here — uv 0..1 maps straight across.

struct VertexOut {
    float4 position [[position]];
    float2 uv;
};

vertex VertexOut screen_vertex(uint vid [[vertex_id]]) {
    // Triangle strip: bottom-left, bottom-right, top-left, top-right.
    const float2 pos[4] = { float2(-1, -1), float2(1, -1), float2(-1, 1), float2(1, 1) };
    // Texture origin is top-left, so the top of the image (v = 0) maps to NDC y = +1.
    const float2 uv[4]  = { float2(0, 1), float2(1, 1), float2(0, 0), float2(1, 0) };
    VertexOut out;
    out.position = float4(pos[vid], 0, 1);
    out.uv = uv[vid];
    return out;
}

fragment float4 screen_fragment(VertexOut in [[stage_in]],
                                texture2d<float> tex [[texture(0)]]) {
    // Nearest magnification keeps the pixel art crisp when scaled up.
    constexpr sampler smp(mag_filter::nearest, min_filter::linear, address::clamp_to_edge);
    return tex.sample(smp, in.uv);
}
