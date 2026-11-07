struct Parameters {
    crop: vec4<u32>,
    output: vec4<u32>,
};

@group(0) @binding(0) var desktop: texture_2d<f32>;
@group(0) @binding(1) var<uniform> params: Parameters;

@vertex
fn vertex_main(@builtin(vertex_index) index: u32) -> @builtin(position) vec4<f32> {
    let positions = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0), vec2<f32>(3.0, -1.0), vec2<f32>(-1.0, 3.0)
    );
    return vec4<f32>(positions[index], 0.0, 1.0);
}

@fragment
fn fragment_main(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    if params.output.z == 0u {
        return vec4<f32>(0.0, 0.0, 0.0, 1.0);
    }
    // Match the CPU path's integer nearest-neighbor mapping, including fractional scales.
    let pixel = vec2<u32>(position.xy);
    let source = params.crop.xy + pixel * params.crop.zw / params.output.xy;
    if any(source >= textureDimensions(desktop)) {
        return vec4<f32>(0.0, 0.0, 0.0, 1.0);
    }
    var rgb = textureLoad(desktop, vec2<i32>(source), 0).rgb;
    // Preserve encoded desktop colors when the compositor offers an sRGB attachment.
    if params.output.w != 0u {
        rgb = select(pow((rgb + 0.055) / 1.055, vec3<f32>(2.4)), rgb / 12.92, rgb <= vec3<f32>(0.04045));
    }
    return vec4<f32>(rgb, 1.0);
}
