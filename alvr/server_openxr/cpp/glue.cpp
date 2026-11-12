// glue.cpp — minimal replacements for the symbols that normally live in the
// heavy OpenVR driver translation unit (alvr_server.cpp). The OpenXR runtime
// reuses the video encoder + FrameRender from server_openvr but not the SteamVR
// driver, so we provide just the settings singleton here.

#include "alvr_server/bindings.h"

#include "platform/win32/CEncoder.h"
#include "platform/win32/shared/d3drender.h"
#include <cmath>
#include <chrono>
#include <cstdio>
#include <cstdint>
#include <map>
#include <memory>
#include <unordered_map>
#include <unordered_set>
#include <vector>
#include <cstring>
#include <mutex>
#include <tuple>
#include <windows.h>
#include <d3d11_1.h>
#include <d3dcompiler.h>
#include <dxgi.h>

#pragma comment(lib, "d3dcompiler.lib")

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
static std::mutex g_encoderMutex;

static bool openxr_perf_enabled() {
    static const bool enabled = [] {
        char value[2] = {};
        return GetEnvironmentVariableA("ALVR_OPENXR_PERF", value, 2) == 1 && value[0] == '1';
    }();
    return enabled;
}

using OpenXrClock = std::chrono::steady_clock;

struct OpenXrSubmitTimings {
    uint32_t frameCount = 0;
    OpenXrClock::time_point windowStart;
    double waitEncodeMs = 0.0;
    double composeMs = 0.0;
    double encoderStageMs = 0.0;
    double flushMs = 0.0;
    double waitEncodeMaxMs = 0.0;
    double composeMaxMs = 0.0;
    double encoderStageMaxMs = 0.0;
    double flushMaxMs = 0.0;
};

static OpenXrSubmitTimings g_openxrSubmitTimings;

static double openxr_elapsed_ms(OpenXrClock::time_point start) {
    return std::chrono::duration<double, std::milli>(OpenXrClock::now() - start).count();
}

static void record_openxr_submit_timing(double waitEncodeMs, double composeMs,
                                        double encoderStageMs, double flushMs) {
    if (!openxr_perf_enabled()) return;
    auto& timings = g_openxrSubmitTimings;
    if (timings.frameCount == 0) timings.windowStart = OpenXrClock::now();

    timings.frameCount++;
    timings.waitEncodeMs += waitEncodeMs;
    timings.composeMs += composeMs;
    timings.encoderStageMs += encoderStageMs;
    timings.flushMs += flushMs;
    if (waitEncodeMs > timings.waitEncodeMaxMs) timings.waitEncodeMaxMs = waitEncodeMs;
    if (composeMs > timings.composeMaxMs) timings.composeMaxMs = composeMs;
    if (encoderStageMs > timings.encoderStageMaxMs) timings.encoderStageMaxMs = encoderStageMs;
    if (flushMs > timings.flushMaxMs) timings.flushMaxMs = flushMs;

    if (timings.frameCount >= 120) {
        const double count = timings.frameCount;
        const double elapsedSeconds = std::chrono::duration<double>(
            OpenXrClock::now() - timings.windowStart).count();
        fprintf(stderr,
                "[alvr] OpenXR submit %.1f fps/120f avg ms: wait_encode=%.2f "
                "compose=%.2f encoder_stage=%.2f flush=%.2f; max ms: "
                "wait_encode=%.2f compose=%.2f encoder_stage=%.2f flush=%.2f\n",
                elapsedSeconds > 0.0 ? count / elapsedSeconds : 0.0,
                timings.waitEncodeMs / count, timings.composeMs / count,
                timings.encoderStageMs / count, timings.flushMs / count,
                timings.waitEncodeMaxMs, timings.composeMaxMs,
                timings.encoderStageMaxMs, timings.flushMaxMs);
        fflush(stderr);
        timings = {};
    }
}

struct AlvrOpenXrDrawDesc {
    void* sharedHandle;
    uint32_t sharedType; // 0: legacy shared handle, 1: NT handle
    int64_t format;      // typed DXGI format for the source view
    uint32_t arraySlice;
    float uvBounds[4];   // uMin, uMax, vMin, vMax
    float clipVertices[16]; // four homogeneous clip-space vertices (x,y,z,w)
    uint32_t alphaMode;  // 0 opaque, 1 premultiplied, 2 unpremultiplied
};

