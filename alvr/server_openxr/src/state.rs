//! Shared runtime state: the ALVR server-core context plus the head-pose and
//! view-params bookkeeping the encoder FFI needs to timestamp frames.

use alvr_common::{
    once_cell::sync::Lazy,
    parking_lot::{Mutex, RwLock},
    Fov, Pose, ViewParams,
};
use openxr_sys as sys;
use std::{
    collections::VecDeque,
    sync::atomic::{AtomicBool, AtomicU8},
    time::Duration,
};

const DEFAULT_VIEW_PARAMS: [ViewParams; 2] = [ViewParams {
    pose: Pose {
        orientation: alvr_common::glam::Quat::IDENTITY,
        position: alvr_common::glam::Vec3::ZERO,
    },
    fov: Fov {
        left: -1.0,
        right: 1.0,
        up: 1.0,
        down: -1.0,
    },
}; 2];

pub static LOCAL_VIEW_PARAMS: RwLock<[ViewParams; 2]> = RwLock::new(DEFAULT_VIEW_PARAMS);
pub static CONTROLLER_POSES: Lazy<RwLock<[Pose; 2]>> = Lazy::new(|| {
    RwLock::new([
        Pose {
            orientation: alvr_common::glam::Quat::IDENTITY,
            position: alvr_common::glam::Vec3::ZERO,
        },
        Pose {
            orientation: alvr_common::glam::Quat::IDENTITY,
            position: alvr_common::glam::Vec3::ZERO,
        },
    ])
});
/// Whether the latest client tracking sample contains each controller. Keep
/// this separate from the last pose so a temporarily lost controller cannot
/// leave a stale ray active in the application.
pub static CONTROLLER_TRACKED: Lazy<RwLock<[bool; 2]>> = Lazy::new(|| RwLock::new([false; 2]));
pub static HEAD_POSE_QUEUE: Mutex<VecDeque<(Duration, Pose)>> = Mutex::new(VecDeque::new());
/// Maps a frame's OpenXR `predicted_display_time` (ns) to the exact client
/// tracking sample and pose used by `xrLocateViews`. `xrEndFrame` must use that
/// same pose when composing the image and tag it with that sample's timestamp,
/// so the client's asynchronous timewarp starts from the pose that rendered it.
pub static RENDER_TRACKING_TS: Mutex<VecDeque<(i64, i64, Pose)>> = Mutex::new(VecDeque::new());
pub static REQUEST_IDR: AtomicBool = AtomicBool::new(false);
/// Number of pending OpenXR recenter events. VDXR emits two events for one
/// physical recenter: LOCAL first, then STAGE.
pub static PENDING_RECENTER: AtomicU8 = AtomicU8::new(0);

#[cfg(test)]
mod tests {
    use super::DEFAULT_VIEW_PARAMS;

    #[test]
    fn default_fov_is_non_degenerate() {
        for view in DEFAULT_VIEW_PARAMS {
            assert!(view.fov.left < view.fov.right);
            assert!(view.fov.down < view.fov.up);
        }
    }
}

/// Runtime session state machine, driven by `xrPollEvent`.
///
/// Only one session exists at a time. `pending_states` holds the transitions
/// not yet delivered to the app; the app drives itself forward as it reads them
/// (IDLE→READY, then after `xrBeginSession` READY→SYNCHRONIZED→VISIBLE→FOCUSED).
#[derive(Default)]
pub struct SessionState {
    pub created: bool,
    pub begun: bool,
    pub current: Option<sys::SessionState>,
    pub pending_states: VecDeque<sys::SessionState>,
}

pub static SESSION: Mutex<SessionState> = Mutex::new(SessionState {
    created: false,
    begun: false,
    current: None,
    pending_states: VecDeque::new(),
});

/// Which graphics binding the app created the session with.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum GraphicsApi {
    #[default]
    None,
    D3D11,
    OpenGL,
    Vulkan,
}

/// The app-provided D3D11 device (from `GraphicsBindingD3D11KHR`) and the
/// swapchains created on it. Pointers are stored as `usize` so the statics stay
/// `Send`/`Sync`; access is single-threaded through the OpenXR call sequence.
#[derive(Default)]
pub struct GraphicsState {
    pub vulkan: Option<std::sync::Arc<crate::vulkan::VulkanFns>>,
    pub api: GraphicsApi,
    /// `*mut ID3D11Device` as usize. For OpenGL sessions this is a runtime-owned
    /// device the encoder + interop share; for D3D11 sessions it's the app's.
    pub d3d11_device: usize,
    /// True when `d3d11_device` was created by this runtime rather than supplied
    /// in the application's graphics binding.
    pub owns_d3d11_device: bool,
    /// `HDC` / `HGLRC` from `GraphicsBindingOpenGLWin32KHR`, 0 if not OpenGL.
    pub gl_hdc: usize,
    pub gl_hglrc: usize,
    /// `wglDXOpenDeviceNV` handle for the interop session, 0 until opened.
    pub gl_interop_device: usize,
    /// App-provided Vulkan handles (from `GraphicsBindingVulkanKHR`), stored as
    /// `usize` so the statics stay `Send`/`Sync`. 0 unless the session is Vulkan.
    pub vk_instance: usize,
    pub vk_physical_device: usize,
    pub vk_device: usize,
    /// Queue family/index from `GraphicsBindingVulkanKHR`, for the CPU-copy
    /// readback path (universal fallback when external-memory import is
    /// unsupported by the driver).
    pub vk_queue_family_index: u32,
    pub vk_queue_index: u32,
    pub swapchains: Vec<Swapchain>,
}

