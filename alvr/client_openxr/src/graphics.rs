use alvr_common::glam::UVec2;
use alvr_graphics::GraphicsContext;
use alvr_session::ClientsidePostProcessingConfig;
use openxr as xr;
use std::ptr;

fn hvr_result(result: xr::sys::Result) -> xr::Result<()> {
    if result.into_raw() >= 0 {
        Ok(())
    } else {
        Err(result)
    }
}

// Huawei's legacy runtime validates these input pointers even though newer OpenXR runtimes accept
// NULL for empty input structs. The pinned openxr crate passes NULL; use explicit typed structs on
// HVR, exactly like Huawei's official sample.
pub fn hvr_wait_frame(session: &xr::Session<xr::OpenGlEs>) -> xr::Result<xr::FrameState> {
    let info = xr::sys::FrameWaitInfo {
        ty: xr::sys::FrameWaitInfo::TYPE,
        next: ptr::null(),
    };
    let state = unsafe {
        let mut state = xr::sys::FrameState::out(ptr::null_mut());
        let result =
            (session.instance().fp().wait_frame)(session.as_raw(), &info, state.as_mut_ptr());
        hvr_result(result)?;
        state.assume_init()
    };

    Ok(xr::FrameState {
        predicted_display_time: state.predicted_display_time,
        predicted_display_period: state.predicted_display_period,
        should_render: state.should_render.into(),
    })
}

pub fn hvr_begin_frame(session: &xr::Session<xr::OpenGlEs>) -> xr::Result<()> {
    let info = xr::sys::FrameBeginInfo {
        ty: xr::sys::FrameBeginInfo::TYPE,
        next: ptr::null(),
    };
    hvr_result(unsafe { (session.instance().fp().begin_frame)(session.as_raw(), &info) })
}

pub fn hvr_acquire_image(swapchain: &xr::Swapchain<xr::OpenGlEs>) -> xr::Result<u32> {
    let info = xr::sys::SwapchainImageAcquireInfo {
        ty: xr::sys::SwapchainImageAcquireInfo::TYPE,
        next: ptr::null(),
    };
    let mut index = 0;
    let result = unsafe {
        (swapchain.instance().fp().acquire_swapchain_image)(swapchain.as_raw(), &info, &mut index)
    };
    hvr_result(result)?;

    Ok(index)
}

pub fn hvr_wait_image(swapchain: &xr::Swapchain<xr::OpenGlEs>) -> xr::Result<()> {
    let info = xr::sys::SwapchainImageWaitInfo {
        ty: xr::sys::SwapchainImageWaitInfo::TYPE,
        next: ptr::null(),
        timeout: xr::Duration::INFINITE,
    };
    hvr_result(unsafe {
        (swapchain.instance().fp().wait_swapchain_image)(swapchain.as_raw(), &info)
    })
}

pub fn hvr_release_image(swapchain: &xr::Swapchain<xr::OpenGlEs>) -> xr::Result<()> {
    let info = xr::sys::SwapchainImageReleaseInfo {
        ty: xr::sys::SwapchainImageReleaseInfo::TYPE,
        next: ptr::null(),
    };
    hvr_result(unsafe {
        (swapchain.instance().fp().release_swapchain_image)(swapchain.as_raw(), &info)
    })
}

#[allow(unused)]
pub fn session_create_info(ctx: &GraphicsContext) -> xr::opengles::SessionCreateInfo {
    #[cfg(target_os = "android")]
    {
        xr::opengles::SessionCreateInfo::Android {
            display: ctx.egl_display.as_ptr(),
            config: ctx.egl_config.as_ptr(),
            context: ctx.egl_context.as_ptr(),
        }
    }
    #[cfg(not(target_os = "android"))]
    unimplemented!()
}

pub fn swapchain_format(
    gfx_ctx: &GraphicsContext,
    session: &xr::Session<xr::OpenGlEs>,
    enable_hdr: bool,
) -> u32 {
    gfx_ctx.make_current();

    let formats = session.enumerate_swapchain_formats().unwrap();
    alvr_graphics::choose_swapchain_format(&formats, enable_hdr)
}

