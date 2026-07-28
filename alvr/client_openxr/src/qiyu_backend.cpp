#include "alvr_client_core_ffi.h"

#include <cstring>
#include <string.h>
#include <vector>

#include "QiyuApi.h"

#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES3/gl3.h>
#include <android/log.h>
#include <android/native_window.h>
#include <android/native_window_jni.h>
#include <dlfcn.h>
#include <jni.h>

#include <algorithm>
#include <array>
#include <atomic>
#include <chrono>
#include <cmath>
#include <cstdio>
#include <cstdarg>
#include <cstdint>
#include <deque>
#include <map>
#include <mutex>
#include <thread>
#include <utility>

namespace {

constexpr int kEyeCount = 2;
constexpr int kSwapchainLength = 3;
constexpr size_t kMaxTrackingFrames = 360;
constexpr float kButtonEpsilon = 0.001f;
constexpr float kDefaultRefreshRate = 72.0f;

void log(int priority, const char *format, ...);

struct QiyuApi {
    void *library = nullptr;
    decltype(&qiyu_Init) init = nullptr;
    decltype(&qiyu_Release) release = nullptr;
    decltype(&qiyu_StartVR) start_vr = nullptr;
    decltype(&qiyu_EndVR) end_vr = nullptr;
    decltype(&qiyu_StartEye) start_eye = nullptr;
    decltype(&qiyu_EndEye) end_eye = nullptr;
    decltype(&qiyu_SubmitFrame) submit_frame = nullptr;
    decltype(&qiyu_Update) update = nullptr;
    decltype(&qiyu_GetDeviceInfo) get_device_info = nullptr;
    decltype(&qiyu_GetViewMatrix) get_view_matrix = nullptr;
    decltype(&qiyu_PredictDisplayTime) predict_display_time = nullptr;
    decltype(&qiyu_PredictHeadPose) predict_head_pose = nullptr;
    decltype(&qiyu_PostSetEyeBufferSize) post_eye_buffer_size = nullptr;
    decltype(&qiyu_SetTrackingOriginMode) set_tracking_origin = nullptr;
    decltype(&qiyu_IsControllerInit) is_controller_initialized = nullptr;
    decltype(&qiyu_GetControllerData) get_controller_data = nullptr;
    decltype(&qiyu_StartControllerVibration) start_vibration = nullptr;
    decltype(&qiyu_StopControllerVibration) stop_vibration = nullptr;
    decltype(&qiyu_GetBoundaryDimensions) get_boundary_dimensions = nullptr;

    template <typename T>
    bool load_symbol(T &function, const char *name) {
        function = reinterpret_cast<T>(dlsym(library, name));
        if (function == nullptr) {
            log(ANDROID_LOG_ERROR, "Missing QiYu SDK symbol %s: %s", name, dlerror());
            return false;
        }
        return true;
    }

    bool load() {
        library = dlopen("libqiyuapi.so", RTLD_NOW | RTLD_LOCAL);
        if (library == nullptr) {
            log(ANDROID_LOG_ERROR, "Cannot load libqiyuapi.so: %s", dlerror());
            return false;
        }
#define LOAD_QIYU(field, symbol) load_symbol(field, #symbol)
        return LOAD_QIYU(init, qiyu_Init) &&
               LOAD_QIYU(release, qiyu_Release) &&
               LOAD_QIYU(start_vr, qiyu_StartVR) &&
               LOAD_QIYU(end_vr, qiyu_EndVR) &&
               LOAD_QIYU(start_eye, qiyu_StartEye) &&
               LOAD_QIYU(end_eye, qiyu_EndEye) &&
               LOAD_QIYU(submit_frame, qiyu_SubmitFrame) &&
               LOAD_QIYU(update, qiyu_Update) &&
               LOAD_QIYU(get_device_info, qiyu_GetDeviceInfo) &&
               LOAD_QIYU(get_view_matrix, qiyu_GetViewMatrix) &&
               LOAD_QIYU(predict_display_time, qiyu_PredictDisplayTime) &&
               LOAD_QIYU(predict_head_pose, qiyu_PredictHeadPose) &&
               LOAD_QIYU(post_eye_buffer_size, qiyu_PostSetEyeBufferSize) &&
               LOAD_QIYU(set_tracking_origin, qiyu_SetTrackingOriginMode) &&
               LOAD_QIYU(is_controller_initialized, qiyu_IsControllerInit) &&
               LOAD_QIYU(get_controller_data, qiyu_GetControllerData) &&
               LOAD_QIYU(start_vibration, qiyu_StartControllerVibration) &&
               LOAD_QIYU(stop_vibration, qiyu_StopControllerVibration) &&
               LOAD_QIYU(get_boundary_dimensions, qiyu_GetBoundaryDimensions);
#undef LOAD_QIYU
    }

    void unload() {
        if (library != nullptr) {
            dlclose(library);
            library = nullptr;
        }
    }
};

QiyuApi api;

void log(int priority, const char *format, ...) {
    va_list args;
    va_start(args, format);
    __android_log_vprint(priority, "ALVR-QIYU", format, args);
    va_end(args);
}

struct EglState {
    EGLDisplay display = EGL_NO_DISPLAY;
    EGLContext context = EGL_NO_CONTEXT;
    EGLSurface surface = EGL_NO_SURFACE;
    EGLConfig config = nullptr;
};

struct EyeBuffers {
    std::array<GLuint, kSwapchainLength> textures{};
    uint32_t index = 0;
};

struct InputIds {
    uint64_t head = 0;
    uint64_t hands[kEyeCount]{};
    uint64_t haptics[kEyeCount]{};
    uint64_t profile = 0;
    uint64_t menu = 0;
    uint64_t system = 0;
    uint64_t face_click[kEyeCount][2]{};
    uint64_t face_touch[kEyeCount][2]{};
    uint64_t squeeze_click[kEyeCount]{};
    uint64_t squeeze_value[kEyeCount]{};
    uint64_t trigger_click[kEyeCount]{};
    uint64_t trigger_value[kEyeCount]{};
    uint64_t trigger_touch[kEyeCount]{};
    uint64_t stick_x[kEyeCount]{};
    uint64_t stick_y[kEyeCount]{};
    uint64_t stick_click[kEyeCount]{};
    uint64_t stick_touch[kEyeCount]{};
};

struct State {
    JavaVM *java_vm = nullptr;
    jobject activity = nullptr;
    EglState egl;
    ANativeWindow *window = nullptr;
    std::atomic_bool initialized = false;
    std::atomic_bool running = false;
    std::atomic_bool starting = false;
    std::atomic_bool streaming = false;
    std::atomic<float> refresh_rate = kDefaultRefreshRate;
    std::atomic<float> tracking_offset = 0.0f;
    std::thread tracking_thread;
    EyeBuffers lobby[kEyeCount];
    EyeBuffers stream[kEyeCount];
    uint32_t view_width = 1;
    uint32_t view_height = 1;
    uint32_t stream_width = 1;
    uint32_t stream_height = 1;
    uint64_t last_frame_time_ns = 0;
    bool decoder_initialized = false;
    int decoder_codec = -1;
    bool controller_connected[kEyeCount]{};
    int last_controller_battery[kEyeCount] = {-1, -1};
    InputIds ids;
    std::map<uint64_t, AlvrButtonValue> button_states;
    std::deque<std::pair<uint64_t, qiyu_HeadPoseState>> tracking_frames;
    std::mutex tracking_mutex;
};

State state;

uint64_t clock_ns() {
    timespec time{};
    clock_gettime(CLOCK_BOOTTIME, &time);
    return uint64_t(time.tv_sec) * 1'000'000'000ULL + uint64_t(time.tv_nsec);
}

bool initialize_egl() {
    state.egl.display = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    if (state.egl.display == EGL_NO_DISPLAY ||
        !eglInitialize(state.egl.display, nullptr, nullptr)) {
        log(ANDROID_LOG_ERROR, "eglInitialize failed: 0x%x", eglGetError());
        return false;
    }

    constexpr EGLint attributes[] = {
        EGL_RENDERABLE_TYPE, EGL_OPENGL_ES3_BIT_KHR,
        EGL_SURFACE_TYPE, EGL_PBUFFER_BIT | EGL_WINDOW_BIT,
        EGL_RED_SIZE, 8,
        EGL_GREEN_SIZE, 8,
        EGL_BLUE_SIZE, 8,
        EGL_ALPHA_SIZE, 8,
        EGL_DEPTH_SIZE, 0,
        EGL_STENCIL_SIZE, 0,
        EGL_SAMPLES, 0,
        EGL_NONE,
    };
    EGLint config_count = 0;
    if (!eglChooseConfig(state.egl.display, attributes, &state.egl.config, 1,
                         &config_count) || config_count == 0) {
        log(ANDROID_LOG_ERROR, "eglChooseConfig failed: 0x%x", eglGetError());
        return false;
    }

    constexpr EGLint context_attributes[] = {EGL_CONTEXT_CLIENT_VERSION, 3, EGL_NONE};
    state.egl.context = eglCreateContext(state.egl.display, state.egl.config,
                                         EGL_NO_CONTEXT, context_attributes);
    constexpr EGLint surface_attributes[] = {EGL_WIDTH, 16, EGL_HEIGHT, 16, EGL_NONE};
    state.egl.surface = eglCreatePbufferSurface(state.egl.display, state.egl.config,
                                                surface_attributes);
    if (state.egl.context == EGL_NO_CONTEXT || state.egl.surface == EGL_NO_SURFACE ||
        !eglMakeCurrent(state.egl.display, state.egl.surface, state.egl.surface,
                        state.egl.context)) {
        log(ANDROID_LOG_ERROR, "EGL context creation failed: 0x%x", eglGetError());
        return false;
    }

    return true;
}

void destroy_egl() {
    if (state.egl.display == EGL_NO_DISPLAY) {
        return;
    }
    eglMakeCurrent(state.egl.display, EGL_NO_SURFACE, EGL_NO_SURFACE, EGL_NO_CONTEXT);
    if (state.egl.surface != EGL_NO_SURFACE) {
        eglDestroySurface(state.egl.display, state.egl.surface);
    }
    if (state.egl.context != EGL_NO_CONTEXT) {
        eglDestroyContext(state.egl.display, state.egl.context);
    }
    eglTerminate(state.egl.display);
    state.egl = {};
}

bool create_buffers(EyeBuffers (&buffers)[kEyeCount], uint32_t width, uint32_t height) {
    for (auto &eye : buffers) {
        glGenTextures(kSwapchainLength, eye.textures.data());
        eye.index = 0;
        for (GLuint texture : eye.textures) {
            glBindTexture(GL_TEXTURE_2D, texture);
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_LINEAR);
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_LINEAR);
            glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, static_cast<GLsizei>(width),
                         static_cast<GLsizei>(height), 0, GL_RGBA,
                         GL_UNSIGNED_BYTE, nullptr);
        }
    }
    glBindTexture(GL_TEXTURE_2D, 0);
    const auto error = glGetError();
    if (error != GL_NO_ERROR) {
        log(ANDROID_LOG_ERROR, "Eye texture creation failed: 0x%x", error);
        return false;
    }
    return true;
}

