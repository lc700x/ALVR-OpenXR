mod c_api;
mod extra_extensions;
mod graphics;
#[cfg(target_os = "android")]
mod hvr;
mod interaction;
mod lobby;
mod passthrough;
#[cfg(target_os = "android")]
mod qiyu;
mod stream;

use crate::stream::ParsedStreamConfig;
use alvr_client_core::{ClientCapabilities, ClientCoreContext, ClientCoreEvent};
use alvr_common::settings_schema::Switch;
use alvr_common::{
    error,
    glam::{Quat, UVec2, Vec3},
    info,
    parking_lot::RwLock,
    Fov, Pose, HAND_LEFT_ID,
};
use alvr_graphics::GraphicsContext;
use alvr_session::{BodyTrackingBDConfig, BodyTrackingSourcesConfig};
use alvr_system_info::Platform;
use extra_extensions::{
    BD_BODY_TRACKING_EXTENSION_NAME, BD_MOTION_TRACKING_EXTENSION_NAME,
    META_BODY_TRACKING_FULL_BODY_EXTENSION_NAME, META_DETACHED_CONTROLLERS_EXTENSION_NAME,
    META_SIMULTANEOUS_HANDS_AND_CONTROLLERS_EXTENSION_NAME, PICO_CONFIGURATION_EXTENSION_NAME,
};
use interaction::{InteractionContext, InteractionSourcesConfig};
use lobby::Lobby;
use openxr as xr;
use passthrough::PassthroughLayer;
use std::{path::Path, rc::Rc, sync::Arc, thread, time::Duration};
use stream::StreamContext;

fn should_initialize_android_loader(platform: Platform) -> bool {
    platform != Platform::Hvr
}

fn environment_blend_mode(platform: Platform) -> xr::EnvironmentBlendMode {
    if platform == Platform::Hvr {
        xr::EnvironmentBlendMode::ALPHA_BLEND
    } else {
        xr::EnvironmentBlendMode::OPAQUE
    }
}

/// Reports whether the runtime can composite the submitted layers against the real world through
/// the standard alpha-blend environment mode. This is the only passthrough route on runtimes that
/// expose no composition-layer passthrough extension ALVR understands: QiYu ships an undocumented
/// XR_QIYU_passthrough rather than the FB or HTC one, so `PassthroughLayer::new` always fails
/// there even though the headset has cameras.
fn supports_alpha_blend(xr_instance: &xr::Instance, xr_system: xr::SystemId) -> bool {
    match xr_instance
        .enumerate_environment_blend_modes(xr_system, xr::ViewConfigurationType::PRIMARY_STEREO)
    {
        Ok(modes) => {
            info!("OpenXR environment blend modes: {modes:?}");
            modes.contains(&xr::EnvironmentBlendMode::ALPHA_BLEND)
        }
        Err(error) => {
            error!("Cannot enumerate environment blend modes: {error}");
            false
        }
    }
}

fn tracking_reference_space_type(platform: Platform) -> xr::ReferenceSpaceType {
    if platform == Platform::Hvr {
        xr::ReferenceSpaceType::LOCAL
    } else {
        xr::ReferenceSpaceType::STAGE
    }
}

#[cfg(test)]
mod tests {
    use super::{
        environment_blend_mode, should_initialize_android_loader, tracking_reference_space_type,
        Platform,
    };
    use openxr as xr;

    #[test]
    fn hvr_uses_legacy_android_instance_initialization() {
        assert!(!should_initialize_android_loader(Platform::Hvr));
        assert!(should_initialize_android_loader(Platform::Quest2));
        assert!(should_initialize_android_loader(Platform::Pico4));
        assert!(should_initialize_android_loader(Platform::Yvr));
    }

    #[test]
    fn hvr_matches_official_compositor_contract() {
        assert_eq!(
            environment_blend_mode(Platform::Hvr),
            xr::EnvironmentBlendMode::ALPHA_BLEND
        );
        assert_eq!(
            tracking_reference_space_type(Platform::Hvr),
            xr::ReferenceSpaceType::LOCAL
        );
        assert_eq!(
            environment_blend_mode(Platform::Quest2),
            xr::EnvironmentBlendMode::OPAQUE
        );
        assert_eq!(
            tracking_reference_space_type(Platform::Quest2),
            xr::ReferenceSpaceType::STAGE
        );
    }
}