#[allow(unused_variables)]
pub fn create_swapchain(
    session: &xr::Session<xr::OpenGlEs>,
    gfx_ctx: &GraphicsContext,
    resolution: UVec2,
    format: u32,
    foveation: Option<&xr::FoveationProfileFB>,
    platform: alvr_system_info::Platform,
) -> xr::Swapchain<xr::OpenGlEs> {
    gfx_ctx.make_current();

    let swapchain_info = xr::SwapchainCreateInfo {
        create_flags: xr::SwapchainCreateFlags::EMPTY,
        usage_flags: xr::SwapchainUsageFlags::COLOR_ATTACHMENT | xr::SwapchainUsageFlags::SAMPLED,
        format,
        sample_count: 1,
        width: resolution.x,
        height: resolution.y,
        face_count: 1,
        array_size: 1,
        mip_count: 1,
    };

    let is_hvr = platform == alvr_system_info::Platform::Hvr;

    if is_hvr {
        if let Ok(formats) = session.enumerate_swapchain_formats() {
            alvr_common::info!(
                "[HVR-DIAG] create_swapchain: requested format {format:#x} ({}x{}); \
                 runtime-supported formats: {formats:x?}",
                resolution.x,
                resolution.y
            );
        }
    }

    let swapchain = session.create_swapchain(&swapchain_info).unwrap();

    if is_hvr {
        if let Ok(images) = swapchain.enumerate_images() {
            alvr_common::info!(
                "[HVR-DIAG] create_swapchain: enumerated GL image handles: {images:?}"
            );
        }
    }

    swapchain
}

/// Huawei fills in a swapchain image's GL texture handle only when that image is first acquired, so
/// `xrEnumerateSwapchainImages` returns zeros right after creation and gains one handle per
/// acquire. Rendering is skipped until every handle is valid, costing one black frame per image.
/// Cycling the images here populates all the handles at once.
///
/// Acquire alone populates the handle, so the blocking `xrWaitSwapchainImage` is skipped: waiting
/// on each image stalls for a vsync and made priming cost ~290 ms, which showed up as a freeze when
/// the stream started.
///
/// Must be called inside an active frame: Huawei's runtime kills the process if the swapchain image
/// calls happen between `xrEndFrame` and the next `xrBeginFrame`.
pub fn hvr_prime_swapchain(swapchain: &xr::Swapchain<xr::OpenGlEs>) {
    let image_count = swapchain.enumerate_images().map(|i| i.len()).unwrap_or(0);

    for _ in 0..image_count {
        if hvr_acquire_image(swapchain).is_err() {
            break;
        }
        let _ = hvr_release_image(swapchain);
    }

    if let Ok(images) = swapchain.enumerate_images() {
        alvr_common::error!("[HVR-DIAG] GL image handles after priming: {images:?}");
    }
}

pub struct ProjectionLayerAlphaConfig {
    pub premultiplied: bool,
}

// This is needed to work around lifetime limitations. Deref cannot be implemented because there are
// nested references, and in a way or the other I would get `cannot return reference to temporary
// value`
pub struct ProjectionLayerBuilder<'a> {
    reference_space: &'a xr::Space,
    layers: [xr::CompositionLayerProjectionView<'a, xr::OpenGlEs>; 2],
    alpha: Option<ProjectionLayerAlphaConfig>,
    composition_layer_settings: Option<xr::sys::CompositionLayerSettingsFB>,
}

impl<'a> ProjectionLayerBuilder<'a> {
    pub fn new(
        reference_space: &'a xr::Space,
        layers: [xr::CompositionLayerProjectionView<'a, xr::OpenGlEs>; 2],
        alpha: Option<ProjectionLayerAlphaConfig>,
        clientside_post_processing_config: Option<ClientsidePostProcessingConfig>,
    ) -> Self {
        let composition_layer_settings = clientside_post_processing_config
            .map(|post_processing| {
                xr::CompositionLayerSettingsFlagsFB::from_raw(
                    (post_processing.sharpening as u64) | (post_processing.super_sampling as u64),
                )
            })
            .filter(|&flags| flags != xr::CompositionLayerSettingsFlagsFB::EMPTY)
            .map(|flags| xr::sys::CompositionLayerSettingsFB {
                ty: xr::StructureType::COMPOSITION_LAYER_SETTINGS_FB,
                next: std::ptr::null(),
                layer_flags: flags,
            });
        Self {
            reference_space,
            layers,
            alpha,
            composition_layer_settings,
        }
    }

    pub fn build(&self) -> xr::CompositionLayerProjection<xr::OpenGlEs> {
        let mut flags = xr::CompositionLayerFlags::EMPTY;

        if let Some(alpha) = &self.alpha {
            flags |= xr::CompositionLayerFlags::BLEND_TEXTURE_SOURCE_ALPHA;

            if !alpha.premultiplied {
                flags |= xr::CompositionLayerFlags::UNPREMULTIPLIED_ALPHA;
            }
        }

        let layer = xr::CompositionLayerProjection::new()
            .layer_flags(flags)
            .space(self.reference_space)
            .views(&self.layers);

        if let Some(composition_layer_settings) = self.composition_layer_settings.as_ref() {
            unsafe {
                xr::CompositionLayerProjection::from_raw(xr::sys::CompositionLayerProjection {
                    next: std::ptr::from_ref(composition_layer_settings).cast(),
                    ..layer.into_raw()
                })
            }
        } else {
            layer
        }
    }
}