void destroy_buffers(EyeBuffers (&buffers)[kEyeCount]) {
    for (auto &eye : buffers) {
        glDeleteTextures(kSwapchainLength, eye.textures.data());
        eye = {};
    }
}

AlvrFov get_fov(const qiyu_DeviceInfo &device, int eye) {
    const auto &frustum = eye == 0 ? device.frustumLeftEye : device.frustumRightEye;
    return {
        std::atan(frustum.left / frustum.near),
        std::atan(frustum.right / frustum.near),
        std::atan(frustum.top / frustum.near),
        std::atan(frustum.bottom / frustum.near),
    };
}

float get_ipd(const qiyu_DeviceInfo &device) {
    const auto &left = device.frustumLeftEye.position;
    const auto &right = device.frustumRightEye.position;
    const float x = right.x - left.x;
    const float y = right.y - left.y;
    const float z = right.z - left.z;
    return std::sqrt(x * x + y * y + z * z);
}

AlvrQuat head_orientation(const qiyu_Quaternion &rotation) {
    return {rotation.x, rotation.y, rotation.z, -rotation.w};
}

AlvrPose head_pose(const qiyu_HeadPoseState &tracking) {
    AlvrPose pose{};
    pose.orientation = head_orientation(tracking.pose.rotation);
    pose.position[0] = -tracking.pose.position.x;
    pose.position[1] = -tracking.pose.position.y - state.tracking_offset.load();
    pose.position[2] = -tracking.pose.position.z;
    return pose;
}

float matrix_minor(const qiyu_Matrix4 &matrix, int r0, int r1, int r2,
                   int c0, int c1, int c2) {
    return matrix.M[r0][c0] *
               (matrix.M[r1][c1] * matrix.M[r2][c2] -
                matrix.M[r2][c1] * matrix.M[r1][c2]) -
           matrix.M[r0][c1] *
               (matrix.M[r1][c0] * matrix.M[r2][c2] -
                matrix.M[r2][c0] * matrix.M[r1][c2]) +
           matrix.M[r0][c2] *
               (matrix.M[r1][c0] * matrix.M[r2][c1] -
                matrix.M[r2][c0] * matrix.M[r1][c1]);
}

qiyu_Matrix4 matrix_inverse(const qiyu_Matrix4 &matrix) {
    const float inverse_determinant =
        1.0f / (matrix.M[0][0] * matrix_minor(matrix, 1, 2, 3, 1, 2, 3) -
                matrix.M[0][1] * matrix_minor(matrix, 1, 2, 3, 0, 2, 3) +
                matrix.M[0][2] * matrix_minor(matrix, 1, 2, 3, 0, 1, 3) -
                matrix.M[0][3] * matrix_minor(matrix, 1, 2, 3, 0, 1, 2));
    qiyu_Matrix4 output{};
    output.M[0][0] = matrix_minor(matrix, 1, 2, 3, 1, 2, 3) * inverse_determinant;
    output.M[0][1] = -matrix_minor(matrix, 0, 2, 3, 1, 2, 3) * inverse_determinant;
    output.M[0][2] = matrix_minor(matrix, 0, 1, 3, 1, 2, 3) * inverse_determinant;
    output.M[0][3] = -matrix_minor(matrix, 0, 1, 2, 1, 2, 3) * inverse_determinant;
    output.M[1][0] = -matrix_minor(matrix, 1, 2, 3, 0, 2, 3) * inverse_determinant;
    output.M[1][1] = matrix_minor(matrix, 0, 2, 3, 0, 2, 3) * inverse_determinant;
    output.M[1][2] = -matrix_minor(matrix, 0, 1, 3, 0, 2, 3) * inverse_determinant;
    output.M[1][3] = matrix_minor(matrix, 0, 1, 2, 0, 2, 3) * inverse_determinant;
    output.M[2][0] = matrix_minor(matrix, 1, 2, 3, 0, 1, 3) * inverse_determinant;
    output.M[2][1] = -matrix_minor(matrix, 0, 2, 3, 0, 1, 3) * inverse_determinant;
    output.M[2][2] = matrix_minor(matrix, 0, 1, 3, 0, 1, 3) * inverse_determinant;
    output.M[2][3] = -matrix_minor(matrix, 0, 1, 2, 0, 1, 3) * inverse_determinant;
    output.M[3][0] = -matrix_minor(matrix, 1, 2, 3, 0, 1, 2) * inverse_determinant;
    output.M[3][1] = matrix_minor(matrix, 0, 2, 3, 0, 1, 2) * inverse_determinant;
    output.M[3][2] = -matrix_minor(matrix, 0, 1, 3, 0, 1, 2) * inverse_determinant;
    output.M[3][3] = matrix_minor(matrix, 0, 1, 2, 0, 1, 2) * inverse_determinant;
    return output;
}