pub struct Swapchain {
    /// `*mut ID3D11Texture2D` per image, as usize.
    pub textures: Vec<usize>,
    /// GL texture names per image (OpenGL sessions only), empty for D3D11.
    pub gl_textures: Vec<u32>,
    /// Imported `(VkImage, VkDeviceMemory)` per image aliased onto the D3D11
    /// textures (Vulkan sessions only), empty otherwise.
    pub vk_images: Vec<(u64, u64)>,
    /// NT shared handles per image (Vulkan sessions only). Created once with
    /// `CreateSharedHandle` and reused for both the Vulkan external-memory
    /// import and the encoder's `OpenSharedResource1`; empty otherwise.
    pub nt_handles: Vec<usize>,
    /// When true, this is a Vulkan swapchain without zero-copy interop: the
    /// VkImages are runtime-owned standalone images whose pixels are read back
    /// and CPU-copied into the D3D11 textures at submit time (universal
    /// fallback for drivers that reject D3D11<->Vulkan external memory).
    pub vk_cpu_copy: bool,
    /// Command pools, command buffers and fences used only by Vulkan sessions.
    pub vk_commands: Vec<crate::vulkan::CommandResources>,
    /// WGL interop object handles per image (OpenGL sessions only).
    pub gl_interop_handles: Vec<usize>,
    /// Whether each WGL interop object is currently locked for GL ownership.
    pub gl_interop_locked: Vec<bool>,
    /// When true, this is an OpenGL swapchain without zero-copy interop: the GL
    /// textures are standalone and their pixels are CPU-copied into the D3D11
    /// textures at submit time (universal fallback for GPUs lacking
    /// WGL_NV_DX_interop2).
    pub cpu_copy: bool,
    /// Index the app will next acquire (round-robin).
    pub next_index: u32,
    /// FIFO required because OpenXR acquire/wait/release calls do not pass an
    /// image index except from acquire.
    pub acquisition_order: VecDeque<u32>,
    /// The most recently released image is the one composition layers consume.
    pub last_released_index: Option<u32>,
    pub release_serial: u64,
    pub image_states: Vec<SwapchainImageState>,
    pub format: i64,
    pub array_size: u32,
    /// Width/height, needed for the CPU-copy readback.
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SwapchainImageState {
    pub acquired: bool,
    pub waited: bool,
    pub ever_acquired: bool,
    /// True while this image is protected for use by the next xrEndFrame.
    pub reserved_for_composition: bool,
    /// The per-image Vulkan fence has an outstanding acquire or release submit.
    pub vk_fence_pending: bool,
}

pub static GRAPHICS: Mutex<GraphicsState> = Mutex::new(GraphicsState {
    vulkan: None,
    api: GraphicsApi::None,
    d3d11_device: 0,
    owns_d3d11_device: false,
    gl_hdc: 0,
    gl_hglrc: 0,
    gl_interop_device: 0,
    vk_instance: 0,
    vk_physical_device: 0,
    vk_device: 0,
    vk_queue_family_index: 0,
    vk_queue_index: 0,
    swapchains: Vec::new(),
});

/// Frame-loop bookkeeping. `predicted_display_time` advances one nominal frame
/// period per `xrWaitFrame` so timestamps are monotonic even before real
/// client-driven pacing is wired in.
pub struct FrameLoop {
    pub predicted_display_time_ns: i64,
    pub frame_period_ns: i64,
    pub next_wake_instant: Option<std::time::Instant>,
    pub time_origin_instant: Option<std::time::Instant>,
    pub tracking_time_offset_ns: Option<i64>,
}

pub static FRAME_LOOP: Mutex<FrameLoop> = Mutex::new(FrameLoop {
    predicted_display_time_ns: 0,
    // ~90 Hz nominal until client-driven pacing (blueprint A8b) is wired in.
    frame_period_ns: 11_111_111,
    next_wake_instant: None,
    time_origin_instant: None,
    tracking_time_offset_ns: None,
});