fn from_xr_vec3(v: xr::Vector3f) -> Vec3 {
    Vec3::new(v.x, v.y, v.z)
}

fn to_xr_vec3(v: Vec3) -> xr::Vector3f {
    xr::Vector3f {
        x: v.x,
        y: v.y,
        z: v.z,
    }
}

fn from_xr_quat(q: xr::Quaternionf) -> Quat {
    Quat::from_xyzw(q.x, q.y, q.z, q.w)
}

fn to_xr_quat(q: Quat) -> xr::Quaternionf {
    xr::Quaternionf {
        x: q.x,
        y: q.y,
        z: q.z,
        w: q.w,
    }
}

fn from_xr_pose(p: xr::Posef) -> Pose {
    Pose {
        orientation: from_xr_quat(p.orientation),
        position: from_xr_vec3(p.position),
    }
}

fn to_xr_pose(p: Pose) -> xr::Posef {
    xr::Posef {
        orientation: to_xr_quat(p.orientation),
        position: to_xr_vec3(p.position),
    }
}

fn from_xr_fov(f: xr::Fovf) -> Fov {
    Fov {
        left: f.angle_left,
        right: f.angle_right,
        up: f.angle_up,
        down: f.angle_down,
    }
}

fn to_xr_fov(f: Fov) -> xr::Fovf {
    xr::Fovf {
        angle_left: f.left,
        angle_right: f.right,
        angle_up: f.up,
        angle_down: f.down,
    }
}

fn from_xr_time(timestamp: xr::Time) -> Duration {
    Duration::from_nanos(timestamp.as_nanos() as _)
}

fn to_xr_time(timestamp: Duration) -> xr::Time {
    xr::Time::from_nanos(timestamp.as_nanos() as _)
}

fn default_view() -> xr::View {
    xr::View {
        pose: xr::Posef {
            orientation: xr::Quaternionf {
                x: 0.0,
                y: 0.0,
                z: 0.0,
                w: 1.0,
            },
            position: xr::Vector3f::default(),
        },
        fov: xr::Fovf {
            angle_left: -1.0,
            angle_right: 1.0,
            angle_up: 1.0,
            angle_down: -1.0,
        },
    }
}

// This exists to circumvent dead-code analysis
fn create_session(
    xr_instance: &xr::Instance,
    xr_system: xr::SystemId,
    graphics_context: &GraphicsContext,
) -> (
    xr::Session<xr::OpenGlEs>,
    xr::FrameWaiter,
    xr::FrameStream<xr::OpenGlEs>,
) {
    unsafe {
        xr_instance
            .create_session(xr_system, &graphics::session_create_info(graphics_context))
            .unwrap()
    }
}