void fill_layout(qiyu_RenderLayer_ScreenPosUV &layout) {
    constexpr float lower_left[4] = {-1.0f, -1.0f, 0.0f, 1.0f};
    constexpr float lower_right[4] = {1.0f, -1.0f, 0.0f, 1.0f};
    constexpr float upper_left[4] = {-1.0f, 1.0f, 0.0f, 1.0f};
    constexpr float upper_right[4] = {1.0f, 1.0f, 0.0f, 1.0f};
    constexpr float lower_uv[4] = {0.0f, 0.0f, 1.0f, 0.0f};
    constexpr float upper_uv[4] = {0.0f, 1.0f, 1.0f, 1.0f};
    std::memcpy(layout.LowerLeftPos, lower_left, sizeof(lower_left));
    std::memcpy(layout.LowerRightPos, lower_right, sizeof(lower_right));
    std::memcpy(layout.UpperLeftPos, upper_left, sizeof(upper_left));
    std::memcpy(layout.UpperRightPos, upper_right, sizeof(upper_right));
    std::memcpy(layout.LowerUVs, lower_uv, sizeof(lower_uv));
    std::memcpy(layout.UpperUVs, upper_uv, sizeof(upper_uv));
}

void initialize_ids() {
    auto id = [](const char *path) { return alvr_path_string_to_id(path); };
    state.ids.head = id("/user/head");
    state.ids.hands[0] = id("/user/hand/left");
    state.ids.hands[1] = id("/user/hand/right");
    state.ids.haptics[0] = id("/user/hand/left/output/haptic");
    state.ids.haptics[1] = id("/user/hand/right/output/haptic");
    // Must match QUEST_CONTROLLER_PROFILE_PATH exactly: alvr_common::inputs builds it as
    // "/interaction_profiles/" + "oculus/touch" + "_controller". The id is a hash, so dropping
    // the "_controller" suffix yields an id absent from CONTROLLER_PROFILE_INFO; the server then
    // leaves controller_button_mapping_manager unset and silently discards every button entry
    // (poses still work, since tracking bypasses button mapping).
    state.ids.profile = id("/interaction_profiles/oculus/touch_controller");
    // The Oculus Touch profile names this button per hand: menu/click on the left, system/click on
    // the right (see CONTROLLER_PROFILE_INFO in alvr_common::inputs). Sending the right
    // controller's BT_Home_Menu to a left-hand path would be dropped by the server's button set.
    state.ids.menu = id("/user/hand/left/input/menu/click");
    state.ids.system = id("/user/hand/right/input/system/click");

    const char *hands[kEyeCount] = {"left", "right"};
    const char *faces[kEyeCount][2] = {{"x", "y"}, {"a", "b"}};
    for (int hand = 0; hand < kEyeCount; ++hand) {
        char path[128];
        for (int face = 0; face < 2; ++face) {
            std::snprintf(path, sizeof(path), "/user/hand/%s/input/%s/click",
                          hands[hand], faces[hand][face]);
            state.ids.face_click[hand][face] = id(path);
            std::snprintf(path, sizeof(path), "/user/hand/%s/input/%s/touch",
                          hands[hand], faces[hand][face]);
            state.ids.face_touch[hand][face] = id(path);
        }
        std::snprintf(path, sizeof(path), "/user/hand/%s/input/squeeze/click", hands[hand]);
        state.ids.squeeze_click[hand] = id(path);
        std::snprintf(path, sizeof(path), "/user/hand/%s/input/squeeze/value", hands[hand]);
        state.ids.squeeze_value[hand] = id(path);
        std::snprintf(path, sizeof(path), "/user/hand/%s/input/trigger/click", hands[hand]);
        state.ids.trigger_click[hand] = id(path);
        std::snprintf(path, sizeof(path), "/user/hand/%s/input/trigger/value", hands[hand]);
        state.ids.trigger_value[hand] = id(path);
        std::snprintf(path, sizeof(path), "/user/hand/%s/input/trigger/touch", hands[hand]);
        state.ids.trigger_touch[hand] = id(path);
        std::snprintf(path, sizeof(path), "/user/hand/%s/input/thumbstick/x", hands[hand]);
        state.ids.stick_x[hand] = id(path);
        std::snprintf(path, sizeof(path), "/user/hand/%s/input/thumbstick/y", hands[hand]);
        state.ids.stick_y[hand] = id(path);
        std::snprintf(path, sizeof(path), "/user/hand/%s/input/thumbstick/click", hands[hand]);
        state.ids.stick_click[hand] = id(path);
        std::snprintf(path, sizeof(path), "/user/hand/%s/input/thumbstick/touch", hands[hand]);
        state.ids.stick_touch[hand] = id(path);
    }
}

void update_binary(uint64_t path, bool value) {
    auto &previous = state.button_states[path];
    if (previous.tag != ALVR_BUTTON_VALUE_BINARY || previous.binary != value) {
        previous.tag = ALVR_BUTTON_VALUE_BINARY;
        previous.binary = value;
        alvr_send_button(path, previous);
    }
}

void update_scalar(uint64_t path, float value) {
    auto &previous = state.button_states[path];
    if (previous.tag != ALVR_BUTTON_VALUE_SCALAR ||
        std::abs(previous.scalar - value) > kButtonEpsilon) {
        previous.tag = ALVR_BUTTON_VALUE_SCALAR;
        previous.scalar = value;
        alvr_send_button(path, previous);
    }
}

void clear_controller_buttons(int hand) {
    if (hand == 0) {
        update_binary(state.ids.menu, false);
    } else {
        update_binary(state.ids.system, false);
    }
    for (int face = 0; face < 2; ++face) {
        update_binary(state.ids.face_click[hand][face], false);
        update_binary(state.ids.face_touch[hand][face], false);
    }
    update_binary(state.ids.squeeze_click[hand], false);
    update_scalar(state.ids.squeeze_value[hand], 0.0f);
    update_binary(state.ids.trigger_click[hand], false);
    update_scalar(state.ids.trigger_value[hand], 0.0f);
    update_binary(state.ids.trigger_touch[hand], false);
    update_scalar(state.ids.stick_x[hand], 0.0f);
    update_scalar(state.ids.stick_y[hand], 0.0f);
    update_binary(state.ids.stick_click[hand], false);
    update_binary(state.ids.stick_touch[hand], false);
}

void update_controller_buttons(const qiyu_ControllerData &controller, int hand) {
    if (!controller.isConnect) {
        if (state.controller_connected[hand]) {
            clear_controller_buttons(hand);
        }
        state.controller_connected[hand] = false;
        return;
    }

    state.controller_connected[hand] = true;
    if (hand == 0) {
        update_binary(state.ids.menu, controller.button & BT_Home_Menu);
    } else {
        update_binary(state.ids.system, controller.button & BT_Home_Menu);
    }
    update_binary(state.ids.face_click[hand][0], controller.button & BT_A_X);
    update_binary(state.ids.face_touch[hand][0], controller.buttonTouch & BT_A_X);
    update_binary(state.ids.face_click[hand][1], controller.button & BT_B_Y);
    update_binary(state.ids.face_touch[hand][1], controller.buttonTouch & BT_B_Y);
    update_binary(state.ids.squeeze_click[hand], controller.button & BT_Grip);
    update_scalar(state.ids.squeeze_value[hand], controller.gripForce);
    update_binary(state.ids.trigger_click[hand], controller.button & BT_Trigger);
    update_scalar(state.ids.trigger_value[hand], controller.triggerForce);
    update_binary(state.ids.trigger_touch[hand], controller.buttonTouch & BT_Trigger);
    update_scalar(state.ids.stick_x[hand], controller.joyStickPos.x);
    update_scalar(state.ids.stick_y[hand], controller.joyStickPos.y);
    update_binary(state.ids.stick_click[hand], controller.button & BT_JoyStick);
    update_binary(state.ids.stick_touch[hand], controller.buttonTouch & BT_JoyStick);
}