class OpenXrCompositor {
public:
    bool Initialize(ID3D11Device* device) {
        const char* vertexSource =
            "struct V { float4 p : POSITION; float2 uv : TEXCOORD0; };\n"
            "struct O { float4 p : SV_POSITION; float2 uv : TEXCOORD0; };\n"
            "O main(V i) { O o; o.p=i.p; o.uv=i.uv; return o; }\n";
        // Keep the PS inputs in VS output register order, including SV_POSITION;
        // declaring only TEXCOORD0 puts it in register 0 and collapses sampled UVs.
        const char* pixelSource =
            "Texture2D image : register(t0);\n"
            "SamplerState imageSampler : register(s0);\n"
            "float4 main(float4 position : SV_POSITION, float2 uv : TEXCOORD0) : SV_TARGET { "
            "return image.Sample(imageSampler, uv); }\n";
        const char* arrayPixelSource =
            "Texture2DArray image : register(t0);\n"
            "SamplerState imageSampler : register(s0);\n"
            "float4 main(float4 position : SV_POSITION, float2 uv : TEXCOORD0) : SV_TARGET { "
            "return image.Sample(imageSampler, float3(uv, 0)); }\n";
        ComPtr<ID3DBlob> vsBlob, psBlob, arrayPsBlob, errors;
        ComPtr<ID3D11PixelShader> arrayPixelShader;
        HRESULT hr = D3DCompile(
            vertexSource, strlen(vertexSource), nullptr, nullptr, nullptr, "main", "vs_4_0",
            D3DCOMPILE_ENABLE_STRICTNESS, 0, &vsBlob, &errors
        );
        if (FAILED(hr)) {
            LogShaderError("vertex", errors.Get());
            return false;
        }
        errors.Reset();
        hr = D3DCompile(
            pixelSource, strlen(pixelSource), nullptr, nullptr, nullptr, "main", "ps_4_0",
            D3DCOMPILE_ENABLE_STRICTNESS, 0, &psBlob, &errors
        );
        if (FAILED(hr)) {
            LogShaderError("pixel", errors.Get());
            return false;
        }
        errors.Reset();
        hr = D3DCompile(
            arrayPixelSource, strlen(arrayPixelSource), nullptr, nullptr, nullptr, "main", "ps_4_0",
            D3DCOMPILE_ENABLE_STRICTNESS, 0, &arrayPsBlob, &errors
        );
        if (FAILED(hr)) {
            LogShaderError("array pixel", errors.Get());
            return false;
        }
        if (FAILED(device->CreateVertexShader(
                vsBlob->GetBufferPointer(), vsBlob->GetBufferSize(), nullptr,
                m_vertexShader.GetAddressOf()))
            || FAILED(device->CreatePixelShader(
                psBlob->GetBufferPointer(), psBlob->GetBufferSize(), nullptr,
                m_pixelShader.GetAddressOf()))
            || FAILED(device->CreatePixelShader(
                arrayPsBlob->GetBufferPointer(), arrayPsBlob->GetBufferSize(), nullptr,
                arrayPixelShader.GetAddressOf()))) {
            return false;
        }
        const D3D11_INPUT_ELEMENT_DESC layout[] = {
            { "POSITION", 0, DXGI_FORMAT_R32G32B32A32_FLOAT, 0, 0,
              D3D11_INPUT_PER_VERTEX_DATA, 0 },
            { "TEXCOORD", 0, DXGI_FORMAT_R32G32_FLOAT, 0, 16,
              D3D11_INPUT_PER_VERTEX_DATA, 0 },
        };
        if (FAILED(device->CreateInputLayout(
                layout, ARRAYSIZE(layout), vsBlob->GetBufferPointer(), vsBlob->GetBufferSize(),
                m_inputLayout.GetAddressOf()))) {
            return false;
        }
        D3D11_BUFFER_DESC vertexDesc = {};
        vertexDesc.ByteWidth = sizeof(Vertex) * 6;
        vertexDesc.Usage = D3D11_USAGE_DYNAMIC;
        vertexDesc.BindFlags = D3D11_BIND_VERTEX_BUFFER;
        vertexDesc.CPUAccessFlags = D3D11_CPU_ACCESS_WRITE;
        if (FAILED(device->CreateBuffer(&vertexDesc, nullptr, m_vertices.GetAddressOf()))) {
            return false;
        }
        D3D11_SAMPLER_DESC samplerDesc = {};
        samplerDesc.Filter = D3D11_FILTER_MIN_MAG_MIP_LINEAR;
        samplerDesc.AddressU = samplerDesc.AddressV = samplerDesc.AddressW =
            D3D11_TEXTURE_ADDRESS_CLAMP;
        samplerDesc.MaxAnisotropy = 1;
        samplerDesc.ComparisonFunc = D3D11_COMPARISON_NEVER;
        samplerDesc.MaxLOD = D3D11_FLOAT32_MAX;
        if (FAILED(device->CreateSamplerState(&samplerDesc, m_sampler.GetAddressOf()))) {
            return false;
        }
        if (!CreateBlendState(device, D3D11_BLEND_ONE, m_premultiplied)
            || !CreateBlendState(device, D3D11_BLEND_SRC_ALPHA, m_unpremultiplied)) {
            return false;
        }
        D3D11_RASTERIZER_DESC rasterDesc = {};
        rasterDesc.FillMode = D3D11_FILL_SOLID;
        rasterDesc.CullMode = D3D11_CULL_NONE;
        rasterDesc.DepthClipEnable = TRUE;
        if (FAILED(device->CreateRasterizerState(&rasterDesc, m_rasterizer.GetAddressOf()))) {
            return false;
        }
        m_arrayPixelShader = std::move(arrayPixelShader);
        m_device = device;
        return true;
    }

