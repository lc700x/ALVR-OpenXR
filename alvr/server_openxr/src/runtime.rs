//! OpenXR runtime dispatch table.
//!
//! `get_instance_proc_addr` (in [`crate::loader`]) resolves named `xr*` calls to
//! the functions here. This is the incremental core: instance + system
//! lifecycle, enough for a conformance app (`hello_xr`) to reach `xrGetSystem`.
//! Session / swapchain / frame-loop entry points land in later increments
//! (blueprint step 4).

#![allow(non_snake_case)]

use openxr_sys as sys;
// openxr_sys::Handle is not pub-exported at crate root in 0.10; methods are inherent
use alvr_common::{once_cell::sync::Lazy, parking_lot::Mutex};
use ash::vk;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    ffi::{c_char, CStr},
    time::{Duration, Instant},
};

/// Single system id we expose (one virtual HMD).
const ALVR_SYSTEM_ID: u64 = 1;
/// Opaque instance handle value (single-instance runtime).
const ALVR_INSTANCE_HANDLE: u64 = 1;
/// Opaque session handle value (single-session runtime).
const ALVR_SESSION_HANDLE: u64 = 1;
/// Reference-space handles are allocated from here upward so each created space
/// (including per-recenter/seat-adjust spaces, which carry their own
/// `pose_in_reference_space`) gets a distinct handle disjoint from action
/// spaces (0x1000+).
const REFERENCE_SPACE_HANDLE_BASE: u64 = 0x4000_0000;

/// Per-created reference space: its OpenXR type plus the app-supplied
/// `pose_in_reference_space` offset (in the shared client "stage" frame). Apps
/// like Desktop2Stereo recenter/seat-adjust by creating a new reference space
/// with a non-identity offset, so honoring this is required for the seat
/// position to match other OpenXR runtimes.
static REFERENCE_SPACES: Lazy<Mutex<HashMap<u64, (sys::ReferenceSpaceType, alvr_common::Pose)>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));
static NEXT_REFERENCE_SPACE: Mutex<u64> = Mutex::new(REFERENCE_SPACE_HANDLE_BASE);
/// Fixed LOCAL origin captured once from the first tracked head pose of a
/// session, matching VDXR's `locateSpaceToOrigin`: LOCAL is a pure eye-level
/// translation `(0, eye_height, 0)` with identity orientation. Horizontal
/// position is NOT recentered (X/Z stay absolute in the client stage frame);
/// only Y is lifted to eye level so LOCAL-relative head Y starts at 0.
static LOCAL_ORIGIN: Mutex<Option<alvr_common::Pose>> = Mutex::new(None);

/// Base value for swapchain handles (index offset from here).
const SWAPCHAIN_HANDLE_BASE: u64 = 0x200;

/// The one primary stereo view configuration ALVR exposes.
const PRIMARY_VIEW_CONFIG: sys::ViewConfigurationType = sys::ViewConfigurationType::PRIMARY_STEREO;
/// Stereo => two views.
const VIEW_COUNT: usize = 2;
static VULKAN_BACKEND_LOGGED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
/// Placeholder per-eye render resolution until the negotiated config is wired in.
const RECOMMENDED_VIEW_WIDTH: u32 = 2016;
const RECOMMENDED_VIEW_HEIGHT: u32 = 2240;

#[repr(C)]
#[derive(Clone, Copy)]
struct AlvrOpenXrDrawDesc {
    shared_handle: *mut std::ffi::c_void,
    shared_type: u32,
    format: i64,
    array_slice: u32,
    uv_bounds: [f32; 4],
    clip_vertices: [f32; 16],
    alpha_mode: u32,
}

fn next_acquirable_image(
    states: &[crate::state::SwapchainImageState],
    next_index: u32,
) -> Option<usize> {
    let count = states.len();
    if count == 0 {
        return None;
    }
    (0..count)
        .map(|offset| (next_index as usize + offset) % count)
        .find(|&index| !states[index].acquired && !states[index].reserved_for_composition)
}

fn next_unwaited_image(
    order: &VecDeque<u32>,
    states: &[crate::state::SwapchainImageState],
) -> Option<u32> {
    order.iter().copied().find(|&index| {
        states
            .get(index as usize)
            .is_some_and(|state| !state.waited)
    })
}

fn record_released_image(
    states: &mut [crate::state::SwapchainImageState],
    previous: &mut Option<u32>,
    release_serial: &mut u64,
    index: u32,
) {
    if let Some(previous) = *previous {
        if let Some(state) = states.get_mut(previous as usize) {
            state.reserved_for_composition = false;
        }
    }
    *release_serial = (*release_serial).wrapping_add(1);
    *previous = Some(index);
    if let Some(state) = states.get_mut(index as usize) {
        state.reserved_for_composition = true;
    }
}

fn valid_array_slice(array_size: u32, array_slice: u32) -> bool {
    array_slice < array_size
}

fn vk_wait_timeout_ns(timeout: sys::Duration) -> Result<u64, sys::Result> {
    let timeout_ns = timeout.as_nanos();
    if timeout_ns < -1 {
        Err(sys::Result::ERROR_VALIDATION_FAILURE)
    } else if timeout_ns < 0 {
        Ok(u64::MAX)
    } else {
        Ok(timeout_ns as u64)
    }
}

fn openxr_vk_wait_result(result: Result<(), vk::Result>) -> sys::Result {
    match result {
        Ok(()) => sys::Result::SUCCESS,
        Err(vk::Result::TIMEOUT) => sys::Result::TIMEOUT_EXPIRED,
        Err(_) => sys::Result::ERROR_RUNTIME_FAILURE,
    }
}

unsafe extern "C" {
    /// Draw per-eye OpenXR composition layers and submit the composed frame.
    /// Safe no-op until the encoder is started.
    fn alvr_openxr_encoder_submit_layers(
        layers_left: *const AlvrOpenXrDrawDesc,
        layer_count_left: u32,
        layers_right: *const AlvrOpenXrDrawDesc,
        layer_count_right: u32,
        width_left: u32,
        height_left: u32,
        width_right: u32,
        height_right: u32,
        fov_left: *const f32,
        fov_right: *const f32,
        eye_to_head_left: *const f32,
        eye_to_head_right: *const f32,
        timestamp_ns: u64,
        idr: bool,
    );
    fn alvr_openxr_encoder_forget_texture(shared_handle: *mut std::ffi::c_void);
}

const RUNTIME_NAME: &str = "ALVR";
/// Extensions the runtime advertises. Graphics bindings per blueprint Q5.
const SUPPORTED_EXTENSIONS: &[&str] = &[
    "XR_KHR_D3D11_enable",
    "XR_KHR_opengl_enable",
    "XR_KHR_vulkan_enable",
    "XR_KHR_vulkan_enable2",
];

/// Copy a `&str` (plus NUL) into a fixed C char buffer, truncating if needed.
fn write_cstr(dst: &mut [c_char], src: &str) {
    let bytes = src.as_bytes();
    let n = bytes.len().min(dst.len().saturating_sub(1));
    for (slot, &b) in dst.iter_mut().zip(&bytes[..n]) {
        *slot = b as c_char;
    }
    if let Some(term) = dst.get_mut(n) {
        *term = 0;
    }
}

/// Standard two-call enumeration: fill `count_output`, and `array` if capacity allows.
fn enumerate<T: Copy>(
    items: &[T],
    capacity_input: u32,
    count_output: *mut u32,
    array: *mut T,
) -> sys::Result {
    if !count_output.is_null() {
        unsafe { *count_output = items.len() as u32 };
    }

    if capacity_input == 0 {
        return sys::Result::SUCCESS;
    }
    if (capacity_input as usize) < items.len() || array.is_null() {
        return sys::Result::ERROR_SIZE_INSUFFICIENT;
    }

    unsafe {
        std::ptr::copy_nonoverlapping(items.as_ptr(), array, items.len());
    }
    sys::Result::SUCCESS
}

pub unsafe extern "system" fn enumerate_instance_extension_properties(
    _layer_name: *const c_char,
    property_capacity_input: u32,
    property_count_output: *mut u32,
    properties: *mut sys::ExtensionProperties,
) -> sys::Result {
    if !property_count_output.is_null() {
        unsafe { *property_count_output = SUPPORTED_EXTENSIONS.len() as u32 };
    }

    if property_capacity_input == 0 {
        return sys::Result::SUCCESS;
    }
    if (property_capacity_input as usize) < SUPPORTED_EXTENSIONS.len() || properties.is_null() {
        return sys::Result::ERROR_SIZE_INSUFFICIENT;
    }

    for (i, name) in SUPPORTED_EXTENSIONS.iter().enumerate() {
        // # Safety: capacity checked >= len; i in bounds.
        let prop = unsafe { &mut *properties.add(i) };
        prop.ty = sys::StructureType::EXTENSION_PROPERTIES;
        prop.next = std::ptr::null_mut();
        prop.extension_version = 1;
        write_cstr(&mut prop.extension_name, name);
    }
    sys::Result::SUCCESS
}

pub unsafe extern "system" fn create_instance(
    create_info: *const sys::InstanceCreateInfo,
    instance: *mut sys::Instance,
) -> sys::Result {
    if create_info.is_null() || instance.is_null() {
        return sys::Result::ERROR_VALIDATION_FAILURE;
    }

    // # Safety: non-null checked; loader passes a valid create-info.
    let info = unsafe { &*create_info };

    // Reject any extension we don't advertise.
    for i in 0..info.enabled_extension_count {
        let name_ptr = unsafe { *info.enabled_extension_names.add(i as usize) };
        if name_ptr.is_null() {
            continue;
        }
        let name = unsafe { CStr::from_ptr(name_ptr) };
        let known = SUPPORTED_EXTENSIONS
            .iter()
            .any(|e| name.to_bytes() == e.as_bytes());
        if !known {
            return sys::Result::ERROR_EXTENSION_NOT_PRESENT;
        }
    }

    unsafe { *instance = sys::Instance::from_raw(ALVR_INSTANCE_HANDLE) };
    sys::Result::SUCCESS
}

pub unsafe extern "system" fn destroy_instance(_instance: sys::Instance) -> sys::Result {
    // The loader may unload our DLL immediately after this call returns.
    crate::streaming::shutdown();
    sys::Result::SUCCESS
}

pub unsafe extern "system" fn get_instance_properties(
    _instance: sys::Instance,
    instance_properties: *mut sys::InstanceProperties,
) -> sys::Result {
    if instance_properties.is_null() {
        return sys::Result::ERROR_VALIDATION_FAILURE;
    }

    let props = unsafe { &mut *instance_properties };
    props.ty = sys::StructureType::INSTANCE_PROPERTIES;
    props.runtime_version = sys::CURRENT_API_VERSION;
    write_cstr(&mut props.runtime_name, RUNTIME_NAME);
    sys::Result::SUCCESS
}

pub unsafe extern "system" fn get_system(
    _instance: sys::Instance,
    get_info: *const sys::SystemGetInfo,
    system_id: *mut sys::SystemId,
) -> sys::Result {
    if get_info.is_null() || system_id.is_null() {
        return sys::Result::ERROR_VALIDATION_FAILURE;
    }

    let info = unsafe { &*get_info };
    if info.form_factor != sys::FormFactor::HEAD_MOUNTED_DISPLAY {
        return sys::Result::ERROR_FORM_FACTOR_UNSUPPORTED;
    }

    unsafe { *system_id = sys::SystemId::from_raw(ALVR_SYSTEM_ID) };
    sys::Result::SUCCESS
}

pub unsafe extern "system" fn get_system_properties(
    _instance: sys::Instance,
    _system_id: sys::SystemId,
    properties: *mut sys::SystemProperties,
) -> sys::Result {
    if properties.is_null() {
        return sys::Result::ERROR_VALIDATION_FAILURE;
    }

    let props = unsafe { &mut *properties };
    props.ty = sys::StructureType::SYSTEM_PROPERTIES;
    props.system_id = sys::SystemId::from_raw(ALVR_SYSTEM_ID);
    props.vendor_id = 0;
    write_cstr(&mut props.system_name, RUNTIME_NAME);
    // ponytail: placeholder graphics/tracking limits; real values come from the
    // negotiated streaming config when the session lifecycle lands.
    props.graphics_properties.max_swapchain_image_width = 8192;
    props.graphics_properties.max_swapchain_image_height = 8192;
    props.graphics_properties.max_layer_count = 16;
    props.tracking_properties.orientation_tracking = sys::TRUE;
    props.tracking_properties.position_tracking = sys::TRUE;
    sys::Result::SUCCESS
}

// --- View configuration ----------------------------------------------------

pub unsafe extern "system" fn enumerate_view_configurations(
    _instance: sys::Instance,
    _system_id: sys::SystemId,
    capacity_input: u32,
    count_output: *mut u32,
    view_configuration_types: *mut sys::ViewConfigurationType,
) -> sys::Result {
    enumerate(
        &[PRIMARY_VIEW_CONFIG],
        capacity_input,
        count_output,
        view_configuration_types,
    )
}

pub unsafe extern "system" fn get_view_configuration_properties(
    _instance: sys::Instance,
    _system_id: sys::SystemId,
    view_configuration_type: sys::ViewConfigurationType,
    configuration_properties: *mut sys::ViewConfigurationProperties,
) -> sys::Result {
    if configuration_properties.is_null() {
        return sys::Result::ERROR_VALIDATION_FAILURE;
    }
    if view_configuration_type != PRIMARY_VIEW_CONFIG {
        return sys::Result::ERROR_VIEW_CONFIGURATION_TYPE_UNSUPPORTED;
    }

    let props = unsafe { &mut *configuration_properties };
    props.ty = sys::StructureType::VIEW_CONFIGURATION_PROPERTIES;
    props.view_configuration_type = PRIMARY_VIEW_CONFIG;
    props.fov_mutable = sys::TRUE;
    sys::Result::SUCCESS
}

pub unsafe extern "system" fn enumerate_view_configuration_views(
    _instance: sys::Instance,
    _system_id: sys::SystemId,
    view_configuration_type: sys::ViewConfigurationType,
    view_capacity_input: u32,
    view_count_output: *mut u32,
    views: *mut sys::ViewConfigurationView,
) -> sys::Result {
    if view_configuration_type != PRIMARY_VIEW_CONFIG {
        return sys::Result::ERROR_VIEW_CONFIGURATION_TYPE_UNSUPPORTED;
    }
    if !view_count_output.is_null() {
        unsafe { *view_count_output = VIEW_COUNT as u32 };
    }
    if view_capacity_input == 0 {
        return sys::Result::SUCCESS;
    }
    if (view_capacity_input as usize) < VIEW_COUNT || views.is_null() {
        return sys::Result::ERROR_SIZE_INSUFFICIENT;
    }

    // Prefer the ALVR-configured stereo resolution (per eye) so the app's
    // swapchains match the dashboard setting; fall back to the placeholder if
    // settings can't be read yet.
    let (view_width, view_height) = crate::streaming::recommended_view_size()
        .unwrap_or((RECOMMENDED_VIEW_WIDTH, RECOMMENDED_VIEW_HEIGHT));

    for i in 0..VIEW_COUNT {
        let view = unsafe { &mut *views.add(i) };
        view.ty = sys::StructureType::VIEW_CONFIGURATION_VIEW;
        view.recommended_image_rect_width = view_width;
        view.max_image_rect_width = view_width;
        view.recommended_image_rect_height = view_height;
        view.max_image_rect_height = view_height;
        view.recommended_swapchain_sample_count = 1;
        view.max_swapchain_sample_count = 1;
    }
    sys::Result::SUCCESS
}