void send_view_params() {
    const auto device = api.get_device_info();
    const float half_ipd = get_ipd(device) * 0.5f;
    AlvrViewParams views[kEyeCount]{};
    views[0].pose.orientation.w = 1.0f;
    views[1].pose.orientation.w = 1.0f;
    views[0].pose.position[0] = -half_ipd;
    views[1].pose.position[0] = half_ipd;
    views[0].fov = get_fov(device, 0);
    views[1].fov = get_fov(device, 1);
    static int vp_dbg = 0;
    if (vp_dbg++ < 3) {
        log(ANDROID_LOG_WARN,
            "[VIEW-DBG] L fov(l=%.4f r=%.4f u=%.4f d=%.4f) R fov(l=%.4f r=%.4f u=%.4f d=%.4f) ipd=%.4f "
            "Lfrustum(l=%.4f r=%.4f t=%.4f b=%.4f n=%.4f) Rfrustum(l=%.4f r=%.4f t=%.4f b=%.4f n=%.4f)",
            views[0].fov.left, views[0].fov.right, views[0].fov.up, views[0].fov.down,
            views[1].fov.left, views[1].fov.right, views[1].fov.up, views[1].fov.down, half_ipd * 2.0f,
            device.frustumLeftEye.left, device.frustumLeftEye.right, device.frustumLeftEye.top,
            device.frustumLeftEye.bottom, device.frustumLeftEye.near,
            device.frustumRightEye.left, device.frustumRightEye.right, device.frustumRightEye.top,
            device.frustumRightEye.bottom, device.frustumRightEye.near);
        // [GEOM-DBG] SXR logs a "display frustum" AND a narrower "render frustum" at startup, but
        // qiyu_DeviceInfo only exposes one pair. Print the SDK's own eye-target size next to the
        // buffers we actually allocated: if the frustum we build FOV from is the display one while
        // the compositor samples the render one, the image is magnified (~21%) and soft.
        log(ANDROID_LOG_WARN,
            "[GEOM-DBG] sdk_eye_target=%dx%d our_view=%ux%u stream=%ux%u "
            "Lpos(%.4f,%.4f,%.4f) Rpos(%.4f,%.4f,%.4f) Lfar=%.4f Rfar=%.4f",
            device.iEyeTargetWidth, device.iEyeTargetHeight, state.view_width, state.view_height,
            state.stream_width, state.stream_height,
            device.frustumLeftEye.position.x, device.frustumLeftEye.position.y,
            device.frustumLeftEye.position.z, device.frustumRightEye.position.x,
            device.frustumRightEye.position.y, device.frustumRightEye.position.z,
            device.frustumLeftEye.far, device.frustumRightEye.far);
    }
    alvr_send_view_params(views);
}

AlvrDeviceMotion controller_motion(const qiyu_ControllerData &controller,
                                   int hand, float prediction_seconds) {
    AlvrDeviceMotion motion{};
    motion.device_id = state.ids.hands[hand];
    motion.pose.orientation = {
        controller.rotation.x,
        controller.rotation.y,
        controller.rotation.z,
        controller.rotation.w,
    };
    motion.pose.position[0] = controller.position.x + controller.velocity.x * prediction_seconds;
    motion.pose.position[1] = controller.position.y + controller.velocity.y * prediction_seconds -
                              state.tracking_offset.load();
    motion.pose.position[2] = controller.position.z + controller.velocity.z * prediction_seconds;
    motion.linear_velocity[0] = controller.velocity.x;
    motion.linear_velocity[1] = controller.velocity.y;
    motion.linear_velocity[2] = -controller.velocity.z;
    motion.angular_velocity[0] = controller.angVelocity.x;
    motion.angular_velocity[1] = controller.angVelocity.y;
    motion.angular_velocity[2] = -controller.angVelocity.z;
    return motion;
}

void tracking_loop() {
    JNIEnv *jni_env = nullptr;
    bool attached_to_jvm = false;
    if (state.java_vm != nullptr) {
        const jint get_env_result =
            state.java_vm->GetEnv(reinterpret_cast<void **>(&jni_env), JNI_VERSION_1_6);
        if (get_env_result == JNI_EDETACHED) {
            if (state.java_vm->AttachCurrentThread(&jni_env, nullptr) == JNI_OK) {
                attached_to_jvm = true;
            } else {
                log(ANDROID_LOG_ERROR, "Failed to attach QiYu tracking thread to JVM");
            }
        } else if (get_env_result != JNI_OK) {
            log(ANDROID_LOG_ERROR, "Cannot get JNI environment for QiYu tracking thread: %d",
                get_env_result);
        }
    }

    auto next_tick = std::chrono::steady_clock::now();
    auto next_slow_update = next_tick;

    while (state.running.load()) {
        const uint64_t prediction_ns = alvr_get_prediction_offset_ns();
        const uint64_t target_timestamp_ns = clock_ns() + prediction_ns;
        const auto head = api.predict_head_pose(static_cast<float>(prediction_ns) / 1e6f);

        std::vector<AlvrDeviceMotion> motions;
        motions.reserve(3);
        AlvrDeviceMotion head_motion{};
        head_motion.device_id = state.ids.head;
        head_motion.pose = head_pose(head);
        motions.push_back(head_motion);

        {
            std::lock_guard lock(state.tracking_mutex);
            state.tracking_frames.push_front({target_timestamp_ns, head});
            if (state.tracking_frames.size() > kMaxTrackingFrames) {
                state.tracking_frames.pop_back();
            }
        }

        const bool controller_module_ready = api.is_controller_initialized();
        if (controller_module_ready) {
            qiyu_ControllerData controllers[kEyeCount]{};
            api.get_controller_data(&controllers[0], &controllers[1]);
            const float prediction_seconds = static_cast<float>(prediction_ns) / 1e9f;
            // [BTN-DIAG] With a single (right) physical controller, the click bits for A/B were
            // seen in the hand=0 slot in one session and never in hand=1 in another, while
            // buttonTouch fired in both. Print battery and isShow alongside, since those identify
            // which slot the real device actually landed in.
            static int last_button[kEyeCount] = {-1, -1};
            static int last_touch[kEyeCount] = {-1, -1};
            for (int hand = 0; hand < kEyeCount; ++hand) {
                if (controllers[hand].button != last_button[hand] ||
                    controllers[hand].buttonTouch != last_touch[hand]) {
                    last_button[hand] = controllers[hand].button;
                    last_touch[hand] = controllers[hand].buttonTouch;
                    log(ANDROID_LOG_WARN,
                        "[BTN-DIAG] hand=%d connect=%d show=%d batt=%d button=0x%08x touch=0x%08x "
                        "trigger=%.3f grip=%.3f stick=(%.3f,%.3f)",
                        hand, controllers[hand].isConnect, controllers[hand].isShow,
                        controllers[hand].batteryLevel, controllers[hand].button,
                        controllers[hand].buttonTouch, controllers[hand].triggerForce,
                        controllers[hand].gripForce, controllers[hand].joyStickPos.x,
                        controllers[hand].joyStickPos.y);
                }
            }
            for (int hand = 0; hand < kEyeCount; ++hand) {
                update_controller_buttons(controllers[hand], hand);
                if (controllers[hand].isConnect) {
                    motions.push_back(controller_motion(controllers[hand], hand,
                                                        prediction_seconds));
                }
            }

            if (std::chrono::steady_clock::now() >= next_slow_update) {
                log(ANDROID_LOG_INFO,
                    "Controllers: module=ready head(%.3f,%.3f,%.3f) left(connect=%d show=%d pos=%.3f,%.3f,%.3f) "
                    "right(connect=%d show=%d pos=%.3f,%.3f,%.3f)",
                    head.pose.position.x, head.pose.position.y, head.pose.position.z,
                    controllers[0].isConnect, controllers[0].isShow,
                    controllers[0].position.x, controllers[0].position.y,
                    controllers[0].position.z, controllers[1].isConnect,
                    controllers[1].isShow, controllers[1].position.x,
                    controllers[1].position.y, controllers[1].position.z);
                for (int hand = 0; hand < kEyeCount; ++hand) {
                    const int battery = controllers[hand].batteryLevel;
                    if (controllers[hand].isConnect &&
                        battery != state.last_controller_battery[hand]) {
                        const float gauge = battery <= 1 ? static_cast<float>(battery)
                                                         : static_cast<float>(battery) / 100.0f;
                        alvr_send_battery(state.ids.hands[hand],
                                          std::clamp(gauge, 0.0f, 1.0f), false);
                        state.last_controller_battery[hand] = battery;
                    }
                }
            }
        } else {
            if (std::chrono::steady_clock::now() >= next_slow_update) {
                log(ANDROID_LOG_WARN, "Controllers: module=not-ready");
            }
            for (int hand = 0; hand < kEyeCount; ++hand) {
                if (state.controller_connected[hand]) {
                    clear_controller_buttons(hand);
                    state.controller_connected[hand] = false;
                }
            }
        }

        alvr_send_tracking(target_timestamp_ns, motions.data(), motions.size(), nullptr, nullptr);

        const auto now = std::chrono::steady_clock::now();
        if (now >= next_slow_update) {
            send_view_params();
            next_slow_update = now + std::chrono::seconds(1);
        }
        const float rate = std::max(state.refresh_rate.load(), 60.0f) * 3.0f;
        next_tick += std::chrono::nanoseconds(static_cast<int64_t>(1e9f / rate));
        std::this_thread::sleep_until(next_tick);
    }

    if (attached_to_jvm) {
        state.java_vm->DetachCurrentThread();
    }
}

