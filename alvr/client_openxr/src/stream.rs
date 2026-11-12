use crate::{
    graphics::{self, ProjectionLayerAlphaConfig, ProjectionLayerBuilder},
    interaction::{self, InteractionContext, InteractionSourcesConfig},
};
use alvr_client_core::{
    video_decoder::{self, VideoDecoderConfig, VideoDecoderSource},
    ClientCoreContext,
};
use alvr_common::{
    anyhow::Result,
    error,
    glam::{UVec2, Vec2},
    parking_lot::RwLock,
    Pose, RelaxedAtomic, ViewParams, HAND_LEFT_ID, HAND_RIGHT_ID, HEAD_ID,
};
use alvr_graphics::{
    compute_target_view_resolution, GraphicsContext, StreamRenderer, StreamViewParams,
};
use alvr_packets::{FaceData, RealTimeConfig, StreamConfig};
use alvr_session::{
    ClientsideFoveationConfig, ClientsideFoveationMode, ClientsidePostProcessingConfig, CodecType,
    FoveatedEncodingConfig, MediacodecProperty, PassthroughMode, UpscalingConfig,
};
use alvr_system_info::Platform;
use openxr as xr;
use std::{
    collections::HashMap,
    ptr,
    rc::Rc,
    sync::Arc,
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const DECODER_MAX_TIMEOUT_MULTIPLIER: f32 = 0.8;

fn uses_custom_reprojection(platform: Platform) -> bool {
    platform.is_yvr() || platform == Platform::Hvr
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hvr_uses_current_runtime_views_for_projection() {
        assert!(uses_custom_reprojection(Platform::Hvr));
        assert!(uses_custom_reprojection(Platform::Yvr));
        assert!(!uses_custom_reprojection(Platform::Quest2));
    }
}

pub struct ParsedStreamConfig {
    pub view_resolution: UVec2,
    pub refresh_rate_hint: f32,
    pub use_full_range: bool,
    pub encoding_gamma: f32,
    pub enable_hdr: bool,
    pub passthrough: Option<PassthroughMode>,
    pub foveated_encoding_config: Option<FoveatedEncodingConfig>,
    pub clientside_foveation_config: Option<ClientsideFoveationConfig>,
    pub clientside_post_processing: Option<ClientsidePostProcessingConfig>,
    pub upscaling: Option<UpscalingConfig>,
    pub force_software_decoder: bool,
    pub max_buffering_frames: f32,
    pub buffering_history_weight: f32,
    pub decoder_options: Vec<(String, MediacodecProperty)>,
    pub interaction_sources: InteractionSourcesConfig,
}

impl ParsedStreamConfig {
    pub fn new(config: &StreamConfig) -> Self {
        Self {
            view_resolution: config.negotiated_config.view_resolution,
            refresh_rate_hint: config.negotiated_config.refresh_rate_hint,
            use_full_range: config.negotiated_config.use_full_range,
            encoding_gamma: config.negotiated_config.encoding_gamma,
            enable_hdr: config.negotiated_config.enable_hdr,
            passthrough: config.settings.video.passthrough.as_option().cloned(),
            foveated_encoding_config: config
                .negotiated_config
                .enable_foveated_encoding
                .then(|| config.settings.video.foveated_encoding.as_option().cloned())
                .flatten(),
            clientside_foveation_config: config
                .settings
                .video
                .clientside_foveation
                .as_option()
                .cloned(),
            clientside_post_processing: config
                .settings
                .video
                .clientside_post_processing
                .as_option()
                .cloned(),
            upscaling: config.settings.video.upscaling.as_option().cloned(),
            force_software_decoder: config.settings.video.force_software_decoder,
            max_buffering_frames: config.settings.video.max_buffering_frames,
            buffering_history_weight: config.settings.video.buffering_history_weight,
            decoder_options: config.settings.video.mediacodec_extra_options.clone(),
            interaction_sources: InteractionSourcesConfig::new(config),
        }
    }
}

pub struct StreamContext {
    core_context: Arc<ClientCoreContext>,
    xr_session: xr::Session<xr::OpenGlEs>,
    interaction_context: Arc<RwLock<InteractionContext>>,
    platform: Platform,
    reference_space_type: xr::ReferenceSpaceType,
    stage_reference_space: Arc<xr::Space>,
    hvr_reference_space: Option<Arc<xr::Space>>,
    view_reference_space: Arc<xr::Space>,
    swapchains: [xr::Swapchain<xr::OpenGlEs>; 2],
    last_good_view_params: [ViewParams; 2],
    input_thread: Option<JoinHandle<()>>,
    input_thread_running: Arc<RelaxedAtomic>,
    config: ParsedStreamConfig,
    target_view_resolution: UVec2,
    renderer: StreamRenderer,
    decoder: Option<(VideoDecoderConfig, VideoDecoderSource)>,
    use_custom_reprojection: bool,
    /// Counts every decoded video frame handed to the renderer. Only used for HVR diagnostics.
    hvr_video_frame_count: u32,
    hvr_target_wait_frames: u32,
    hvr_context_restores: u32,
    hvr_swapchains_primed: bool,
    hvr_frame_index: u32,
    hvr_previous_frame_at: Option<Instant>,
    hvr_previous_had_video: Option<bool>,
}

impl StreamContext {
    pub fn new(
        core_ctx: Arc<ClientCoreContext>,
        xr_session: xr::Session<xr::OpenGlEs>,
        gfx_ctx: Rc<GraphicsContext>,
        interaction_ctx: Arc<RwLock<InteractionContext>>,
        platform: Platform,
        config: ParsedStreamConfig,
        hvr_reference_space: Option<Arc<xr::Space>>,
    ) -> StreamContext {
        interaction_ctx
            .write()
            .select_sources(&config.interaction_sources);

        let xr_exts = xr_session.instance().exts();

        if xr_exts.fb_display_refresh_rate.is_some() {
            xr_session
                .request_display_refresh_rate(config.refresh_rate_hint)
                .unwrap();
        }

        let foveation_profile = if let Some(config) = &config.clientside_foveation_config {
            if xr_exts.fb_swapchain_update_state.is_some()
                && xr_exts.fb_foveation.is_some()
                && xr_exts.fb_foveation_configuration.is_some()
            {
                let level;
                let dynamic;
                match config.mode {
                    ClientsideFoveationMode::Static { level: lvl } => {
                        level = lvl;
                        dynamic = false;
                    }
                    ClientsideFoveationMode::Dynamic { max_level } => {
                        level = max_level;
                        dynamic = true;
                    }
                };

                xr_session
                    .create_foveation_profile(Some(xr::FoveationLevelProfile {
                        level: xr::FoveationLevelFB::from_raw(level as i32),
                        vertical_offset: config.vertical_offset_deg,
                        dynamic: xr::FoveationDynamicFB::from_raw(dynamic as i32),
                    }))
                    .ok()
            } else {
                None
            }
        } else {
            None
        };

        let target_view_resolution =
            compute_target_view_resolution(config.view_resolution, &config.upscaling);
        let format = graphics::swapchain_format(&gfx_ctx, &xr_session, config.enable_hdr);

        let swapchains = [
            graphics::create_swapchain(
                &xr_session,
                &gfx_ctx,
                target_view_resolution,
                format,
                foveation_profile.as_ref(),
                platform,
            ),
            graphics::create_swapchain(
                &xr_session,
                &gfx_ctx,
                target_view_resolution,
                format,
                foveation_profile.as_ref(),
                platform,
            ),
        ];

        let renderer = StreamRenderer::new(
            gfx_ctx,
            config.view_resolution,
            target_view_resolution,
            [
                swapchains[0]
                    .enumerate_images()
                    .unwrap()
                    .iter()
                    .map(|i| *i as _)
                    .collect(),
                swapchains[1]
                    .enumerate_images()
                    .unwrap()
                    .iter()
                    .map(|i| *i as _)
                    .collect(),
            ],
            format,
            config.foveated_encoding_config.clone(),
            platform != Platform::Lynx && !((platform.is_pico()) && config.enable_hdr),
            config.use_full_range && !config.enable_hdr, // TODO: figure out why HDR doesn't need the limited range hackfix in staging?
            config.encoding_gamma,
            config.upscaling.clone(),
        );

        if platform == Platform::Hvr {
            // Huawei's logd drops info/warn records while the vendor SDK floods the buffer, so
            // every HVR diagnostic that has to survive a capture is logged at error level.
            alvr_common::error!(
                "[HVR-DIAG] stream renderer created: targets_ready={}",
                renderer.render_targets_ready()
            );
        }

        core_ctx.send_active_interaction_profile(
            *HAND_LEFT_ID,
            interaction_ctx.read().hands_interaction[0].controllers_profile_id,
        );
        core_ctx.send_active_interaction_profile(
            *HAND_RIGHT_ID,
            interaction_ctx.read().hands_interaction[1].controllers_profile_id,
        );

        let input_thread_running = Arc::new(RelaxedAtomic::new(false));

        let reference_space_type = crate::tracking_reference_space_type(platform);
        let stage_reference_space = if let Some(space) = &hvr_reference_space {
            Arc::clone(space)
        } else {
            Arc::new(interaction::get_reference_space(
                &xr_session,
                reference_space_type,
            ))
        };
        let view_reference_space = Arc::new(interaction::get_reference_space(
            &xr_session,
            xr::ReferenceSpaceType::VIEW,
        ));

        let mut this = StreamContext {
            core_context: core_ctx,
            xr_session,
            interaction_context: interaction_ctx,
            platform,
            reference_space_type,
            stage_reference_space,
            hvr_reference_space,
            view_reference_space,
            swapchains,
            last_good_view_params: [ViewParams::default(); 2],
            input_thread: None,
            input_thread_running,
            config,
            target_view_resolution,
            renderer,
            decoder: None,
            use_custom_reprojection: uses_custom_reprojection(platform),
            hvr_video_frame_count: 0,
            hvr_target_wait_frames: 0,
            hvr_context_restores: 0,
            hvr_swapchains_primed: false,
            hvr_frame_index: 0,
            hvr_previous_frame_at: None,
            hvr_previous_had_video: None,
        };

        this.update_reference_space();

        this
    }

    pub fn uses_passthrough(&self) -> bool {
        self.config.passthrough.is_some()
    }

    pub fn update_reference_space(&mut self) {
        self.input_thread_running.set(false);

        // Recreating the Huawei space would re-anchor it to the current head pose, so the streamed
        // world would face wherever the user happened to be looking. Keep the one created at
        // session setup, which the lobby also uses.
        if self.hvr_reference_space.is_none() {
            self.stage_reference_space = Arc::new(interaction::get_reference_space(
                &self.xr_session,
                self.reference_space_type,
            ));
        }
        self.view_reference_space = Arc::new(interaction::get_reference_space(
            &self.xr_session,
            xr::ReferenceSpaceType::VIEW,
        ));

        // Huawei's legacy runtime returns an error from xrGetReferenceSpaceBoundsRect, and it is
        // called again on every ReferenceSpaceChangePending event, so unwrapping here crashed the
        // client mid-session. A missing playspace is already a supported state.
        self.core_context.send_playspace(
            self.xr_session
                .reference_space_bounds_rect(self.reference_space_type)
                .ok()
                .flatten()
                .map(|a| Vec2::new(a.width, a.height)),
        );

        if let Some(running) = self.input_thread.take() {
            running.join().ok();
        }

        self.input_thread_running.set(true);

        self.input_thread = Some(thread::spawn({
            let core_ctx = Arc::clone(&self.core_context);
            let xr_session = self.xr_session.clone();
            let interaction_ctx = Arc::clone(&self.interaction_context);
            let stage_reference_space = Arc::clone(&self.stage_reference_space);
            let view_reference_space = Arc::clone(&self.view_reference_space);
            let refresh_rate = self.config.refresh_rate_hint;
            let running = Arc::clone(&self.input_thread_running);
            #[cfg(target_os = "android")]
            let platform = self.platform;
            move || {
                // Huawei's OpenXR runtime performs JNI calls (e.g. inside xrSyncActions) on the
                // calling thread. Unlike the render thread (attached in android_main), this input
                // thread is not attached to the JVM, so those JNI calls dereference a null
                // JNIEnv/array and crash in libvrsdk (art::JNI::GetArrayLength). Attach the thread
                // for the lifetime of the loop; `vm` and `_env` must both stay alive because the
                // attach guard borrows the vm.
                #[cfg(target_os = "android")]
                let vm = (platform == Platform::Hvr)
                    .then(|| {
                        let context = ndk_context::android_context();
                        unsafe { jni::JavaVM::from_raw(context.vm().cast()) }.ok()
                    })
                    .flatten();
                #[cfg(target_os = "android")]
                let _env = vm.as_ref().and_then(|vm| match vm.attach_current_thread() {
                    Ok(env) => Some(env),
                    Err(e) => {
                        alvr_common::error!("[HVR-DIAG] failed to attach input thread to JVM: {e}");
                        None
                    }
                });

                stream_input_loop(
                    &core_ctx,
                    xr_session,
                    &interaction_ctx,
                    &stage_reference_space,
                    &view_reference_space,
                    refresh_rate,
                    running,
                )
            }
        }));
    }

    pub fn maybe_initialize_decoder(&mut self, codec: CodecType, config_nal: Vec<u8>) {
        let new_config = VideoDecoderConfig {
            codec,
            force_software_decoder: self.config.force_software_decoder,
            max_buffering_frames: self.config.max_buffering_frames,
            buffering_history_weight: self.config.buffering_history_weight,
            options: self.config.decoder_options.clone(),
            config_buffer: config_nal,
        };

        let maybe_config = if let Some((config, _)) = &self.decoder {
            (new_config != *config).then_some(new_config)
        } else {
            Some(new_config)
        };

        if let Some(config) = maybe_config {
            let (mut sink, source) = video_decoder::create_decoder(config.clone(), {
                let ctx = Arc::clone(&self.core_context);
                move |maybe_timestamp: Result<Duration>| match maybe_timestamp {
                    Ok(timestamp) => ctx.report_frame_decoded(timestamp),
                    Err(e) => ctx.report_fatal_decoder_error(&e.to_string()),
                }
            });
            self.decoder = Some((config, source));

            self.core_context.set_decoder_input_callback(Box::new(
                move |timestamp, buffer| -> bool { sink.push_nal(timestamp, buffer) },
            ));
        }
    }

    pub fn update_real_time_config(&mut self, config: &RealTimeConfig) {
        self.config.passthrough = config.passthrough.clone();
        self.config.clientside_post_processing = config.clientside_post_processing.clone();
    }

    pub fn render(
        &mut self,
        frame_interval: Duration,
        vsync_time: Duration,
    ) -> (ProjectionLayerBuilder, Duration) {
        let xr_vsync_time = xr::Time::from_nanos(vsync_time.as_nanos() as _);
        let frame_poll_deadline = Instant::now()
            + Duration::from_secs_f32(
                frame_interval.as_secs_f32() * DECODER_MAX_TIMEOUT_MULTIPLIER,
            );
        let mut frame_result = None;
        if let Some((_, source)) = &mut self.decoder {
            while frame_result.is_none() && Instant::now() < frame_poll_deadline {
                frame_result = source.get_frame();
                thread::sleep(Duration::from_micros(500));
            }
        }

        let (timestamp, view_params, buffer_ptr) =
            if let Some((timestamp, buffer_ptr)) = frame_result {
                let view_params = self.core_context.report_compositor_start(timestamp);

                self.last_good_view_params = view_params;

                (timestamp, view_params, buffer_ptr)
            } else {
                (vsync_time, self.last_good_view_params, ptr::null_mut())
            };

        let is_hvr = self.platform == Platform::Hvr;

        if !buffer_ptr.is_null() {
            self.hvr_video_frame_count += 1;
            if is_hvr
                && (self.hvr_video_frame_count <= 2 || self.hvr_video_frame_count % 300 == 0)
            {
                alvr_common::error!(
                    "[HVR-DIAG] decoded video frame {}: timestamp={timestamp:?}, vsync={vsync_time:?}",
                    self.hvr_video_frame_count
                );
            }
        }

        let left_swapchain_idx = if is_hvr {
            // Populate the Huawei GL texture handles in one go, so the stream does not skip a frame
            // per swapchain image while they trickle in. Only valid inside an active frame.
            if !self.hvr_swapchains_primed {
                self.hvr_swapchains_primed = true;
                for swapchain in &self.swapchains {
                    graphics::hvr_prime_swapchain(swapchain);
                }
            }

            graphics::hvr_acquire_image(&mut self.swapchains[0]).unwrap_or(0)
        } else {
            self.swapchains[0].acquire_image().unwrap()
        };
        let right_swapchain_idx = if is_hvr {
            graphics::hvr_acquire_image(&mut self.swapchains[1]).unwrap_or(0)
        } else {
            self.swapchains[1].acquire_image().unwrap()
        };

        if is_hvr {
            let _ = graphics::hvr_wait_image(&mut self.swapchains[0]);
            let _ = graphics::hvr_wait_image(&mut self.swapchains[1]);
        } else {
            self.swapchains[0]
                .wait_image(xr::Duration::INFINITE)
                .unwrap();
            self.swapchains[1]
                .wait_image(xr::Duration::INFINITE)
                .unwrap();
        }

        // Huawei's runtime binds its own EGL context on this thread inside the swapchain calls.
        // Drawing without rebinding ours sends every GL command to the wrong context, which is why
        // the streamed image intermittently freezes or goes black. The lobby already does this.
        #[cfg(target_os = "android")]
        if is_hvr {
            match self.renderer.ensure_hvr_current() {
                Ok(true) => {
                    self.hvr_context_restores += 1;
                    if self.hvr_context_restores <= 3 || self.hvr_context_restores % 600 == 0 {
                        alvr_common::error!(
                            "[HVR-DIAG] restored Huawei EGL context after stream swapchain wait \
                             ({} times)",
                            self.hvr_context_restores
                        );
                    }
                }
                Ok(false) => {}
                Err(error) => alvr_common::error!("[HVR-DIAG] {error}"),
            }
        }

        // Huawei returns zero swapchain texture handles until the images are acquired/waited inside
        // an active frame. Now that they have been, re-enumerate and (re)build the render targets.
        if is_hvr && !self.renderer.render_targets_ready() {
            if let (Ok(left_handles), Ok(right_handles)) = (
                self.swapchains[0].enumerate_images(),
                self.swapchains[1].enumerate_images(),
            ) {
                let left: Vec<u32> = left_handles.iter().map(|&h| h as u32).collect();
                let right: Vec<u32> = right_handles.iter().map(|&h| h as u32).collect();
                if left.iter().all(|&h| h != 0) && right.iter().all(|&h| h != 0) {
                    self.renderer.set_swapchain_textures([left, right]);
                    alvr_common::error!(
                        "[HVR-DIAG] stream render targets built from in-frame handles"
                    );
                } else {
                    // Until this succeeds the stream renderer draws nothing, so the headset shows
                    // a frozen image. Report the progression (the lobby fills one handle per
                    // acquired image) so a capture distinguishes "still filling" from "stuck".
                    self.hvr_target_wait_frames += 1;
                    if self.hvr_target_wait_frames <= 8 || self.hvr_target_wait_frames % 300 == 0
                    {
                        alvr_common::error!(
                            "[HVR-DIAG] stream targets not ready after \
                             {} frames: acquired idx L={left_swapchain_idx} R={right_swapchain_idx}, \
                             handles left={left:?} right={right:?}",
                            self.hvr_target_wait_frames
                        );
                    }
                }
            } else {
                alvr_common::error!("[HVR-DIAG] stream in-frame enumerate_images failed");
            }
        }

        // Probe: on HVR, record every frame that could be the one the user sees glitch. Sampling
        // periodically missed it, so report on change instead: the first frames, any frame that
        // took noticeably longer than a vsync, and every transition between having and not having
        // a decoded video frame.
        if is_hvr {
            self.hvr_frame_index += 1;
            let index = self.hvr_frame_index;

            let has_video = !buffer_ptr.is_null();
            let since_previous = self
                .hvr_previous_frame_at
                .map(|at| at.elapsed().as_millis())
                .unwrap_or(0);

            let slow = since_previous > 50;
            let video_changed = self.hvr_previous_had_video != Some(has_video);
            if index <= 20 || slow || video_changed || index % 600 == 0 {
                let reason = if video_changed {
                    "video-change"
                } else if slow {
                    "slow"
                } else {
                    "sample"
                };
                alvr_common::error!(
                    "[HVR-PROBE] frame {index} ({reason}): dt={since_previous}ms video={has_video} \
                     targets_ready={} decoder={} idx=L{left_swapchain_idx}/R{right_swapchain_idx} \
                     pose={:?}",
                    self.renderer.render_targets_ready(),
                    self.decoder.is_some(),
                    view_params[0].pose.position,
                );
            }

            self.hvr_previous_had_video = Some(has_video);
            self.hvr_previous_frame_at = Some(Instant::now());
        }

        let (flags, maybe_views) = self
            .xr_session
            .locate_views(
                xr::ViewConfigurationType::PRIMARY_STEREO,
                xr_vsync_time,
                &self.stage_reference_space,
            )
            .unwrap();

        let current_headset_views = if flags.contains(xr::ViewStateFlags::ORIENTATION_VALID) {
            maybe_views
        } else {
            vec![crate::default_view(), crate::default_view()]
        };

        // Huawei's xrLocateViews ignores the requested reference space; the VIEW space relation
        // carries the real floor-relative pose.
        let mut current_headset_views = current_headset_views;
        if self.platform == Platform::Hvr {
            if let Ok((location, _)) = self
                .view_reference_space
                .relate(&self.stage_reference_space, xr_vsync_time)
            {
                for view in &mut current_headset_views {
                    view.pose = location.pose;
                }
            }
        }

        // The poses and FoVs we received from the PC runtime, which may differ and/or include
        // altered FoVs based on settings and view conversions done for canting.
        let input_view_params = view_params;
        let mut output_view_params = input_view_params;
        // Avoid passing invalid timestamp to runtime.
        // `timestamp` is generally a current vsync time, but may be repeated if frames are
        // dropped. Some runtimes dislike it if the timestamp is repeated for too long, so after
        // one second we begin presenting a lagged vsync time instead.
        let mut openxr_display_time =
            Duration::max(timestamp, vsync_time.saturating_sub(Duration::from_secs(1)));

        // (shinyquagsire23) I don't entirely trust runtimes to implement CompositionLayerProjectionView
        // correctly, but if we do trust them, avoid doing rotation ourselves. Otherwise, rerender.
        // Ex: YVR/PFDMR has issues with aspect ratio mismatches and passthrough compositing.
        if self.use_custom_reprojection {
            output_view_params = [
                ViewParams {
                    pose: crate::from_xr_pose(current_headset_views[0].pose),
                    fov: crate::from_xr_fov(current_headset_views[0].fov),
                },
                ViewParams {
                    pose: crate::from_xr_pose(current_headset_views[1].pose),
                    fov: crate::from_xr_fov(current_headset_views[1].fov),
                },
            ];

            openxr_display_time = vsync_time;
        }

        unsafe {
            self.renderer.render(
                buffer_ptr,
                [
                    StreamViewParams {
                        swapchain_index: left_swapchain_idx,
                        input_view_params: input_view_params[0],
                        output_view_params: output_view_params[0],
                    },
                    StreamViewParams {
                        swapchain_index: right_swapchain_idx,
                        input_view_params: input_view_params[1],
                        output_view_params: output_view_params[1],
                    },
                ],
                self.config.passthrough.as_ref(),
            )
        };

        if is_hvr {
            // Huawei's compositor reads the released swapchain image immediately. Ensure both the
            // decoded-frame copy and wgpu draw have completed before handing the image back.
            self.renderer.finish();
            let _ = graphics::hvr_release_image(&mut self.swapchains[0]);
            let _ = graphics::hvr_release_image(&mut self.swapchains[1]);
        } else {
            self.swapchains[0].release_image().unwrap();
            self.swapchains[1].release_image().unwrap();
        }

        if !buffer_ptr.is_null() {
            if let Some(xr_now) = crate::xr_runtime_now(self.xr_session.instance()) {
                self.core_context.report_submit(
                    timestamp,
                    vsync_time.saturating_sub(Duration::from_nanos(xr_now.as_nanos() as u64)),
                );
            }
        }

        let rect = xr::Rect2Di {
            offset: xr::Offset2Di { x: 0, y: 0 },
            extent: xr::Extent2Di {
                width: self.target_view_resolution.x as _,
                height: self.target_view_resolution.y as _,
            },
        };

        let clientside_post_processing = self
            .xr_session
            .instance()
            .exts()
            .fb_composition_layer_settings
            .and(self.config.clientside_post_processing.clone());

        let layer = ProjectionLayerBuilder::new(
            &self.stage_reference_space,
            [
                xr::CompositionLayerProjectionView::new()
                    .pose(crate::to_xr_pose(output_view_params[0].pose))
                    .fov(crate::to_xr_fov(output_view_params[0].fov))
                    .sub_image(
                        xr::SwapchainSubImage::new()
                            .swapchain(&self.swapchains[0])
                            .image_array_index(0)
                            .image_rect(rect),
                    ),
                xr::CompositionLayerProjectionView::new()
                    .pose(crate::to_xr_pose(output_view_params[1].pose))
                    .fov(crate::to_xr_fov(output_view_params[1].fov))
                    .sub_image(
                        xr::SwapchainSubImage::new()
                            .swapchain(&self.swapchains[1])
                            .image_array_index(0)
                            .image_rect(rect),
                    ),
            ],
            self.config
                .passthrough
                .clone()
                .map(|mode| ProjectionLayerAlphaConfig {
                    premultiplied: matches!(
                        mode,
                        PassthroughMode::Blend {
                            premultiplied_alpha: true,
                            ..
                        } | PassthroughMode::RgbChromaKey(_)
                            | PassthroughMode::HsvChromaKey(_)
                    ),
                }),
            clientside_post_processing,
        );

        (layer, openxr_display_time)
    }
}

impl Drop for StreamContext {
    fn drop(&mut self) {
        self.input_thread_running.set(false);
        self.input_thread.take().unwrap().join().ok();
    }
}

fn stream_input_loop(
    core_ctx: &ClientCoreContext,
    xr_session: xr::Session<xr::OpenGlEs>,
    interaction_ctx: &RwLock<InteractionContext>,
    stage_reference_space: &xr::Space,
    view_reference_space: &xr::Space,
    refresh_rate: f32,
    running: Arc<RelaxedAtomic>,
) {
    let platform = alvr_system_info::platform();

    let mut last_controller_poses = [Pose::default(); 2];
    let mut last_palm_poses = [Pose::default(); 2];
    let mut last_view_params = [ViewParams::default(); 2];
    let mut next_hvr_controller_log = Instant::now();
    let mut hvr_button_values = HashMap::new();

    let mut deadline = Instant::now();
    let frame_interval = Duration::from_secs_f32(1.0 / refresh_rate);
    while running.value() {
        let int_ctx = &*interaction_ctx.read();
        // Streaming related inputs are updated here. Make sure every input poll is done in this
        // thread
        if let Err(e) = xr_session.sync_actions(&[(&int_ctx.action_set).into()]) {
            error!("{e}");
            return;
        }

        let Some(now) = crate::xr_runtime_now(xr_session.instance()).map(crate::from_xr_time)
        else {
            error!("Cannot poll tracking: invalid time");
            return;
        };

        let target_time = now + core_ctx.get_total_prediction_offset();

        let Some((head_motion, local_views)) = interaction::get_head_data(
            &xr_session,
            platform,
            stage_reference_space,
            view_reference_space,
            now,
            target_time,
            &last_view_params,
        ) else {
            continue;
        };

        if let Some(views) = local_views {
            core_ctx.send_view_params(views);
            last_view_params = views;
        }

        let mut device_motions = Vec::with_capacity(3);

        device_motions.push((*HEAD_ID, head_motion));

        let (mut left_hand_motion, left_hand_skeleton) = crate::interaction::get_hand_data(
            &xr_session,
            platform,
            stage_reference_space,
            now,
            target_time,
            &int_ctx.hands_interaction[0],
            &mut last_controller_poses[0],
            &mut last_palm_poses[0],
        );
        let (mut right_hand_motion, right_hand_skeleton) = crate::interaction::get_hand_data(
            &xr_session,
            platform,
            stage_reference_space,
            now,
            target_time,
            &int_ctx.hands_interaction[1],
            &mut last_controller_poses[1],
            &mut last_palm_poses[1],
        );


        if platform == Platform::Hvr && Instant::now() >= next_hvr_controller_log {
            alvr_common::info!(
                "[HVR-DIAG] controller poses in shared reference space: head={:?}, left={:?}, right={:?}",
                head_motion.pose.position,
                left_hand_motion.map(|motion| motion.pose.position),
                right_hand_motion.map(|motion| motion.pose.position)
            );
            next_hvr_controller_log = Instant::now() + Duration::from_secs(1);
        }

        // Note: When multimodal input is enabled, we are sure that when free hands are used
        // (not holding controllers) the controller data is None.
        if int_ctx.multimodal_hands_enabled || left_hand_skeleton.is_none() {
            if let Some(motion) = left_hand_motion {
                device_motions.push((*HAND_LEFT_ID, motion));
            }
        }
        if int_ctx.multimodal_hands_enabled || right_hand_skeleton.is_none() {
            if let Some(motion) = right_hand_motion {
                device_motions.push((*HAND_RIGHT_ID, motion));
            }
        }

        let face_data = FaceData {
            eye_gazes: interaction::get_eye_gazes(
                &xr_session,
                &int_ctx.face_sources,
                stage_reference_space,
                now,
            ),
            fb_face_expression: interaction::get_fb_face_expression(&int_ctx.face_sources, now).or(
                interaction::get_pico_face_expression(&int_ctx.face_sources, now),
            ),
            htc_eye_expression: interaction::get_htc_eye_expression(&int_ctx.face_sources, now),
            htc_lip_expression: interaction::get_htc_lip_expression(&int_ctx.face_sources, now),
        };

        if let Some((tracker, joint_count)) = &int_ctx.body_sources.body_tracker_fb {
            device_motions.append(&mut interaction::get_fb_body_tracking_points(
                stage_reference_space,
                now,
                tracker,
                *joint_count,
            ));
        }

        if let Some(tracker) = &int_ctx.body_sources.body_tracker_bd {
            device_motions.append(&mut interaction::get_bd_body_tracking_points(
                stage_reference_space,
                now,
                tracker,
            ));
        }

        if let Some(tracker) = &int_ctx.body_sources.motion_tracker_bd {
            device_motions.append(&mut interaction::get_bd_motion_trackers(now, tracker));
        }

        core_ctx.send_tracking(
            Duration::from_nanos(now.as_nanos() as u64),
            device_motions,
            [left_hand_skeleton, right_hand_skeleton],
            face_data,
        );

        let button_entries = interaction::update_buttons(
            &xr_session,
            &int_ctx.button_actions,
            platform,
            int_ctx.hvr_position_tracking,
            &mut hvr_button_values,
        );
        if !button_entries.is_empty() {
            core_ctx.send_buttons(button_entries);
        }

        deadline += frame_interval / 3;
        thread::sleep(deadline.saturating_duration_since(Instant::now()));
    }
}