    bool Compose(
        const AlvrOpenXrDrawDesc* left, uint32_t leftCount,
        const AlvrOpenXrDrawDesc* right, uint32_t rightCount,
        uint32_t leftWidth, uint32_t leftHeight,
        uint32_t rightWidth, uint32_t rightHeight,
        ID3D11Texture2D* outputs[2]
    ) {
        const uint32_t widths[] = { leftWidth, rightWidth };
        const uint32_t heights[] = { leftHeight, rightHeight };
        if (!m_device || !leftWidth || !leftHeight || !rightWidth || !rightHeight
            || !EnsureTargets(widths, heights)) {
            return false;
        }
        std::vector<ComPtr<IDXGIKeyedMutex>> locked;
        if (!LockSources(left, leftCount, right, rightCount, locked)) {
            return false;
        }
        auto context = g_render->GetContext();
        const float clear[4] = { 0, 0, 0, 1 };
        for (int eye = 0; eye < 2; ++eye) {
            ID3D11RenderTargetView* target = m_targets[eye].Get();
            context->OMSetRenderTargets(1, &target, nullptr);
            context->ClearRenderTargetView(target, clear);
            D3D11_VIEWPORT viewport = {};
            viewport.Width = static_cast<float>(widths[eye]);
            viewport.Height = static_cast<float>(heights[eye]);
            viewport.MinDepth = 0.0f;
            viewport.MaxDepth = 1.0f;
            context->RSSetViewports(1, &viewport);
            context->RSSetState(m_rasterizer.Get());
            context->IASetInputLayout(m_inputLayout.Get());
            context->IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            context->VSSetShader(m_vertexShader.Get(), nullptr, 0);
            context->PSSetShader(m_pixelShader.Get(), nullptr, 0);
            ID3D11SamplerState* sampler = m_sampler.Get();
            context->PSSetSamplers(0, 1, &sampler);
            const auto* layers = eye == 0 ? left : right;
            const uint32_t count = eye == 0 ? leftCount : rightCount;
            for (uint32_t i = 0; i < count; ++i) {
                ID3D11Texture2D* texture = GetSharedTexture(layers[i].sharedHandle,
                                                             layers[i].sharedType);
                if (!texture || !DrawLayer(context, texture, layers[i])) {
                    UnbindSource(context);
                    context->Flush();
                    ReleaseLocks(locked);
                    return false;
                }
            }
            UnbindSource(context);
            outputs[eye] = m_eyeTextures[eye].Get();
        }
        // Ensure all reads have reached the shared-texture device before handing
        // key 0 back to the Vulkan queue or D3D11 producer.
        context->Flush();
        ReleaseLocks(locked);
        if (!m_loggedFirstFrame) {
            fprintf(stderr,
                    "[alvr] compositor first frame: target L=%ux%u layers=%u R=%ux%u layers=%u\n",
                    leftWidth, leftHeight, leftCount, rightWidth, rightHeight, rightCount);
            const AlvrOpenXrDrawDesc* eyes[] = { left, right };
            const uint32_t counts[] = { leftCount, rightCount };
            for (int eye = 0; eye < 2; ++eye) {
                if (!counts[eye]) {
                    fprintf(stderr, "[alvr] compositor first frame: eye=%c source=none\n",
                            eye == 0 ? 'L' : 'R');
                    continue;
                }
                const auto& layer = eyes[eye][0];
                D3D11_TEXTURE2D_DESC desc = {};
                GetSharedTexture(layer.sharedHandle, layer.sharedType)->GetDesc(&desc);
                fprintf(stderr,
                        "[alvr] compositor first frame: eye=%c source=%ux%u array_size=%u "
                        "backing_format=%d view_format=%lld array_slice=%u alpha_mode=%u "
                        "uv=[%.6f,%.6f,%.6f,%.6f]\n",
                        eye == 0 ? 'L' : 'R', desc.Width, desc.Height, desc.ArraySize,
                        static_cast<int>(desc.Format), static_cast<long long>(layer.format),
                        layer.arraySlice, layer.alphaMode,
                        layer.uvBounds[0], layer.uvBounds[1], layer.uvBounds[2], layer.uvBounds[3]);
            }
            fflush(stderr);
            m_loggedFirstFrame = true;
        }
        return true;
    }

    ID3D11Texture2D* GetSharedTexture(void* handle, uint32_t sharedType) {
        if (!handle || !g_render) {
            return nullptr;
        }
        auto found = m_sharedTextures.find(handle);
        if (found != m_sharedTextures.end()) {
            return found->second.Get();
        }
        ComPtr<ID3D11Texture2D> texture;
        HRESULT hr = E_NOINTERFACE;
        if (sharedType == 1) {
            ComPtr<ID3D11Device1> device1;
            if (SUCCEEDED(g_render->GetDevice()->QueryInterface(
                    IID_PPV_ARGS(device1.GetAddressOf())))) {
                hr = device1->OpenSharedResource1(
                    static_cast<HANDLE>(handle), __uuidof(ID3D11Texture2D),
                    reinterpret_cast<void**>(texture.GetAddressOf()));
            }
        } else {
            hr = g_render->GetDevice()->OpenSharedResource(
                static_cast<HANDLE>(handle), __uuidof(ID3D11Texture2D),
                reinterpret_cast<void**>(texture.GetAddressOf()));
        }
        if (FAILED(hr)) {
            return nullptr;
        }
        return m_sharedTextures.emplace(handle, std::move(texture)).first->second.Get();
    }

    void ForgetTexture(void* handle) {
        const std::uintptr_t sharedHandle = reinterpret_cast<std::uintptr_t>(handle);
        for (auto view = m_shaderViews.begin(); view != m_shaderViews.end();) {
            if (std::get<0>(view->first) == sharedHandle) {
                view = m_shaderViews.erase(view);
            } else {
                ++view;
            }
        }
        m_sharedTextures.erase(handle);
    }

    void Shutdown() {
        m_shaderViews.clear();
        m_sharedTextures.clear();
        for (auto& texture : m_eyeTextures) texture.Reset();
        for (auto& target : m_targets) target.Reset();
        m_premultiplied.Reset();
        m_unpremultiplied.Reset();
        m_vertices.Reset();
        m_inputLayout.Reset();
        m_vertexShader.Reset();
        m_pixelShader.Reset();
        m_arrayPixelShader.Reset();
        m_sampler.Reset();
        m_rasterizer.Reset();
        m_device = nullptr;
        m_loggedFirstFrame = false;
        for (int eye = 0; eye < 2; ++eye) m_width[eye] = m_height[eye] = 0;
    }

private:
    struct Vertex { float position[4]; float uv[2]; };