void initialize_decoder(AlvrCodec codec) {
    // The server can re-send DecoderConfig repeatedly. Only (re)create the MediaCodec decoder when
    // it isn't already up for this codec; otherwise we tear down and rebuild the decoder on every
    // event (observed thousands of MediaCodec recreations), which prevents any frame from decoding.
    if (state.decoder_initialized && state.decoder_codec == static_cast<int>(codec)) {
        return;
    }
    if (state.decoder_initialized) {
        alvr_destroy_decoder();
    }
    const uint64_t size = alvr_get_decoder_config(nullptr);
    std::vector<char> config(size);
    if (size != 0) {
        alvr_get_decoder_config(config.data());
    }
    const AlvrDecoderConfig decoder_config{
        codec,
        false,
        2.0f,
        0.9f,
        nullptr,
        0,
        reinterpret_cast<const uint8_t *>(config.data()),
        size,
    };
    alvr_create_decoder(decoder_config);
    state.decoder_initialized = true;
    state.decoder_codec = static_cast<int>(codec);
}

void start_stream(const StreamingStarted_Body &config) {
    state.stream_width = config.view_width;
    state.stream_height = config.view_height;
    state.refresh_rate.store(config.refresh_rate_hint > 0.0f
                                 ? config.refresh_rate_hint
                                 : kDefaultRefreshRate);

    EyeBuffers old[kEyeCount] = {state.stream[0], state.stream[1]};
    state.stream[0] = {};
    state.stream[1] = {};
    if (!create_buffers(state.stream, state.stream_width, state.stream_height)) {
        state.stream[0] = old[0];
        state.stream[1] = old[1];
        return;
    }

    const uint32_t *textures[kEyeCount] = {
        state.stream[0].textures.data(), state.stream[1].textures.data()};
    AlvrStreamConfig renderer_config{};
    renderer_config.view_resolution_width = state.stream_width;
    renderer_config.view_resolution_height = state.stream_height;
    renderer_config.swapchain_textures = textures;
    renderer_config.swapchain_length = kSwapchainLength;
    alvr_start_stream_opengl(renderer_config);

    for (auto &eye : old) {
        glDeleteTextures(kSwapchainLength, eye.textures.data());
    }

    api.post_eye_buffer_size(state.stream_width, state.stream_height);
    send_view_params();
    alvr_send_active_interaction_profile(state.ids.hands[0], state.ids.profile);
    alvr_send_active_interaction_profile(state.ids.hands[1], state.ids.profile);
    const auto boundary = api.get_boundary_dimensions();
    alvr_send_playspace(std::max(0.0f, boundary.x * 2.0f),
                        std::max(0.0f, boundary.z * 2.0f));
    state.streaming.store(true);
}

void stop_stream() {
    state.streaming.store(false);
    if (state.decoder_initialized) {
        alvr_destroy_decoder();
        state.decoder_initialized = false;
        state.decoder_codec = -1;
    }
}

void poll_core_events() {
    AlvrEvent event{};
    while (alvr_poll_event(&event)) {
        switch (event.tag) {
        case ALVR_EVENT_HUD_MESSAGE_UPDATED: {
            const uint64_t size = alvr_hud_message(nullptr);
            std::vector<char> message(size);
            if (size != 0) {
                alvr_hud_message(message.data());
                alvr_update_hud_message_opengl(message.data());
            }
            break;
        }
        case ALVR_EVENT_STREAMING_STARTED:
            log(ANDROID_LOG_INFO, "event STREAMING_STARTED %ux%u", event.STREAMING_STARTED.view_width,
                event.STREAMING_STARTED.view_height);
            start_stream(event.STREAMING_STARTED);
            log(ANDROID_LOG_INFO, "streaming=%d after start_stream", state.streaming.load());
            break;
        case ALVR_EVENT_STREAMING_STOPPED:
            log(ANDROID_LOG_INFO, "event STREAMING_STOPPED");
            stop_stream();
            break;
        case ALVR_EVENT_HAPTICS: {
            const int hand = event.HAPTICS.device_id == state.ids.hands[1] ||
                                     event.HAPTICS.device_id == state.ids.haptics[1]
                                 ? 1
                                 : 0;
            const auto mask = hand == 0 ? qiyu_ControllerMask::CM_Left
                                        : qiyu_ControllerMask::CM_Right;
            api.start_vibration(mask,
                std::clamp(event.HAPTICS.amplitude, 0.0f, 1.0f),
                std::max(event.HAPTICS.duration_s, 0.0f));
            break;
        }
        case ALVR_EVENT_DECODER_CONFIG:
            log(ANDROID_LOG_INFO, "event DECODER_CONFIG codec=%d", event.DECODER_CONFIG.codec);
            initialize_decoder(event.DECODER_CONFIG.codec);
            break;
        case ALVR_EVENT_REAL_TIME_CONFIG:
            break;
        }
    }
}

qiyu_HeadPoseState tracking_for_timestamp(uint64_t timestamp_ns) {
    std::lock_guard lock(state.tracking_mutex);
    for (const auto &[tracking_timestamp, tracking] : state.tracking_frames) {
        if (tracking_timestamp <= timestamp_ns) {
            return tracking;
        }
    }
    return api.predict_head_pose(api.predict_display_time());
}

// Render/submit indices of the frame currently being built, captured so one log line can compare
// what wgpu drew into against what SXR is handed. Two separately throttled logs sampled different
// frames and could not prove they agree.
int g_render_index[kEyeCount] = {-1, -1};
uint32_t g_render_tex[kEyeCount] = {0, 0};

void fill_frame_layers(qiyu_FrameParam &frame, EyeBuffers (&buffers)[kEyeCount]) {
    static int layer_dbg = 0;
    const bool trace_layers = (layer_dbg++ % 72) == 0;
    for (int eye = 0; eye < kEyeCount; ++eye) {
        auto &layer = frame.renderLayers[eye];
        layer.imageHandle = static_cast<int32_t>(buffers[eye].textures[buffers[eye].index]);
        layer.imageType = TT_Texture;
        fill_layout(layer.imageCoords);
        layer.eyeMask = eye == 0 ? RL_EyeMask_Left : RL_EyeMask_Right;
        if (trace_layers) {
            log(ANDROID_LOG_WARN,
                "[FRAME-DBG] eye=%d submit(idx=%u tex=%d) render(idx=%d tex=%u) mask=0x%x", eye,
                buffers[eye].index, layer.imageHandle, g_render_index[eye], g_render_tex[eye],
                static_cast<int>(layer.eyeMask));
        }
        buffers[eye].index = (buffers[eye].index + 1) % kSwapchainLength;
    }
}

