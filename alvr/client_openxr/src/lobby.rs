use crate::{
    from_xr_time,
    graphics::{self, ProjectionLayerAlphaConfig, ProjectionLayerBuilder},
    interaction::{self, InteractionContext},
};
use alvr_common::{glam::UVec2, parking_lot::RwLock, Pose};
use alvr_graphics::{
    BodyTrackingType, GraphicsContext, LobbyRenderer, LobbyViewParams, SDR_FORMAT_GL,
};
use alvr_system_info::Platform;
use openxr as xr;
use std::{rc::Rc, sync::Arc, time::Duration};

// todo: add interaction?
pub struct Lobby {
    xr_session: xr::Session<xr::OpenGlEs>,
    interaction_ctx: Arc<RwLock<InteractionContext>>,
    platform: Platform,
    reference_space: Arc<xr::Space>,
    hvr_reference_space: Option<Arc<xr::Space>>,
    hvr_swapchains_primed: bool,
    swapchains: [xr::Swapchain<xr::OpenGlEs>; 2],
    view_resolution: UVec2,
    reference_space_type: xr::ReferenceSpaceType,
    next_hvr_pose_log: Duration,
    hvr_controller_y_calibrations: [Option<(f32, f32)>; 2],
    renderer: LobbyRenderer,
}

impl Lobby {
    pub fn new(
        xr_session: xr::Session<xr::OpenGlEs>,
        gfx_ctx: Rc<GraphicsContext>,
        interaction_ctx: Arc<RwLock<InteractionContext>>,
        platform: Platform,
        view_resolution: UVec2,
        initial_hud_message: &str,
        hvr_reference_space: Option<Arc<xr::Space>>,
    ) -> Self {
        let reference_space_type = if platform == Platform::Hvr {
            xr::ReferenceSpaceType::LOCAL
        } else if xr_session.instance().exts().ext_local_floor.is_some() {
            xr::ReferenceSpaceType::LOCAL_FLOOR_EXT
        } else {
            // The Quest 1 doesn't support LOCAL_FLOOR_EXT, recentering is required for AppLab, but
            // the Quest 1 is excluded from AppLab anyway.
            xr::ReferenceSpaceType::STAGE
        };

        let reference_space = if let Some(space) = &hvr_reference_space {
            Arc::clone(space)
        } else {
            Arc::new(interaction::get_reference_space(
                &xr_session,
                reference_space_type,
            ))
        };

        let swapchains = [
            graphics::create_swapchain(
                &xr_session,
                &gfx_ctx,
                view_resolution,
                SDR_FORMAT_GL,
                None,
                platform,
            ),
            graphics::create_swapchain(
                &xr_session,
                &gfx_ctx,
                view_resolution,
                SDR_FORMAT_GL,
                None,
                platform,
            ),
        ];

        let renderer = LobbyRenderer::new(
            gfx_ctx,
            view_resolution,
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
            initial_hud_message,
        );

        Self {
            xr_session,
            interaction_ctx,
            platform,
            reference_space,
            hvr_reference_space,
            hvr_swapchains_primed: false,
            swapchains,
            view_resolution,
            reference_space_type,
            next_hvr_pose_log: Duration::ZERO,
            hvr_controller_y_calibrations: [None, None],
            renderer,
        }
    }

    pub fn update_reference_space(&mut self) {
        // Recreating the Huawei space would re-anchor it to the current head pose and desync it
        // from the stream, which shares the same space.
        if self.hvr_reference_space.is_some() {
            return;
        }

        self.reference_space = Arc::new(interaction::get_reference_space(
            &self.xr_session,
            self.reference_space_type,
        ));
    }

    pub fn update_hud_message(&self, message: &str) {
        if self.platform == Platform::Hvr {
            self.renderer.update_hud_message_hvr(message);
        } else {
            self.renderer.update_hud_message(message);
        }
    }

