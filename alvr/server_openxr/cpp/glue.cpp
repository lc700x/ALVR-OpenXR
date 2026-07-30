// glue.cpp — minimal replacements for the symbols that normally live in the
// heavy OpenVR driver translation unit (alvr_server.cpp). The OpenXR runtime
// reuses the video encoder + FrameRender from server_openvr but not the SteamVR
// driver, so we provide just the settings singleton here.

#include "alvr_server/bindings.h"

#include "platform/win32/CEncoder.h"
#include "platform/win32/shared/d3drender.h"
#include <cmath>
#include <cstdio>
#include <memory>
#include <windows.h>

// Globals normally defined in alvr_server.cpp (excluded here). Set from Rust.
const char* g_sessionPath = nullptr;
const char* g_driverRootDir = nullptr;

// Callback slots normally defined by alvr_server.cpp. Rust initializes these
// before any reused C++ code runs. They must be data symbols (function-pointer
// variables), not same-named function symbols: bindings.h declares pointers.
extern "C" {
void (*LogError)(const char*) = nullptr;
void (*LogWarn)(const char*) = nullptr;
void (*LogInfo)(const char*) = nullptr;
void (*LogDebug)(const char*) = nullptr;
void (*LogEncoder)(const char*) = nullptr;
void (*LogPeriodically)(const char*, const char*) = nullptr;
void (*SetVideoConfigNals)(const unsigned char*, int, int) = nullptr;
void (*VideoSend)(unsigned long long, unsigned char*, int, bool) = nullptr;
FfiDynamicEncoderParams (*GetDynamicEncoderParams)() = nullptr;
void (*ReportComposed)(unsigned long long, unsigned long long) = nullptr;
void (*ReportPresent)(unsigned long long, unsigned long long) = nullptr;
}

// Shader-blob globals the C++ FrameRender links against. The Rust side
// (graphics::initialize_shaders) fills these with the embedded .cso bytes.
extern "C" {
const unsigned char* FRAME_RENDER_VS_CSO_PTR = nullptr;
unsigned int FRAME_RENDER_VS_CSO_LEN = 0;
const unsigned char* FRAME_RENDER_PS_CSO_PTR = nullptr;
unsigned int FRAME_RENDER_PS_CSO_LEN = 0;
const unsigned char* QUAD_SHADER_CSO_PTR = nullptr;
unsigned int QUAD_SHADER_CSO_LEN = 0;
const unsigned char* COMPRESS_AXIS_ALIGNED_CSO_PTR = nullptr;
unsigned int COMPRESS_AXIS_ALIGNED_CSO_LEN = 0;
const unsigned char* COLOR_CORRECTION_CSO_PTR = nullptr;
unsigned int COLOR_CORRECTION_CSO_LEN = 0;
const unsigned char* RGBTOYUV420_CSO_PTR = nullptr;
unsigned int RGBTOYUV420_CSO_LEN = 0;
const unsigned char* QUAD_SHADER_COMP_SPV_PTR = nullptr;
unsigned int QUAD_SHADER_COMP_SPV_LEN = 0;
const unsigned char* COLOR_SHADER_COMP_SPV_PTR = nullptr;
unsigned int COLOR_SHADER_COMP_SPV_LEN = 0;
const unsigned char* FFR_SHADER_COMP_SPV_PTR = nullptr;
unsigned int FFR_SHADER_COMP_SPV_LEN = 0;
const unsigned char* RGBTOYUV420_SHADER_COMP_SPV_PTR = nullptr;
unsigned int RGBTOYUV420_SHADER_COMP_SPV_LEN = 0;
}

// In v20, Settings is a private-constructor singleton; access via Instance().
const Settings* Settings_Instance() { return &Settings::Instance(); }

// Called by Rust immediately after paths are resolved so the Settings
// singleton can read the correct session.json.
extern "C" void alvr_openxr_set_paths(const char* session_path, const char* driver_root) {
    g_sessionPath = session_path;
    g_driverRootDir = driver_root;
}

// C++ portion of the settings load, kept in its own function so the SEH
// wrapper (`__try/__except`) does not sit in a frame that also needs C++
// object unwinding (an MSVC restriction). std::exception from picojson is
// handled here; a hard access violation is caught by the caller's `__except`.
static void alvr_openxr_load_settings_inner() {
    try {
        Settings::Instance().Load();
    } catch (const std::exception& e) {
        fprintf(stderr, "[alvr] load_settings: exception: %s\n", e.what());
        fflush(stderr);
    } catch (...) {
        fprintf(stderr, "[alvr] load_settings: caught unknown C++ fault\n");
        fflush(stderr);
    }
}