bool render_stream(qiyu_FrameParam &frame) {
    uint64_t timestamp_ns = 0;
    void *hardware_buffer = nullptr;
    if (!state.decoder_initialized ||
        !alvr_get_frame(&timestamp_ns, &hardware_buffer)) {
        static int miss_log = 0;
        if ((miss_log++ % 72) == 0) {
            log(ANDROID_LOG_WARN, "render_stream skip: decoder_init=%d get_frame_failed",
                state.decoder_initialized);
        }
        return false;
    }
    static int hit_log = 0;
    if ((hit_log++ % 72) == 0) {
        log(ANDROID_LOG_INFO, "render_stream frame ts=%llu buf=%p",
            (unsigned long long)timestamp_ns, hardware_buffer);
    }

    auto tracking = tracking_for_timestamp(timestamp_ns);
    AlvrViewParams input_views[kEyeCount]{};
    alvr_report_compositor_start(timestamp_ns, input_views);
    const auto current_orientation =
        head_orientation(api.predict_head_pose(api.predict_display_time()).pose.rotation);
    AlvrStreamViewParams views[kEyeCount]{};
    for (int eye = 0; eye < kEyeCount; ++eye) {
        views[eye].swapchain_index = state.stream[eye].index;
        views[eye].reprojection_rotation =
            alvr_rotation_delta(input_views[eye].pose.orientation, current_orientation);
        views[eye].fov = input_views[eye].fov;
    }

    // [STREAM-DBG] Only one eye shows streamed video on device while the lobby (same textures,
    // same submit path) renders both. That narrows the fault to what wgpu's StreamRenderer is
    // handed: it picks its render target as views_objects[eye].render_target[swapchain_index],
    // so identical per-eye indices would make both eyes draw into the same target, and identical
    // per-eye fov would mean report_compositor_start never split the views. Log both.
    static int stream_dbg = 0;
    if ((stream_dbg++ % 72) == 0) {
        for (int eye = 0; eye < kEyeCount; ++eye) {
            log(ANDROID_LOG_WARN,
                "[STREAM-DBG] eye=%d sc_idx=%u tex=%u fov(l=%.4f r=%.4f u=%.4f d=%.4f) "
                "in_pos(%.3f,%.3f,%.3f)",
                eye, views[eye].swapchain_index,
                state.stream[eye].textures[state.stream[eye].index], views[eye].fov.left,
                views[eye].fov.right, views[eye].fov.up, views[eye].fov.down,
                input_views[eye].pose.position[0], input_views[eye].pose.position[1],
                input_views[eye].pose.position[2]);
        }
    }

    // Record what wgpu is about to draw into so fill_frame_layers can print render vs submit for
    // the SAME frame.
    for (int eye = 0; eye < kEyeCount; ++eye) {
        g_render_index[eye] = static_cast<int>(views[eye].swapchain_index);
        g_render_tex[eye] = state.stream[eye].textures[views[eye].swapchain_index];
    }

    api.start_eye(false, EYE_Left, TT_Texture);
    api.start_eye(false, EYE_Right, TT_Texture);
    // Bracket the call: if "pre" appears without "post", alvr_render_stream_opengl never returned;
    // if both appear while the Rust-side probes stay silent, the call is resolving somewhere other
    // than the freshly built libalvr_client_openxr.so.
    static int call_dbg = 0;
    const bool trace_call = (call_dbg++ % 72) == 0;
    if (trace_call) log(ANDROID_LOG_WARN, "[STREAM-CALL] pre alvr_render_stream_opengl");
    alvr_render_stream_opengl(hardware_buffer, views);
    if (trace_call) log(ANDROID_LOG_WARN, "[STREAM-CALL] post alvr_render_stream_opengl");
    api.end_eye(false, EYE_Left, TT_Texture);
    api.end_eye(false, EYE_Right, TT_Texture);

    const float vsync_queue_ms = api.predict_display_time();
    alvr_report_submit(timestamp_ns,
                       static_cast<uint64_t>(std::max(0.0f, vsync_queue_ms) * 1e6f));
    fill_frame_layers(frame, state.stream);
    frame.headPoseState = tracking;
    return true;
}

void render_lobby(qiyu_FrameParam &frame) {
    static int lob = 0;
    const bool trace = lob++ < 3;
    if (trace) log(ANDROID_LOG_INFO, "lobby[%d] A: get_device_info", lob - 1);
    const auto device = api.get_device_info();
    if (trace) log(ANDROID_LOG_INFO, "lobby[%d] B: predict_head_pose", lob - 1);
    const auto tracking = api.predict_head_pose(api.predict_display_time());
    qiyu_Matrix4 view_matrices[kEyeCount]{};
    float tracking_offset = state.tracking_offset.load();
    if (trace) log(ANDROID_LOG_INFO, "lobby[%d] C: get_view_matrix", lob - 1);
    api.get_view_matrix(view_matrices[0], view_matrices[1], tracking_offset, tracking,
                        device.frustumLeftEye.rotation,
                        device.frustumRightEye.rotation);
    state.tracking_offset.store(tracking_offset);

    AlvrLobbyViewParams views[kEyeCount]{};
    for (int eye = 0; eye < kEyeCount; ++eye) {
        const auto inverse = matrix_inverse(view_matrices[eye]);
        views[eye].swapchain_index = state.lobby[eye].index;
        views[eye].pose.orientation = head_orientation(tracking.pose.rotation);
        views[eye].pose.position[0] = -inverse.M[0][3];
        views[eye].pose.position[1] = -inverse.M[1][3] - tracking_offset;
        views[eye].pose.position[2] = -inverse.M[2][3];
        views[eye].fov = get_fov(device, eye);
    }

    if (trace) log(ANDROID_LOG_INFO, "lobby[%d] D: start_eye L/R", lob - 1);
    api.start_eye(false, EYE_Left, TT_Texture);
    api.start_eye(false, EYE_Right, TT_Texture);
    if (trace) log(ANDROID_LOG_INFO, "lobby[%d] E: alvr_render_lobby_opengl", lob - 1);
    alvr_render_lobby_opengl(views, true);
    if (trace) log(ANDROID_LOG_INFO, "lobby[%d] F: end_eye L/R", lob - 1);
    api.end_eye(false, EYE_Left, TT_Texture);
    api.end_eye(false, EYE_Right, TT_Texture);
    fill_frame_layers(frame, state.lobby);
    frame.headPoseState = tracking;
    if (trace) log(ANDROID_LOG_INFO, "lobby[%d] G: done", lob - 1);
}

} // namespace