    pub fn render(&mut self, vsync_time: Duration) -> ProjectionLayerBuilder {
        let xr_vsync_time = xr::Time::from_nanos(vsync_time.as_nanos() as _);

        let (flags, maybe_views) = self
            .xr_session
            .locate_views(
                xr::ViewConfigurationType::PRIMARY_STEREO,
                xr_vsync_time,
                &self.reference_space,
            )
            .unwrap();

        let views = if flags.contains(xr::ViewStateFlags::ORIENTATION_VALID) {
            maybe_views
        } else {
            vec![crate::default_view(), crate::default_view()]
        };

        self.xr_session
            .sync_actions(&[(&self.interaction_ctx.read().action_set).into()])
            .ok();

        // future_time doesn't have to be any particular value, just something after vsync_time
        let future_time = vsync_time + Duration::from_millis(80);
        let mut left_hand_data = interaction::get_hand_data(
            &self.xr_session,
            self.platform,
            &self.reference_space,
            vsync_time,
            future_time,
            &self.interaction_ctx.read().hands_interaction[0],
            &mut Pose::default(),
            &mut Pose::default(),
        );
        let mut right_hand_data = interaction::get_hand_data(
            &self.xr_session,
            self.platform,
            &self.reference_space,
            vsync_time,
            future_time,
            &self.interaction_ctx.read().hands_interaction[1],
            &mut Pose::default(),
            &mut Pose::default(),
        );

        if self.platform == Platform::Hvr {
            let head_y = (views[0].pose.position.y + views[1].pose.position.y) * 0.5;
            for (calibration, (hand_name, hand_data)) in
                self.hvr_controller_y_calibrations.iter_mut().zip([
                    ("left", &mut left_hand_data),
                    ("right", &mut right_hand_data),
                ])
            {
                if let Some(motion) = &mut hand_data.0 {
                    let raw_controller_y = motion.pose.position.y;
                    let newly_calibrated = calibration.is_none();
                    let (initial_head_y, initial_controller_y) =
                        *calibration.get_or_insert((head_y, raw_controller_y));
                    motion.pose.position.y = crate::stream::hvr_corrected_controller_y(
                        initial_head_y,
                        initial_controller_y,
                        raw_controller_y,
                    );

                    if newly_calibrated {
                        alvr_common::info!(
                            "[HVR-DIAG] reflected lobby {hand_name} controller height: \
                             head_y={initial_head_y:.3}, raw_y={initial_controller_y:.3}, \
                             corrected_y={:.3}",
                            motion.pose.position.y
                        );
                    }
                }
            }
        }

        if self.platform == Platform::Hvr && vsync_time >= self.next_hvr_pose_log {
            alvr_common::info!(
                "[HVR-DIAG] lobby poses in shared reference space: head_left={:?}, \
                 head_right={:?}, left={:?}, right={:?}",
                views[0].pose.position,
                views[1].pose.position,
                left_hand_data.0.map(|motion| motion.pose.position),
                right_hand_data.0.map(|motion| motion.pose.position)
            );
            self.next_hvr_pose_log = vsync_time + Duration::from_secs(1);
        }

        let additional_motions = self
            .interaction_ctx
            .read()
            .body_sources
            .motion_tracker_bd
            .as_ref()
            .map(|tracker| {
                interaction::get_bd_motion_trackers(from_xr_time(xr_vsync_time), tracker)
            })
            .map(|vec| vec.iter().map(|(_, motion)| *motion).collect());

        let body_skeleton_fb = self
            .interaction_ctx
            .read()
            .body_sources
            .body_tracker_fb
            .as_ref()
            .and_then(|(tracker, joint_count)| {
                interaction::get_fb_body_skeleton(
                    &self.reference_space,
                    xr_vsync_time,
                    tracker,
                    *joint_count,
                )
            });

        let body_skeleton_bd = self
            .interaction_ctx
            .read()
            .body_sources
            .body_tracker_bd
            .as_ref()
            .and_then(|tracker| {
                interaction::get_bd_body_skeleton(&self.reference_space, xr_vsync_time, tracker)
            });

        let (body_skeleton, body_tracking_type) = {
            if body_skeleton_fb.is_some() {
                (body_skeleton_fb, Some(BodyTrackingType::Meta))
            } else if body_skeleton_bd.is_some() {
                (body_skeleton_bd, Some(BodyTrackingType::Pico))
            } else {
                (None, None)
            }
        };

        // Huawei's legacy runtime returns non-success codes from the swapchain image calls but
        // still functions; its official demo ignores all these return codes. On HVR we tolerate
        // errors (defaulting the image index to 0) instead of panicking. Other platforms keep the
        // strict unwrap.
        let is_hvr = self.platform == Platform::Hvr;

        // Populate the Huawei GL texture handles in one go, so the lobby does not skip a frame per
        // swapchain image while they trickle in. Only valid inside an active frame.
        if is_hvr && !self.hvr_swapchains_primed {
            self.hvr_swapchains_primed = true;
            for swapchain in &self.swapchains {
                graphics::hvr_prime_swapchain(swapchain);
            }
        }

        let acquire = |swapchain: &mut xr::Swapchain<xr::OpenGlEs>| -> u32 {
            let result = if is_hvr {
                graphics::hvr_acquire_image(swapchain)
            } else {
                swapchain.acquire_image()
            };
            match result {
                Ok(idx) => idx,
                Err(e) if is_hvr => {
                    alvr_common::warn!("[HVR-DIAG] lobby acquire_image returned {e:?}; using 0");
                    0
                }
                Err(e) => panic!("acquire_image failed: {e:?}"),
            }
        };
        let left_swapchain_idx = acquire(&mut self.swapchains[0]);
        let right_swapchain_idx = acquire(&mut self.swapchains[1]);

        for swapchain in &mut self.swapchains {
            let result = if is_hvr {
                graphics::hvr_wait_image(swapchain)
            } else {
                swapchain.wait_image(xr::Duration::INFINITE)
            };
            if let Err(e) = result {
                if is_hvr {
                    alvr_common::warn!("[HVR-DIAG] lobby wait_image returned {e:?}; continuing");
                } else {
                    panic!("wait_image failed: {e:?}");
                }
            }
        }

        #[cfg(target_os = "android")]
        if is_hvr && !self.renderer.render_targets_ready() {
            match self.renderer.ensure_hvr_current() {
                Ok(true) => alvr_common::info!(
                    "[HVR-DIAG] restored Huawei EGL context after swapchain wait"
                ),
                Ok(false) => {}
                Err(error) => alvr_common::warn!("[HVR-DIAG] {error}"),
            }
        }

        // Huawei deferred swapchain path: the runtime only populates valid GL texture handles once
        // the images have been acquired/waited inside an active frame. Now that the first
        // acquire/wait has happened, re-enumerate and hand the (hopefully non-zero) handles to the
        // renderer so it can build its wgpu render targets. Runs once. HVR only.
        if self.platform == Platform::Hvr && !self.renderer.render_targets_ready() {
            match (
                self.swapchains[0].enumerate_images(),
                self.swapchains[1].enumerate_images(),
            ) {
                (Ok(left_handles), Ok(right_handles)) => {
                    alvr_common::info!(
                        "[HVR-DIAG] lobby in-frame swapchain handles: left={left_handles:?}, \
                         right={right_handles:?}"
                    );
                    if left_handles.iter().all(|&h| h != 0) && right_handles.iter().all(|&h| h != 0)
                    {
                        self.renderer
                            .set_swapchain_textures([left_handles, right_handles]);
                        alvr_common::info!(
                            "[HVR-DIAG] lobby render targets built from in-frame handles"
                        );
                    } else {
                        alvr_common::warn!(
                            "[HVR-DIAG] in-frame swapchain handles still contain zeros; \
                             lobby will not render this frame"
                        );
                    }
                }
                (left, right) => {
                    alvr_common::warn!(
                        "[HVR-DIAG] in-frame enumerate_images failed: left={left:?}, right={right:?}"
                    );
                }
            }
        }

        self.renderer.render(
            [
                LobbyViewParams {
                    pose: crate::from_xr_pose(views[0].pose),
                    fov: crate::from_xr_fov(views[0].fov),
                    swapchain_index: left_swapchain_idx,
                },
                LobbyViewParams {
                    pose: crate::from_xr_pose(views[1].pose),
                    fov: crate::from_xr_fov(views[1].fov),
                    swapchain_index: right_swapchain_idx,
                },
            ],
            [left_hand_data, right_hand_data],
            additional_motions,
            body_skeleton,
            body_tracking_type,
            // Huawei VR Glass has no passthrough camera, so a transparent lobby renders as black.
            // Draw the opaque background/ground on HVR so the HUD and environment are visible.
            is_hvr,
            cfg!(debug_assertions),
        );

        if is_hvr {
            self.renderer.finish();
        }

        for swapchain in &mut self.swapchains {
            let result = if is_hvr {
                graphics::hvr_release_image(swapchain)
            } else {
                swapchain.release_image()
            };
            if let Err(e) = result {
                if is_hvr {
                    alvr_common::warn!("[HVR-DIAG] lobby release_image returned {e:?}; continuing");
                } else {
                    panic!("release_image failed: {e:?}");
                }
            }
        }

        let rect = xr::Rect2Di {
            offset: xr::Offset2Di { x: 0, y: 0 },
            extent: xr::Extent2Di {
                width: self.view_resolution.x as _,
                height: self.view_resolution.y as _,
            },
        };

        ProjectionLayerBuilder::new(
            &self.reference_space,
            [
                xr::CompositionLayerProjectionView::new()
                    .pose(views[0].pose)
                    .fov(views[0].fov)
                    .sub_image(
                        xr::SwapchainSubImage::new()
                            .swapchain(&self.swapchains[0])
                            .image_array_index(0)
                            .image_rect(rect),
                    ),
                xr::CompositionLayerProjectionView::new()
                    .pose(views[1].pose)
                    .fov(views[1].fov)
                    .sub_image(
                        xr::SwapchainSubImage::new()
                            .swapchain(&self.swapchains[1])
                            .image_array_index(0)
                            .image_rect(rect),
                    ),
            ],
            // Huawei's only environment blend mode is ALPHA_BLEND and the headset has no
            // passthrough camera, so any layer alpha below 1 composites to black. The official
            // HVRSDK_XrDemo submits its projection layer with layerFlags = 0, which makes the
            // runtime treat the layer as opaque regardless of what the texture's alpha holds.
            // Match that: requesting BLEND_TEXTURE_SOURCE_ALPHA here only risks a black view.
            (!is_hvr).then_some(ProjectionLayerAlphaConfig {
                premultiplied: true,
            }),
            None,
        )
    }
}