pub unsafe extern "system" fn enumerate_environment_blend_modes(
    _instance: sys::Instance,
    _system_id: sys::SystemId,
    view_configuration_type: sys::ViewConfigurationType,
    capacity_input: u32,
    count_output: *mut u32,
    environment_blend_modes: *mut sys::EnvironmentBlendMode,
) -> sys::Result {
    if view_configuration_type != PRIMARY_VIEW_CONFIG {
        return sys::Result::ERROR_VIEW_CONFIGURATION_TYPE_UNSUPPORTED;
    }
    enumerate(
        &[sys::EnvironmentBlendMode::OPAQUE],
        capacity_input,
        count_output,
        environment_blend_modes,
    )
}

// --- Session ---------------------------------------------------------------

pub unsafe extern "system" fn create_session(
    _instance: sys::Instance,
    create_info: *const sys::SessionCreateInfo,
    session: *mut sys::Session,
) -> sys::Result {
    if create_info.is_null() || session.is_null() {
        return sys::Result::ERROR_VALIDATION_FAILURE;
    }

    let mut state = crate::state::SESSION.lock();
    if state.created {
        return sys::Result::ERROR_LIMIT_REACHED;
    }
    state.created = true;
    state.begun = false;
    state.current = None;
    state.pending_states.clear();
    *LOCAL_ORIGIN.lock() = None;
    crate::actions::clear_buttons();
    // First transition the app observes: the session becomes READY so it can
    // call xrBeginSession.
    state.pending_states.push_back(sys::SessionState::IDLE);
    state.pending_states.push_back(sys::SessionState::READY);

    // Capture the app's graphics binding (D3D11 device or OpenGL context).
    let info = unsafe { &*create_info };
    unsafe { capture_graphics_binding(info.next) };

    {
        let mut graphics = crate::state::GRAPHICS.lock();
        if graphics.api == crate::state::GraphicsApi::Vulkan {
            graphics.vulkan = unsafe {
                crate::vulkan::VulkanFns::load(
                    graphics.vk_instance,
                    graphics.vk_physical_device,
                    graphics.vk_device,
                    graphics.vk_queue_family_index,
                    graphics.vk_queue_index,
                )
            }
            .map(std::sync::Arc::new);
            if graphics.vulkan.is_none() {
                *graphics = crate::state::GraphicsState::default();
                *state = crate::state::SessionState::default();
                return sys::Result::ERROR_RUNTIME_FAILURE;
            }
        }
    }
    unsafe { *session = sys::Session::from_raw(ALVR_SESSION_HANDLE) };
    sys::Result::SUCCESS
}

pub unsafe extern "system" fn destroy_session(_session: sys::Session) -> sys::Result {
    // Stop the encoder before releasing shared textures or its D3D11 device.
    crate::streaming::set_session_active(false);
    let mut graphics = crate::state::GRAPHICS.lock();
    let owns_d3d11_device = graphics.owns_d3d11_device;
    let d3d11_device = graphics.d3d11_device;
    let vk = if graphics.api == crate::state::GraphicsApi::Vulkan {
        graphics.vulkan.clone()
    } else {
        None
    };
    if graphics.api == crate::state::GraphicsApi::Vulkan && vk.is_none() {
        return sys::Result::ERROR_RUNTIME_FAILURE;
    }
    let api = graphics.api;
    for swapchain in &mut graphics.swapchains {
        if let Err(result) =
            unsafe { destroy_swapchain_contents(api, d3d11_device, vk.as_deref(), swapchain) }
        {
            return result;
        }
    }
    *graphics = crate::state::GraphicsState::default();
    drop(graphics);
    if owns_d3d11_device {
        unsafe { crate::d3d11::release_device(d3d11_device) };
    }
    let mut state = crate::state::SESSION.lock();
    *state = crate::state::SessionState::default();
    crate::actions::clear_buttons();
    REFERENCE_SPACES.lock().clear();
    *LOCAL_ORIGIN.lock() = None;
    sys::Result::SUCCESS
}

pub unsafe extern "system" fn begin_session(
    _session: sys::Session,
    begin_info: *const sys::SessionBeginInfo,
) -> sys::Result {
    if begin_info.is_null() {
        return sys::Result::ERROR_VALIDATION_FAILURE;
    }
    let info = unsafe { &*begin_info };
    if info.primary_view_configuration_type != PRIMARY_VIEW_CONFIG {
        return sys::Result::ERROR_VIEW_CONFIGURATION_TYPE_UNSUPPORTED;
    }

    let mut state = crate::state::SESSION.lock();
    if !state.created {
        return sys::Result::ERROR_SESSION_NOT_RUNNING;
    }
    state.begun = true;
    // Progress toward focus so the app starts submitting frames.
    state
        .pending_states
        .push_back(sys::SessionState::SYNCHRONIZED);
    state.pending_states.push_back(sys::SessionState::VISIBLE);
    state.pending_states.push_back(sys::SessionState::FOCUSED);
    drop(state);

    // Bring up ALVR streaming (idempotent): connect to a client and start the
    // encoder event loop.
    crate::streaming::set_session_active(true);
    sys::Result::SUCCESS
}

pub unsafe extern "system" fn end_session(_session: sys::Session) -> sys::Result {
    let mut state = crate::state::SESSION.lock();
    state.begun = false;
    sys::Result::SUCCESS
}

pub unsafe extern "system" fn request_exit_session(_session: sys::Session) -> sys::Result {
    let mut state = crate::state::SESSION.lock();
    state.pending_states.push_back(sys::SessionState::STOPPING);
    state.pending_states.push_back(sys::SessionState::IDLE);
    state.pending_states.push_back(sys::SessionState::EXITING);
    sys::Result::SUCCESS
}

// --- Reference spaces ------------------------------------------------------

pub unsafe extern "system" fn enumerate_reference_spaces(
    _session: sys::Session,
    capacity_input: u32,
    count_output: *mut u32,
    spaces: *mut sys::ReferenceSpaceType,
) -> sys::Result {
    enumerate(
        &[
            sys::ReferenceSpaceType::VIEW,
            sys::ReferenceSpaceType::LOCAL,
            sys::ReferenceSpaceType::STAGE,
        ],
        capacity_input,
        count_output,
        spaces,
    )
}

pub unsafe extern "system" fn create_reference_space(
    _session: sys::Session,
    create_info: *const sys::ReferenceSpaceCreateInfo,
    space: *mut sys::Space,
) -> sys::Result {
    if create_info.is_null() || space.is_null() {
        return sys::Result::ERROR_VALIDATION_FAILURE;
    }
    let info = unsafe { &*create_info };
    // Store the reference-space type and the app's `pose_in_reference_space`
    // offset so xrLocateSpace / xrLocateViews can express poses in this exact
    // space (seat-adjust / recenter create offset spaces).
    let mut next = NEXT_REFERENCE_SPACE.lock();
    let raw = *next;
    *next += 1;
    REFERENCE_SPACES.lock().insert(
        raw,
        (
            info.reference_space_type,
            xr_to_pose(info.pose_in_reference_space),
        ),
    );
    unsafe { *space = sys::Space::from_raw(raw) };
    sys::Result::SUCCESS
}

pub unsafe extern "system" fn destroy_space(_space: sys::Space) -> sys::Result {
    REFERENCE_SPACES.lock().remove(&_space.into_raw());
    sys::Result::SUCCESS
}

// --- D3D11 graphics binding + swapchains ------------------------------------

pub unsafe extern "system" fn get_d3d11_graphics_requirements(
    _instance: sys::Instance,
    _system_id: sys::SystemId,
    graphics_requirements: *mut sys::GraphicsRequirementsD3D11KHR,
) -> sys::Result {
    if graphics_requirements.is_null() {
        return sys::Result::ERROR_VALIDATION_FAILURE;
    }

    let req = unsafe { &mut *graphics_requirements };
    req.ty = sys::StructureType::GRAPHICS_REQUIREMENTS_D3D11_KHR;
    // Report the default adapter's real LUID. Apps like hello_xr strictly call
    // EnumAdapterByLuid and fail (DXGI_ERROR_NOT_FOUND) on a zero LUID.
    req.adapter_luid = match crate::d3d11::default_adapter_luid() {
        Some((low, high)) => sys::platform::LUID {
            LowPart: low,
            HighPart: high,
        },
        None => return sys::Result::ERROR_GRAPHICS_DEVICE_INVALID,
    };
    // D3D_FEATURE_LEVEL_11_0
    req.min_feature_level = 0xb000;
    sys::Result::SUCCESS
}

pub unsafe extern "system" fn get_opengl_graphics_requirements(
    _instance: sys::Instance,
    _system_id: sys::SystemId,
    graphics_requirements: *mut sys::GraphicsRequirementsOpenGLKHR,
) -> sys::Result {
    if graphics_requirements.is_null() {
        return sys::Result::ERROR_VALIDATION_FAILURE;
    }

    let req = unsafe { &mut *graphics_requirements };
    req.ty = sys::StructureType::GRAPHICS_REQUIREMENTS_OPENGL_KHR;
    // Report a low minimum so any modern GL context is accepted: apps throw if
    // min_api_version_supported > their context version. Our GL path only needs
    // core GL 1.1 (CPU-copy fallback) or WGL_NV_DX_interop2 (zero-copy), neither
    // of which requires GL 4.5.
    req.min_api_version_supported = sys::Version::new(3, 0, 0);
    req.max_api_version_supported = sys::Version::new(4, 6, 0);
    sys::Result::SUCCESS
}

// --- Vulkan graphics binding ------------------------------------------------

/// Shared implementation for `xrGetVulkanGraphicsRequirementsKHR` and the
/// `enable2` variant `xrGetVulkanGraphicsRequirements2KHR` (identical struct).
pub unsafe extern "system" fn get_vulkan_graphics_requirements(
    _instance: sys::Instance,
    _system_id: sys::SystemId,
    graphics_requirements: *mut sys::GraphicsRequirementsVulkanKHR,
) -> sys::Result {
    if graphics_requirements.is_null() {
        return sys::Result::ERROR_VALIDATION_FAILURE;
    }
    let req = unsafe { &mut *graphics_requirements };
    req.ty = sys::StructureType::GRAPHICS_REQUIREMENTS_VULKAN_KHR;
    // Accept any Vulkan 1.0+ instance; our interop needs only KHR external
    // memory extensions, all available since 1.0 as KHR.
    req.min_api_version_supported = sys::Version::new(1, 0, 0);
    req.max_api_version_supported = sys::Version::new(1, 3, 0);
    sys::Result::SUCCESS
}

/// Copy a space-separated extension list into the app buffer with the standard
/// two-call sizing protocol.
fn write_extension_list(
    exts: &[&std::ffi::CStr],
    capacity_input: u32,
    count_output: *mut u32,
    buffer: *mut c_char,
) -> sys::Result {
    let mut joined = String::new();
    for (i, ext) in exts.iter().enumerate() {
        if i > 0 {
            joined.push(' ');
        }
        joined.push_str(&ext.to_string_lossy());
    }
    let bytes = joined.as_bytes();
    let needed = bytes.len() as u32 + 1; // + NUL

    if !count_output.is_null() {
        unsafe { *count_output = needed };
    }
    if capacity_input == 0 {
        return sys::Result::SUCCESS;
    }
    if capacity_input < needed || buffer.is_null() {
        return sys::Result::ERROR_SIZE_INSUFFICIENT;
    }
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr().cast(), buffer, bytes.len());
        *buffer.add(bytes.len()) = 0;
    }
    sys::Result::SUCCESS
}

pub unsafe extern "system" fn get_vulkan_instance_extensions(
    _instance: sys::Instance,
    _system_id: sys::SystemId,
    buffer_capacity_input: u32,
    buffer_count_output: *mut u32,
    buffer: *mut c_char,
) -> sys::Result {
    write_extension_list(
        crate::vulkan::REQUIRED_INSTANCE_EXTENSIONS,
        buffer_capacity_input,
        buffer_count_output,
        buffer,
    )
}

pub unsafe extern "system" fn get_vulkan_device_extensions(
    _instance: sys::Instance,
    _system_id: sys::SystemId,
    buffer_capacity_input: u32,
    buffer_count_output: *mut u32,
    buffer: *mut c_char,
) -> sys::Result {
    write_extension_list(
        crate::vulkan::REQUIRED_DEVICE_EXTENSIONS,
        buffer_capacity_input,
        buffer_count_output,
        buffer,
    )
}

/// `xrGetVulkanGraphicsDeviceKHR` — pick the VkPhysicalDevice matching the
/// encoder's D3D11 adapter LUID.
pub unsafe extern "system" fn get_vulkan_graphics_device(
    _instance: sys::Instance,
    _system_id: sys::SystemId,
    vk_instance: sys::platform::VkInstance,
    vk_physical_device: *mut sys::platform::VkPhysicalDevice,
) -> sys::Result {
    if vk_physical_device.is_null() {
        return sys::Result::ERROR_VALIDATION_FAILURE;
    }
    let Some(luid) = crate::vulkan::default_adapter_luid_bytes() else {
        return sys::Result::ERROR_GRAPHICS_DEVICE_INVALID;
    };
    match unsafe { crate::vulkan::physical_device_for_luid(vk_instance as usize, luid) } {
        Some(pd) => {
            unsafe { *vk_physical_device = pd as sys::platform::VkPhysicalDevice };
            sys::Result::SUCCESS
        }
        None => sys::Result::ERROR_RUNTIME_FAILURE,
    }
}

/// `xrGetVulkanGraphicsDevice2KHR` — same as above via the get-info struct.
pub unsafe extern "system" fn get_vulkan_graphics_device2(
    _instance: sys::Instance,
    get_info: *const sys::VulkanGraphicsDeviceGetInfoKHR,
    vk_physical_device: *mut sys::platform::VkPhysicalDevice,
) -> sys::Result {
    if get_info.is_null() || vk_physical_device.is_null() {
        return sys::Result::ERROR_VALIDATION_FAILURE;
    }
    let info = unsafe { &*get_info };
    let Some(luid) = crate::vulkan::default_adapter_luid_bytes() else {
        return sys::Result::ERROR_GRAPHICS_DEVICE_INVALID;
    };
    match unsafe { crate::vulkan::physical_device_for_luid(info.vulkan_instance as usize, luid) } {
        Some(pd) => {
            unsafe { *vk_physical_device = pd as sys::platform::VkPhysicalDevice };
            sys::Result::SUCCESS
        }
        None => sys::Result::ERROR_RUNTIME_FAILURE,
    }
}

/// `xrCreateVulkanInstanceKHR` — create the app's VkInstance with the interop
/// instance extensions injected.
pub unsafe extern "system" fn create_vulkan_instance(
    _instance: sys::Instance,
    create_info: *const sys::VulkanInstanceCreateInfoKHR,
    vulkan_instance: *mut sys::platform::VkInstance,
    vulkan_result: *mut sys::platform::VkResult,
) -> sys::Result {
    if create_info.is_null() || vulkan_instance.is_null() || vulkan_result.is_null() {
        return sys::Result::ERROR_VALIDATION_FAILURE;
    }
    let info = unsafe { &*create_info };
    let Some(gipa) = info.pfn_get_instance_proc_addr else {
        return sys::Result::ERROR_VALIDATION_FAILURE;
    };
    let mut out_instance = 0usize;
    let mut out_result = 0i32;
    let ok = unsafe {
        crate::vulkan::create_instance(
            gipa as usize,
            info.vulkan_create_info.cast::<vk::InstanceCreateInfo<'_>>(),
            info.vulkan_allocator.cast::<vk::AllocationCallbacks<'_>>(),
            &mut out_instance,
            &mut out_result,
        )
    };
    unsafe {
        *vulkan_instance = out_instance as sys::platform::VkInstance;
        *vulkan_result = out_result;
    }
    let _ = ok;
    sys::Result::SUCCESS
}

