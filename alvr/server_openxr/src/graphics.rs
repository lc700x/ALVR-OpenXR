//! Publishes the precompiled shader blobs (reused in place from server_openvr)
//! to the C++ FrameRender pipeline via the extern-C pointer globals.

use crate::bindings;

const OPENVR_CPP: &str = "../server_openvr/cpp";

static FRAME_RENDER_VS_CSO: &[u8] =
    include_bytes!("../../server_openvr/cpp/platform/win32/FrameRenderVS.cso");
static FRAME_RENDER_PS_CSO: &[u8] =
    include_bytes!("../../server_openvr/cpp/platform/win32/FrameRenderPS.cso");
static QUAD_SHADER_CSO: &[u8] =
    include_bytes!("../../server_openvr/cpp/platform/win32/QuadVertexShader.cso");
static COMPRESS_AXIS_ALIGNED_CSO: &[u8] =
    include_bytes!("../../server_openvr/cpp/platform/win32/CompressAxisAlignedPixelShader.cso");
static COLOR_CORRECTION_CSO: &[u8] =
    include_bytes!("../../server_openvr/cpp/platform/win32/ColorCorrectionPixelShader.cso");
static RGBTOYUV420_CSO: &[u8] =
    include_bytes!("../../server_openvr/cpp/platform/win32/rgbtoyuv420.cso");

pub fn initialize_shaders() {
    // ponytail: OPENVR_CPP kept for clarity of provenance; blobs are compiled in.
    let _ = OPENVR_CPP;

    unsafe {
        bindings::FRAME_RENDER_VS_CSO_PTR = FRAME_RENDER_VS_CSO.as_ptr();
        bindings::FRAME_RENDER_VS_CSO_LEN = FRAME_RENDER_VS_CSO.len() as _;
        bindings::FRAME_RENDER_PS_CSO_PTR = FRAME_RENDER_PS_CSO.as_ptr();
        bindings::FRAME_RENDER_PS_CSO_LEN = FRAME_RENDER_PS_CSO.len() as _;
        bindings::QUAD_SHADER_CSO_PTR = QUAD_SHADER_CSO.as_ptr();
        bindings::QUAD_SHADER_CSO_LEN = QUAD_SHADER_CSO.len() as _;
        bindings::COMPRESS_AXIS_ALIGNED_CSO_PTR = COMPRESS_AXIS_ALIGNED_CSO.as_ptr();
        bindings::COMPRESS_AXIS_ALIGNED_CSO_LEN = COMPRESS_AXIS_ALIGNED_CSO.len() as _;
        bindings::COLOR_CORRECTION_CSO_PTR = COLOR_CORRECTION_CSO.as_ptr();
        bindings::COLOR_CORRECTION_CSO_LEN = COLOR_CORRECTION_CSO.len() as _;
        bindings::RGBTOYUV420_CSO_PTR = RGBTOYUV420_CSO.as_ptr();
        bindings::RGBTOYUV420_CSO_LEN = RGBTOYUV420_CSO.len() as _;
    }
}