extern "C" bool alvr_qiyu_initialize(void *activity, void *java_vm) {
    if (state.initialized.load()) {
        return true;
    }
    // Initialize ALVR core logging so connection/streaming events are visible in logcat. Without
    // this, alvr_common::info!/error! produce no output (the OpenXR path calls init_logging in
    // Rust; the SXR path never did), leaving connection failures invisible.
    alvr_initialize_logging();
    // Build marker: lets a captured log prove which APK the device actually ran, so a missing
    // diagnostic can be told apart from a stale install.
    log(ANDROID_LOG_WARN, "[BUILD-ID] qiyu_backend build30 %s %s", __DATE__, __TIME__);
    log(ANDROID_LOG_INFO, "init step 1: enter (activity=%p vm=%p)", activity, java_vm);
    state.java_vm = static_cast<JavaVM *>(java_vm);

    // The official ALVR-QIYU attaches the calling thread to the JVM (getOvrJava(true) ->
    // AttachCurrentThread) and passes a GLOBAL ref activity (NewGlobalRef in initializeNative)
    // BEFORE qiyu_Init. QVR_Init dereferences the JNIEnv of the current thread and the activity
    // object; without an attached thread / global ref it crashes with SIGSEGV at addr 0x0.
    JNIEnv *jni_env = nullptr;
    if (state.java_vm != nullptr) {
        jint get_env = state.java_vm->GetEnv(reinterpret_cast<void **>(&jni_env), JNI_VERSION_1_6);
        if (get_env == JNI_EDETACHED) {
            JavaVMAttachArgs args{JNI_VERSION_1_6, "alvr_qiyu_main", nullptr};
            if (state.java_vm->AttachCurrentThread(&jni_env, &args) != JNI_OK) {
                log(ANDROID_LOG_ERROR, "init step 1b: AttachCurrentThread FAILED");
                jni_env = nullptr;
            } else {
                log(ANDROID_LOG_INFO, "init step 1b: AttachCurrentThread ok");
            }
        } else {
            log(ANDROID_LOG_INFO, "init step 1b: thread already attached (env=%p)", jni_env);
        }
    }

    // Promote the activity to a global reference so QVR_Init can safely hold it.
    jobject activity_local = static_cast<jobject>(activity);
    if (jni_env != nullptr && activity_local != nullptr) {
        state.activity = jni_env->NewGlobalRef(activity_local);
        log(ANDROID_LOG_INFO, "init step 1c: activity global ref=%p", state.activity);
    } else {
        state.activity = activity_local;
        log(ANDROID_LOG_WARN, "init step 1c: using activity as-is (no env)");
    }

    if (!api.load()) {
        log(ANDROID_LOG_ERROR, "init step 2: api.load() FAILED");
        api.unload();
        return false;
    }
    log(ANDROID_LOG_INFO, "init step 2: api.load() ok");
    if (!initialize_egl()) {
        log(ANDROID_LOG_ERROR, "init step 3: initialize_egl() FAILED");
        destroy_egl();
        api.unload();
        return false;
    }
    log(ANDROID_LOG_INFO, "init step 3: initialize_egl() ok");
    log(ANDROID_LOG_INFO, "init step 4: calling qiyu_Init...");
    if (!api.init(state.activity, state.java_vm, qiyu_GraphicsApi::GA_OpenGLES,
                  qiyu_TrackingOriginMode::TM_Ground, false)) {
        log(ANDROID_LOG_ERROR, "init step 4: qiyu_Init FAILED");
        destroy_egl();
        api.unload();
        return false;
    }
    log(ANDROID_LOG_INFO, "init step 4: qiyu_Init ok");

    const auto device = api.get_device_info();
    log(ANDROID_LOG_INFO, "init step 5: get_device_info ok (%dx%d)",
        device.iEyeTargetWidth, device.iEyeTargetHeight);
    state.view_width = std::max(device.iEyeTargetWidth, 1);
    state.view_height = std::max(device.iEyeTargetHeight, 1);
    const float refresh_rates[] = {72.0f, 90.0f};
    const AlvrClientCapabilities capabilities{
        state.view_width,
        state.view_height,
        refresh_rates,
        2,
        true,
        true,
        true,
        false,
        false,
        true,
        1.0f,
        false,
    };
    log(ANDROID_LOG_INFO, "init step 6: alvr_initialize_android_context...");
    // Pass the GLOBAL ref (state.activity), not the raw local `activity`. ALVR core (app_dirs2)
    // stores this jobject and later calls GetObjectClass on it from the render thread; a local
    // ref would be invalid there and triggers a JNI abort (JniAbort in DecodeJObject).
    alvr_initialize_android_context(java_vm, state.activity);
    log(ANDROID_LOG_INFO, "init step 7: alvr_initialize...");
    alvr_initialize(capabilities);
    log(ANDROID_LOG_INFO, "init step 8: alvr_initialize_opengl_external...");
    // Make the SXR EGL context current so wgpu adopts IT (shared GL namespace) instead of
    // creating a separate context. Separate contexts leave the eye swapchain textures and
    // qiyu_SubmitFrame in different namespaces -> timewarp sees empty frames -> black screen.
    eglMakeCurrent(state.egl.display, state.egl.surface, state.egl.surface, state.egl.context);
    if (!alvr_initialize_opengl_external()) {
        log(ANDROID_LOG_ERROR, "init step 8: alvr_initialize_opengl_external FAILED");
        destroy_egl();
        api.unload();
        return false;
    }
    // wgpu's adapter init may have switched the current context; restore SXR's for subsequent
    // GL work (create_buffers, StartEye/EndEye, SubmitFrame).
    eglMakeCurrent(state.egl.display, state.egl.surface, state.egl.surface, state.egl.context);
    initialize_ids();
    api.post_eye_buffer_size(state.view_width, state.view_height);
    state.initialized.store(true);
    log(ANDROID_LOG_INFO, "QiYu backend initialized at %ux%u", state.view_width,
        state.view_height);
    return true;
}

extern "C" bool alvr_qiyu_start(void *native_window) {
    if (!state.initialized.load() || state.running.load() || native_window == nullptr) {
        return state.running.load();
    }
    // qiyu_StartVR internally cycles SXR pause/resume before running is set; that cycle can
    // re-enter alvr_qiyu_start via onResumeNative while running is still false. Guard the whole
    // start with a dedicated flag so the reentrant call is rejected (avoids double StartVR that
    // kills the render loop -> black screen).
    if (state.starting.exchange(true)) {
        log(ANDROID_LOG_WARN, "alvr_qiyu_start: already starting, rejecting reentrant call");
        return false;
    }
    log(ANDROID_LOG_INFO, "start step 1: enter (window=%p)", native_window);
    state.window = static_cast<ANativeWindow *>(native_window);
    // qiyu_StartVR reads the thread's *current* EGL surface to derive the eye-buffer config. After
    // a pause the previous SXR surface is destroyed, and if it is still bound here SXR queries a
    // dead handle ("Config ID of eye surface: -1" / "Unable to find current eye surface config!")
    // and comes back up with a broken eye context. Bind our own long-lived pbuffer first so SXR
    // always reads a valid surface/config on both the first start and every resume.
    eglMakeCurrent(state.egl.display, state.egl.surface, state.egl.surface, state.egl.context);
    if (!api.start_vr(state.window, PL_System, PL_System)) {
        log(ANDROID_LOG_ERROR, "start step 2: qiyu_StartVR FAILED");
        state.window = nullptr;
        state.starting.store(false);
        return false;
    }
    log(ANDROID_LOG_INFO, "start step 2: qiyu_StartVR ok");
    api.set_tracking_origin(qiyu_TrackingOriginMode::TM_Ground);
    if (!create_buffers(state.lobby, state.view_width, state.view_height)) {
        log(ANDROID_LOG_ERROR, "start step 3: create_buffers FAILED");
        api.end_vr();
        state.window = nullptr;
        state.starting.store(false);
        return false;
    }
    log(ANDROID_LOG_INFO, "start step 3: create_buffers ok");
    const uint32_t *textures[kEyeCount] = {
        state.lobby[0].textures.data(), state.lobby[1].textures.data()};
    alvr_resume_opengl(state.view_width, state.view_height, textures,
                       kSwapchainLength);
    api.post_eye_buffer_size(state.view_width, state.view_height);
    state.last_frame_time_ns = clock_ns();
    state.running.store(true);
    state.tracking_thread = std::thread(tracking_loop);
    alvr_resume();
    state.starting.store(false);
    log(ANDROID_LOG_INFO, "start step 4: running");
    return true;
}

extern "C" void alvr_qiyu_stop() {
    log(ANDROID_LOG_WARN, "alvr_qiyu_stop: enter (running=%d)", state.running.load());
    if (!state.running.exchange(false)) {
        log(ANDROID_LOG_WARN, "alvr_qiyu_stop: already stopped, no-op");
        return;
    }
    if (state.tracking_thread.joinable()) {
        state.tracking_thread.join();
    }
    stop_stream();
    for (int hand = 0; hand < kEyeCount; ++hand) {
        clear_controller_buttons(hand);
        api.stop_vibration(hand == 0 ? qiyu_ControllerMask::CM_Left
                         : qiyu_ControllerMask::CM_Right);
    }
    alvr_pause();
    alvr_pause_opengl();
    destroy_buffers(state.stream);
    destroy_buffers(state.lobby);
    {
        std::lock_guard lock(state.tracking_mutex);
        state.tracking_frames.clear();
    }
    api.end_vr();
    // end_vr destroys SXR's EGL surfaces. They stay bound to this thread unless we release them,
    // and the next qiyu_StartVR would then inspect a destroyed surface. Drop back to our own
    // pbuffer so the thread always has a valid surface/context bound across the pause.
    eglMakeCurrent(state.egl.display, state.egl.surface, state.egl.surface, state.egl.context);
    state.window = nullptr;
}