/// `xrCreateVulkanDeviceKHR` — create the app's VkDevice with the interop device
/// extensions injected.
pub unsafe extern "system" fn create_vulkan_device(
    _instance: sys::Instance,
    create_info: *const sys::VulkanDeviceCreateInfoKHR,
    vulkan_device: *mut sys::platform::VkDevice,
    vulkan_result: *mut sys::platform::VkResult,
) -> sys::Result {
    if create_info.is_null() || vulkan_device.is_null() || vulkan_result.is_null() {
        return sys::Result::ERROR_VALIDATION_FAILURE;
    }
    let info = unsafe { &*create_info };
    let Some(gipa) = info.pfn_get_instance_proc_addr else {
        return sys::Result::ERROR_VALIDATION_FAILURE;
    };
    let mut out_device = 0usize;
    let mut out_result = 0i32;
    let ok = unsafe {
        crate::vulkan::create_device(
            gipa as usize,
            info.vulkan_physical_device as usize,
            info.vulkan_create_info.cast::<vk::DeviceCreateInfo<'_>>(),
            info.vulkan_allocator.cast::<vk::AllocationCallbacks<'_>>(),
            &mut out_device,
            &mut out_result,
        )
    };
    unsafe {
        *vulkan_device = out_device as sys::platform::VkDevice;
        *vulkan_result = out_result;
    }
    let _ = ok;
    sys::Result::SUCCESS
}

/// Inspect a session's graphics-binding chain and record the API + handles.
///
/// # Safety
/// `next` must be null or a valid OpenXR struct chain.
unsafe fn capture_graphics_binding(next: *const std::ffi::c_void) {
    let mut cursor = next;
    while !cursor.is_null() {
        let base = cursor.cast::<sys::BaseInStructure>();
        let ty = unsafe { (*base).ty };
        let mut graphics = crate::state::GRAPHICS.lock();
        if ty == sys::StructureType::GRAPHICS_BINDING_D3D11_KHR {
            let binding = cursor.cast::<sys::GraphicsBindingD3D11KHR>();
            graphics.api = crate::state::GraphicsApi::D3D11;
            graphics.d3d11_device = unsafe { (*binding).device } as usize;
            graphics.owns_d3d11_device = false;
            return;
        }
        if ty == sys::StructureType::GRAPHICS_BINDING_OPENGL_WIN32_KHR {
            let binding = cursor.cast::<sys::GraphicsBindingOpenGLWin32KHR>();
            graphics.api = crate::state::GraphicsApi::OpenGL;
            graphics.gl_hdc = unsafe { (*binding).h_dc } as usize;
            graphics.gl_hglrc = unsafe { (*binding).h_glrc } as usize;
            return;
        }
        if ty == sys::StructureType::GRAPHICS_BINDING_VULKAN_KHR {
            let binding = cursor.cast::<sys::GraphicsBindingVulkanKHR>();
            graphics.api = crate::state::GraphicsApi::Vulkan;
            graphics.vk_instance = unsafe { (*binding).instance } as usize;
            graphics.vk_physical_device = unsafe { (*binding).physical_device } as usize;
            graphics.vk_device = unsafe { (*binding).device } as usize;
            graphics.vk_queue_family_index = unsafe { (*binding).queue_family_index };
            graphics.vk_queue_index = unsafe { (*binding).queue_index };
            return;
        }
        drop(graphics);
        cursor = unsafe { (*base).next }.cast();
    }
}

/// Swapchain formats the runtime accepts for D3D11 sessions (DXGI values).
/// RGBA8/BGRA8 + sRGB variants — what the reused FrameRender/encoder consume.
const SUPPORTED_SWAPCHAIN_FORMATS_D3D11: &[i64] = &[
    28, // DXGI_FORMAT_R8G8B8A8_UNORM
    29, // DXGI_FORMAT_R8G8B8A8_UNORM_SRGB
    87, // DXGI_FORMAT_B8G8R8A8_UNORM
    91, // DXGI_FORMAT_B8G8R8A8_UNORM_SRGB
];

/// Swapchain formats the runtime accepts for OpenGL sessions (GL internal
/// format enums). sRGB first (apps prefer it), then linear RGBA8.
const SUPPORTED_SWAPCHAIN_FORMATS_GL: &[i64] = &[
    0x8C43, // GL_SRGB8_ALPHA8
    0x8058, // GL_RGBA8
];

/// Swapchain formats the runtime accepts for Vulkan sessions (VkFormat enums).
/// sRGB first (apps prefer it), then linear, RGBA then BGRA.
const SUPPORTED_SWAPCHAIN_FORMATS_VK: &[i64] = &[
    crate::vulkan::VK_FORMAT_R8G8B8A8_SRGB,
    crate::vulkan::VK_FORMAT_B8G8R8A8_SRGB,
    crate::vulkan::VK_FORMAT_R8G8B8A8_UNORM,
    crate::vulkan::VK_FORMAT_B8G8R8A8_UNORM,
];

/// Map a GL internal format enum to the equivalent DXGI format used for the
/// backing D3D11 texture. Falls back to R8G8B8A8_UNORM_SRGB.
fn gl_format_to_dxgi(gl_format: i64) -> i32 {
    match gl_format {
        0x8058 => 28, // GL_RGBA8 -> DXGI_FORMAT_R8G8B8A8_UNORM
        0x8C43 => 29, // GL_SRGB8_ALPHA8 -> DXGI_FORMAT_R8G8B8A8_UNORM_SRGB
        _ => 29,
    }
}

pub unsafe extern "system" fn enumerate_swapchain_formats(
    _session: sys::Session,
    format_capacity_input: u32,
    format_count_output: *mut u32,
    formats: *mut i64,
) -> sys::Result {
    let is_gl = crate::state::GRAPHICS.lock().api == crate::state::GraphicsApi::OpenGL;
    let list = if is_gl {
        SUPPORTED_SWAPCHAIN_FORMATS_GL
    } else if crate::state::GRAPHICS.lock().api == crate::state::GraphicsApi::Vulkan {
        SUPPORTED_SWAPCHAIN_FORMATS_VK
    } else {
        SUPPORTED_SWAPCHAIN_FORMATS_D3D11
    };
    enumerate(list, format_capacity_input, format_count_output, formats)
}

fn vulkan_image_usage(usage: sys::SwapchainUsageFlags) -> Option<vk::ImageUsageFlags> {
    use sys::SwapchainUsageFlags as XrUsage;
    if usage.contains(XrUsage::DEPTH_STENCIL_ATTACHMENT)
        || usage.contains(XrUsage::UNORDERED_ACCESS)
    {
        return None;
    }
    // Every color swapchain is acquired in COLOR_ATTACHMENT_OPTIMAL, even when
    // the application only requests transfer or sampled access.
    let mut result = vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::COLOR_ATTACHMENT;
    if usage.contains(XrUsage::TRANSFER_SRC) {
        result |= vk::ImageUsageFlags::TRANSFER_SRC;
    }
    if usage.contains(XrUsage::TRANSFER_DST) {
        result |= vk::ImageUsageFlags::TRANSFER_DST;
    }
    Some(result)
}

fn d3d11_bind_flags() -> u32 {
    use windows::Win32::Graphics::Direct3D11::{
        D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE,
    };
    (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32
}

pub unsafe extern "system" fn create_swapchain(
    _session: sys::Session,
    create_info: *const sys::SwapchainCreateInfo,
    swapchain: *mut sys::Swapchain,
) -> sys::Result {
    if create_info.is_null() || swapchain.is_null() {
        return sys::Result::ERROR_VALIDATION_FAILURE;
    }
    let info = unsafe { &*create_info };
    let session_api = crate::state::GRAPHICS.lock().api;
    let is_vk_session = session_api == crate::state::GraphicsApi::Vulkan;
    if info.width == 0 || info.height == 0 || info.array_size == 0 {
        return sys::Result::ERROR_VALIDATION_FAILURE;
    }
    if info.mip_count != 1 || info.face_count != 1 {
        return sys::Result::ERROR_FEATURE_UNSUPPORTED;
    }
    if is_vk_session
        && (info.sample_count != 1
            || !crate::vulkan::is_supported_format(info.format)
            || vulkan_image_usage(info.usage_flags).is_none())
    {
        return sys::Result::ERROR_FEATURE_UNSUPPORTED;
    }

    // OpenGL and Vulkan sessions have no app D3D11 device; create a
    // runtime-owned one the encoder and interop share.
    {
        let mut graphics = crate::state::GRAPHICS.lock();
        let needs_device = graphics.api == crate::state::GraphicsApi::OpenGL
            || graphics.api == crate::state::GraphicsApi::Vulkan;
        if graphics.d3d11_device == 0 && needs_device {
            match unsafe { crate::d3d11::create_device() } {
                Ok(dev) => {
                    graphics.d3d11_device = dev;
                    graphics.owns_d3d11_device = true;
                }
                Err(_) => return sys::Result::ERROR_GRAPHICS_DEVICE_INVALID,
            }
        }
    }

    let device_ptr = crate::state::GRAPHICS.lock().d3d11_device;
    if device_ptr == 0 {
        return sys::Result::ERROR_GRAPHICS_DEVICE_INVALID;
    }

    // The backing textures are always D3D11. For OpenGL sessions the app's
    // `info.format` is a GL enum, so translate it to the matching DXGI format.
    let is_gl_session = session_api == crate::state::GraphicsApi::OpenGL;
    let vk_usage = if is_vk_session {
        match vulkan_image_usage(info.usage_flags) {
            Some(usage) => usage,
            None => return sys::Result::ERROR_FEATURE_UNSUPPORTED,
        }
    } else {
        vk::ImageUsageFlags::empty()
    };
    let dxgi_format = if is_gl_session {
        gl_format_to_dxgi(info.format) as i64
    } else if is_vk_session {
        crate::vulkan::vk_format_to_dxgi(info.format) as i64
    } else {
        info.format
    };

    // Vulkan sessions import the D3D11 textures via NT shared handles
    // (CreateSharedHandle), which is the only D3D11<->Vulkan external-memory
    // path Vulkan drivers reliably accept; the legacy KMT handle used by the
    // D3D11/GL paths is rejected with VK_ERROR_INVALID_EXTERNAL_HANDLE.
    let mut textures = match unsafe {
        if is_vk_session {
            crate::d3d11::create_swapchain_textures_nt(
                device_ptr,
                dxgi_format,
                info.width,
                info.height,
                info.array_size,
                info.sample_count,
                d3d11_bind_flags(),
            )
        } else {
            crate::d3d11::create_swapchain_textures(
                device_ptr,
                dxgi_format,
                info.width,
                info.height,
                info.array_size,
                info.sample_count,
            )
        }
    } {
        Ok(t) => t,
        Err(_) => return sys::Result::ERROR_RUNTIME_FAILURE,
    };

    let mut graphics = crate::state::GRAPHICS.lock();
    let handle = SWAPCHAIN_HANDLE_BASE + graphics.swapchains.len() as u64;

    // For OpenGL sessions, alias each D3D11 texture to a GL texture name via
    // WGL_NV_DX_interop2 so the app renders GL while the encoder reads D3D11
    // (zero-copy). If interop is unavailable (some AMD/Intel drivers), fall
    // back to standalone GL textures + CPU copy at submit time — this works on
    // every GPU/driver.
    let (gl_textures, gl_interop_handles, cpu_copy) =
        if graphics.api == crate::state::GraphicsApi::OpenGL {
            match unsafe { register_gl_aliases(&mut graphics, &textures) } {
                Ok((gl_tex, handles)) => (gl_tex, handles, false),
                Err(_) => {
                    // Zero-copy path failed — build a CPU-copy swapchain instead.
                    match unsafe {
                        create_cpu_copy_textures(
                            textures.len(),
                            info.width,
                            info.height,
                            info.format as i32,
                        )
                    } {
                        Some(gl_tex) => {
                            eprintln!(
                                "[alvr] create_swapchain: using CPU-copy GL fallback ({} textures) \
                                 — universal GPU compatibility",
                                gl_tex.len()
                            );
                            (gl_tex, Vec::new(), true)
                        }
                        None => {
                            unsafe { crate::d3d11::release_textures(&textures) };
                            return sys::Result::ERROR_RUNTIME_FAILURE;
                        }
                    }
                }
            }
        } else {
            (Vec::new(), Vec::new(), false)
        };

    // For Vulkan sessions, import each shared D3D11 texture into the app's
    // VkDevice as a VkImage aliased onto the same GPU memory (zero-copy). If the
    // driver rejects D3D11<->Vulkan external memory (common on integrated GPUs),
    // fall back to a universal CPU-copy path: the app renders into runtime-owned
    // VkImages whose pixels are read back and uploaded into typed D3D11 textures
    // at submit time (works on every GPU/driver, mirroring the OpenGL fallback).
    let mut vk_fallback_reason = None;
    let (vk_images, nt_handles, vk_cpu_copy) = if graphics.api == crate::state::GraphicsApi::Vulkan
    {
        match unsafe {
            import_vulkan_images(
                &graphics,
                &textures,
                info.format,
                info.width,
                info.height,
                info.array_size,
                vk_usage,
            )
        } {
            Ok((imgs, handles)) => (imgs, handles, false),
            Err(error) => {
                vk_fallback_reason = Some(error);
                // Zero-copy import unsupported — rebuild as a CPU-copy swapchain.
                unsafe { crate::d3d11::release_textures(&textures) };
                let typed_format = crate::vulkan::vk_format_to_dxgi_typed(info.format) as i64;
                textures = match unsafe {
                    crate::d3d11::create_swapchain_textures(
                        device_ptr,
                        typed_format,
                        info.width,
                        info.height,
                        info.array_size,
                        info.sample_count,
                    )
                } {
                    Ok(t) => t,
                    Err(_) => return sys::Result::ERROR_RUNTIME_FAILURE,
                };
                match unsafe {
                    create_owned_vulkan_images(
                        &graphics,
                        textures.len(),
                        info.format,
                        info.width,
                        info.height,
                        info.array_size,
                        vk_usage,
                    )
                } {
                    Ok(imgs) => (imgs, Vec::new(), true),
                    Err(status) => {
                        unsafe { crate::d3d11::release_textures(&textures) };
                        return status;
                    }
                }
            }
        }
    } else {
        (Vec::new(), Vec::new(), false)
    };

    let vk_commands = if is_vk_session {
        let Some(vk) = graphics.vulkan.clone() else {
            unsafe {
                crate::d3d11::release_textures(&textures);
                for handle in &nt_handles {
                    crate::d3d11::close_handle(*handle);
                }
            }
            return sys::Result::ERROR_RUNTIME_FAILURE;
        };
        match vk.create_command_resources(vk_images.len()) {
            Ok(resources) => resources,
            Err(_) => {
                unsafe {
                    free_vulkan_images(&vk, &vk_images);
                    crate::d3d11::release_textures(&textures);
                    for handle in &nt_handles {
                        crate::d3d11::close_handle(*handle);
                    }
                }
                return sys::Result::ERROR_RUNTIME_FAILURE;
            }
        }
    } else {
        Vec::new()
    };

    if is_vk_session && !VULKAN_BACKEND_LOGGED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        if vk_cpu_copy {
            eprintln!(
                "[alvr] Vulkan swapchain backend: CPU-copy fallback ({:?})",
                vk_fallback_reason.unwrap_or(sys::Result::ERROR_RUNTIME_FAILURE)
            );
        } else {
            eprintln!("[alvr] Vulkan swapchain backend: D3D11/Vulkan GPU interop");
        }
    }

    let image_count = textures.len();
    graphics.swapchains.push(crate::state::Swapchain {
        textures,
        gl_textures,
        vk_images,
        nt_handles,
        vk_cpu_copy,
        vk_commands,
        gl_interop_locked: vec![true; gl_interop_handles.len()],
        gl_interop_handles,
        cpu_copy,
        next_index: 0,
        acquisition_order: VecDeque::new(),
        last_released_index: None,
        release_serial: 0,
        image_states: vec![crate::state::SwapchainImageState::default(); image_count],
        format: info.format,
        array_size: info.array_size,
        width: info.width,
        height: info.height,
    });

    unsafe { *swapchain = sys::Swapchain::from_raw(handle) };
    sys::Result::SUCCESS
}