// Called by the Rust side after paths are set, to populate the Settings
// singleton from the session JSON file.
extern "C" void alvr_openxr_load_settings() {
    // A structured-exception (SEH) barrier around the reused C++ settings loader.
    // `Settings::Load()` reads session.json via picojson; if the file is being
    // rewritten concurrently by the dashboard/host it can observe a half-written
    // document and dereference a null/invalid picojson value, raising a Windows
    // access violation. A C++ `catch (...)` does NOT reliably trap an access
    // violation, so it previously propagated into and killed the host Python
    // process. `__try/__except` catches the SEH so a bad read degrades to
    // "settings unavailable" (the caller falls back to its placeholder) instead
    // of crashing. The actual C++ work is delegated to a helper because a
    // function using `__try` may not also require C++ object unwinding.
    __try {
        alvr_openxr_load_settings_inner();
    } __except (EXCEPTION_EXECUTE_HANDLER) {
        fprintf(stderr, "[alvr] load_settings: caught SEH access violation\n");
        fflush(stderr);
    }
}

// Read the encoder GPU adapter index from the loaded Settings (avoids taking
// the Rust session lock on the ClientConnected hot path).
extern "C" int32_t alvr_openxr_adapter_index() {
    return Settings::Instance().m_nAdapterIndex;
}

extern "C" uint32_t alvr_openxr_refresh_rate() {
    return Settings::Instance().m_refreshRate;
}

// Total stereo render target width/height from the loaded ALVR settings. The
// OpenXR runtime advertises per-eye size = width/2 x height so the app's
// swapchains match the resolution configured in ALVR.
extern "C" uint32_t alvr_openxr_render_width() {
    return Settings::Instance().m_renderWidth;
}

extern "C" uint32_t alvr_openxr_render_height() {
    return Settings::Instance().m_renderHeight;
}

// Normally in alvr_server.cpp: the encoders call this to force a keyframe.
// The frame loop drives IDR explicitly via the submit path, so this is a no-op.
extern "C" void RequestIDR() {}

// --- Encoder lifecycle (driven from the OpenXR frame loop) -----------------

static std::shared_ptr<CD3DRender> g_render;
static std::shared_ptr<CEncoder> g_encoder;
static bool g_encoderStarted = false;

// Start the video encoder on its own D3D11 device. Must be called only after
// settings are populated (FrameRender reads Settings_Instance for resolution).
extern "C" void alvr_openxr_encoder_start(uint32_t adapterIndex) {
    if (g_encoderStarted) {
        return;
    }

    // /EHa lets catch(...) trap access violations from the reused C++ pipeline.
    try {
        g_render = std::make_shared<CD3DRender>();
        if (!g_render->Initialize(adapterIndex)) {
            fprintf(stderr, "[alvr] encoder_start FAIL: CD3DRender::Initialize(%u)\n", adapterIndex);
            fflush(stderr);
            g_render.reset();
            return;
        }

        g_encoder = std::make_shared<CEncoder>();
        g_encoder->Initialize(g_render);

        g_encoder->OnStreamStart();
        g_encoder->Start();
        g_encoderStarted = true;
    } catch (const std::exception& e) {
        fprintf(stderr, "[alvr] encoder_start FAIL: exception: %s\n", e.what());
        fflush(stderr);
        g_encoder.reset();
        g_render.reset();
    } catch (...) {
        fprintf(stderr, "[alvr] encoder_start FAIL: caught SEH/unknown fault during init\n");
        fflush(stderr);
        g_encoder.reset();
        g_render.reset();
    }
}

extern "C" void alvr_openxr_encoder_stop() {
    if (g_encoder) {
        g_encoder->Stop();
        g_encoder.reset();
    }
    g_render.reset();
    g_encoderStarted = false;
}

// Convert an OpenXR field-of-view (radians) to the tangent-based HmdRect2_t the
// FrameRender projection expects.
static vr::HmdRect2_t fovToProjection(const float fov[4]) {
    vr::HmdRect2_t proj;
    proj.vTopLeft.v[0] = std::tan(fov[0]);     // angleLeft
    proj.vTopLeft.v[1] = std::tan(fov[2]);     // angleUp
    proj.vBottomRight.v[0] = std::tan(fov[1]); // angleRight
    proj.vBottomRight.v[1] = std::tan(fov[3]); // angleDown
    return proj;
}