    static void LogShaderError(const char* stage, ID3DBlob* errors) {
        if (errors && errors->GetBufferPointer()) {
            fprintf(stderr, "[alvr] Vulkan compositor %s shader: %s\n", stage,
                    static_cast<const char*>(errors->GetBufferPointer()));
            fflush(stderr);
        }
    }

    static bool CreateBlendState(ID3D11Device* device, D3D11_BLEND source,
                                 ComPtr<ID3D11BlendState>& output) {
        D3D11_BLEND_DESC desc = {};
        auto& rt = desc.RenderTarget[0];
        rt.BlendEnable = TRUE;
        rt.SrcBlend = source;
        rt.DestBlend = D3D11_BLEND_INV_SRC_ALPHA;
        rt.BlendOp = D3D11_BLEND_OP_ADD;
        rt.SrcBlendAlpha = D3D11_BLEND_ONE;
        rt.DestBlendAlpha = D3D11_BLEND_INV_SRC_ALPHA;
        rt.BlendOpAlpha = D3D11_BLEND_OP_ADD;
        rt.RenderTargetWriteMask = D3D11_COLOR_WRITE_ENABLE_ALL;
        return SUCCEEDED(device->CreateBlendState(&desc, output.GetAddressOf()));
    }

    bool EnsureTargets(const uint32_t widths[2], const uint32_t heights[2]) {
        for (int eye = 0; eye < 2; ++eye) {
            if (m_width[eye] == widths[eye] && m_height[eye] == heights[eye]
                && m_eyeTextures[eye] && m_targets[eye]) continue;
            m_eyeTextures[eye].Reset();
            m_targets[eye].Reset();
            D3D11_TEXTURE2D_DESC desc = {};
            desc.Width = widths[eye];
            desc.Height = heights[eye];
            desc.MipLevels = 1;
            desc.ArraySize = 1;
            desc.Format = DXGI_FORMAT_R8G8B8A8_UNORM_SRGB;
            desc.SampleDesc.Count = 1;
            desc.Usage = D3D11_USAGE_DEFAULT;
            desc.BindFlags = D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE;
            if (FAILED(m_device->CreateTexture2D(
                    &desc, nullptr, m_eyeTextures[eye].GetAddressOf()))
                || FAILED(m_device->CreateRenderTargetView(
                    m_eyeTextures[eye].Get(), nullptr, m_targets[eye].GetAddressOf()))) {
                return false;
            }
            m_width[eye] = widths[eye];
            m_height[eye] = heights[eye];
        }
        return true;
    }

    bool LockSources(const AlvrOpenXrDrawDesc* left, uint32_t leftCount,
                     const AlvrOpenXrDrawDesc* right, uint32_t rightCount,
                     std::vector<ComPtr<IDXGIKeyedMutex>>& locked) {
        std::unordered_set<void*> seen;
        const AlvrOpenXrDrawDesc* eyes[] = { left, right };
        const uint32_t counts[] = { leftCount, rightCount };
        for (int eye = 0; eye < 2; ++eye) {
            for (uint32_t i = 0; i < counts[eye]; ++i) {
                const auto& layer = eyes[eye][i];
                if (!seen.insert(layer.sharedHandle).second) continue;
                ID3D11Texture2D* texture = GetSharedTexture(layer.sharedHandle,
                                                             layer.sharedType);
                if (!texture) {
                    ReleaseLocks(locked);
                    return false;
                }
                ComPtr<IDXGIKeyedMutex> keyedMutex;
                if (SUCCEEDED(texture->QueryInterface(
                        IID_PPV_ARGS(keyedMutex.GetAddressOf())))) {
                    if (keyedMutex->AcquireSync(0, INFINITE) != S_OK) {
                        ReleaseLocks(locked);
                        return false;
                    }
                    locked.push_back(std::move(keyedMutex));
                }
            }
        }
        return true;
    }

    static void ReleaseLocks(std::vector<ComPtr<IDXGIKeyedMutex>>& locked) {
        for (auto& mutex : locked) mutex->ReleaseSync(0);
        locked.clear();
    }

    static void UnbindSource(ID3D11DeviceContext* context) {
        ID3D11ShaderResourceView* empty = nullptr;
        ID3D11RenderTargetView* noTarget = nullptr;
        context->PSSetShaderResources(0, 1, &empty);
        context->OMSetRenderTargets(1, &noTarget, nullptr);
    }