/// Import shared D3D11 swapchain textures into the app's VkDevice as VkImages.
/// Returns the imported `(VkImage, VkDeviceMemory)` pairs plus the NT shared
/// handles (reused by the encoder's OpenSharedResource1 at submit time).
///
/// # Safety
/// `textures` are valid shared `ID3D11Texture2D` pointers; the graphics state
/// holds valid live Vulkan handles.
unsafe fn import_vulkan_images(
    graphics: &crate::state::GraphicsState,
    textures: &[usize],
    vk_format: i64,
    width: u32,
    height: u32,
    array_size: u32,
    usage: vk::ImageUsageFlags,
) -> Result<(Vec<(u64, u64)>, Vec<usize>), sys::Result> {
    let Some(vk) = graphics.vulkan.clone() else {
        eprintln!("[alvr] import_vulkan_images FAIL: could not load Vulkan device functions");
        return Err(sys::Result::ERROR_RUNTIME_FAILURE);
    };

    let mut images = Vec::with_capacity(textures.len());
    let mut handles = Vec::with_capacity(textures.len());
    for &tex in textures {
        // NT shared handle (reused later by the encoder's OpenSharedResource1).
        let Some(nt) = (unsafe { crate::d3d11::create_nt_shared_handle(tex) }) else {
            eprintln!("[alvr] import_vulkan_images FAIL: create_nt_shared_handle failed");
            unsafe { free_vulkan_images(&vk, &images) };
            unsafe {
                for handle in handles {
                    crate::d3d11::close_handle(handle);
                }
            }
            return Err(sys::Result::ERROR_RUNTIME_FAILURE);
        };
        handles.push(nt);
        let Some(pair) =
            (unsafe { vk.import_shared_texture(nt, vk_format, width, height, array_size, usage) })
        else {
            eprintln!("[alvr] import_vulkan_images FAIL: import_shared_texture failed");
            unsafe {
                free_vulkan_images(&vk, &images);
                for handle in handles {
                    crate::d3d11::close_handle(handle);
                }
            }
            return Err(sys::Result::ERROR_RUNTIME_FAILURE);
        };
        images.push(pair);
    }
    Ok((images, handles))
}

/// Free imported Vulkan images on error paths.
///
/// # Safety
/// `images` came from [`crate::vulkan::VulkanFns::import_shared_texture`] on `vk`.
unsafe fn free_vulkan_images(vk: &crate::vulkan::VulkanFns, images: &[(u64, u64)]) {
    for &(image, memory) in images {
        unsafe { vk.destroy_image(image, memory) };
    }
}

/// Create runtime-owned VkImages for the universal Vulkan CPU-copy fallback.
/// The app renders into these standalone images; at submit their pixels are
/// read back and uploaded into the typed D3D11 textures the encoder reads.
///
/// # Safety
/// The graphics state holds valid live Vulkan handles.
unsafe fn create_owned_vulkan_images(
    graphics: &crate::state::GraphicsState,
    count: usize,
    vk_format: i64,
    width: u32,
    height: u32,
    array_size: u32,
    usage: vk::ImageUsageFlags,
) -> Result<Vec<(u64, u64)>, sys::Result> {
    let Some(vk) = graphics.vulkan.clone() else {
        return Err(sys::Result::ERROR_RUNTIME_FAILURE);
    };
    let mut images = Vec::with_capacity(count);
    for _ in 0..count {
        let Some(pair) =
            (unsafe { vk.create_owned_image(vk_format, width, height, array_size, usage) })
        else {
            eprintln!("[alvr] create_owned_vulkan_images FAIL: create_owned_image");
            unsafe { free_vulkan_images(&vk, &images) };
            return Err(sys::Result::ERROR_RUNTIME_FAILURE);
        };
        images.push(pair);
    }
    Ok(images)
}

/// Create `count` standalone RGBA8 GL textures for the CPU-copy fallback.
///
/// # Safety
/// The app's GL context must be current on the calling thread.
unsafe fn create_cpu_copy_textures(
    count: usize,
    width: u32,
    height: u32,
    internal_format: i32,
) -> Option<Vec<u32>> {
    let copy = unsafe { crate::gl_interop::GlCopyFns::load() }?;
    let mut names = Vec::with_capacity(count);
    for _ in 0..count {
        names.push(unsafe { copy.create_texture(width, height, internal_format) }?);
    }
    Some(names)
}

/// Register D3D11 textures as GL aliases for an OpenGL session.
///
/// # Safety
/// The app's GL context must be current (it is, inside an OpenXR call on the
/// app's render thread). `textures` are valid D3D11 texture pointers.
unsafe fn register_gl_aliases(
    graphics: &mut crate::state::GraphicsState,
    textures: &[usize],
) -> Result<(Vec<u32>, Vec<usize>), sys::Result> {
    let Some(interop) = (unsafe { crate::gl_interop::InteropFns::load() }) else {
        // ponytail: driver lacks WGL_NV_DX_interop2 — CPU-copy fallback lands
        // with the frame loop (blueprint Q6 option (i)).
        eprintln!("[alvr] register_gl_aliases FAIL: WGL_NV_DX_interop2 not available");
        return Err(sys::Result::ERROR_RUNTIME_FAILURE);
    };

    if graphics.gl_interop_device == 0 {
        let dev = unsafe { interop.open_device(graphics.d3d11_device) };
        if dev.is_null() {
            eprintln!("[alvr] register_gl_aliases FAIL: open_device returned null");
            return Err(sys::Result::ERROR_RUNTIME_FAILURE);
        }
        graphics.gl_interop_device = dev as usize;
    }
    let interop_device = graphics.gl_interop_device as *mut std::ffi::c_void;

    let mut gl_textures = Vec::with_capacity(textures.len());
    let mut gl_interop_handles = Vec::with_capacity(textures.len());
    for &tex in textures {
        let Some((gl_name, handle)) = (unsafe { interop.register_texture(interop_device, tex) })
        else {
            eprintln!("[alvr] register_gl_aliases FAIL: register_texture failed");
            return Err(sys::Result::ERROR_RUNTIME_FAILURE);
        };
        // WGL_NV_DX_interop objects must be locked before OpenGL may use the
        // registered texture (including attaching it to an FBO).
        let mut object = handle;
        if !unsafe { interop.lock(interop_device, &mut object) } {
            eprintln!("[alvr] register_gl_aliases FAIL: initial lock failed");
            return Err(sys::Result::ERROR_RUNTIME_FAILURE);
        }
        gl_textures.push(gl_name);
        gl_interop_handles.push(handle as usize);
    }
    Ok((gl_textures, gl_interop_handles))
}

/// Index of a swapchain in the GRAPHICS list from its handle, if valid.
fn swapchain_index(handle: sys::Swapchain) -> Option<usize> {
    let raw = handle.into_raw();
    raw.checked_sub(SWAPCHAIN_HANDLE_BASE).map(|i| i as usize)
}

pub unsafe extern "system" fn destroy_swapchain(swapchain: sys::Swapchain) -> sys::Result {
    if let Some(i) = swapchain_index(swapchain) {
        let mut graphics = crate::state::GRAPHICS.lock();
        let vk = if graphics.api == crate::state::GraphicsApi::Vulkan {
            graphics.vulkan.clone()
        } else {
            None
        };
        if graphics.api == crate::state::GraphicsApi::Vulkan && vk.is_none() {
            return sys::Result::ERROR_RUNTIME_FAILURE;
        }
        let api = graphics.api;
        let d3d11_device = graphics.d3d11_device;
        let Some(sc) = graphics.swapchains.get_mut(i) else {
            return sys::Result::ERROR_HANDLE_INVALID;
        };
        if !sc.acquisition_order.is_empty() {
            return sys::Result::ERROR_CALL_ORDER_INVALID;
        }
        if let Err(result) =
            unsafe { destroy_swapchain_contents(api, d3d11_device, vk.as_deref(), sc) }
        {
            return result;
        }
    }
    sys::Result::SUCCESS
}

/// Release all resources associated with one swapchain and leave its handle as
/// a tombstone so later swapchain handles retain their stable indices.
unsafe fn destroy_swapchain_contents(
    api: crate::state::GraphicsApi,
    d3d11_device: usize,
    vk: Option<&crate::vulkan::VulkanFns>,
    sc: &mut crate::state::Swapchain,
) -> Result<(), sys::Result> {
    if !sc.acquisition_order.is_empty() {
        return Err(sys::Result::ERROR_CALL_ORDER_INVALID);
    }
    if api == crate::state::GraphicsApi::Vulkan && !sc.vk_cpu_copy {
        for &texture in &sc.textures {
            if !unsafe { crate::d3d11::synchronize_keyed_texture(texture) } {
                return Err(sys::Result::ERROR_RUNTIME_FAILURE);
            }
        }
    }
    if let Some(vk) = vk {
        for (index, command) in sc.vk_commands.iter().enumerate() {
            if sc
                .image_states
                .get(index)
                .is_some_and(|state| state.vk_fence_pending)
            {
                unsafe { vk.wait_fence(command.fence, u64::MAX) }
                    .map_err(|_| sys::Result::ERROR_RUNTIME_FAILURE)?;
            }
        }
        unsafe { vk.destroy_command_resources(&sc.vk_commands) };
        for &(image, memory) in &sc.vk_images {
            unsafe { vk.destroy_image(image, memory) };
        }
    }
    for (index, &texture) in sc.textures.iter().enumerate() {
        let handle = sc
            .nt_handles
            .get(index)
            .copied()
            .or_else(|| unsafe { crate::d3d11::shared_handle(texture) });
        if let Some(handle) = handle {
            unsafe { alvr_openxr_encoder_forget_texture(handle as *mut std::ffi::c_void) };
        }
    }
    unsafe { crate::d3d11::release_textures(&sc.textures) };
    for &handle in &sc.nt_handles {
        unsafe { crate::d3d11::close_handle(handle) };
    }
    if api == crate::state::GraphicsApi::OpenGL && d3d11_device != 0 {
        unsafe { crate::d3d11::flush_device(d3d11_device) };
    }
    sc.textures.clear();
    sc.gl_textures.clear();
    sc.vk_images.clear();
    sc.nt_handles.clear();
    sc.vk_commands.clear();
    sc.gl_interop_handles.clear();
    sc.gl_interop_locked.clear();
    sc.image_states.clear();
    sc.acquisition_order.clear();
    sc.last_released_index = None;
    Ok(())
}

pub unsafe extern "system" fn enumerate_swapchain_images(
    swapchain: sys::Swapchain,
    image_capacity_input: u32,
    image_count_output: *mut u32,
    images: *mut sys::SwapchainImageBaseHeader,
) -> sys::Result {
    let Some(i) = swapchain_index(swapchain) else {
        return sys::Result::ERROR_HANDLE_INVALID;
    };
    let graphics = crate::state::GRAPHICS.lock();
    let Some(sc) = graphics.swapchains.get(i) else {
        return sys::Result::ERROR_HANDLE_INVALID;
    };
    let count = sc.textures.len();

    if !image_count_output.is_null() {
        unsafe { *image_count_output = count as u32 };
    }
    if image_capacity_input == 0 {
        return sys::Result::SUCCESS;
    }
    if (image_capacity_input as usize) < count || images.is_null() {
        return sys::Result::ERROR_SIZE_INSUFFICIENT;
    }

    if graphics.api == crate::state::GraphicsApi::OpenGL {
        let images = images.cast::<sys::SwapchainImageOpenGLKHR>();
        for (j, &gl_name) in sc.gl_textures.iter().enumerate() {
            let img = unsafe { &mut *images.add(j) };
            img.ty = sys::StructureType::SWAPCHAIN_IMAGE_OPENGL_KHR;
            img.next = std::ptr::null_mut();
            img.image = gl_name;
        }
    } else if graphics.api == crate::state::GraphicsApi::Vulkan {
        let images = images.cast::<sys::SwapchainImageVulkanKHR>();
        for (j, &(vk_image, _memory)) in sc.vk_images.iter().enumerate() {
            let img = unsafe { &mut *images.add(j) };
            img.ty = sys::StructureType::SWAPCHAIN_IMAGE_VULKAN_KHR;
            img.next = std::ptr::null_mut();
            img.image = vk_image as _;
        }
    } else {
        let images = images.cast::<sys::SwapchainImageD3D11KHR>();
        for (j, &tex) in sc.textures.iter().enumerate() {
            let img = unsafe { &mut *images.add(j) };
            img.ty = sys::StructureType::SWAPCHAIN_IMAGE_D3D11_KHR;
            img.next = std::ptr::null_mut();
            img.texture = tex as *mut _;
        }
    }
    sys::Result::SUCCESS
}

