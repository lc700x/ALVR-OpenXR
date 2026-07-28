mod lobby;
mod staging;
mod stream;

pub use lobby::*;
pub use stream::*;

use alvr_common::{
    glam::{Mat4, UVec2, Vec4},
    Fov,
};
use glow::{self as gl, HasContext};
use khronos_egl as egl;
use std::{ffi::c_void, num::NonZeroU32, ptr};
use wgpu::{
    hal::{self, api, MemoryFlags, TextureUses},
    Adapter, Device, Extent3d, Instance, Queue, Texture, TextureDescriptor, TextureDimension,
    TextureFormat, TextureUsages, TextureView,
};

pub const SDR_FORMAT: TextureFormat = TextureFormat::Rgba8Unorm;
pub const SDR_FORMAT_GL: u32 = gl::RGBA8;
pub const GL_TEXTURE_EXTERNAL_OES: u32 = 0x8D65;
pub const MAX_PUSH_CONSTANTS_SIZE: u32 = 128;

type CreateImageFn = unsafe extern "C" fn(
    egl::EGLDisplay,
    egl::EGLContext,
    egl::Enum,
    egl::EGLClientBuffer,
    *const egl::Int,
) -> egl::EGLImage;
type DestroyImageFn = unsafe extern "C" fn(egl::EGLDisplay, egl::EGLImage) -> egl::Boolean;
type GetNativeClientBufferFn = unsafe extern "C" fn(*const c_void) -> egl::EGLClientBuffer;
type ImageTargetTexture2DFn = unsafe extern "C" fn(egl::Enum, egl::EGLImage);

pub fn check_error(gl: &gl::Context, message_context: &str) {
    let err = unsafe { gl.get_error() };
    if err != glow::NO_ERROR {
        alvr_common::error!("gl error {message_context} -> {err}");
        std::process::abort();
    }
}

fn check_error_nonfatal(gl: &gl::Context, message_context: &str) -> bool {
    let err = unsafe { gl.get_error() };
    if err == glow::NO_ERROR {
        true
    } else {
        alvr_common::warn!("[HVR-DIAG] gl error {message_context} -> {err}; skipping frame");
        false
    }
}

fn clear_errors(gl: &gl::Context) {
    while unsafe { gl.get_error() } != glow::NO_ERROR {}
}

macro_rules! ck {
    ($gl_ctx:ident.$($gl_cmd:tt)*) => {{
        let res = $gl_ctx.$($gl_cmd)*;

        #[cfg(debug_assertions)]
        crate::check_error(&$gl_ctx, &format!("{}:{}: {}", file!(), line!(), stringify!($($gl_cmd)*)));

        res
    }};
}
pub(crate) use ck;

fn projection_from_fov(fov: Fov) -> Mat4 {
    const NEAR: f32 = 0.1;

    let tanl = f32::tan(fov.left);
    let tanr = f32::tan(fov.right);
    let tanu = f32::tan(fov.up);
    let tand = f32::tan(fov.down);
    let a = 2.0 / (tanr - tanl);
    let b = 2.0 / (tanu - tand);
    let c = (tanr + tanl) / (tanr - tanl);
    let d = (tanu + tand) / (tanu - tand);

    // note: for wgpu compatibility, the b and d components should be flipped. Maybe a bug in the
    // viewport handling in wgpu?
    Mat4::from_cols(
        Vec4::new(a, 0.0, c, 0.0),
        Vec4::new(0.0, -b, -d, 0.0),
        Vec4::new(0.0, 0.0, -1.0, -NEAR),
        Vec4::new(0.0, 0.0, -1.0, 0.0),
    )
    .transpose()
}