pub fn entry_point() {
    alvr_client_core::init_logging();

    let platform = alvr_system_info::platform();
    let is_hvr = platform == Platform::Hvr;

    let loader_suffix = match platform {
        Platform::Quest1 => "_quest1",
        Platform::PicoNeo3
        | Platform::PicoG3
        | Platform::Pico4
        | Platform::Pico4Pro
        | Platform::Pico4Enterprise => "_pico_old",
        p if p.is_yvr() => "_yvr",
        Platform::Lynx => "_lynx",
        Platform::Hvr => "_hvr",
        // QiYu SDK v1 is SXR, not an OpenXR loader. Newer firmware can expose
        // the Android generic OpenXR loader; native SXR libraries are packaged
        // for compatibility but a legacy SXR backend is outside this client.
        Platform::Qiyu => "",
        _ => "",
    };
    let xr_entry = unsafe {
        if is_hvr {
            info!("[HVR-DIAG] loading Huawei OpenXR loader");
        }
        xr::Entry::load_from(Path::new(&format!("libopenxr_loader{loader_suffix}.so"))).unwrap()
    };
    if is_hvr {
        info!("[HVR-DIAG] Huawei OpenXR loader loaded");
    }

    #[cfg(target_os = "android")]
    if !should_initialize_android_loader(platform) {
        info!("[HVR-DIAG] legacy loader: using XR_KHR_android_create_instance");
    } else {
        xr_entry.initialize_android_loader().unwrap();
    }

    let available_extensions = xr_entry.enumerate_extensions().unwrap();
    alvr_common::info!("OpenXR available extensions: {available_extensions:#?}");

    // todo: switch to vulkan
    assert!(available_extensions.khr_opengl_es_enable);

    let mut exts = xr::ExtensionSet::default();
    exts.bd_controller_interaction = available_extensions.bd_controller_interaction;
    exts.ext_eye_gaze_interaction = available_extensions.ext_eye_gaze_interaction;
    exts.ext_hand_tracking = available_extensions.ext_hand_tracking;
    exts.ext_local_floor = available_extensions.ext_local_floor;
    exts.fb_body_tracking = available_extensions.fb_body_tracking;
    exts.fb_color_space = available_extensions.fb_color_space;
    exts.fb_composition_layer_settings = available_extensions.fb_composition_layer_settings;
    exts.fb_display_refresh_rate = available_extensions.fb_display_refresh_rate;
    exts.fb_eye_tracking_social = available_extensions.fb_eye_tracking_social;
    exts.fb_face_tracking2 = available_extensions.fb_face_tracking2;
    exts.fb_foveation = available_extensions.fb_foveation;
    exts.fb_foveation_configuration = available_extensions.fb_foveation_configuration;
    exts.fb_passthrough = available_extensions.fb_passthrough;
    exts.fb_swapchain_update_state = available_extensions.fb_swapchain_update_state;
    exts.htc_facial_tracking = available_extensions.htc_facial_tracking;
    exts.htc_passthrough = available_extensions.htc_passthrough;
    exts.htc_vive_focus3_controller_interaction =
        available_extensions.htc_vive_focus3_controller_interaction;
    // Note: Huawei's legacy loader enumerates XR_HUAWEI_controller_interaction as available but
    // rejects enabling it at xrCreateInstance (ERROR_EXTENSION_NOT_PRESENT). The official Huawei
    // demo does not enable it as an instance extension; the 6dof controller interaction profile is
    // suggested without it. Only enable it on non-Huawei runtimes that accept it.
    if !is_hvr {
        exts.huawei_controller_interaction = available_extensions.huawei_controller_interaction;
    }
    #[cfg(target_os = "android")]
    {
        exts.khr_android_create_instance = true;
    }
    exts.khr_convert_timespec_time = available_extensions.khr_convert_timespec_time;
    exts.khr_opengl_es_enable = true;
    exts.other = available_extensions
        .other
        .into_iter()
        .filter(|ext| {
            // QiYu ships its features (foveation, hand tracking, passthrough, boundary, refresh
            // rate, spaces) as vendor extensions rather than the KHR/FB equivalents ALVR enables
            // above, and they are dropped unless explicitly kept here.
            ext.starts_with("XR_QIYU_")
                || [
                    META_BODY_TRACKING_FULL_BODY_EXTENSION_NAME,
                    META_SIMULTANEOUS_HANDS_AND_CONTROLLERS_EXTENSION_NAME,
                    META_DETACHED_CONTROLLERS_EXTENSION_NAME,
                    BD_BODY_TRACKING_EXTENSION_NAME,
                    BD_MOTION_TRACKING_EXTENSION_NAME,
                    PICO_CONFIGURATION_EXTENSION_NAME,
                ]
                .contains(&ext.as_str())
        })
        .collect();
    alvr_common::info!("OpenXR enabled vendor extensions: {:?}", exts.other);

    let available_layers = xr_entry.enumerate_layers().unwrap();
    alvr_common::info!("OpenXR available layers: {available_layers:#?}");

    let xr_instance = xr_entry
        .create_instance(
            &xr::ApplicationInfo {
                application_name: "ALVR Client",
                application_version: 0,
                engine_name: "ALVR",
                engine_version: 0,
            },
            &exts,
            &[],
        )
        .unwrap();
    if is_hvr {
        info!("[HVR-DIAG] OpenXR instance created");
    }

    #[cfg(target_os = "android")]
    let graphics_context = if is_hvr {
        let native_window = hvr::native_window().expect("Huawei NativeWindow unavailable");
        let context = unsafe { GraphicsContext::new_gl_hvr(native_window) }.unwrap();
        info!("[HVR-DIAG] EGL window surface current");
        context
    } else {
        GraphicsContext::new_gl()
    };
    #[cfg(not(target_os = "android"))]
    let graphics_context = GraphicsContext::new_gl();
    let graphics_context = Rc::new(graphics_context);

    let mut last_lobby_message = String::new();

    'session_loop: loop {
        let xr_system = xr_instance
            .system(xr::FormFactor::HEAD_MOUNTED_DISPLAY)
            .unwrap();
        if is_hvr {
            info!("[HVR-DIAG] HMD system acquired");
        }

        // mandatory call
        let _ = xr_instance
            .graphics_requirements::<xr::OpenGlEs>(xr_system)
            .unwrap();

        let (xr_session, mut xr_frame_waiter, mut xr_frame_stream) =
            create_session(&xr_instance, xr_system, &graphics_context);
        if is_hvr {
            info!("[HVR-DIAG] OpenGL ES session created");
        }

        let views_config = xr_instance
            .enumerate_view_configuration_views(
                xr_system,
                xr::ViewConfigurationType::PRIMARY_STEREO,
            )
            .unwrap();
        assert_eq!(views_config.len(), 2);

        let default_view_resolution = UVec2::new(
            views_config[0].recommended_image_rect_width,
            views_config[0].recommended_image_rect_height,
        );

        let refresh_rates = if exts.fb_display_refresh_rate {
            xr_session.enumerate_display_refresh_rates().unwrap()
        } else {
            vec![90.0]
        };

        if exts.fb_color_space {
            xr_session
                .set_color_space(xr::ColorSpaceFB::REC709)
                .unwrap();
        }

        let capabilities = ClientCapabilities {
            default_view_resolution,
            refresh_rates,
            foveated_encoding: platform != Platform::Unknown,
            encoder_high_profile: platform != Platform::Unknown,
            encoder_10_bits: platform != Platform::Unknown,
            encoder_av1: matches!(
                platform,
                Platform::Quest3 | Platform::Quest3S | Platform::Pico4Ultra
            ),
            prefer_10bit: false,
            prefer_full_range: true,
            preferred_encoding_gamma: 1.0,
            prefer_hdr: false,
        };
        let core_context = Arc::new(ClientCoreContext::new(capabilities));

        let interaction_context = Arc::new(RwLock::new(InteractionContext::new(
            xr_session.clone(),
            exts.other.clone(),
            xr_system,
            platform,
        )));

        // Session lifecycle state. Populated by the SessionState::READY event in the render loop
        // below. On Huawei, the swapchain GL texture handles are only valid in-frame (after the
        // first xrAcquireSwapchainImage/xrWaitSwapchainImage), so the lobby renderer defers wrapping
        // them until then (see graphics::LobbyRenderer deferred path). No special session ordering
        // is needed here; all platforms share this flow.
        let mut session_running = false;
        let mut passthrough_layer = None;

        let mut lobby = Lobby::new(
            xr_session.clone(),
            Rc::clone(&graphics_context),
            Arc::clone(&interaction_context),
            platform,
            default_view_resolution,
            &last_lobby_message,
        );
        let lobby_body_tracking_config = BodyTrackingSourcesConfig {
            body_tracking_fb: Switch::Disabled,
            body_tracking_bd: Switch::Enabled(BodyTrackingBDConfig::BodyTracking {
                high_accuracy: true,
                prompt_calibration_on_start: false,
            }),
        };
        let lobby_interaction_sources = InteractionSourcesConfig {
            face_tracking: None,
            body_tracking: Some(lobby_body_tracking_config),
            prefers_multimodal_input: true,
        };
        interaction_context
            .write()
            .select_sources(&lobby_interaction_sources);

        let mut logged_first_frame = false;
        let mut logged_hvr_should_render_false = false;
        let mut hvr_stream_frame_diag_count: u32 = 0;
        let mut stream_context = None::<StreamContext>;
        let opaque_blend_mode = environment_blend_mode(platform);
        let alpha_blend_supported = supports_alpha_blend(&xr_instance, xr_system);
        let mut environment_blend_mode = opaque_blend_mode;

        let mut event_storage = xr::EventDataBuffer::new();
        'render_loop: loop {
            while let Some(event) = xr_instance.poll_event(&mut event_storage).unwrap() {
                match event {
                    xr::Event::EventsLost(event) => {
                        error!("OpenXR: lost {} events!", event.lost_event_count());
                    }
                    xr::Event::InstanceLossPending(_) => break 'session_loop,
                    xr::Event::SessionStateChanged(event) => match event.state() {
                        xr::SessionState::READY => {
                            if is_hvr {
                                info!("[HVR-DIAG] session READY; beginning session");
                            }
                            xr_session
                                .begin(xr::ViewConfigurationType::PRIMARY_STEREO)
                                .unwrap();

                            core_context.resume();

                            passthrough_layer = PassthroughLayer::new(&xr_session, platform).ok();

                            session_running = true;
                        }
                        xr::SessionState::STOPPING => {
                            if is_hvr {
                                info!("[HVR-DIAG] session STOPPING");
                            }
                            session_running = false;

                            passthrough_layer = None;

                            core_context.pause();

                            xr_session.end().unwrap();
                        }
                        xr::SessionState::EXITING | xr::SessionState::LOSS_PENDING => {
                            break 'render_loop
                        }
                        _ => (),
                    },
                    xr::Event::ReferenceSpaceChangePending(event) => {
                        info!(
                            "ReferenceSpaceChangePending type: {:?}",
                            event.reference_space_type()
                        );

                        lobby.update_reference_space();

                        if let Some(stream) = &mut stream_context {
                            stream.update_reference_space();
                        }
                    }
                    xr::Event::PerfSettingsEXT(event) => {
                        info!(
                            "Perf: from {:?} to {:?}, domain: {:?}/{:?}",
                            event.from_level(),
                            event.to_level(),
                            event.domain(),
                            event.sub_domain(),
                        );
                    }
                    xr::Event::InteractionProfileChanged(_)
                    | xr::Event::PassthroughStateChangedFB(_) => {
                        // Log the newly activated interaction profile per hand. On QiYu this
                        // surfaces the runtime's real /interaction_profiles/... path so controller
                        // bindings can be matched precisely.
                        for (hand_name, hand_path) in
                            [("left", "/user/hand/left"), ("right", "/user/hand/right")]
                        {
                            if let Ok(top_level) = xr_instance.string_to_path(hand_path) {
                                match xr_session.current_interaction_profile(top_level) {
                                    Ok(profile_path) => {
                                        let profile_str = xr_instance
                                            .path_to_string(profile_path)
                                            .unwrap_or_else(|_| "<unknown>".into());
                                        info!(
                                            "[PROFILE-DIAG] {hand_name} hand interaction profile changed to: '{profile_str}'"
                                        );
                                    }
                                    Err(error) => info!(
                                        "[PROFILE-DIAG] {hand_name} hand profile query failed: {error}"
                                    ),
                                }
                            }
                        }
                    }
                    _ => (),
                }
            }

            if !session_running {
                thread::sleep(Duration::from_millis(100));
                continue;
            }

            while let Some(event) = core_context.poll_event() {
                match event {
                    ClientCoreEvent::UpdateHudMessage(message) => {
                        if is_hvr {
                            info!("[HVR-DIAG] HUD updated: {message:?}");
                        }
                        last_lobby_message.clone_from(&message);
                        lobby.update_hud_message(&message);
                    }
                    ClientCoreEvent::StreamingStarted(config) => {
                        let config = ParsedStreamConfig::new(&config);

                        let context = StreamContext::new(
                            Arc::clone(&core_context),
                            xr_session.clone(),
                            Rc::clone(&graphics_context),
                            Arc::clone(&interaction_context),
                            platform,
                            config,
                        );

                        if !context.uses_passthrough() {
                            passthrough_layer = None;
                        }

                        // With no passthrough composition layer the real world can still show
                        // through the stream, but only if the runtime blends the submitted layers
                        // against it. Ask for that mode while the server wants passthrough.
                        if alpha_blend_supported && passthrough_layer.is_none() {
                            environment_blend_mode = if context.uses_passthrough() {
                                xr::EnvironmentBlendMode::ALPHA_BLEND
                            } else {
                                opaque_blend_mode
                            };
                        }

                        stream_context = Some(context);
                    }
                    ClientCoreEvent::StreamingStopped => {
                        environment_blend_mode = opaque_blend_mode;

                        if passthrough_layer.is_none() {
                            passthrough_layer = PassthroughLayer::new(&xr_session, platform).ok();
                        }

                        interaction_context
                            .write()
                            .select_sources(&lobby_interaction_sources);

                        stream_context = None;
                    }
                    ClientCoreEvent::Haptics {
                        device_id,
                        duration,
                        frequency,
                        amplitude,
                    } => {
                        let idx = if device_id == *HAND_LEFT_ID { 0 } else { 1 };

                        // QiYu's simple-controller haptic output is not driven by the runtime, so
                        // route vibration through the same Native SDK bridge that supplies its
                        // buttons.
                        #[cfg(target_os = "android")]
                        let handled_by_qiyu =
                            platform == Platform::Qiyu && interaction::qiyu_controllers_ready();
                        #[cfg(not(target_os = "android"))]
                        let handled_by_qiyu = false;

                        if handled_by_qiyu {
                            #[cfg(target_os = "android")]
                            interaction::qiyu_controllers_vibrate(
                                idx,
                                amplitude.clamp(0.0, 1.0),
                                duration.as_secs_f32(),
                            );
                        } else {
                            let interaction_context = interaction_context.read();
                            let hand = &interaction_context.hands_interaction[idx];

                            hand.vibration_action
                                .apply_feedback(
                                    &xr_session,
                                    hand.subaction_path,
                                    &xr::HapticVibration::new()
                                        .amplitude(amplitude.clamp(0.0, 1.0))
                                        .frequency(frequency.max(0.0))
                                        .duration(xr::Duration::from_nanos(
                                            duration.as_nanos() as _
                                        )),
                                )
                                .unwrap();
                        }
                    }
                    ClientCoreEvent::DecoderConfig { codec, config_nal } => {
                        if let Some(stream) = &mut stream_context {
                            stream.maybe_initialize_decoder(codec, config_nal);
                        }
                    }
                    ClientCoreEvent::RealTimeConfig(config) => {
                        if config.passthrough.is_some() && passthrough_layer.is_none() {
                            passthrough_layer = PassthroughLayer::new(&xr_session, platform).ok();
                        } else if config.passthrough.is_none() && passthrough_layer.is_some() {
                            passthrough_layer = None;
                        }

                        if alpha_blend_supported && passthrough_layer.is_none() {
                            environment_blend_mode = if config.passthrough.is_some() {
                                xr::EnvironmentBlendMode::ALPHA_BLEND
                            } else {
                                opaque_blend_mode
                            };
                        }

                        if let Some(stream) = &mut stream_context {
                            stream.update_real_time_config(&config);
                        }
                    }
                }
            }

            let frame_state_result = if is_hvr {
                graphics::hvr_wait_frame(&xr_session)
            } else {
                xr_frame_waiter.wait()
            };
            let frame_state = match frame_state_result {
                Ok(state) => state,
                Err(e) => {
                    error!("{e}");
                    panic!();
                }
            };
            let frame_interval =
                Duration::from_nanos(frame_state.predicted_display_period.as_nanos() as _);
            let vsync_time =
                Duration::from_nanos(frame_state.predicted_display_time.as_nanos() as _);

            // On Huawei, xrBeginFrame can transiently fail with ERROR_VALIDATION_FAILURE until the
            // session settles into a frame-accepting state. Proceeding to render with a frame that
            // was not begun crashes Huawei's native runtime (SIGSEGV in libvrsdk), so on HVR we skip
            // the rest of this frame and retry next iteration instead of rendering. Other platforms
            // keep the strict unwrap.
            let begin_result = if is_hvr {
                graphics::hvr_begin_frame(&xr_session)
            } else {
                xr_frame_stream.begin()
            };

            // Visible (error-level) diagnostic for the first stream frames: info! is filtered out
            // of Huawei's logcat, so use error! to confirm whether the stream frame loop stalls in
            // wait/begin. Rate-limited to the first frames after streaming starts.
            if is_hvr && stream_context.is_some() && hvr_stream_frame_diag_count < 10 {
                hvr_stream_frame_diag_count += 1;
                error!(
                    "[HVR-DIAG] stream frame {hvr_stream_frame_diag_count}: should_render={}, begin_result={begin_result:?}",
                    frame_state.should_render
                );
            }

            match begin_result {
                Ok(()) => {}
                Err(e) if is_hvr => {
                    if !logged_first_frame {
                        info!(
                            "[HVR-DIAG] xrBeginFrame returned {e:?}; skipping frame (will retry)"
                        );
                    }
                    thread::sleep(Duration::from_millis(10));
                    continue;
                }
                Err(e) => panic!("xrBeginFrame failed: {e:?}"),
            }

            // Huawei's runtime drops shouldRender to false whenever its SDK considers the app
            // backgrounded (visible in logcat as "App is on background, do nothing"), and it does
            // so while the session is still focused and rendering. Submitting zero layers then
            // leaves the compositor with nothing to show, which is exactly the flash-then-black
            // symptom. The official HVRSDK_XrDemo never reads shouldRender: its render loop calls
            // RenderFrame unconditionally while the session runs, and always submits the projection
            // layer. Follow the sample on HVR and keep the spec-correct skip everywhere else.
            if !frame_state.should_render && !is_hvr {
                xr_frame_stream
                    .end(
                        frame_state.predicted_display_time,
                        environment_blend_mode,
                        &[],
                    )
                    .unwrap();

                continue;
            }

            if is_hvr && !frame_state.should_render && !logged_hvr_should_render_false {
                logged_hvr_should_render_false = true;
                error!(
                    "[HVR-DIAG] shouldRender went false; rendering anyway to match HVRSDK_XrDemo"
                );
            }

            // todo: allow rendering lobby and stream layers at the same time and add cross fade
            let (layer, display_time) = if let Some(stream) = &mut stream_context {
                if is_hvr && hvr_stream_frame_diag_count < 10 {
                    error!("[HVR-DIAG] entering stream.render (frame diag {hvr_stream_frame_diag_count})");
                }
                let out = stream.render(frame_interval, vsync_time);
                if is_hvr && hvr_stream_frame_diag_count < 10 {
                    error!("[HVR-DIAG] returned from stream.render (frame diag {hvr_stream_frame_diag_count})");
                }
                out
            } else {
                (lobby.render(vsync_time), vsync_time)
            };

            let layers: &[&xr::CompositionLayerBase<_>] =
                if let Some(passthrough_layer) = &passthrough_layer {
                    &[passthrough_layer, &layer.build()]
                } else {
                    &[&layer.build()]
                };

            // Huawei/Wolvic keep one externally-owned EGL context current throughout the frame.
            // Rebinding it after xrWaitFrame returns EGL_BAD_ACCESS on Huawei's driver.
            if !is_hvr {
                graphics_context.make_current();
            }
            let res = xr_frame_stream.end(to_xr_time(display_time), environment_blend_mode, layers);

            if is_hvr && !logged_first_frame {
                info!("[HVR-DIAG] first rendered frame result: {res:?}");
                logged_first_frame = true;
            }

            if let Err(e) = res {
                let time = to_xr_time(display_time);
                error!("End frame failed! {e}, timestamp: {display_time:?}, time: {time:?}");

                if is_hvr {
                    // Huawei's runtime returns non-success from xrEndFrame but keeps working; its
                    // demo ignores the code. Don't retry-then-panic; just continue.
                } else if !platform.is_vive() {
                    xr_frame_stream
                        .end(
                            frame_state.predicted_display_time,
                            environment_blend_mode,
                            &[],
                        )
                        .unwrap();
                }
            }
        }
    }

    // grapics_context is dropped here
}

