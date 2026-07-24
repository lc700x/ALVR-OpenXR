#pragma once

#include <stdbool.h>
#include <stdint.h>

extern "C" {

enum AlvrCodec : uint8_t {
    ALVR_CODEC_H264 = 0,
    ALVR_CODEC_HEVC = 1,
    ALVR_CODEC_AV1 = 2,
};

enum AlvrEvent_Tag : uint8_t {
    ALVR_EVENT_HUD_MESSAGE_UPDATED,
    ALVR_EVENT_STREAMING_STARTED,
    ALVR_EVENT_STREAMING_STOPPED,
    ALVR_EVENT_HAPTICS,
    ALVR_EVENT_DECODER_CONFIG,
    ALVR_EVENT_REAL_TIME_CONFIG,
};

struct StreamingStarted_Body {
    AlvrEvent_Tag tag;
    uint32_t view_width;
    uint32_t view_height;
    float refresh_rate_hint;
    float encoding_gamma;
    bool enable_foveated_encoding;
    bool enable_hdr;
};

struct Haptics_Body {
    AlvrEvent_Tag tag;
    uint64_t device_id;
    float duration_s;
    float frequency;
    float amplitude;
};

struct DecoderConfig_Body {
    AlvrEvent_Tag tag;
    AlvrCodec codec;
};

union AlvrEvent {
    AlvrEvent_Tag tag;
    StreamingStarted_Body STREAMING_STARTED;
    Haptics_Body HAPTICS;
    DecoderConfig_Body DECODER_CONFIG;
};

enum AlvrButtonValue_Tag {
    ALVR_BUTTON_VALUE_BINARY,
    ALVR_BUTTON_VALUE_SCALAR,
};

struct AlvrButtonValue {
    AlvrButtonValue_Tag tag;
    union {
        bool binary;
        float scalar;
    };
};

struct AlvrQuat {
    float x, y, z, w;
};

struct AlvrPose {
    AlvrQuat orientation;
    float position[3];
};

struct AlvrFov {
    float left, right, up, down;
};

struct AlvrViewParams {
    AlvrPose pose;
    AlvrFov fov;
};

struct AlvrDeviceMotion {
    uint64_t device_id;
    AlvrPose pose;
    float linear_velocity[3];
    float angular_velocity[3];
};

struct AlvrClientCapabilities {
    uint32_t default_view_width;
    uint32_t default_view_height;
    const float *refresh_rates;
    uint64_t refresh_rates_count;
    bool foveated_encoding;
    bool encoder_high_profile;
    bool encoder_10_bits;
    bool encoder_av1;
    bool prefer_10bit;
    bool prefer_full_range;
    float preferred_encoding_gamma;
    bool prefer_hdr;
};

struct AlvrLobbyViewParams {
    uint32_t swapchain_index;
    AlvrPose pose;
    AlvrFov fov;
};

struct AlvrStreamViewParams {
    uint32_t swapchain_index;
    AlvrQuat reprojection_rotation;
    AlvrFov fov;
};

struct AlvrStreamConfig {
    uint32_t view_resolution_width;
    uint32_t view_resolution_height;
    const uint32_t **swapchain_textures;
    uint32_t swapchain_length;
    bool enable_foveation;
    float foveation_center_size_x;
    float foveation_center_size_y;
    float foveation_center_shift_x;
    float foveation_center_shift_y;
    float foveation_edge_ratio_x;
    float foveation_edge_ratio_y;
    bool enable_upscaling;
    bool upscaling_edge_direction;
    float upscaling_edge_threshold;
    float upscaling_edge_sharpness;
    float upscale_factor;
};

enum AlvrMediacodecPropType : uint8_t {
    ALVR_MEDIACODEC_PROP_TYPE_FLOAT,
    ALVR_MEDIACODEC_PROP_TYPE_INT32,
    ALVR_MEDIACODEC_PROP_TYPE_INT64,
    ALVR_MEDIACODEC_PROP_TYPE_STRING,
};

union AlvrMediacodecPropValue {
    float float_;
    int32_t int32;
    int64_t int64;
    const char *string;
};

struct AlvrMediacodecOption {
    const char *key;
    AlvrMediacodecPropType ty;
    AlvrMediacodecPropValue value;
};

struct AlvrDecoderConfig {
    AlvrCodec codec;
    bool force_software_decoder;
    float max_buffering_frames;
    float buffering_history_weight;
    const AlvrMediacodecOption *options;
    uint64_t options_count;
    const uint8_t *config_buffer;
    uint64_t config_buffer_size;
};

void alvr_initialize_android_context(void *java_vm, void *context);
void alvr_initialize(AlvrClientCapabilities capabilities);
void alvr_destroy();
void alvr_resume();
void alvr_pause();
bool alvr_poll_event(AlvrEvent *out_event);
uint64_t alvr_hud_message(char *message_buffer);
uint64_t alvr_get_decoder_config(char *out_buffer);
uint64_t alvr_path_string_to_id(const char *path);
void alvr_send_battery(uint64_t device_id, float gauge_value, bool is_plugged);
void alvr_send_playspace(float width, float height);
void alvr_send_active_interaction_profile(uint64_t device_id, uint64_t profile_id);
void alvr_send_button(uint64_t path_id, AlvrButtonValue value);
void alvr_send_view_params(const AlvrViewParams *view_params);
void alvr_send_tracking(uint64_t poll_timestamp_ns,
                        const AlvrDeviceMotion *device_motions,
                        uint64_t device_motions_count,
                        const AlvrPose *const *hand_skeletons,
                        const AlvrPose *const *eye_gazes);
uint64_t alvr_get_prediction_offset_ns();
void alvr_report_compositor_start(uint64_t target_timestamp_ns,
                                  AlvrViewParams *out_view_params);
void alvr_report_submit(uint64_t target_timestamp_ns, uint64_t vsync_queue_ns);
void alvr_initialize_opengl();
void alvr_destroy_opengl();
void alvr_resume_opengl(uint32_t preferred_view_width,
                        uint32_t preferred_view_height,
                        const uint32_t **swapchain_textures,
                        uint32_t swapchain_length);
void alvr_pause_opengl();
void alvr_update_hud_message_opengl(const char *message);
void alvr_start_stream_opengl(AlvrStreamConfig config);
void alvr_render_lobby_opengl(const AlvrLobbyViewParams *view_inputs,
                              bool render_background);
void alvr_render_stream_opengl(void *hardware_buffer,
                               const AlvrStreamViewParams *view_params);
void alvr_create_decoder(AlvrDecoderConfig config);
void alvr_destroy_decoder();
bool alvr_get_frame(uint64_t *out_timestamp_ns, void **out_buffer_ptr);
AlvrQuat alvr_rotation_delta(AlvrQuat source, AlvrQuat destination);

} // extern "C"