    bool DrawLayer(ID3D11DeviceContext* context, ID3D11Texture2D* texture,
                   const AlvrOpenXrDrawDesc& layer) {
        const auto key = std::make_tuple(
            reinterpret_cast<std::uintptr_t>(layer.sharedHandle), layer.sharedType,
            layer.format, layer.arraySlice);
        auto found = m_shaderViews.find(key);
        if (found == m_shaderViews.end()) {
            D3D11_TEXTURE2D_DESC textureDesc = {};
            texture->GetDesc(&textureDesc);
            if (layer.arraySlice >= textureDesc.ArraySize) return false;
            D3D11_SHADER_RESOURCE_VIEW_DESC viewDesc = {};
            viewDesc.Format = static_cast<DXGI_FORMAT>(layer.format);
            if (textureDesc.ArraySize > 1) {
                viewDesc.ViewDimension = D3D11_SRV_DIMENSION_TEXTURE2DARRAY;
                viewDesc.Texture2DArray.MostDetailedMip = 0;
                viewDesc.Texture2DArray.MipLevels = 1;
                viewDesc.Texture2DArray.FirstArraySlice = layer.arraySlice;
                viewDesc.Texture2DArray.ArraySize = 1;
            } else {
                viewDesc.ViewDimension = D3D11_SRV_DIMENSION_TEXTURE2D;
                viewDesc.Texture2D.MostDetailedMip = 0;
                viewDesc.Texture2D.MipLevels = 1;
            }
            ComPtr<ID3D11ShaderResourceView> view;
            if (FAILED(m_device->CreateShaderResourceView(
                    texture, &viewDesc, view.GetAddressOf()))) {
                return false;
            }
            found = m_shaderViews.emplace(
                key, ShaderViewEntry { std::move(view), textureDesc.ArraySize > 1 }).first;
        }
        const float u0 = layer.uvBounds[0], u1 = layer.uvBounds[1];
        const float v0 = layer.uvBounds[2], v1 = layer.uvBounds[3];
        const auto vertex = [&](int corner, float u, float v) {
            Vertex result = {};
            memcpy(result.position, &layer.clipVertices[corner * 4], sizeof(float) * 4);
            result.uv[0] = u0 + (u1 - u0) * u;
            result.uv[1] = v0 + (v1 - v0) * v;
            return result;
        };
        const Vertex vertices[6] = {
            vertex(0, 0, 0), vertex(1, 1, 0), vertex(2, 0, 1),
            vertex(2, 0, 1), vertex(1, 1, 0), vertex(3, 1, 1),
        };
        D3D11_MAPPED_SUBRESOURCE mapped = {};
        if (FAILED(context->Map(m_vertices.Get(), 0, D3D11_MAP_WRITE_DISCARD, 0, &mapped))) {
            return false;
        }
        memcpy(mapped.pData, vertices, sizeof(vertices));
        context->Unmap(m_vertices.Get(), 0);
        const UINT stride = sizeof(Vertex), offset = 0;
        ID3D11Buffer* buffer = m_vertices.Get();
        context->IASetVertexBuffers(0, 1, &buffer, &stride, &offset);
        ID3D11ShaderResourceView* shaderView = found->second.view.Get();
        context->PSSetShaderResources(0, 1, &shaderView);
        context->PSSetShader(
            found->second.isArray ? m_arrayPixelShader.Get() : m_pixelShader.Get(),
            nullptr, 0
        );
        if (layer.alphaMode == 1) {
            context->OMSetBlendState(m_premultiplied.Get(), nullptr, 0xffffffff);
        } else if (layer.alphaMode == 2) {
            context->OMSetBlendState(m_unpremultiplied.Get(), nullptr, 0xffffffff);
        } else {
            context->OMSetBlendState(nullptr, nullptr, 0xffffffff);
        }
        context->Draw(6, 0);
        return true;
    }

    ID3D11Device* m_device = nullptr;
    bool m_loggedFirstFrame = false;
    uint32_t m_width[2] = {}, m_height[2] = {};
    std::unordered_map<void*, ComPtr<ID3D11Texture2D>> m_sharedTextures;
    struct ShaderViewEntry {
        ComPtr<ID3D11ShaderResourceView> view;
        bool isArray;
    };
    using ShaderViewKey = std::tuple<std::uintptr_t, uint32_t, int64_t, uint32_t>;
    std::map<ShaderViewKey, ShaderViewEntry> m_shaderViews;
    ComPtr<ID3D11VertexShader> m_vertexShader;
    ComPtr<ID3D11PixelShader> m_pixelShader;
    ComPtr<ID3D11PixelShader> m_arrayPixelShader;
    ComPtr<ID3D11InputLayout> m_inputLayout;
    ComPtr<ID3D11Buffer> m_vertices;
    ComPtr<ID3D11SamplerState> m_sampler;
    ComPtr<ID3D11BlendState> m_premultiplied, m_unpremultiplied;
    ComPtr<ID3D11RasterizerState> m_rasterizer;
    ComPtr<ID3D11Texture2D> m_eyeTextures[2];
    ComPtr<ID3D11RenderTargetView> m_targets[2];
};

static OpenXrCompositor g_openxrCompositor;

// Start the video encoder on its own D3D11 device. Must be called only after
// settings are populated (FrameRender reads Settings_Instance for resolution).
extern "C" bool alvr_openxr_encoder_start(uint32_t adapterIndex) {
    std::lock_guard<std::mutex> lock(g_encoderMutex);
    if (g_encoderStarted) {
        return true;
    }

    // /EHa lets catch(...) trap access violations from the reused C++ pipeline.
    try {
        g_render = std::make_shared<CD3DRender>();
        if (!g_render->Initialize(adapterIndex)) {
            fprintf(stderr, "[alvr] encoder_start FAIL: CD3DRender::Initialize(%u)\n", adapterIndex);
            fflush(stderr);
            g_render->Shutdown();
            g_render.reset();
            return false;
        }

        g_encoder = std::make_shared<CEncoder>();
        g_encoder->Initialize(g_render);

        if (!g_openxrCompositor.Initialize(g_render->GetDevice())) {
            fprintf(stderr, "[alvr] encoder_start FAIL: D3D11 compositor initialization\n");
            fflush(stderr);
            g_encoder.reset();
            g_openxrCompositor.Shutdown();
            g_render->Shutdown();
            g_render.reset();
            return false;
        }

        g_encoder->OnStreamStart();
        g_encoder->Start();
        g_encoderStarted = true;
    } catch (const std::exception& e) {
        fprintf(stderr, "[alvr] encoder_start FAIL: exception: %s\n", e.what());
        fflush(stderr);
        g_encoder.reset();
        g_openxrCompositor.Shutdown();
        if (g_render) g_render->Shutdown();
        g_render.reset();
    } catch (...) {
        fprintf(stderr, "[alvr] encoder_start FAIL: caught SEH/unknown fault during init\n");
        fflush(stderr);
        g_encoder.reset();
        g_openxrCompositor.Shutdown();
        if (g_render) g_render->Shutdown();
        g_render.reset();
    }
    return g_encoderStarted;
}