#[allow(unused)]
fn xr_runtime_now(xr_instance: &xr::Instance) -> Option<xr::Time> {
    #[cfg(target_os = "android")]
    if xr_instance.exts().khr_convert_timespec_time.is_none() {
        // Huawei's legacy runtime does not expose KHR_convert_timespec_time, but its official
        // sample uses CLOCK_MONOTONIC nanoseconds directly as XrTime.
        let mut time = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut time) } != 0 {
            return None;
        }

        let nanos = (time.tv_sec as i64)
            .checked_mul(1_000_000_000)?
            .checked_add(time.tv_nsec as i64)?;

        return (nanos > 0).then(|| xr::Time::from_nanos(nanos));
    }

    xr_instance
        .now()
        .ok()
        .filter(|&time_nanos| time_nanos.as_nanos() > 0)
}

#[cfg(target_os = "android")]
#[no_mangle]
fn android_main(app: android_activity::AndroidApp) {
    use android_activity::{InputStatus, MainEvent, PollEvent};

    alvr_client_core::init_logging();

    let platform = alvr_system_info::platform();
    // A091/kona's native SXR path crashes inside QVR_Init because the QiYu SDK requires the
    // official Java VRActivity architecture (custom Activity + SurfaceView + HandlerThread),
    // which this NativeActivity-based client does not provide. Confirmed by tombstone:
    //   #00 QVR_Init+200 libqiyivrsdkcore.so (null deref)  #01 qiyu_Init libqiyuapi.so
    // So only legacy SXR headsets (A5/A6-B) use the native backend; A091 falls through to the
    // working OpenXR path (video + ray-following controller via khr/simple_controller).
    if platform == Platform::Qiyu && alvr_system_info::qiyu_uses_native_sxr() {
        qiyu::entry_point(app);
        return;
    }

    if platform == Platform::Hvr {
        match hvr::initialize_bridge(&app) {
            Ok(method) => alvr_common::info!("[HVR-DIAG] Huawei runtime loaded via {method}"),
            Err(error) => alvr_common::error!("[HVR-DIAG] LibUpdateClient failed: {error}"),
        }
    }

    let mut should_quit = false;
    if platform == Platform::Hvr {
        while hvr::native_window().is_none() && !should_quit {
            app.poll_events(Some(Duration::from_millis(100)), |event| match event {
                PollEvent::Main(MainEvent::Destroy) => should_quit = true,
                PollEvent::Main(MainEvent::InputAvailable) => {
                    if let Ok(mut iter) = app.input_events_iter() {
                        while iter.next(|_| InputStatus::Unhandled) {}
                    }
                }
                _ => (),
            });

            if let Some(window) = app.native_window() {
                hvr::set_native_window(window.ptr().as_ptr().cast());
                alvr_common::info!("[HVR-DIAG] NativeWindow acquired");
            }
        }
    }

    if should_quit {
        return;
    }

    let rendering_thread = thread::spawn(|| {
        // workaround for the Pico runtime
        let context = ndk_context::android_context();
        let vm = unsafe { jni::JavaVM::from_raw(context.vm().cast()) }.unwrap();
        let _env = vm.attach_current_thread().unwrap();

        entry_point();
    });

    while !should_quit {
        app.poll_events(Some(Duration::from_millis(100)), |event| match event {
            PollEvent::Main(MainEvent::Destroy) => {
                should_quit = true;
            }
            PollEvent::Main(MainEvent::InputAvailable) => {
                if let Ok(mut iter) = app.input_events_iter() {
                    while iter.next(|_| InputStatus::Unhandled) {}
                }
            }
            _ => (),
        });
    }

    // Note: the quit event is sent from OpenXR too, this will return rather quicly.
    rendering_thread.join().unwrap();
}