pub unsafe extern "system" fn acquire_swapchain_image(
    swapchain: sys::Swapchain,
    _acquire_info: *const sys::SwapchainImageAcquireInfo,
    index: *mut u32,
) -> sys::Result {
    let _perf = crate::vulkan::PerfScope::new("acquire_swapchain_image");
    if index.is_null() {
        return sys::Result::ERROR_VALIDATION_FAILURE;
    }
    let Some(i) = swapchain_index(swapchain) else {
        return sys::Result::ERROR_HANDLE_INVALID;
    };
    let mut graphics = crate::state::GRAPHICS.lock();
    let api = graphics.api;
    let vk = if api == crate::state::GraphicsApi::Vulkan {
        graphics.vulkan.clone()
    } else {
        None
    };
    if api == crate::state::GraphicsApi::Vulkan && vk.is_none() {
        return sys::Result::ERROR_RUNTIME_FAILURE;
    }
    let is_gl = graphics.api == crate::state::GraphicsApi::OpenGL;
    let interop_device = graphics.gl_interop_device as *mut std::ffi::c_void;
    let Some(sc) = graphics.swapchains.get_mut(i) else {
        return sys::Result::ERROR_HANDLE_INVALID;
    };
    if sc.textures.is_empty() || sc.image_states.is_empty() {
        return sys::Result::ERROR_HANDLE_INVALID;
    }

    let count = sc.image_states.len();
    let acquired = next_acquirable_image(&sc.image_states, sc.next_index);
    let Some(acquired) = acquired else {
        return sys::Result::ERROR_CALL_ORDER_INVALID;
    };
    let acquired = acquired as u32;
    sc.next_index = (acquired + 1) % count as u32;

    if let Some(vk) = &vk {
        let (image, memory) = sc.vk_images[acquired as usize];
        let command = sc.vk_commands[acquired as usize];
        let state = sc.image_states[acquired as usize];
        if unsafe {
            vk.acquire_image(
                image,
                memory,
                sc.array_size,
                command,
                !sc.vk_cpu_copy,
                !state.ever_acquired,
                state.vk_fence_pending,
            )
        }
        .is_err()
        {
            return sys::Result::ERROR_RUNTIME_FAILURE;
        }
        sc.image_states[acquired as usize].vk_fence_pending = true;
        sc.image_states[acquired as usize].ever_acquired = true;
    }

    if is_gl && !sc.cpu_copy && !sc.gl_interop_locked[acquired as usize] {
        let Some(interop) = (unsafe { crate::gl_interop::InteropFns::load() }) else {
            return sys::Result::ERROR_RUNTIME_FAILURE;
        };
        let mut object = sc.gl_interop_handles[acquired as usize] as *mut std::ffi::c_void;
        if !unsafe { interop.lock(interop_device, &mut object) } {
            return sys::Result::ERROR_RUNTIME_FAILURE;
        }
        sc.gl_interop_locked[acquired as usize] = true;
    }
    sc.image_states[acquired as usize].acquired = true;
    sc.image_states[acquired as usize].waited = false;
    sc.acquisition_order.push_back(acquired);
    unsafe { *index = acquired };
    sys::Result::SUCCESS
}

pub unsafe extern "system" fn wait_swapchain_image(
    swapchain: sys::Swapchain,
    wait_info: *const sys::SwapchainImageWaitInfo,
) -> sys::Result {
    let _perf = crate::vulkan::PerfScope::new("wait_swapchain_image");
    if wait_info.is_null() {
        return sys::Result::ERROR_VALIDATION_FAILURE;
    }
    let Some(i) = swapchain_index(swapchain) else {
        return sys::Result::ERROR_HANDLE_INVALID;
    };
    let mut graphics = crate::state::GRAPHICS.lock();
    let api = graphics.api;
    let vk = if api == crate::state::GraphicsApi::Vulkan {
        graphics.vulkan.clone()
    } else {
        None
    };
    if api == crate::state::GraphicsApi::Vulkan && vk.is_none() {
        return sys::Result::ERROR_RUNTIME_FAILURE;
    }
    let Some(sc) = graphics.swapchains.get_mut(i) else {
        return sys::Result::ERROR_HANDLE_INVALID;
    };
    let Some(index) = next_unwaited_image(&sc.acquisition_order, &sc.image_states) else {
        return sys::Result::ERROR_CALL_ORDER_INVALID;
    };
    if let Some(vk) = &vk {
        let timeout_ns = match vk_wait_timeout_ns(unsafe { (*wait_info).timeout }) {
            Ok(timeout_ns) => timeout_ns,
            Err(result) => return result,
        };
        let result = unsafe { vk.wait_fence(sc.vk_commands[index as usize].fence, timeout_ns) };
        let result = openxr_vk_wait_result(result);
        if result != sys::Result::SUCCESS {
            return result;
        }
    }
    sc.image_states[index as usize].waited = true;
    sys::Result::SUCCESS
}

pub unsafe extern "system" fn release_swapchain_image(
    swapchain: sys::Swapchain,
    _release_info: *const sys::SwapchainImageReleaseInfo,
) -> sys::Result {
    let _perf = crate::vulkan::PerfScope::new("release_swapchain_image");
    let Some(i) = swapchain_index(swapchain) else {
        return sys::Result::ERROR_HANDLE_INVALID;
    };
    let mut graphics = crate::state::GRAPHICS.lock();
    let api = graphics.api;
    let vk = if api == crate::state::GraphicsApi::Vulkan {
        graphics.vulkan.clone()
    } else {
        None
    };
    if api == crate::state::GraphicsApi::Vulkan && vk.is_none() {
        return sys::Result::ERROR_RUNTIME_FAILURE;
    }
    let is_gl = graphics.api == crate::state::GraphicsApi::OpenGL;
    let d3d11_device = graphics.d3d11_device;
    let interop_device = graphics.gl_interop_device as *mut std::ffi::c_void;

    let Some(sc) = graphics.swapchains.get_mut(i) else {
        return sys::Result::ERROR_HANDLE_INVALID;
    };
    let Some(&index) = sc.acquisition_order.front() else {
        return sys::Result::ERROR_CALL_ORDER_INVALID;
    };
    if !sc.image_states[index as usize].waited {
        return sys::Result::ERROR_CALL_ORDER_INVALID;
    }
    let index = index as usize;

    if is_gl {
        if !sc.cpu_copy && sc.gl_interop_locked[index] {
            let Some(interop) = (unsafe { crate::gl_interop::InteropFns::load() }) else {
                return sys::Result::ERROR_RUNTIME_FAILURE;
            };
            let mut object = sc.gl_interop_handles[index] as *mut std::ffi::c_void;
            // GL rendering is asynchronous. The encoder can otherwise observe
            // one eye from the current frame and one eye from an older frame
            // after their aliases are unlocked, which is most visible during
            // rapid head motion. Complete GL work before returning this image
            // to the D3D11 side of WGL_NV_DX_interop2.
            unsafe { interop.finish() };
            if !unsafe { interop.unlock(interop_device, &mut object) } {
                return sys::Result::ERROR_RUNTIME_FAILURE;
            }
            sc.gl_interop_locked[index] = false;
        }
        // Submit the interop GL→D3D copy on the runtime's own D3D11 device
        // before the encoder (a *separate* D3D11 device) opens this shared
        // texture in submit_projection. The shared texture has no keyed mutex,
        // so without this flush the consumer can read while the copy is still
        // pending, producing a half-old/half-new tear within one eye. The
        // D3D11-session path below already flushes; the GL path must too.
        if d3d11_device != 0 {
            unsafe { crate::d3d11::flush_device(d3d11_device) };
        }
    }

    if api == crate::state::GraphicsApi::D3D11 && d3d11_device != 0 {
        unsafe { crate::d3d11::flush_device(d3d11_device) };
    }

    if let Some(vk) = &vk {
        let (image, memory) = sc.vk_images[index];
        if unsafe {
            vk.release_image(
                image,
                memory,
                sc.array_size,
                sc.vk_commands[index],
                !sc.vk_cpu_copy,
                sc.image_states[index].vk_fence_pending,
            )
        }
        .is_err()
        {
            return sys::Result::ERROR_RUNTIME_FAILURE;
        }
        sc.image_states[index].vk_fence_pending = true;
    }
    sc.acquisition_order.pop_front();
    sc.image_states[index].acquired = false;
    sc.image_states[index].waited = false;
    record_released_image(
        &mut sc.image_states,
        &mut sc.last_released_index,
        &mut sc.release_serial,
        index as u32,
    );
    sys::Result::SUCCESS
}

// --- Frame loop ------------------------------------------------------------

pub unsafe extern "system" fn wait_frame(
    _session: sys::Session,
    _frame_wait_info: *const sys::FrameWaitInfo,
    frame_state: *mut sys::FrameState,
) -> sys::Result {
    if frame_state.is_null() {
        return sys::Result::ERROR_VALIDATION_FAILURE;
    }

    // Pace frames to the nominal refresh period without returning a display time
    // that has already passed. The scheduled instant is the start of the frame
    // interval; OpenXR's predicted display time is its midpoint, leaving the app
    // time to render before the compositor presents it.
    let (sleep_duration, display_time, period) = {
        let mut frame = crate::state::FRAME_LOOP.lock();
        let now = Instant::now();
        let period_duration = Duration::from_nanos(frame.frame_period_ns.max(1) as u64);
        let half_period = Duration::from_nanos((period_duration.as_nanos() / 2) as u64);
        let origin = *frame.time_origin_instant.get_or_insert(now);

        let mut interval_start = frame
            .next_wake_instant
            .unwrap_or_else(|| now + period_duration);
        let mut predicted_display_time = interval_start + half_period;

        // A slow app may finish after its predicted display interval. Skip expired
        // intervals so the next frame receives a future display time instead of
        // accumulating an ever-staler pose prediction.
        if predicted_display_time <= now {
            let lateness = now.duration_since(predicted_display_time);
            let missed_intervals =
                (lateness.as_nanos() / period_duration.as_nanos() + 1).min(u32::MAX as u128) as u32;
            interval_start += period_duration * missed_intervals;
            predicted_display_time = interval_start + half_period;
        }

        frame.next_wake_instant = Some(interval_start + period_duration);
        frame.predicted_display_time_ns =
            predicted_display_time.duration_since(origin).as_nanos() as i64;

        (
            interval_start.saturating_duration_since(now),
            frame.predicted_display_time_ns,
            frame.frame_period_ns,
        )
    };

    if !sleep_duration.is_zero() {
        std::thread::sleep(sleep_duration);
    }

    let state = unsafe { &mut *frame_state };
    state.ty = sys::StructureType::FRAME_STATE;
    state.predicted_display_time = sys::Time::from_nanos(display_time);
    state.predicted_display_period = sys::Duration::from_nanos(period);
    state.should_render = sys::TRUE;
    sys::Result::SUCCESS
}

pub unsafe extern "system" fn begin_frame(
    _session: sys::Session,
    _frame_begin_info: *const sys::FrameBeginInfo,
) -> sys::Result {
    sys::Result::SUCCESS
}

fn pose_to_xr(pose: alvr_common::Pose) -> sys::Posef {
    sys::Posef {
        orientation: sys::Quaternionf {
            x: pose.orientation.x,
            y: pose.orientation.y,
            z: pose.orientation.z,
            w: pose.orientation.w,
        },
        position: sys::Vector3f {
            x: pose.position.x,
            y: pose.position.y,
            z: pose.position.z,
        },
    }
}

fn xr_to_pose(pose: sys::Posef) -> alvr_common::Pose {
    alvr_common::Pose {
        orientation: alvr_common::glam::Quat::from_xyzw(
            pose.orientation.x,
            pose.orientation.y,
            pose.orientation.z,
            pose.orientation.w,
        ),
        position: alvr_common::glam::Vec3::new(pose.position.x, pose.position.y, pose.position.z),
    }
}

/// The type + offset recorded for a created reference space, if it is one.
fn reference_space_info(space: sys::Space) -> Option<(sys::ReferenceSpaceType, alvr_common::Pose)> {
    REFERENCE_SPACES.lock().get(&space.into_raw()).copied()
}

fn reference_space_pose(
    space: sys::Space,
    head_pose: alvr_common::Pose,
) -> Option<alvr_common::Pose> {
    let (ty, offset) = reference_space_info(space)?;
    let origin = match ty {
        sys::ReferenceSpaceType::VIEW => head_pose,
        sys::ReferenceSpaceType::LOCAL => {
            // VDXR: LOCAL = Translation({0, eye_height, 0}) captured once from
            // the first head pose's Y. Identity orientation, X/Z absolute.
            let mut local_origin = LOCAL_ORIGIN.lock();
            *local_origin.get_or_insert(alvr_common::Pose {
                orientation: alvr_common::glam::Quat::IDENTITY,
                position: alvr_common::glam::Vec3::new(0.0, head_pose.position.y, 0.0),
            })
        }
        // STAGE (and any other) = floor origin identity. The client streams in
        // the STAGE/floor frame, so this matches VDXR's STAGE = Pose::Identity().
        _ => alvr_common::Pose::default(),
    };

    Some(origin * offset)
}

/// VDXR's exact Touch-controller transform from OpenXR grip to OpenXR aim,
/// derived from its `oculusGripPose` and `oculusAimPose` calibration constants.
/// The ALVR client already streams a canonical grip pose (after its platform
/// correction), so grip is exposed unchanged and only aim needs this transform.
fn grip_to_aim(grip: alvr_common::Pose) -> alvr_common::Pose {
    use alvr_common::glam::{Quat, Vec3};

    let grip_to_aim = alvr_common::Pose {
        orientation: Quat::from_rotation_x(-60.0_f32.to_radians()),
        position: Vec3::new(0.0, -0.06727241, -0.073480755),
    };
    grip * grip_to_aim
}

fn sample_head_pose(_display_time_ns: i64) -> Option<(alvr_common::Pose, i64)> {
    // Always render from the most recent client head sample and tag the frame
    // with that sample's real tracking timestamp. The client reprojects from the
    // pose it stored under this exact timestamp to the current head pose; using
    // the freshest sample minimizes that reprojection delta, so head motion stays
    // aligned between the eyes. Mapping the app's synthetic display time onto an
    // older history sample (previous approach) made the render pose lag and jitter
    // relative to the client's reprojection, which showed up as eye misalignment
    // during motion. This is headset-agnostic — no per-device assumptions.
    let queue = crate::state::HEAD_POSE_QUEUE.lock();
    let &(latest_ts, latest_pose) = queue.back()?;
    Some((latest_pose, latest_ts.as_nanos() as i64))
}

fn take_render_sample(display_time_ns: i64) -> Option<(alvr_common::Pose, i64)> {
    let mut samples = crate::state::RENDER_TRACKING_TS.lock();
    let nearest = samples
        .iter()
        .enumerate()
        .min_by_key(|(_, &(sample_display_time_ns, _, _))| {
            (sample_display_time_ns - display_time_ns).abs()
        })
        .map(|(index, &(_, tracking_ts, pose))| (index, pose, tracking_ts));

    nearest.map(|(index, pose, tracking_ts)| {
        samples.remove(index);
        (pose, tracking_ts)
    })
}