// Submit a stereo frame: two shared D3D11 textures (one per eye) plus per-eye
// FOV. Safe no-op until the encoder is started.
extern "C" void alvr_openxr_encoder_submit(
    void* sharedHandleLeft,
    void* sharedHandleRight,
    const float fovLeft[4],
    const float fovRight[4],
    const float eyeToHeadLeft[3],
    const float eyeToHeadRight[3],
    // Per-eye normalized UV crop [uMin, uMax, vMin, vMax] from the app's
    // sub_image.image_rect. Selects each eye's region within its swapchain
    // texture (double-wide / sub-rect layouts).
    const float boundsLeft[4],
    const float boundsRight[4],
    uint64_t timestampNs,
    bool idr,
    bool flipV
) {
    if (!g_encoderStarted || !g_encoder || !g_render) {
        static int warned = 0;
        if (warned++ < 3) {
            fprintf(stderr, "[alvr] submit DROP: encoder not started (started=%d)\n",
                    g_encoderStarted ? 1 : 0);
            fflush(stderr);
        }
        return;
    }

    ID3D11Texture2D* texLeft = g_render->GetSharedTexture((HANDLE)sharedHandleLeft);
    ID3D11Texture2D* texRight = g_render->GetSharedTexture((HANDLE)sharedHandleRight);
    if (!texLeft || !texRight) {
        static int warned = 0;
        if (warned++ < 3) {
            fprintf(stderr, "[alvr] submit DROP: GetSharedTexture null (L=%p R=%p)\n",
                    (void*)texLeft, (void*)texRight);
            fflush(stderr);
        }
        return;
    }

    vr::HmdMatrix34_t identity = {};
    identity.m[0][0] = identity.m[1][1] = identity.m[2][2] = 1.0f;
    vr::HmdMatrix34_t eyeToHeadL = identity;
    vr::HmdMatrix34_t eyeToHeadR = identity;
    for (int i = 0; i < 3; i++) {
        eyeToHeadL.m[i][3] = eyeToHeadLeft[i];
        eyeToHeadR.m[i][3] = eyeToHeadRight[i];
    }

    // Mirror Hmd::SetViewsConfig in the original direct-mode path. FrameRender
    // applies these transforms independently for the two eyes; identity here
    // removes the negotiated IPD and makes its stereo correction asymmetric.
    g_encoder->SetViewsConfig(
        fovToProjection(fovLeft), eyeToHeadL, fovToProjection(fovRight), eyeToHeadR
    );

    ID3D11Texture2D* textures[1][2] = { { texLeft, texRight } };
    // Per-eye UV crop from the app's image_rect. OpenGL's bottom-left texture
    // origin flips V only; swap vMin/vMax for GL. Do NOT flip U or the eyes
    // diverge.
    const float uMinL = boundsLeft[0],  uMaxL = boundsLeft[1];
    const float uMinR = boundsRight[0], uMaxR = boundsRight[1];
    const float vMinL = flipV ? boundsLeft[3]  : boundsLeft[2];
    const float vMaxL = flipV ? boundsLeft[2]  : boundsLeft[3];
    const float vMinR = flipV ? boundsRight[3] : boundsRight[2];
    const float vMaxR = flipV ? boundsRight[2] : boundsRight[3];
    vr::VRTextureBounds_t bounds[1][2] = {
        { { uMinL, vMinL, uMaxL, vMaxL }, { uMinR, vMinR, uMaxR, vMaxR } }
    };
    vr::HmdMatrix34_t poses[1] = { identity };

    if (idr) {
        g_encoder->InsertIDR();
    }

    // Match v20 OvrDirectModeComponent: serialize access to the shared encoder
    // D3D context, compose this frame, then flush before waking encode thread.
    g_encoder->WaitForEncode();
    g_encoder->CopyToStaging(
        textures, bounds, poses, 1, false, 0, timestampNs, "", ""
    );
    g_render->GetContext()->Flush();
    g_encoder->NewFrameReady();

    // Report composition + presentation for this frame so the client can measure
    // the real end-to-end pipeline latency. Without these the client's total
    // latency estimate is too low, so its head-pose prediction falls short and
    // fast head rotation shows a visible offset. OvrDirectModeComponent does this
    // in the SteamVR path; the driverless OpenXR path must do it too. Offsets are
    // zero (we report at the moment of composition/submit).
    if (ReportComposed) {
        ReportComposed(timestampNs, 0);
    }
    if (ReportPresent) {
        ReportPresent(timestampNs, 0);
    }
}