extern "C" void alvr_openxr_encoder_stop() {
    std::lock_guard<std::mutex> lock(g_encoderMutex);
    if (g_encoder) {
        g_encoder->Stop();
        g_encoder.reset();
    }
    g_openxrCompositor.Shutdown();
    if (g_render) g_render->Shutdown();
    g_render.reset();
    g_encoderStarted = false;
}

extern "C" void alvr_openxr_encoder_forget_texture(void* sharedHandle) {
    std::lock_guard<std::mutex> lock(g_encoderMutex);
    g_openxrCompositor.ForgetTexture(sharedHandle);
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

// Draw all accepted OpenXR composition layers into the two typed sRGB eye
// targets, then pass the composed stereo projection through the existing encoder.
extern "C" void alvr_openxr_encoder_submit_layers(
    const AlvrOpenXrDrawDesc* layersLeft,
    uint32_t layerCountLeft,
    const AlvrOpenXrDrawDesc* layersRight,
    uint32_t layerCountRight,
    uint32_t widthLeft,
    uint32_t heightLeft,
    uint32_t widthRight,
    uint32_t heightRight,
    const float fovLeft[4],
    const float fovRight[4],
    const float eyeToHeadLeft[3],
    const float eyeToHeadRight[3],
    uint64_t timestampNs,
    bool idr
) {
    std::lock_guard<std::mutex> lock(g_encoderMutex);
    if (!g_encoderStarted || !g_encoder || !g_render) {
        static int warned = 0;
        if (warned++ < 3) {
            fprintf(stderr, "[alvr] submit DROP: encoder not started (started=%d)\n",
                    g_encoderStarted ? 1 : 0);
            fflush(stderr);
        }
        return;
    }

    // Match the encoder's own context serialization before the compositor uses
    // that same immediate context for its layer draws.
    auto phaseStart = OpenXrClock::now();
    g_encoder->WaitForEncode();
    const double waitEncodeMs = openxr_elapsed_ms(phaseStart);
    ID3D11Texture2D* composed[2] = {};
    phaseStart = OpenXrClock::now();
    const bool composeSucceeded = g_openxrCompositor.Compose(
            layersLeft, layerCountLeft, layersRight, layerCountRight,
            widthLeft, heightLeft, widthRight, heightRight, composed);
    const double composeMs = openxr_elapsed_ms(phaseStart);
    if (!composeSucceeded || !composed[0] || !composed[1]) {
        static int warned = 0;
        if (warned++ < 3) {
            fprintf(stderr, "[alvr] submit DROP: OpenXR layer composition failed\n");
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

    ID3D11Texture2D* textures[1][2] = { { composed[0], composed[1] } };
    vr::VRTextureBounds_t bounds[1][2] = {
        { { 0.0f, 0.0f, 1.0f, 1.0f }, { 0.0f, 0.0f, 1.0f, 1.0f } }
    };
    vr::HmdMatrix34_t poses[1] = { identity };

    if (idr) {
        g_encoder->InsertIDR();
    }

    // The layer compositor has produced one projection image per eye; retain
    // the existing view/FOV reprojection and encoder path from this point on.
    phaseStart = OpenXrClock::now();
    g_encoder->CopyToStaging(
        textures, bounds, poses, 1, false, 0, timestampNs, "", ""
    );
    const double encoderStageMs = openxr_elapsed_ms(phaseStart);
    phaseStart = OpenXrClock::now();
    g_render->GetContext()->Flush();
    const double flushMs = openxr_elapsed_ms(phaseStart);
    g_encoder->NewFrameReady();
    record_openxr_submit_timing(waitEncodeMs, composeMs, encoderStageMs, flushMs);

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

// Called only by the ignored Windows GPU unit test, not the runtime dispatch table.
extern "C" bool alvr_openxr_test_compositor(uint32_t adapterIndex) {
    std::lock_guard<std::mutex> lock(g_encoderMutex);
    if (g_render || g_encoder || g_encoderStarted) {
        fprintf(stderr, "[alvr] compositor GPU test requires an idle encoder\n");
        return false;
    }

    CD3DRender producer;
    OpenXrCompositor compositor;
    g_render = std::make_shared<CD3DRender>();
    const bool success = [&]() {
        const auto fail = [](const char* step) {
            fprintf(stderr, "[alvr] compositor GPU test FAIL: %s\n", step);
            return false;
        };
        if (!producer.Initialize(adapterIndex) || !g_render->Initialize(adapterIndex)
            || !compositor.Initialize(g_render->GetDevice())) {
            return fail("initialize producer/consumer devices and compositor");
        }
        int32_t selectedIndex = -1;
        std::wstring adapterName;
        if (g_render->GetAdapterInfo(&selectedIndex, adapterName)) {
            fprintf(stderr, "[alvr] compositor GPU test adapter %d: %ls\n",
                    selectedIndex, adapterName.c_str());
        }

        struct SharedSource {
            ComPtr<ID3D11Texture2D> texture;
            HANDLE handle = nullptr;
            void Reset() {
                if (handle) CloseHandle(handle);
                handle = nullptr;
                texture.Reset();
            }
            ~SharedSource() { Reset(); }
        };
        const auto createSource = [&](SharedSource& source, UINT width, UINT height,
                                      UINT arraySize, const std::vector<uint8_t>& pixels) {
            D3D11_TEXTURE2D_DESC desc = {};
            desc.Width = width;
            desc.Height = height;
            desc.ArraySize = arraySize;
            desc.MipLevels = 1;
            desc.Format = DXGI_FORMAT_R8G8B8A8_TYPELESS;
            desc.SampleDesc.Count = 1;
            desc.Usage = D3D11_USAGE_DEFAULT;
            desc.BindFlags = D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE;
            desc.MiscFlags = D3D11_RESOURCE_MISC_SHARED_NTHANDLE
                | D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX;
            if (FAILED(producer.GetDevice()->CreateTexture2D(
                    &desc, nullptr, source.texture.GetAddressOf()))) {
                return fail("create typeless NT-shared source");
            }
            ComPtr<IDXGIResource1> resource;
            ComPtr<IDXGIKeyedMutex> mutex;
            if (FAILED(source.texture.As(&resource)) || FAILED(source.texture.As(&mutex))
                || FAILED(resource->CreateSharedHandle(
                    nullptr, DXGI_SHARED_RESOURCE_READ | DXGI_SHARED_RESOURCE_WRITE,
                    nullptr, &source.handle))
                || mutex->AcquireSync(0, 1000) != S_OK) {
                return fail("share and acquire source texture");
            }
            for (UINT layer = 0; layer < arraySize; ++layer) {
                producer.GetContext()->UpdateSubresource(
                    source.texture.Get(), layer, nullptr,
                    pixels.data() + static_cast<size_t>(layer) * width * height * 4,
                    width * 4, 0);
            }
            producer.GetContext()->Flush();
            if (mutex->ReleaseSync(0) != S_OK) return fail("release producer mutex");
            return true;
        };
        const auto solidPixels = [](size_t count, uint8_t r, uint8_t g, uint8_t b,
                                    uint8_t a = 255) {
            std::vector<uint8_t> pixels(count * 4);
            for (size_t i = 0; i < count; ++i) {
                pixels[i * 4] = r;
                pixels[i * 4 + 1] = g;
                pixels[i * 4 + 2] = b;
                pixels[i * 4 + 3] = a;
            }
            return pixels;
        };
        const auto descriptor = [](const SharedSource& source, uint32_t slice = 0) {
            AlvrOpenXrDrawDesc result = {};
            result.sharedHandle = source.handle;
            result.sharedType = 1;
            result.format = DXGI_FORMAT_R8G8B8A8_UNORM_SRGB;
            result.arraySlice = slice;
            result.uvBounds[1] = result.uvBounds[3] = 1;
            const float clip[] = {
                -1, 1, 0.5f, 1, 1, 1, 0.5f, 1, -1, -1, 0.5f, 1, 1, -1, 0.5f, 1
            };
            memcpy(result.clipVertices, clip, sizeof(clip));
            return result;
        };
        const auto checkPixels = [&](ID3D11Texture2D* texture,
                                     const std::vector<uint8_t>& expected,
                                     const char* label, int eye) {
            D3D11_TEXTURE2D_DESC desc = {};
            texture->GetDesc(&desc);
            desc.Usage = D3D11_USAGE_STAGING;
            desc.BindFlags = desc.MiscFlags = 0;
            desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
            ComPtr<ID3D11Texture2D> staging;
            if (expected.size() != static_cast<size_t>(desc.Width) * desc.Height * 4
                || FAILED(g_render->GetDevice()->CreateTexture2D(
                    &desc, nullptr, staging.GetAddressOf()))) {
                return fail("create readback texture");
            }
            auto context = g_render->GetContext();
            context->CopyResource(staging.Get(), texture);
            D3D11_MAPPED_SUBRESOURCE mapped = {};
            if (FAILED(context->Map(staging.Get(), 0, D3D11_MAP_READ, 0, &mapped))) {
                return fail("map composed eye pixels");
            }
            bool matches = true;
            for (UINT y = 0; y < desc.Height && matches; ++y) {
                const auto* row = static_cast<const uint8_t*>(mapped.pData) + y * mapped.RowPitch;
                for (UINT x = 0; x < desc.Width * 4; ++x) {
                    const auto wanted = expected[static_cast<size_t>(y) * desc.Width * 4 + x];
                    const int difference = static_cast<int>(row[x]) - wanted;
                    if (difference < -2 || difference > 2) {
                        fprintf(stderr,
                                "[alvr] compositor GPU test FAIL: %s eye=%d pixel=%u,%u "
                                "channel=%u actual=%u expected=%u\n",
                                label, eye, x / 4, y, x % 4, row[x], wanted);
                        matches = false;
                        break;
                    }
                }
            }
            if (!matches && desc.Width <= 4 && desc.Height <= 4) {
                for (UINT y = 0; y < desc.Height; ++y) {
                    const auto* row = static_cast<const uint8_t*>(mapped.pData) + y * mapped.RowPitch;
                    fprintf(stderr, "[alvr] compositor GPU test actual row %u:", y);
                    for (UINT x = 0; x < desc.Width; ++x) {
                        fprintf(stderr, " (%u,%u,%u,%u)", row[x * 4], row[x * 4 + 1],
                                row[x * 4 + 2], row[x * 4 + 3]);
                    }
                    fprintf(stderr, "\n");
                }
            }
            context->Unmap(staging.Get(), 0);
            return matches;
        };
        const auto compose = [&](const AlvrOpenXrDrawDesc* left, UINT leftCount,
                                 const AlvrOpenXrDrawDesc* right, UINT rightCount,
                                 UINT size, const std::vector<uint8_t>& expectedLeft,
                                 const std::vector<uint8_t>& expectedRight, const char* label) {
            ID3D11Texture2D* outputs[2] = {};
            if (!compositor.Compose(left, leftCount, right, rightCount,
                                    size, size, size, size, outputs)) {
                return fail(label);
            }
            return checkPixels(outputs[0], expectedLeft, label, 0)
                && checkPixels(outputs[1], expectedRight, label, 1);
        };

        std::vector<uint8_t> quadrants(4 * 4 * 4);
        const uint8_t colors[4][4] = {
            { 255, 0, 0, 255 }, { 0, 255, 0, 255 },
            { 0, 0, 255, 255 }, { 255, 255, 255, 255 }
        };
        for (UINT y = 0; y < 4; ++y) {
            for (UINT x = 0; x < 4; ++x) {
                memcpy(&quadrants[(y * 4 + x) * 4], colors[(y / 2) * 2 + x / 2], 4);
            }
        }
        SharedSource pattern;
        if (!createSource(pattern, 4, 4, 1, quadrants)) return false;
        auto* importedPattern = compositor.GetSharedTexture(pattern.handle, 1);
        ComPtr<IDXGIKeyedMutex> sourceMutex;
        if (!importedPattern || FAILED(importedPattern->QueryInterface(
                IID_PPV_ARGS(sourceMutex.GetAddressOf())))
            || sourceMutex->AcquireSync(0, 1000) != S_OK) {
            return fail("acquire imported pattern for source readback");
        }
        const bool sourceMatches = checkPixels(importedPattern, quadrants, "shared source bytes", 0);
        sourceMutex->ReleaseSync(0);
        if (!sourceMatches) return false;
        auto draw = descriptor(pattern);
        for (int repeat = 0; repeat < 3; ++repeat) {
            if (!compose(&draw, 1, &draw, 1, 4, quadrants, quadrants,
                         "same keyed source in both eyes, repeat and orientation")) return false;
        }
        draw.uvBounds[0] = 0.5f;
        draw.uvBounds[3] = 0.5f;
        const auto green = solidPixels(4, 0, 255, 0);
        if (!compose(&draw, 1, &draw, 1, 2, green, green, "top-right UV crop")) return false;

        SharedSource array;
        const auto red = solidPixels(4, 255, 0, 0);
        const auto blue = solidPixels(4, 0, 0, 255);
        auto arrayPixels = red;
        arrayPixels.insert(arrayPixels.end(), blue.begin(), blue.end());
        if (!createSource(array, 2, 2, 2, arrayPixels)) return false;
        auto leftSlice = descriptor(array, 0), rightSlice = descriptor(array, 1);
        if (!compose(&leftSlice, 1, &rightSlice, 1, 2, red, blue, "different array slices")) {
            return false;
        }

        SharedSource transparent, base, straight, premultiplied;
        const auto transparentGreen = solidPixels(4, 0, 255, 0, 0);
        if (!createSource(transparent, 2, 2, 1, transparentGreen)
            || !createSource(base, 2, 2, 1, red)
            || !createSource(straight, 2, 2, 1, solidPixels(4, 0, 255, 0, 128))
            || !createSource(premultiplied, 2, 2, 1, solidPixels(4, 0, 188, 0, 128))) {
            return false;
        }
        draw = descriptor(transparent);
        if (!compose(&draw, 1, &draw, 1, 2, transparentGreen, transparentGreen,
                     "opaque layer ignores source alpha")) return false;
        AlvrOpenXrDrawDesc layers[] = { descriptor(base), descriptor(straight) };
        layers[1].alphaMode = 2;
        const auto mixed = solidPixels(4, 187, 188, 0);
        if (!compose(layers, 2, layers, 2, 2, mixed, mixed, "straight alpha in linear color")) {
            return false;
        }
        layers[1] = descriptor(premultiplied);
        layers[1].alphaMode = 1;
        if (!compose(layers, 2, layers, 2, 2, mixed, mixed, "premultiplied alpha")) return false;

        compositor.ForgetTexture(pattern.handle);
        pattern.Reset();
        const auto yellow = solidPixels(16, 255, 255, 0);
        if (!createSource(pattern, 4, 4, 1, yellow)) return false;
        draw = descriptor(pattern);
        if (!compose(&draw, 1, &draw, 1, 4, yellow, yellow, "recreated source and targets")) {
            return false;
        }
        compositor.Shutdown();
        if (!compositor.Initialize(g_render->GetDevice())) return fail("reinitialize compositor");
        return compose(&draw, 1, &draw, 1, 4, yellow, yellow, "recreated compositor session");
    }();
    compositor.Shutdown();
    producer.Shutdown();
    g_render->Shutdown();
    g_render.reset();
    fprintf(stderr, "[alvr] compositor GPU test %s (adapter %u)\n",
            success ? "PASS" : "FAIL", adapterIndex);
    fflush(stderr);
    return success;
}