extern "C" void alvr_qiyu_render_frame() {
    if (!state.running.load()) {
        return;
    }

    // Ensure a GL context is current on this render thread so qiyu_SubmitFrame can reach the shared
    // GL textures. SXR normally leaves its own (namespace-sharing) context current here; only bind
    // ours if nothing is current (observed ctx=0x0 at submit -> timewarp gets no valid frame ->
    // black screen). Bind the pbuffer rather than EGL_NO_SURFACE: this driver rejects surfaceless
    // binds with EGL_BAD_SURFACE.
    if (eglGetCurrentContext() == EGL_NO_CONTEXT) {
        eglMakeCurrent(state.egl.display, state.egl.surface, state.egl.surface, state.egl.context);
    }

    static int render_heartbeat = 0;
    if ((render_heartbeat++ % 72) == 0) {
        log(ANDROID_LOG_INFO, "render_frame heartbeat #%d streaming=%d decoder_init=%d",
            render_heartbeat - 1, state.streaming.load(), state.decoder_initialized);
    }

    poll_core_events();

    const uint64_t now_ns = clock_ns();
    const float delta_seconds = state.last_frame_time_ns == 0
                                    ? 0.0f
                                    : static_cast<float>(now_ns - state.last_frame_time_ns) / 1e9f;
    state.last_frame_time_ns = now_ns;
    api.update(std::clamp(delta_seconds, 0.0f, 0.1f));

    qiyu_FrameParam frame{};
    frame.minVsyncs = 1;
    if (state.streaming.load()) {
        if (!render_stream(frame)) {
            return;
        }
    } else {
        render_lobby(frame);
    }
    if (!api.submit_frame(frame)) {
        log(ANDROID_LOG_WARN, "qiyu_SubmitFrame failed");
    }
    static int tail = 0;
    if (tail++ < 3) {
        log(ANDROID_LOG_INFO, "render_frame[%d] returned (submit done)", tail - 1);
    }
}

extern "C" void alvr_qiyu_destroy() {
    if (!state.initialized.exchange(false)) {
        return;
    }
    alvr_qiyu_stop();
    api.release();
    alvr_destroy_opengl();
    alvr_destroy();
    destroy_egl();
    api.unload();
}

// ---------------------------------------------------------------------------
// JNI bridge for the official-style Java `alvr.client.VRActivity`.
//
// The QiYu SXR SDK (qiyu_Init -> QVR_Init) requires being driven from a real
// Android Activity on a dedicated Java thread (HandlerThread), the same way the
// official ALVR-QIYU client works. Driving it from a NativeActivity's
// android_main thread makes QVR_Init crash (null deref). These functions let the
// Java VRActivity own the lifecycle and forward to the existing backend logic.
//
// initializeNative / renderNative / onResumeNative etc. all run on the same
// VRActivity rendering HandlerThread, so the EGL context created in
// initialize_egl stays current for rendering.
// ---------------------------------------------------------------------------

extern "C" JNIEXPORT void JNICALL
Java_alvr_client_VRActivity_initializeNative(JNIEnv *env, jobject thiz) {
    JavaVM *vm = nullptr;
    env->GetJavaVM(&vm);
    // Pass the real Activity jobject (thiz). alvr_qiyu_initialize promotes it to a
    // global ref and attaches the thread as needed.
    alvr_qiyu_initialize(static_cast<void *>(thiz), static_cast<void *>(vm));
}

extern "C" JNIEXPORT void JNICALL
Java_alvr_client_VRActivity_onResumeNative(JNIEnv *env, jobject /*thiz*/, jobject surface) {
    log(ANDROID_LOG_WARN, "onResumeNative: enter (running=%d)", state.running.load());
    if (surface == nullptr) {
        log(ANDROID_LOG_ERROR, "onResumeNative: null surface");
        return;
    }
    ANativeWindow *window = ANativeWindow_fromSurface(env, surface);
    if (window == nullptr) {
        log(ANDROID_LOG_ERROR, "onResumeNative: ANativeWindow_fromSurface returned null");
        return;
    }
    alvr_qiyu_start(static_cast<void *>(window));
}

extern "C" JNIEXPORT void JNICALL
Java_alvr_client_VRActivity_onPauseNative(JNIEnv * /*env*/, jobject /*thiz*/) {
    log(ANDROID_LOG_WARN, "onPauseNative: enter (running=%d)", state.running.load());
    alvr_qiyu_stop();
}

extern "C" JNIEXPORT void JNICALL
Java_alvr_client_VRActivity_renderNative(JNIEnv * /*env*/, jobject /*thiz*/) {
    alvr_qiyu_render_frame();
}

extern "C" JNIEXPORT void JNICALL
Java_alvr_client_VRActivity_destroyNative(JNIEnv * /*env*/, jobject /*thiz*/) {
    alvr_qiyu_destroy();
}

extern "C" JNIEXPORT void JNICALL
Java_alvr_client_VRActivity_onStreamStartNative(JNIEnv * /*env*/, jobject /*thiz*/) {
    // Streaming start/stop is handled internally via poll_core_events in the render
    // loop; this bridge exists for Java parity and is currently a no-op.
}

extern "C" JNIEXPORT void JNICALL
Java_alvr_client_VRActivity_onStreamStopNative(JNIEnv * /*env*/, jobject /*thiz*/) {
    // See onStreamStartNative.
}

extern "C" JNIEXPORT void JNICALL
Java_alvr_client_VRActivity_onBatteryChangedNative(
    JNIEnv * /*env*/, jobject /*thiz*/, jint battery, jboolean plugged) {
    alvr_send_battery(state.ids.head, static_cast<float>(battery) / 100.0f,
                      static_cast<bool>(plugged));
}

// Explicitly register the VRActivity native methods. The linker's --gc-sections pass removes
// the individually-exported Java_* symbols because nothing in the Rust code references them
// (they are only called dynamically from Java), which caused UnsatisfiedLinkError.
// RegisterNatives references every function pointer here, anchoring them so they survive
// dead-code elimination, and binds them regardless of dynamic symbol export.
//
// This is invoked from the Rust `JNI_OnLoad` (in qiyu.rs), which is `#[no_mangle] pub` and thus
// guaranteed to land in the dynamic symbol table where the Android loader can find it. A C
// JNI_OnLoad here would only be a local symbol in the Rust cdylib and would not be discovered.
extern "C" jint alvr_qiyu_register_natives(JavaVM *vm) {
    JNIEnv *env = nullptr;
    if (vm->GetEnv(reinterpret_cast<void **>(&env), JNI_VERSION_1_6) != JNI_OK) {
        return JNI_VERSION_1_6;
    }
    jclass clazz = env->FindClass("alvr/client/VRActivity");
    if (clazz == nullptr) {
        // Not the QiYu build/class not present; nothing to register.
        env->ExceptionClear();
        return JNI_VERSION_1_6;
    }
    static const JNINativeMethod methods[] = {
        {"initializeNative", "()V",
         reinterpret_cast<void *>(Java_alvr_client_VRActivity_initializeNative)},
        {"destroyNative", "()V",
         reinterpret_cast<void *>(Java_alvr_client_VRActivity_destroyNative)},
        {"onResumeNative", "(Landroid/view/Surface;)V",
         reinterpret_cast<void *>(Java_alvr_client_VRActivity_onResumeNative)},
        {"onPauseNative", "()V",
         reinterpret_cast<void *>(Java_alvr_client_VRActivity_onPauseNative)},
        {"onStreamStartNative", "()V",
         reinterpret_cast<void *>(Java_alvr_client_VRActivity_onStreamStartNative)},
        {"onStreamStopNative", "()V",
         reinterpret_cast<void *>(Java_alvr_client_VRActivity_onStreamStopNative)},
        {"renderNative", "()V",
         reinterpret_cast<void *>(Java_alvr_client_VRActivity_renderNative)},
        {"onBatteryChangedNative", "(IZ)V",
         reinterpret_cast<void *>(Java_alvr_client_VRActivity_onBatteryChangedNative)},
    };
    env->RegisterNatives(clazz, methods,
                         static_cast<jint>(sizeof(methods) / sizeof(methods[0])));
    env->DeleteLocalRef(clazz);
    return JNI_VERSION_1_6;
}