pub unsafe extern "system" fn locate_views(
    _session: sys::Session,
    view_locate_info: *const sys::ViewLocateInfo,
    view_state: *mut sys::ViewState,
    view_capacity_input: u32,
    view_count_output: *mut u32,
    views: *mut sys::View,
) -> sys::Result {
    if view_locate_info.is_null() {
        return sys::Result::ERROR_VALIDATION_FAILURE;
    }
    if !view_count_output.is_null() {
        unsafe { *view_count_output = VIEW_COUNT as u32 };
    }

    // Until a real client head sample exists, report the views as unavailable.
    // OpenXR callers can keep the session alive and wait for tracking recovery
    // when both validity flags are clear.
    let display_time_ns = unsafe { (*view_locate_info).display_time.as_nanos() };
    let sample = sample_head_pose(display_time_ns);
    let head_pose = sample.map(|(head_pose, _)| head_pose);
    if !view_state.is_null() {
        let vs = unsafe { &mut *view_state };
        vs.ty = sys::StructureType::VIEW_STATE;
        vs.view_state_flags = sys::ViewStateFlags::EMPTY;
    }
    if head_pose.is_some() && !view_state.is_null() {
        unsafe {
            (*view_state).view_state_flags = sys::ViewStateFlags::ORIENTATION_VALID
                | sys::ViewStateFlags::POSITION_VALID
                | sys::ViewStateFlags::ORIENTATION_TRACKED
                | sys::ViewStateFlags::POSITION_TRACKED
        };
    }
    if view_capacity_input == 0 {
        return sys::Result::SUCCESS;
    }
    if (view_capacity_input as usize) < VIEW_COUNT || views.is_null() {
        return sys::Result::ERROR_SIZE_INSUFFICIENT;
    }

    // Express tracking-space poses in the caller's actual reference-space
    // origin (VIEW follows the head, LOCAL is fixed at session startup).
    let base_offset_inv = head_pose.and_then(|head_pose| {
        reference_space_pose(unsafe { (*view_locate_info).space }, head_pose)
            .map(|pose| pose.inverse())
    });

    // Remember which real client tracking timestamp produced this frame's render
    // pose so end_frame can tag the encoded video with it (correct client ATW).
    if let Some((render_pose, tracking_ts)) = sample {
        let mut map = crate::state::RENDER_TRACKING_TS.lock();
        map.push_back((display_time_ns, tracking_ts, render_pose));
        while map.len() > 32 {
            map.pop_front();
        }
    }

    let local = crate::state::LOCAL_VIEW_PARAMS.read();

    for i in 0..VIEW_COUNT {
        let view = unsafe { &mut *views.add(i) };
        view.ty = sys::StructureType::VIEW;
        // World-locked 6DOF: bake the real client head pose (sampled at the exact
        // tracking timestamp this frame is tagged with) times the per-eye offset.
        // The app renders world-positioned geometry; the client reconstructs the
        // same layer pose from head_pose(tracking_ts) * local eye and hands it to
        // the headset compositor. Because the app's render pose and the client's
        // layer pose are both head_pose(tracking_ts), there is no double head
        // compensation — the earlier divergence was a timestamp/prediction
        // mismatch, now fixed by exact-ts frame tagging + no server-side predict.
        let eye_pose = match head_pose {
            Some(head_pose) => {
                let eye_pose = head_pose * local[i].pose;
                match base_offset_inv {
                    Some(inv) => inv * eye_pose,
                    None => eye_pose,
                }
            }
            None => local[i].pose,
        };
        view.pose = pose_to_xr(eye_pose);
        view.fov = sys::Fovf {
            angle_left: local[i].fov.left,
            angle_right: local[i].fov.right,
            angle_up: local[i].fov.up,
            angle_down: local[i].fov.down,
        };
    }
    sys::Result::SUCCESS
}

pub unsafe extern "system" fn locate_space(
    space: sys::Space,
    base_space: sys::Space,
    time: sys::Time,
    location: *mut sys::SpaceLocation,
) -> sys::Result {
    if location.is_null() {
        return sys::Result::ERROR_VALIDATION_FAILURE;
    }
    let loc = unsafe { &mut *location };
    loc.ty = sys::StructureType::SPACE_LOCATION;
    loc.location_flags = sys::SpaceLocationFlags::EMPTY;

    let head_pose = sample_head_pose(time.as_nanos())
        .map(|(pose, _)| pose)
        .unwrap_or_default();
    // Everything below is computed in client tracking space, then expressed
    // relative to the caller's VIEW/LOCAL (or internal STAGE) origin.
    let base_offset_inv = reference_space_pose(base_space, head_pose).map(|pose| pose.inverse());
    let to_base = |pose: alvr_common::Pose| match base_offset_inv {
        Some(inv) => inv * pose,
        None => pose,
    };

    if let Some(hand) = crate::actions::action_space_hand(space) {
        // Lock controller tracking before poses, matching the streaming update path.
        let controller_tracked = crate::state::CONTROLLER_TRACKED.read();
        if controller_tracked[hand] {
            let controllers = crate::state::CONTROLLER_POSES.read();
            loc.location_flags = sys::SpaceLocationFlags::ORIENTATION_VALID
                | sys::SpaceLocationFlags::POSITION_VALID
                | sys::SpaceLocationFlags::ORIENTATION_TRACKED
                | sys::SpaceLocationFlags::POSITION_TRACKED;
            // Client platform correction already produces canonical OpenXR
            // grip. VDXR likewise exposes calibrated grip directly and derives
            // aim from the same tracked pose with a fixed local transform.
            let grip = controllers[hand];
            let pose = if crate::actions::action_space_kind(space)
                == Some(crate::actions::PoseKind::Aim)
            {
                grip_to_aim(grip)
            } else {
                grip
            };
            let pose = crate::actions::action_space_offset(space)
                .map(xr_to_pose)
                .map_or(pose, |offset| pose * offset);
            loc.pose = pose_to_xr(to_base(pose));
        }
        return sys::Result::SUCCESS;
    }

    // An action-space that cannot be tied to one hand is not a reference
    // space. Returning the head pose here made controller rays follow the
    // headset whenever an app created its pose action with a null subaction
    // path. Report it untracked instead.
    if crate::actions::is_action_space(space) {
        return sys::Result::SUCCESS;
    }

    // Reference space located relative to the base space. VIEW follows head
    // tracking; LOCAL/STAGE sit at their stored `pose_in_reference_space`
    // offset in the stage frame (identity for the default spaces, non-identity
    // after seat-adjust / recenter).
    let space_in_stage = reference_space_pose(space, head_pose).unwrap_or(head_pose);
    loc.pose = pose_to_xr(to_base(space_in_stage));
    loc.location_flags = sys::SpaceLocationFlags::ORIENTATION_VALID
        | sys::SpaceLocationFlags::POSITION_VALID
        | sys::SpaceLocationFlags::ORIENTATION_TRACKED
        | sys::SpaceLocationFlags::POSITION_TRACKED;

    sys::Result::SUCCESS
}

pub unsafe extern "system" fn end_frame(
    _session: sys::Session,
    frame_end_info: *const sys::FrameEndInfo,
) -> sys::Result {
    let _perf = crate::vulkan::PerfScope::new("end_frame");
    if frame_end_info.is_null() {
        return sys::Result::ERROR_VALIDATION_FAILURE;
    }
    let info = unsafe { &*frame_end_info };
    let display_time_ns = info.display_time.as_nanos().max(0);
    let mapped_render_sample = take_render_sample(display_time_ns);
    let has_locate_views_sample = mapped_render_sample.is_some();
    let render_sample = mapped_render_sample.or_else(|| sample_head_pose(display_time_ns));
    let fallback_tracking_ts = render_sample
        .as_ref()
        .map(|(_, tracking_ts)| (*tracking_ts).max(0) as u64);

    use std::sync::atomic::{AtomicU32, Ordering};

    // Reuse the exact pose sampled by xrLocateViews for this display time. A
    // newer tracking sample may arrive between rendering and xrEndFrame; pairing
    // that newer pose with the earlier image causes translational wobble and
    // reprojects from a different origin.
    static TS_LOG: AtomicU32 = AtomicU32::new(0);
    let timestamp_ns = if has_locate_views_sample {
        fallback_tracking_ts.unwrap_or(display_time_ns.max(0) as u64)
    } else {
        let m = TS_LOG.fetch_add(1, Ordering::Relaxed);
        if m < 5 || m % 300 == 0 {
            if let Some(tracking_ts) = fallback_tracking_ts {
                eprintln!(
                    "[alvr] end_frame: no locate_views sample (miss #{m}); \
                     using latest client tracking timestamp {tracking_ts}"
                );
            } else {
                eprintln!(
                    "[alvr] end_frame: no tracking pose (miss #{m}); \
                     falling back to display time {display_time_ns}"
                );
            }
        }
        fallback_tracking_ts.unwrap_or(display_time_ns.max(0) as u64)
    };

    if info.layer_count == 0 || info.layers.is_null() {
        return sys::Result::SUCCESS;
    }

    let head_pose = render_sample.map(|(pose, _)| pose).unwrap_or_default();
    let local_views = *crate::state::LOCAL_VIEW_PARAMS.read();
    let mut eye_basis = [
        CompositionEyeBasis::from_local(head_pose, local_views[0]),
        CompositionEyeBasis::from_local(head_pose, local_views[1]),
    ];
    let mut eye_width = [RECOMMENDED_VIEW_WIDTH; VIEW_COUNT];
    let mut eye_height = [RECOMMENDED_VIEW_HEIGHT; VIEW_COUNT];

    // Projection views provide the coordinate frame and FOV used to project
    // quad layers. Capture the first valid stereo projection before walking the
    // submitted layer list so layer order remains exactly the app's order.
    for i in 0..info.layer_count as usize {
        let layer_ptr = unsafe { *info.layers.add(i) };
        if layer_ptr.is_null()
            || unsafe { (*layer_ptr).ty } != sys::StructureType::COMPOSITION_LAYER_PROJECTION
        {
            continue;
        }
        let projection = unsafe { &*layer_ptr.cast::<sys::CompositionLayerProjection>() };
        if projection.view_count < VIEW_COUNT as u32 || projection.views.is_null() {
            continue;
        }
        let Some(origin) = reference_space_pose(projection.space, head_pose) else {
            continue;
        };
        for eye in 0..VIEW_COUNT {
            let view = unsafe { *projection.views.add(eye) };
            eye_basis[eye] = CompositionEyeBasis {
                fov: [
                    view.fov.angle_left,
                    view.fov.angle_right,
                    view.fov.angle_up,
                    view.fov.angle_down,
                ],
                eye_pose_in_stage: origin * xr_to_pose(view.pose),
            };
            eye_width[eye] = view.sub_image.image_rect.extent.width.max(1) as u32;
            eye_height[eye] = view.sub_image.image_rect.extent.height.max(1) as u32;
        }
        break;
    }

    let mut layers = [Vec::<AlvrOpenXrDrawDesc>::new(), Vec::new()];
    let mut fallback_copies = HashSet::new();
    for i in 0..info.layer_count as usize {
        let layer_ptr = unsafe { *info.layers.add(i) };
        if layer_ptr.is_null() {
            continue;
        }
        let header = unsafe { &*layer_ptr };
        match header.ty {
            sys::StructureType::COMPOSITION_LAYER_PROJECTION => {
                let projection = unsafe { &*layer_ptr.cast::<sys::CompositionLayerProjection>() };
                if projection.view_count < VIEW_COUNT as u32 || projection.views.is_null() {
                    continue;
                }
                let clip = projection_clip_vertices();
                for eye in 0..VIEW_COUNT {
                    let view = unsafe { *projection.views.add(eye) };
                    if let Some(desc) = unsafe {
                        draw_descriptor_for_subimage(
                            view.sub_image,
                            header.layer_flags,
                            clip,
                            &mut fallback_copies,
                        )
                    } {
                        layers[eye].push(desc);
                    }
                }
            }
            sys::StructureType::COMPOSITION_LAYER_QUAD => {
                let quad = unsafe { &*layer_ptr.cast::<sys::CompositionLayerQuad>() };
                let Some(quad_origin) = reference_space_pose(quad.space, head_pose) else {
                    continue;
                };
                let quad_pose_in_stage = quad_origin * xr_to_pose(quad.pose);
                let clip = [
                    quad_clip_vertices(quad_pose_in_stage, eye_basis[0], quad.size),
                    quad_clip_vertices(quad_pose_in_stage, eye_basis[1], quad.size),
                ];
                for eye in 0..VIEW_COUNT {
                    let visible = match quad.eye_visibility {
                        sys::EyeVisibility::LEFT => eye == 0,
                        sys::EyeVisibility::RIGHT => eye == 1,
                        _ => true,
                    };
                    if !visible {
                        continue;
                    }
                    let Some(clip) = clip[eye] else { continue };
                    if let Some(desc) = unsafe {
                        draw_descriptor_for_subimage(
                            quad.sub_image,
                            header.layer_flags,
                            clip,
                            &mut fallback_copies,
                        )
                    } {
                        layers[eye].push(desc);
                    }
                }
            }
            _ => {}
        }
    }

    let eye_to_head = [
        local_views[0].pose.position.to_array(),
        local_views[1].pose.position.to_array(),
    ];
    let idr = crate::state::REQUEST_IDR.swap(false, std::sync::atomic::Ordering::Relaxed);
    unsafe {
        alvr_openxr_encoder_submit_layers(
            layers[0].as_ptr(),
            layers[0].len() as u32,
            layers[1].as_ptr(),
            layers[1].len() as u32,
            eye_width[0],
            eye_height[0],
            eye_width[1],
            eye_height[1],
            eye_basis[0].fov.as_ptr(),
            eye_basis[1].fov.as_ptr(),
            eye_to_head[0].as_ptr(),
            eye_to_head[1].as_ptr(),
            timestamp_ns,
            idr,
        );
    }
    sys::Result::SUCCESS
}

#[derive(Clone, Copy)]
struct CompositionEyeBasis {
    fov: [f32; 4],
    eye_pose_in_stage: alvr_common::Pose,
}

impl CompositionEyeBasis {
    fn from_local(head_pose: alvr_common::Pose, view: alvr_common::ViewParams) -> Self {
        Self {
            fov: [view.fov.left, view.fov.right, view.fov.up, view.fov.down],
            eye_pose_in_stage: head_pose * view.pose,
        }
    }
}

fn alpha_mode(flags: sys::CompositionLayerFlags) -> u32 {
    if !flags.contains(sys::CompositionLayerFlags::BLEND_TEXTURE_SOURCE_ALPHA) {
        0
    } else if flags.contains(sys::CompositionLayerFlags::UNPREMULTIPLIED_ALPHA) {
        2
    } else {
        1
    }
}

fn projection_clip_vertices() -> [f32; 16] {
    [
        -1.0, 1.0, 0.5, 1.0, 1.0, 1.0, 0.5, 1.0, -1.0, -1.0, 0.5, 1.0, 1.0, -1.0, 0.5, 1.0,
    ]
}

fn quad_clip_vertices(
    quad_pose_in_stage: alvr_common::Pose,
    eye: CompositionEyeBasis,
    size: sys::Extent2Df,
) -> Option<[f32; 16]> {
    let pose = eye.eye_pose_in_stage.inverse() * quad_pose_in_stage;
    let corners = [
        (-0.5 * size.width, 0.5 * size.height),
        (0.5 * size.width, 0.5 * size.height),
        (-0.5 * size.width, -0.5 * size.height),
        (0.5 * size.width, -0.5 * size.height),
    ];
    let tan_left = eye.fov[0].tan();
    let tan_right = eye.fov[1].tan();
    let tan_up = eye.fov[2].tan();
    let tan_down = eye.fov[3].tan();
    if !(tan_right > tan_left && tan_up > tan_down) {
        return None;
    }
    let mut clip = [0.0; 16];
    for (index, (x, y)) in corners.into_iter().enumerate() {
        let point = pose.orientation * alvr_common::glam::Vec3::new(x, y, 0.0) + pose.position;
        let depth = -point.z;
        if !depth.is_finite() || depth <= 0.001 {
            return None;
        }
        let ndc_x = 2.0 * ((point.x / depth - tan_left) / (tan_right - tan_left)) - 1.0;
        let ndc_y = 2.0 * ((point.y / depth - tan_down) / (tan_up - tan_down)) - 1.0;
        let offset = index * 4;
        clip[offset] = ndc_x * depth;
        clip[offset + 1] = ndc_y * depth;
        clip[offset + 2] = depth * 0.5;
        clip[offset + 3] = depth;
    }
    Some(clip)
}