pub fn choose_swapchain_format(supported_formats: &[u32], enable_hdr: bool) -> u32 {
    // Priority-sorted list of swapchain formats we'll accept--
    let mut app_supported_swapchain_formats = vec![gl::SRGB8_ALPHA8, gl::RGBA8];

    // float16 is required for HDR output. However, float16 swapchains
    // have a high perf cost, so only use these if HDR is enabled.
    if enable_hdr {
        app_supported_swapchain_formats.insert(0, gl::RGBA16F);
    }

    for format in app_supported_swapchain_formats {
        if supported_formats.contains(&format) {
            return format;
        }
    }

    // If we can't enumerate, default to a required format
    gl::RGBA8
}

pub fn gl_format_to_wgpu(format: u32) -> TextureFormat {
    match format {
        gl::SRGB8_ALPHA8 => TextureFormat::Rgba8UnormSrgb,
        gl::RGBA8 => TextureFormat::Rgba8Unorm,
        gl::RGBA16F => TextureFormat::Rgba16Float,
        _ => panic!("Unsupported GL format: {format}"),
    }
}

pub fn create_texture(device: &Device, resolution: UVec2, format: TextureFormat) -> Texture {
    device.create_texture(&TextureDescriptor {
        label: None,
        size: Extent3d {
            width: resolution.x,
            height: resolution.y,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: TextureDimension::D2,
        format,
        usage: TextureUsages::COPY_DST | TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    })
}

fn create_texture_from_gles(
    device: &Device,
    texture: u32,
    resolution: UVec2,
    format: TextureFormat,
) -> Texture {
    let size = Extent3d {
        width: resolution.x,
        height: resolution.y,
        depth_or_array_layers: 1,
    };

    unsafe {
        let hal_texture = device.as_hal::<api::Gles, _, _>(|device| {
            device.unwrap().texture_from_raw(
                NonZeroU32::new(texture).unwrap(),
                &hal::TextureDescriptor {
                    label: None,
                    size,
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: TextureDimension::D2,
                    format,
                    usage: TextureUses::COLOR_TARGET,
                    memory_flags: MemoryFlags::empty(),
                    view_formats: vec![],
                },
                Some(Box::new(|| ())),
            )
        });

        device.create_texture_from_hal::<api::Gles>(
            hal_texture,
            &TextureDescriptor {
                label: None,
                size,
                mip_level_count: 1,
                sample_count: 1,
                dimension: TextureDimension::D2,
                format,
                usage: TextureUsages::RENDER_ATTACHMENT,
                view_formats: &[],
            },
        )
    }
}

// This is used to convert OpenXR swapchains to wgpu
pub fn create_gl_swapchain(
    device: &Device,
    gl_textures: &[u32],
    resolution: UVec2,
    format: TextureFormat,
) -> Vec<TextureView> {
    gl_textures
        .iter()
        .map(|gl_tex| {
            create_texture_from_gles(device, *gl_tex, resolution, format)
                .create_view(&Default::default())
        })
        .collect()
}

pub struct GraphicsContext {
    _instance: Instance,

    #[cfg_attr(windows, expect(dead_code))]
    adapter: Adapter,

    device: Device,
    queue: Queue,
    pub egl_display: egl::Display,
    pub egl_config: egl::Config,
    pub egl_context: egl::Context,
    pub gl_context: gl::Context,

    #[cfg_attr(windows, expect(dead_code))]
    dummy_surface: egl::Surface,

    #[cfg(target_os = "android")]
    window_surface: Option<egl::Surface>,

    // Keeps libEGL loaded for the externally-owned Huawei EGL context. Other Android platforms
    // use wgpu's internally-owned EGL context and leave this empty.
    #[cfg(target_os = "android")]
    _hvr_egl_instance: Option<egl::DynamicInstance<egl::EGL1_4>>,

    create_image: CreateImageFn,
    destroy_image: DestroyImageFn,
    get_native_client_buffer: GetNativeClientBufferFn,
    image_target_texture_2d: ImageTargetTexture2DFn,
}

impl GraphicsContext {
    #[cfg(not(windows))]
    pub fn new_gl() -> Self {
        use std::mem;
        use wgpu::{
            Backends, DeviceDescriptor, Features, InstanceDescriptor, InstanceFlags, Limits,
            MemoryHints,
        };

        const CREATE_IMAGE_FN_STR: &str = "eglCreateImageKHR";
        const DESTROY_IMAGE_FN_STR: &str = "eglDestroyImageKHR";
        const GET_NATIVE_CLIENT_BUFFER_FN_STR: &str = "eglGetNativeClientBufferANDROID";
        const IMAGE_TARGET_TEXTURE_2D_FN_STR: &str = "glEGLImageTargetTexture2DOES";

        let flags = if cfg!(debug_assertions) {
            InstanceFlags::DEBUG | InstanceFlags::VALIDATION
        } else {
            InstanceFlags::empty()
        };

        let instance = Instance::new(&InstanceDescriptor {
            backends: Backends::GL,
            flags,
            backend_options: Default::default(),
        });

        let adapter = instance.enumerate_adapters(Backends::GL).remove(0);
        let (device, queue) = pollster::block_on(adapter.request_device(
            &DeviceDescriptor {
                label: None,
                required_features: Features::PUSH_CONSTANTS,
                required_limits: Limits {
                    max_push_constant_size: MAX_PUSH_CONSTANTS_SIZE,
                    ..adapter.limits()
                },
                memory_hints: MemoryHints::Performance,
            },
            None,
        ))
        .unwrap();

        let raw_instance = unsafe { instance.as_hal::<api::Gles>() }.unwrap();

        let egl_display = raw_instance.raw_display();
        let egl_config = raw_instance.egl_config();

        let (
            egl_context,
            gl_context,
            dummy_surface,
            create_image,
            destroy_image,
            get_native_client_buffer,
            image_target_texture_2d,
        ) = unsafe {
            adapter.as_hal::<api::Gles, _, _>(|raw_adapter| {
                let adapter_context = raw_adapter.unwrap().adapter_context();
                let egl_instance = adapter_context.egl_instance().unwrap();

                let egl_context = egl::Context::from_ptr(adapter_context.raw_context());

                const PBUFFER_ATTRIBS: [i32; 5] = [egl::WIDTH, 16, egl::HEIGHT, 16, egl::NONE];
                let dummy_surface = egl_instance
                    .create_pbuffer_surface(egl_display, egl_config, &PBUFFER_ATTRIBS)
                    .unwrap();

                egl_instance
                    .make_current(
                        egl_display,
                        Some(dummy_surface),
                        Some(dummy_surface),
                        Some(egl_context),
                    )
                    .unwrap();

                let gl_context = gl::Context::from_loader_function(|fn_name| {
                    egl_instance
                        .get_proc_address(fn_name)
                        .map_or(ptr::null(), |f| f as *const c_void)
                });

                let get_fn_ptr = |fn_name| {
                    egl_instance
                        .get_proc_address(fn_name)
                        .map_or(ptr::null(), |f| f as *const c_void)
                };

                let create_image: CreateImageFn = mem::transmute(get_fn_ptr(CREATE_IMAGE_FN_STR));
                let destroy_image: DestroyImageFn =
                    mem::transmute(get_fn_ptr(DESTROY_IMAGE_FN_STR));
                let get_native_client_buffer: GetNativeClientBufferFn =
                    mem::transmute(get_fn_ptr(GET_NATIVE_CLIENT_BUFFER_FN_STR));
                let image_target_texture_2d: ImageTargetTexture2DFn =
                    mem::transmute(get_fn_ptr(IMAGE_TARGET_TEXTURE_2D_FN_STR));

                (
                    egl_context,
                    gl_context,
                    dummy_surface,
                    create_image,
                    destroy_image,
                    get_native_client_buffer,
                    image_target_texture_2d,
                )
            })
        };

        Self {
            _instance: instance,
            adapter,
            device,
            queue,
            egl_display,
            egl_config,
            egl_context,
            gl_context,
            dummy_surface,
            #[cfg(target_os = "android")]
            window_surface: None,
            #[cfg(target_os = "android")]
            _hvr_egl_instance: None,
            create_image,
            destroy_image,
            get_native_client_buffer,
            image_target_texture_2d,
        }
    }

    #[cfg(target_os = "android")]
    pub unsafe fn new_gl_hvr(native_window: *mut c_void) -> Result<Self, String> {
        use std::mem;
        use wgpu::{
            Backends, DeviceDescriptor, Features, InstanceDescriptor, InstanceFlags, Limits,
            MemoryHints,
        };

        const CREATE_IMAGE_FN_STR: &str = "eglCreateImageKHR";
        const DESTROY_IMAGE_FN_STR: &str = "eglDestroyImageKHR";
        const GET_NATIVE_CLIENT_BUFFER_FN_STR: &str = "eglGetNativeClientBufferANDROID";
        const IMAGE_TARGET_TEXTURE_2D_FN_STR: &str = "glEGLImageTargetTexture2DOES";

        let egl_instance = unsafe {
            egl::DynamicInstance::<egl::EGL1_4>::load_required()
                .map_err(|error| format!("Cannot load libEGL: {error}"))?
        };
        let display = unsafe { egl_instance.get_display(egl::DEFAULT_DISPLAY) }
            .ok_or_else(|| "eglGetDisplay failed".to_owned())?;
        egl_instance
            .initialize(display)
            .map_err(|error| format!("eglInitialize failed: {error}"))?;

        // Match Wolvic's HVR BrowserEGLContext exactly. Do not use eglChooseConfig: Android may
        // inject forced-MSAA attributes there.
        let mut configs = Vec::with_capacity(
            egl_instance
                .get_config_count(display)
                .map_err(|error| format!("eglGetConfigs count failed: {error}"))?,
        );
        egl_instance
            .get_configs(display, &mut configs)
            .map_err(|error| format!("eglGetConfigs failed: {error}"))?;
        let config = configs
            .into_iter()
            .find(|&config| {
                let attr = |attribute| {
                    egl_instance
                        .get_config_attrib(display, config, attribute)
                        .ok()
                };
                attr(egl::RENDERABLE_TYPE).is_some_and(|value| value & egl::OPENGL_ES3_BIT != 0)
                    && attr(egl::SURFACE_TYPE).is_some_and(|value| {
                        value & (egl::WINDOW_BIT | egl::PBUFFER_BIT)
                            == egl::WINDOW_BIT | egl::PBUFFER_BIT
                    })
                    && [
                        (egl::RED_SIZE, 8),
                        (egl::GREEN_SIZE, 8),
                        (egl::BLUE_SIZE, 8),
                        (egl::ALPHA_SIZE, 8),
                        (egl::DEPTH_SIZE, 24),
                        (egl::STENCIL_SIZE, 0),
                        (egl::SAMPLES, 0),
                    ]
                    .into_iter()
                    .all(|(attribute, expected)| attr(attribute) == Some(expected))
            })
            .ok_or_else(|| "No Huawei-compatible EGL config found".to_owned())?;

        egl_instance
            .bind_api(egl::OPENGL_ES_API)
            .map_err(|error| format!("eglBindAPI failed: {error}"))?;
        let context = egl_instance
            .create_context(
                display,
                config,
                None,
                &[egl::CONTEXT_CLIENT_VERSION, 3, egl::NONE],
            )
            .map_err(|error| format!("eglCreateContext failed: {error}"))?;
        let surface = unsafe {
            egl_instance.create_window_surface(display, config, native_window.cast(), None)
        }
        .map_err(|error| format!("eglCreateWindowSurface failed: {error}"))?;
        egl_instance
            .make_current(display, Some(surface), Some(surface), Some(context))
            .map_err(|error| format!("eglMakeCurrent failed: {error}"))?;

        let flags = if cfg!(debug_assertions) {
            InstanceFlags::DEBUG | InstanceFlags::VALIDATION
        } else {
            InstanceFlags::empty()
        };
        let instance = Instance::new(&InstanceDescriptor {
            backends: Backends::GL,
            flags,
            backend_options: Default::default(),
        });

        // Instance creation may switch EGL contexts. Restore the Huawei context before exposing it
        // to wgpu-hal; external GLES adapters require their context to remain current.
        egl_instance
            .make_current(display, Some(surface), Some(surface), Some(context))
            .map_err(|error| format!("eglMakeCurrent after wgpu init failed: {error}"))?;
        let exposed_adapter = unsafe {
            <api::Gles as hal::Api>::Adapter::new_external(|name| {
                egl_instance
                    .get_proc_address(name)
                    .map_or(ptr::null(), |function| function as *const c_void)
            })
        }
        .ok_or_else(|| "wgpu could not expose Huawei GLES context".to_owned())?;
        let adapter = unsafe { instance.create_adapter_from_hal(exposed_adapter) };
        let (device, queue) = pollster::block_on(adapter.request_device(
            &DeviceDescriptor {
                label: None,
                required_features: Features::PUSH_CONSTANTS,
                required_limits: Limits {
                    max_push_constant_size: MAX_PUSH_CONSTANTS_SIZE,
                    ..adapter.limits()
                },
                memory_hints: MemoryHints::Performance,
            },
            None,
        ))
        .map_err(|error| format!("wgpu request_device failed: {error}"))?;

        let gl_context = gl::Context::from_loader_function(|name| {
            egl_instance
                .get_proc_address(name)
                .map_or(ptr::null(), |function| function as *const c_void)
        });
        let get_fn_ptr = |name| {
            egl_instance
                .get_proc_address(name)
                .map_or(ptr::null(), |function| function as *const c_void)
        };
        let create_image: CreateImageFn = mem::transmute(get_fn_ptr(CREATE_IMAGE_FN_STR));
        let destroy_image: DestroyImageFn = mem::transmute(get_fn_ptr(DESTROY_IMAGE_FN_STR));
        let get_native_client_buffer: GetNativeClientBufferFn =
            mem::transmute(get_fn_ptr(GET_NATIVE_CLIENT_BUFFER_FN_STR));
        let image_target_texture_2d: ImageTargetTexture2DFn =
            mem::transmute(get_fn_ptr(IMAGE_TARGET_TEXTURE_2D_FN_STR));

        let attr = |attribute| {
            egl_instance
                .get_config_attrib(display, config, attribute)
                .unwrap_or(-1)
        };
        alvr_common::info!(
            "[HVR-DIAG] selected EGL config: RGBA={}/{}/{}/{} DEPTH={} STENCIL={} SAMPLES={} \
             SURFACE_TYPE={:#x} RENDERABLE_TYPE={:#x}",
            attr(egl::RED_SIZE),
            attr(egl::GREEN_SIZE),
            attr(egl::BLUE_SIZE),
            attr(egl::ALPHA_SIZE),
            attr(egl::DEPTH_SIZE),
            attr(egl::STENCIL_SIZE),
            attr(egl::SAMPLES),
            attr(egl::SURFACE_TYPE),
            attr(egl::RENDERABLE_TYPE),
        );

        Ok(Self {
            _instance: instance,
            adapter,
            device,
            queue,
            egl_display: display,
            egl_config: config,
            egl_context: context,
            gl_context,
            dummy_surface: surface,
            window_surface: Some(surface),
            _hvr_egl_instance: Some(egl_instance),
            create_image,
            destroy_image,
            get_native_client_buffer,
            image_target_texture_2d,
        })
    }

    /// QiYu SXR path: adopt the EGL context that is *already current* (created by the native SXR
    /// backend's initialize_egl) instead of letting wgpu create its own. This keeps the ALVR
    /// lobby/stream renderers, the eye swapchain textures, and qiyu_SubmitFrame all in the SAME
    /// GL context/namespace. With separate contexts (the default new_gl path) the textures wgpu
    /// renders into live in a different namespace than the ones SXR submits, so timewarp sees
    /// empty frames ("No valid frame to warp", checkFrameCount(0)) and the render loop deadlocks
    /// on the second submit -> black screen. Mirrors the Huawei new_gl_hvr external-adopt pattern.
    #[cfg(target_os = "android")]
    pub unsafe fn new_gl_external() -> Result<Self, String> {
        use std::mem;
        use wgpu::{
            Backends, DeviceDescriptor, Features, InstanceDescriptor, InstanceFlags, Limits,
            MemoryHints,
        };

        const CREATE_IMAGE_FN_STR: &str = "eglCreateImageKHR";
        const DESTROY_IMAGE_FN_STR: &str = "eglDestroyImageKHR";
        const GET_NATIVE_CLIENT_BUFFER_FN_STR: &str = "eglGetNativeClientBufferANDROID";
        const IMAGE_TARGET_TEXTURE_2D_FN_STR: &str = "glEGLImageTargetTexture2DOES";

        let egl_instance = unsafe {
            egl::DynamicInstance::<egl::EGL1_4>::load_required()
                .map_err(|error| format!("Cannot load libEGL: {error}"))?
        };

        // Capture the context/display/surface the SXR backend made current in initialize_egl.
        let display = egl_instance
            .get_current_display()
            .ok_or_else(|| "no current EGL display (SXR context not current)".to_owned())?;
        let context = egl_instance
            .get_current_context()
            .ok_or_else(|| "no current EGL context (SXR context not current)".to_owned())?;
        let surface = egl_instance
            .get_current_surface(egl::DRAW)
            .ok_or_else(|| "no current EGL draw surface".to_owned())?;

        // Resolve the config backing the current context so GraphicsContext has a valid handle.
        let config_id = egl_instance
            .query_context(display, context, egl::CONFIG_ID)
            .map_err(|error| format!("eglQueryContext(CONFIG_ID) failed: {error}"))?;
        let mut configs = Vec::with_capacity(
            egl_instance
                .get_config_count(display)
                .map_err(|error| format!("eglGetConfigs count failed: {error}"))?,
        );
        egl_instance
            .get_configs(display, &mut configs)
            .map_err(|error| format!("eglGetConfigs failed: {error}"))?;
        let config = configs
            .into_iter()
            .find(|&cfg| {
                egl_instance
                    .get_config_attrib(display, cfg, egl::CONFIG_ID)
                    .ok()
                    == Some(config_id)
            })
            .ok_or_else(|| "could not resolve EGL config from current context".to_owned())?;

        let flags = if cfg!(debug_assertions) {
            InstanceFlags::DEBUG | InstanceFlags::VALIDATION
        } else {
            InstanceFlags::empty()
        };
        let instance = Instance::new(&InstanceDescriptor {
            backends: Backends::GL,
            flags,
            backend_options: Default::default(),
        });

        // Instance creation may switch EGL contexts. Restore the SXR context before exposing it to
        // wgpu-hal; external GLES adapters require their context to remain current.
        egl_instance
            .make_current(display, Some(surface), Some(surface), Some(context))
            .map_err(|error| format!("eglMakeCurrent after wgpu init failed: {error}"))?;
        let exposed_adapter = unsafe {
            <api::Gles as hal::Api>::Adapter::new_external(|name| {
                egl_instance
                    .get_proc_address(name)
                    .map_or(ptr::null(), |function| function as *const c_void)
            })
        }
        .ok_or_else(|| "wgpu could not expose the SXR GLES context".to_owned())?;
        let adapter = unsafe { instance.create_adapter_from_hal(exposed_adapter) };
        let (device, queue) = pollster::block_on(adapter.request_device(
            &DeviceDescriptor {
                label: None,
                required_features: Features::PUSH_CONSTANTS,
                required_limits: Limits {
                    max_push_constant_size: MAX_PUSH_CONSTANTS_SIZE,
                    ..adapter.limits()
                },
                memory_hints: MemoryHints::Performance,
            },
            None,
        ))
        .map_err(|error| format!("wgpu request_device failed: {error}"))?;

        let gl_context = gl::Context::from_loader_function(|name| {
            egl_instance
                .get_proc_address(name)
                .map_or(ptr::null(), |function| function as *const c_void)
        });
        let get_fn_ptr = |name| {
            egl_instance
                .get_proc_address(name)
                .map_or(ptr::null(), |function| function as *const c_void)
        };
        let create_image: CreateImageFn = mem::transmute(get_fn_ptr(CREATE_IMAGE_FN_STR));
        let destroy_image: DestroyImageFn = mem::transmute(get_fn_ptr(DESTROY_IMAGE_FN_STR));
        let get_native_client_buffer: GetNativeClientBufferFn =
            mem::transmute(get_fn_ptr(GET_NATIVE_CLIENT_BUFFER_FN_STR));
        let image_target_texture_2d: ImageTargetTexture2DFn =
            mem::transmute(get_fn_ptr(IMAGE_TARGET_TEXTURE_2D_FN_STR));

        Ok(Self {
            _instance: instance,
            adapter,
            device,
            queue,
            egl_display: display,
            egl_config: config,
            egl_context: context,
            gl_context,
            dummy_surface: surface,
            window_surface: Some(surface),
            _hvr_egl_instance: Some(egl_instance),
            create_image,
            destroy_image,
            get_native_client_buffer,
            image_target_texture_2d,
        })
    }

    #[cfg(windows)]
    pub fn new_gl() -> Self {
        unimplemented!()
    }

    pub fn make_current(&self) {
        #[cfg(not(windows))]
        unsafe {
            #[cfg(target_os = "android")]
            let surface = self.window_surface.unwrap_or(self.dummy_surface);
            #[cfg(not(target_os = "android"))]
            let surface = self.dummy_surface;

            #[cfg(target_os = "android")]
            if let Some(egl_instance) = &self._hvr_egl_instance {
                // On Huawei the single EGL context is kept continuously current on the render
                // thread. Callers on other threads (e.g. stream setup) only need it for OpenXR
                // calls that don't require a current GL context, and eglMakeCurrent from a second
                // thread while the context is bound elsewhere fails with EGL_BAD_ACCESS. Treat a
                // failure as non-fatal instead of panicking.
                if let Err(e) = egl_instance.make_current(
                    self.egl_display,
                    Some(surface),
                    Some(surface),
                    Some(self.egl_context),
                ) {
                    alvr_common::warn!("[HVR-DIAG] make_current skipped: {e:?}");
                }
                return;
            }

            self.adapter.as_hal::<api::Gles, _, _>(|raw_adapter| {
                let egl_instance = raw_adapter
                    .unwrap()
                    .adapter_context()
                    .egl_instance()
                    .unwrap();

                egl_instance
                    .make_current(
                        self.egl_display,
                        Some(surface),
                        Some(surface),
                        Some(self.egl_context),
                    )
                    .unwrap();
            })
        };
    }

    #[cfg(target_os = "android")]
    pub fn ensure_hvr_current(&self) -> Result<bool, String> {
        let Some(egl_instance) = &self._hvr_egl_instance else {
            return Ok(false);
        };
        if egl_instance.get_current_context() == Some(self.egl_context) {
            return Ok(false);
        }

        let surface = self.window_surface.unwrap_or(self.dummy_surface);
        egl_instance
            .make_current(
                self.egl_display,
                Some(surface),
                Some(surface),
                Some(self.egl_context),
            )
            .map_err(|error| format!("eglMakeCurrent after swapchain wait failed: {error}"))?;

        Ok(true)
    }

    pub fn gl_texture_status(&self, texture: u32) -> Result<(bool, u32), String> {
        let texture = gl::NativeTexture(
            NonZeroU32::new(texture).ok_or_else(|| "GL texture is zero".to_owned())?,
        );
        unsafe {
            let is_texture = self.gl_context.is_texture(texture);
            let framebuffer = self
                .gl_context
                .create_framebuffer()
                .map_err(|error| format!("glCreateFramebuffer failed: {error}"))?;
            self.gl_context
                .bind_framebuffer(gl::FRAMEBUFFER, Some(framebuffer));
            self.gl_context.framebuffer_texture_2d(
                gl::FRAMEBUFFER,
                gl::COLOR_ATTACHMENT0,
                gl::TEXTURE_2D,
                Some(texture),
                0,
            );
            let status = self.gl_context.check_framebuffer_status(gl::FRAMEBUFFER);
            self.gl_context.bind_framebuffer(gl::FRAMEBUFFER, None);
            self.gl_context.delete_framebuffer(framebuffer);

            Ok((is_texture, status))
        }
    }

    /// Wait until all queued GL work has completed. Huawei's compositor reads an OpenXR swapchain
    /// texture immediately after release; external wgpu submission alone does not guarantee that
    /// the texture is finished by then.
    pub fn finish(&self) {
        unsafe { self.gl_context.finish() };
    }

    /// # Safety
    /// `buffer` must be a valid AHardwareBuffer.
    /// `texture` must be a valid GL texture.
    pub unsafe fn render_ahardwarebuffer_using_texture(
        &self,
        buffer: *const c_void,
        texture: gl::Texture,
        render_cb: impl FnOnce(),
    ) {
        const EGL_NATIVE_BUFFER_ANDROID: u32 = 0x3140;

        if !buffer.is_null() {
            #[cfg(target_os = "android")]
            let is_hvr = self._hvr_egl_instance.is_some();
            #[cfg(not(target_os = "android"))]
            let is_hvr = false;

            // wgpu can leave a GL error pending. EGL entry points do not own GL's error state, so
            // checking glGetError after eglGetNativeClientBufferANDROID misattributes that stale
            // error and used to abort Huawei on the first decoded frame.
            if is_hvr {
                clear_errors(&self.gl_context);
            }

            let client_buffer = (self.get_native_client_buffer)(buffer);
            if is_hvr && client_buffer.is_null() {
                alvr_common::warn!("[HVR-DIAG] eglGetNativeClientBufferANDROID returned null");
                return;
            }
            if !is_hvr {
                check_error(&self.gl_context, "get_native_client_buffer");
            }

            let image = (self.create_image)(
                self.egl_display.as_ptr(),
                egl::NO_CONTEXT,
                EGL_NATIVE_BUFFER_ANDROID,
                client_buffer,
                ptr::null(),
            );
            if is_hvr && image == egl::NO_IMAGE {
                alvr_common::warn!("[HVR-DIAG] eglCreateImageKHR returned EGL_NO_IMAGE");
                return;
            }
            if is_hvr {
                // Discard errors leaked by the two EGL calls before checking real GL operations.
                clear_errors(&self.gl_context);
            } else {
                check_error(&self.gl_context, "create_image");
            }

            self.gl_context
                .bind_texture(GL_TEXTURE_EXTERNAL_OES, Some(texture));
            if is_hvr {
                if !check_error_nonfatal(&self.gl_context, "bind texture OES") {
                    (self.destroy_image)(self.egl_display.as_ptr(), image);
                    return;
                }
            } else {
                check_error(&self.gl_context, "bind texture OES");
            }

            (self.image_target_texture_2d)(GL_TEXTURE_EXTERNAL_OES, image);
            if is_hvr {
                if !check_error_nonfatal(&self.gl_context, "image_target_texture_2d") {
                    (self.destroy_image)(self.egl_display.as_ptr(), image);
                    return;
                }
            } else {
                check_error(&self.gl_context, "image_target_texture_2d");
            }

            render_cb();

            (self.destroy_image)(self.egl_display.as_ptr(), image);
            if is_hvr {
                check_error_nonfatal(&self.gl_context, "render decoded frame");
            } else {
                check_error(&self.gl_context, "destroy_image");
            }
        }
    }
}

#[cfg(not(windows))]
impl Default for GraphicsContext {
    fn default() -> Self {
        Self::new_gl()
    }
}