/// Build a private compositor descriptor for the most recently released image.
/// The Vulkan fallback reads only the selected array layer and restores its
/// COLOR_ATTACHMENT layout before returning it to the application queue.
unsafe fn draw_descriptor_for_subimage(
    sub_image: sys::SwapchainSubImage,
    flags: sys::CompositionLayerFlags,
    clip_vertices: [f32; 16],
    fallback_copies: &mut HashSet<(usize, u32, u32)>,
) -> Option<AlvrOpenXrDrawDesc> {
    let sc_index = swapchain_index(sub_image.swapchain)?;
    let graphics = crate::state::GRAPHICS.lock();
    let sc = graphics.swapchains.get(sc_index)?;
    let released_index = sc.last_released_index?;
    let image_index = released_index as usize;
    let texture = *sc.textures.get(image_index)?;
    if !valid_array_slice(sc.array_size, sub_image.image_array_index) {
        return None;
    }
    let layer = sub_image.image_array_index;

    if graphics.api == crate::state::GraphicsApi::OpenGL && sc.cpu_copy {
        if fallback_copies.insert((sc_index, released_index, layer)) {
            if let (Some(&gl_name), Some(copy)) = (sc.gl_textures.get(image_index), unsafe {
                crate::gl_interop::GlCopyFns::load()
            }) {
                let size = (sc.width as usize) * (sc.height as usize) * 4;
                let mut pixels = vec![0; size];
                unsafe { copy.read_pixels(gl_name, &mut pixels) };
                unsafe {
                    crate::d3d11::update_texture_rgba_layer(
                        graphics.d3d11_device,
                        texture,
                        sc.width,
                        layer,
                        &pixels,
                    );
                    crate::d3d11::flush_device(graphics.d3d11_device);
                }
            }
        }
    }

    if graphics.api == crate::state::GraphicsApi::Vulkan && sc.vk_cpu_copy {
        if fallback_copies.insert((sc_index, released_index, layer)) {
            let (image, _) = *sc.vk_images.get(image_index)?;
            let vk = graphics.vulkan.clone()?;
            let pixels = unsafe {
                vk.readback_image(
                    image,
                    sc.width,
                    sc.height,
                    layer,
                    *sc.vk_commands.get(image_index)?,
                    sc.image_states.get(image_index)?.vk_fence_pending,
                )
            }?;
            unsafe {
                crate::d3d11::update_texture_rgba_layer(
                    graphics.d3d11_device,
                    texture,
                    sc.width,
                    layer,
                    &pixels,
                );
                crate::d3d11::flush_device(graphics.d3d11_device);
            }
        }
    }

    let is_vulkan_interop = graphics.api == crate::state::GraphicsApi::Vulkan && !sc.vk_cpu_copy;
    let shared_handle = if is_vulkan_interop {
        *sc.nt_handles.get(image_index)?
    } else {
        unsafe { crate::d3d11::shared_handle(texture) }?
    };
    let format = match graphics.api {
        crate::state::GraphicsApi::Vulkan => crate::vulkan::vk_format_to_dxgi_typed(sc.format),
        crate::state::GraphicsApi::OpenGL => gl_format_to_dxgi(sc.format),
        _ => sc.format as i32,
    };
    let rect = sub_image.image_rect;
    let mut uv = rect_uv_bounds(rect, sc.width, sc.height);
    if graphics.api == crate::state::GraphicsApi::OpenGL {
        uv.swap(2, 3);
    }
    Some(AlvrOpenXrDrawDesc {
        shared_handle: shared_handle as *mut std::ffi::c_void,
        shared_type: u32::from(is_vulkan_interop),
        format: i64::from(format),
        array_slice: layer,
        uv_bounds: uv,
        clip_vertices,
        alpha_mode: alpha_mode(flags),
    })
}

fn rect_uv_bounds(rect: sys::Rect2Di, width: u32, height: u32) -> [f32; 4] {
    let width = width.max(1) as f32;
    let height = height.max(1) as f32;
    let left = rect.offset.x as f32;
    let top = rect.offset.y as f32;
    let right = left + rect.extent.width as f32;
    let bottom = top + rect.extent.height as f32;
    let bounds = [
        (left / width).clamp(0.0, 1.0),
        (right / width).clamp(0.0, 1.0),
        (top / height).clamp(0.0, 1.0),
        (bottom / height).clamp(0.0, 1.0),
    ];
    if bounds[1] > bounds[0] && bounds[3] > bounds[2] {
        bounds
    } else {
        [0.0, 1.0, 0.0, 1.0]
    }
}

// --- Events ----------------------------------------------------------------

pub unsafe extern "system" fn poll_event(
    _instance: sys::Instance,
    event_data: *mut sys::EventDataBuffer,
) -> sys::Result {
    if event_data.is_null() {
        return sys::Result::ERROR_VALIDATION_FAILURE;
    }

    // Match VDXR: a physical recenter yields LOCAL then STAGE change events.
    // Apps react by rebuilding their reference-space-dependent placement.
    let pending_recenter =
        crate::state::PENDING_RECENTER.load(std::sync::atomic::Ordering::Relaxed);
    if pending_recenter > 0 {
        crate::state::PENDING_RECENTER.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        let event = event_data.cast::<sys::EventDataReferenceSpaceChangePending>();
        unsafe {
            (*event).ty = sys::StructureType::EVENT_DATA_REFERENCE_SPACE_CHANGE_PENDING;
            (*event).next = std::ptr::null();
            (*event).session = sys::Session::from_raw(ALVR_SESSION_HANDLE);
            (*event).reference_space_type = if pending_recenter == 2 {
                sys::ReferenceSpaceType::LOCAL
            } else {
                sys::ReferenceSpaceType::STAGE
            };
            (*event).change_time = sys::Time::from_nanos(0);
            (*event).pose_valid = sys::FALSE;
            (*event).pose_in_previous_space = pose_to_xr(alvr_common::Pose::default());
        }
        return sys::Result::SUCCESS;
    }

    let mut state = crate::state::SESSION.lock();
    let Some(next) = state.pending_states.pop_front() else {
        return sys::Result::EVENT_UNAVAILABLE;
    };
    state.current = Some(next);

    // EventDataSessionStateChanged has the same layout prefix as the buffer;
    // write it in place.
    let event = event_data.cast::<sys::EventDataSessionStateChanged>();
    unsafe {
        (*event).ty = sys::StructureType::EVENT_DATA_SESSION_STATE_CHANGED;
        (*event).next = std::ptr::null();
        (*event).session = sys::Session::from_raw(ALVR_SESSION_HANDLE);
        (*event).state = next;
        (*event).time = sys::Time::from_nanos(0);
    }
    sys::Result::SUCCESS
}

/// Resolve a named `xr*` entry point to its function pointer, or `None`.
pub fn resolve(name: &CStr) -> Option<sys::pfn::VoidFunction> {
    // # Safety of transmutes: each target has the exact ABI of the pfn type it's
    // cast to (verified against openxr-sys signatures).
    macro_rules! f {
        ($func:expr) => {
            Some(unsafe { std::mem::transmute::<_, sys::pfn::VoidFunction>($func) })
        };
    }

    match name.to_bytes() {
        b"xrEnumerateInstanceExtensionProperties" => {
            f!(enumerate_instance_extension_properties
                as sys::pfn::EnumerateInstanceExtensionProperties)
        }
        b"xrCreateInstance" => f!(create_instance as sys::pfn::CreateInstance),
        b"xrDestroyInstance" => f!(destroy_instance as sys::pfn::DestroyInstance),
        b"xrGetInstanceProperties" => {
            f!(get_instance_properties as sys::pfn::GetInstanceProperties)
        }
        b"xrGetSystem" => f!(get_system as sys::pfn::GetSystem),
        b"xrGetSystemProperties" => f!(get_system_properties as sys::pfn::GetSystemProperties),
        b"xrEnumerateViewConfigurations" => {
            f!(enumerate_view_configurations as sys::pfn::EnumerateViewConfigurations)
        }
        b"xrGetViewConfigurationProperties" => {
            f!(get_view_configuration_properties as sys::pfn::GetViewConfigurationProperties)
        }
        b"xrEnumerateViewConfigurationViews" => {
            f!(enumerate_view_configuration_views as sys::pfn::EnumerateViewConfigurationViews)
        }
        b"xrEnumerateEnvironmentBlendModes" => {
            f!(enumerate_environment_blend_modes as sys::pfn::EnumerateEnvironmentBlendModes)
        }
        b"xrCreateSession" => f!(create_session as sys::pfn::CreateSession),
        b"xrDestroySession" => f!(destroy_session as sys::pfn::DestroySession),
        b"xrBeginSession" => f!(begin_session as sys::pfn::BeginSession),
        b"xrEndSession" => f!(end_session as sys::pfn::EndSession),
        b"xrRequestExitSession" => f!(request_exit_session as sys::pfn::RequestExitSession),
        b"xrEnumerateReferenceSpaces" => {
            f!(enumerate_reference_spaces as sys::pfn::EnumerateReferenceSpaces)
        }
        b"xrCreateReferenceSpace" => {
            f!(create_reference_space as sys::pfn::CreateReferenceSpace)
        }
        b"xrDestroySpace" => f!(destroy_space as sys::pfn::DestroySpace),
        b"xrPollEvent" => f!(poll_event as sys::pfn::PollEvent),
        b"xrGetD3D11GraphicsRequirementsKHR" => {
            f!(get_d3d11_graphics_requirements as sys::pfn::GetD3D11GraphicsRequirementsKHR)
        }
        b"xrGetOpenGLGraphicsRequirementsKHR" => {
            f!(get_opengl_graphics_requirements as sys::pfn::GetOpenGLGraphicsRequirementsKHR)
        }
        b"xrGetVulkanGraphicsRequirementsKHR" | b"xrGetVulkanGraphicsRequirements2KHR" => {
            f!(get_vulkan_graphics_requirements as sys::pfn::GetVulkanGraphicsRequirementsKHR)
        }
        b"xrGetVulkanInstanceExtensionsKHR" => {
            f!(get_vulkan_instance_extensions as sys::pfn::GetVulkanInstanceExtensionsKHR)
        }
        b"xrGetVulkanDeviceExtensionsKHR" => {
            f!(get_vulkan_device_extensions as sys::pfn::GetVulkanDeviceExtensionsKHR)
        }
        b"xrGetVulkanGraphicsDeviceKHR" => {
            f!(get_vulkan_graphics_device as sys::pfn::GetVulkanGraphicsDeviceKHR)
        }
        b"xrGetVulkanGraphicsDevice2KHR" => {
            f!(get_vulkan_graphics_device2 as sys::pfn::GetVulkanGraphicsDevice2KHR)
        }
        b"xrCreateVulkanInstanceKHR" => {
            f!(create_vulkan_instance as sys::pfn::CreateVulkanInstanceKHR)
        }
        b"xrCreateVulkanDeviceKHR" => {
            f!(create_vulkan_device as sys::pfn::CreateVulkanDeviceKHR)
        }
        b"xrCreateSwapchain" => f!(create_swapchain as sys::pfn::CreateSwapchain),
        b"xrEnumerateSwapchainFormats" => {
            f!(enumerate_swapchain_formats as sys::pfn::EnumerateSwapchainFormats)
        }
        b"xrDestroySwapchain" => f!(destroy_swapchain as sys::pfn::DestroySwapchain),
        b"xrEnumerateSwapchainImages" => {
            f!(enumerate_swapchain_images as sys::pfn::EnumerateSwapchainImages)
        }
        b"xrAcquireSwapchainImage" => {
            f!(acquire_swapchain_image as sys::pfn::AcquireSwapchainImage)
        }
        b"xrWaitSwapchainImage" => f!(wait_swapchain_image as sys::pfn::WaitSwapchainImage),
        b"xrReleaseSwapchainImage" => {
            f!(release_swapchain_image as sys::pfn::ReleaseSwapchainImage)
        }
        b"xrWaitFrame" => f!(wait_frame as sys::pfn::WaitFrame),
        b"xrBeginFrame" => f!(begin_frame as sys::pfn::BeginFrame),
        b"xrEndFrame" => f!(end_frame as sys::pfn::EndFrame),
        b"xrLocateViews" => f!(locate_views as sys::pfn::LocateViews),
        b"xrLocateSpace" => f!(locate_space as sys::pfn::LocateSpace),
        _ => crate::actions::resolve(name.to_bytes()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn end_frame_uses_the_pose_recorded_for_its_display_time() {
        use alvr_common::glam::{Quat, Vec3};

        let rendered_pose = alvr_common::Pose {
            orientation: Quat::IDENTITY,
            position: Vec3::new(0.1, 1.6, -0.2),
        };
        let newer_pose = alvr_common::Pose {
            orientation: Quat::from_rotation_y(0.2),
            position: Vec3::new(0.2, 1.6, -0.2),
        };
        {
            let mut samples = crate::state::RENDER_TRACKING_TS.lock();
            samples.clear();
            samples.push_back((100, 10, rendered_pose));
            samples.push_back((200, 20, newer_pose));
        }

        let (pose, tracking_ts) = take_render_sample(102).unwrap();

        assert_eq!(tracking_ts, 10);
        assert!(pose.position.abs_diff_eq(rendered_pose.position, 1e-6));
        assert_eq!(crate::state::RENDER_TRACKING_TS.lock().len(), 1);
        crate::state::RENDER_TRACKING_TS.lock().clear();
    }

    #[test]
    fn grip_to_aim_matches_vdxr_calibration() {
        use alvr_common::glam::{Quat, Vec3};

        let aim = grip_to_aim(alvr_common::Pose::default());
        assert!(aim
            .position
            .abs_diff_eq(Vec3::new(0.0, -0.06727241, -0.073480755), 1e-6));

        let expected = Quat::from_rotation_x(-60.0_f32.to_radians());
        assert!((aim.orientation * Vec3::Y).abs_diff_eq(expected * Vec3::Y, 1e-5));
        assert!((aim.orientation * Vec3::Z).abs_diff_eq(expected * Vec3::Z, 1e-5));
    }

    #[test]
    fn grip_to_aim_is_local_to_grip() {
        use alvr_common::glam::{Quat, Vec3};

        let grip = alvr_common::Pose {
            orientation: Quat::from_rotation_y(0.4) * Quat::from_rotation_z(-0.2),
            position: Vec3::new(0.3, 1.1, -0.7),
        };
        let relative = grip.inverse() * grip_to_aim(grip);

        assert!(relative
            .position
            .abs_diff_eq(Vec3::new(0.0, -0.06727241, -0.073480755), 1e-6));
        let expected = Quat::from_rotation_x(-60.0_f32.to_radians());
        assert!((relative.orientation * Vec3::Y).abs_diff_eq(expected * Vec3::Y, 1e-5));
    }

    #[test]
    fn locate_views_keeps_session_running_before_tracking() {
        crate::state::HEAD_POSE_QUEUE.lock().clear();
        crate::state::RENDER_TRACKING_TS.lock().clear();
        let info = sys::ViewLocateInfo {
            ty: sys::StructureType::VIEW_LOCATE_INFO,
            next: std::ptr::null(),
            view_configuration_type: PRIMARY_VIEW_CONFIG,
            display_time: sys::Time::from_nanos(1),
            space: sys::Space::NULL,
        };
        let mut state = sys::ViewState {
            ty: sys::StructureType::VIEW_STATE,
            next: std::ptr::null_mut(),
            view_state_flags: sys::ViewStateFlags::EMPTY,
        };
        let mut count = 0;

        let result = unsafe {
            locate_views(
                sys::Session::NULL,
                &info,
                &mut state,
                0,
                &mut count,
                std::ptr::null_mut(),
            )
        };

        assert_eq!(result, sys::Result::SUCCESS);
        assert_eq!(state.view_state_flags, sys::ViewStateFlags::EMPTY);
        assert_eq!(count, VIEW_COUNT as u32);
        assert!(crate::state::RENDER_TRACKING_TS.lock().is_empty());

        let mut views: [sys::View; VIEW_COUNT] = std::array::from_fn(|_| sys::View {
            ty: sys::StructureType::VIEW,
            next: std::ptr::null_mut(),
            pose: sys::Posef {
                orientation: sys::Quaternionf {
                    x: 0.0,
                    y: 0.0,
                    z: 0.0,
                    w: 1.0,
                },
                position: sys::Vector3f {
                    x: 0.0,
                    y: 0.0,
                    z: 0.0,
                },
            },
            fov: sys::Fovf {
                angle_left: 0.0,
                angle_right: 0.0,
                angle_up: 0.0,
                angle_down: 0.0,
            },
        });
        let result = unsafe {
            locate_views(
                sys::Session::NULL,
                &info,
                &mut state,
                VIEW_COUNT as u32,
                &mut count,
                views.as_mut_ptr(),
            )
        };

        assert_eq!(result, sys::Result::SUCCESS);
        assert_eq!(state.view_state_flags, sys::ViewStateFlags::EMPTY);
        assert_eq!(count, VIEW_COUNT as u32);
        assert!(views.iter().all(|view| view.ty == sys::StructureType::VIEW));
        assert!(crate::state::RENDER_TRACKING_TS.lock().is_empty());
    }

    #[test]
    fn local_space_is_eye_level_with_absolute_horizontal() {
        // VDXR LOCAL: origin lifted to eye height, X/Z absolute. A head at
        // (0.5, 1.7, -0.3) must report (0.5, 0.0, -0.3) in LOCAL at startup.
        let raw = 0x4000_1233;
        let head_pose = alvr_common::Pose {
            orientation: alvr_common::glam::Quat::IDENTITY,
            position: alvr_common::glam::Vec3::new(0.5, 1.7, -0.3),
        };
        *LOCAL_ORIGIN.lock() = None;
        REFERENCE_SPACES.lock().insert(
            raw,
            (sys::ReferenceSpaceType::LOCAL, alvr_common::Pose::default()),
        );

        let local_origin = reference_space_pose(sys::Space::from_raw(raw), head_pose).unwrap();
        let head_in_local = local_origin.inverse() * head_pose;
        assert!(head_in_local
            .position
            .abs_diff_eq(alvr_common::glam::Vec3::new(0.5, 0.0, -0.3), 1e-5));

        REFERENCE_SPACES.lock().remove(&raw);
        *LOCAL_ORIGIN.lock() = None;
    }

    #[test]
    fn stage_space_is_floor_origin() {
        // VDXR STAGE = identity floor origin. The client streams floor-relative,
        // so a head at eye height reports that same height in STAGE.
        let raw = 0x4000_1240;
        let head_pose = alvr_common::Pose {
            orientation: alvr_common::glam::Quat::IDENTITY,
            position: alvr_common::glam::Vec3::new(0.5, 1.7, -0.3),
        };
        REFERENCE_SPACES.lock().insert(
            raw,
            (sys::ReferenceSpaceType::STAGE, alvr_common::Pose::default()),
        );

        let stage_origin = reference_space_pose(sys::Space::from_raw(raw), head_pose).unwrap();
        let head_in_stage = stage_origin.inverse() * head_pose;
        assert!(head_in_stage
            .position
            .abs_diff_eq(alvr_common::glam::Vec3::new(0.5, 1.7, -0.3), 1e-5));

        REFERENCE_SPACES.lock().remove(&raw);
    }

    #[test]
    fn seat_offset_space_reexpresses_pose() {
        // A seat-adjust space created 1 m higher (offset +Y) must report a
        // point at the stage origin as 1 m *below* it (inverse applied).
        let raw = 0x4000_1234;
        let seat_offset = alvr_common::Pose {
            orientation: alvr_common::glam::Quat::IDENTITY,
            position: alvr_common::glam::Vec3::new(0.0, 1.0, 0.0),
        };
        REFERENCE_SPACES
            .lock()
            .insert(raw, (sys::ReferenceSpaceType::STAGE, seat_offset));

        let inv = reference_space_info(sys::Space::from_raw(raw))
            .unwrap()
            .1
            .inverse();
        let stage_origin = alvr_common::Pose::default();
        let in_seat = inv * stage_origin;
        assert!((in_seat.position.y - -1.0).abs() < 1e-5);

        REFERENCE_SPACES.lock().remove(&raw);
    }

    #[test]
    fn extension_enumeration_two_call() {
        let mut count = 0u32;
        let r = unsafe {
            enumerate_instance_extension_properties(
                std::ptr::null(),
                0,
                &mut count,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(r, sys::Result::SUCCESS);
        assert_eq!(count as usize, SUPPORTED_EXTENSIONS.len());
    }

    #[test]
    fn get_system_rejects_handheld() {
        let info = sys::SystemGetInfo {
            ty: sys::StructureType::SYSTEM_GET_INFO,
            next: std::ptr::null(),
            form_factor: sys::FormFactor::HANDHELD_DISPLAY,
        };
        let mut id = sys::SystemId::from_raw(0);
        let r = unsafe { get_system(sys::Instance::from_raw(1), &info, &mut id) };
        assert_eq!(r, sys::Result::ERROR_FORM_FACTOR_UNSUPPORTED);
    }

    #[test]
    fn resolve_known_and_unknown() {
        assert!(resolve(c"xrCreateInstance").is_some());
        assert!(resolve(c"xrNoSuchFunction").is_none());
    }

    #[test]
    fn swapchain_selection_obeys_acquire_wait_release_order() {
        let mut states = vec![crate::state::SwapchainImageState::default(); 3];
        states[0].acquired = true;
        states[1].acquired = true;
        let order = VecDeque::from([0, 1]);
        assert_eq!(next_acquirable_image(&states, 0), Some(2));
        assert_eq!(next_unwaited_image(&order, &states), Some(0));
        states[0].waited = true;
        assert_eq!(next_unwaited_image(&order, &states), Some(1));
        states[1].waited = true;
        assert_eq!(next_unwaited_image(&order, &states), None);
    }

    #[test]
    fn newest_released_image_stays_reserved_until_replaced() {
        let mut states = vec![crate::state::SwapchainImageState::default(); 3];
        let mut latest = None;
        let mut serial = 0;
        record_released_image(&mut states, &mut latest, &mut serial, 1);
        assert_eq!(latest, Some(1));
        assert!(states[1].reserved_for_composition);
        record_released_image(&mut states, &mut latest, &mut serial, 2);
        assert_eq!(latest, Some(2));
        assert_eq!(serial, 2);
        assert!(!states[1].reserved_for_composition);
        assert!(states[2].reserved_for_composition);
        assert_eq!(next_acquirable_image(&states, 2), Some(0));
    }

    #[test]
    fn wait_timeout_maps_infinite_finite_and_retry_results() {
        assert_eq!(
            vk_wait_timeout_ns(sys::Duration::from_nanos(-1)),
            Ok(u64::MAX)
        );
        assert_eq!(vk_wait_timeout_ns(sys::Duration::from_nanos(0)), Ok(0));
        assert_eq!(vk_wait_timeout_ns(sys::Duration::from_nanos(12)), Ok(12));
        assert_eq!(
            vk_wait_timeout_ns(sys::Duration::from_nanos(-2)),
            Err(sys::Result::ERROR_VALIDATION_FAILURE)
        );
        assert_eq!(
            openxr_vk_wait_result(Err(vk::Result::TIMEOUT)),
            sys::Result::TIMEOUT_EXPIRED
        );
    }

    #[test]
    fn vulkan_transfer_only_swapchains_support_runtime_attachment_layout() {
        use sys::SwapchainUsageFlags as XrUsage;
        use windows::Win32::Graphics::Direct3D11::{
            D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE,
        };

        for (requested, expected) in [
            (XrUsage::TRANSFER_DST, vk::ImageUsageFlags::TRANSFER_DST),
            (XrUsage::TRANSFER_SRC, vk::ImageUsageFlags::TRANSFER_SRC),
            (
                XrUsage::TRANSFER_DST | XrUsage::TRANSFER_SRC,
                vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::TRANSFER_SRC,
            ),
            (XrUsage::SAMPLED, vk::ImageUsageFlags::SAMPLED),
        ] {
            let usage = vulkan_image_usage(requested).unwrap();
            assert!(usage.contains(vk::ImageUsageFlags::COLOR_ATTACHMENT));
            assert!(usage.contains(expected));
        }
        let bind_flags = d3d11_bind_flags();
        assert_ne!(bind_flags & D3D11_BIND_RENDER_TARGET.0 as u32, 0);
        assert_ne!(bind_flags & D3D11_BIND_SHADER_RESOURCE.0 as u32, 0);
        assert!(vulkan_image_usage(XrUsage::DEPTH_STENCIL_ATTACHMENT).is_none());
        assert!(vulkan_image_usage(XrUsage::UNORDERED_ACCESS).is_none());
    }

    #[test]
    fn array_slice_and_rect_uv_are_selected_independently() {
        assert!(valid_array_slice(2, 0));
        assert!(valid_array_slice(2, 1));
        assert!(!valid_array_slice(2, 2));
        let rect = sys::Rect2Di {
            offset: sys::Offset2Di { x: 25, y: 10 },
            extent: sys::Extent2Di {
                width: 50,
                height: 40,
            },
        };
        assert_eq!(rect_uv_bounds(rect, 100, 100), [0.25, 0.75, 0.1, 0.5]);
    }

    #[test]
    fn quad_projection_preserves_pose_and_perspective_clip_coordinates() {
        use alvr_common::glam::{Quat, Vec3};
        let eye = CompositionEyeBasis {
            fov: [
                -45.0_f32.to_radians(),
                45.0_f32.to_radians(),
                45.0_f32.to_radians(),
                -45.0_f32.to_radians(),
            ],
            eye_pose_in_stage: alvr_common::Pose::default(),
        };
        let quad = alvr_common::Pose {
            orientation: Quat::IDENTITY,
            position: Vec3::new(0.0, 0.0, -1.0),
        };
        let clip = quad_clip_vertices(
            quad,
            eye,
            sys::Extent2Df {
                width: 2.0,
                height: 2.0,
            },
        )
        .unwrap();
        assert!((clip[0] / clip[3] + 1.0).abs() < 1e-5);
        assert!((clip[1] / clip[3] - 1.0).abs() < 1e-5);
        assert!((clip[4] / clip[7] - 1.0).abs() < 1e-5);
        assert!((clip[13] / clip[15] + 1.0).abs() < 1e-5);
        assert!(quad_clip_vertices(
            alvr_common::Pose {
                position: Vec3::new(0.0, 0.0, 1.0),
                ..quad
            },
            eye,
            sys::Extent2Df {
                width: 2.0,
                height: 2.0
            },
        )
        .is_none());
    }

    #[test]
    fn alpha_modes_and_private_descriptor_layout_match_cpp() {
        use std::mem::size_of;
        assert_eq!(alpha_mode(sys::CompositionLayerFlags::from_raw(0)), 0);
        assert_eq!(
            alpha_mode(sys::CompositionLayerFlags::BLEND_TEXTURE_SOURCE_ALPHA),
            1
        );
        assert_eq!(
            alpha_mode(
                sys::CompositionLayerFlags::BLEND_TEXTURE_SOURCE_ALPHA
                    | sys::CompositionLayerFlags::UNPREMULTIPLIED_ALPHA
            ),
            2
        );
        assert_eq!(size_of::<AlvrOpenXrDrawDesc>(), 112);
    }

    #[test]
    fn swapchain_cleanup_leaves_a_reusable_tombstone() {
        let mut swapchain = crate::state::Swapchain {
            textures: Vec::new(),
            gl_textures: Vec::new(),
            vk_images: Vec::new(),
            nt_handles: Vec::new(),
            vk_cpu_copy: false,
            vk_commands: Vec::new(),
            gl_interop_handles: Vec::new(),
            gl_interop_locked: Vec::new(),
            cpu_copy: false,
            next_index: 0,
            acquisition_order: VecDeque::new(),
            last_released_index: Some(0),
            release_serial: 1,
            image_states: Vec::new(),
            format: 0,
            array_size: 1,
            width: 1,
            height: 1,
        };
        unsafe {
            destroy_swapchain_contents(crate::state::GraphicsApi::D3D11, 0, None, &mut swapchain)
                .unwrap();
        }
        assert!(swapchain.textures.is_empty());
        assert!(swapchain.vk_commands.is_empty());
        assert_eq!(swapchain.last_released_index, None);
    }

    #[test]
    fn failed_vulkan_dispatch_initialization_allows_session_retry() {
        *crate::state::SESSION.lock() = crate::state::SessionState::default();
        *crate::state::GRAPHICS.lock() = crate::state::GraphicsState {
            api: crate::state::GraphicsApi::Vulkan,
            ..Default::default()
        };
        let info = sys::SessionCreateInfo {
            ty: sys::StructureType::SESSION_CREATE_INFO,
            next: std::ptr::null(),
            create_flags: Default::default(),
            system_id: sys::SystemId::from_raw(ALVR_SYSTEM_ID),
        };
        let mut session = sys::Session::from_raw(0);
        assert_eq!(
            unsafe { create_session(sys::Instance::from_raw(1), &info, &mut session) },
            sys::Result::ERROR_RUNTIME_FAILURE
        );
        assert!(!crate::state::SESSION.lock().created);
        assert!(crate::state::GRAPHICS.lock().vulkan.is_none());
        assert_eq!(
            unsafe { create_session(sys::Instance::from_raw(1), &info, &mut session) },
            sys::Result::SUCCESS
        );
        *crate::state::SESSION.lock() = crate::state::SessionState::default();
    }

    #[test]
    fn session_state_machine_progresses() {
        // Reset shared session state for a deterministic run.
        *crate::state::SESSION.lock() = crate::state::SessionState::default();

        let create_info = sys::SessionCreateInfo {
            ty: sys::StructureType::SESSION_CREATE_INFO,
            next: std::ptr::null(),
            create_flags: Default::default(),
            system_id: sys::SystemId::from_raw(ALVR_SYSTEM_ID),
        };
        let mut session = sys::Session::from_raw(0);
        let r = unsafe { create_session(sys::Instance::from_raw(1), &create_info, &mut session) };
        assert_eq!(r, sys::Result::SUCCESS);

        let poll = || {
            let mut buf = sys::EventDataBuffer {
                ty: sys::StructureType::EVENT_DATA_BUFFER,
                next: std::ptr::null(),
                varying: [0; 4000],
            };
            let r = unsafe { poll_event(sys::Instance::from_raw(1), &mut buf) };
            (r, buf)
        };

        let (r, buf) = poll();
        assert_eq!(r, sys::Result::SUCCESS);
        let event = &buf as *const _ as *const sys::EventDataSessionStateChanged;
        assert_eq!(unsafe { (*event).state }, sys::SessionState::IDLE);

        let (_, buf) = poll();
        let event = &buf as *const _ as *const sys::EventDataSessionStateChanged;
        assert_eq!(unsafe { (*event).state }, sys::SessionState::READY);

        // No more events until the app begins the session.
        assert_eq!(poll().0, sys::Result::EVENT_UNAVAILABLE);
    }
}
