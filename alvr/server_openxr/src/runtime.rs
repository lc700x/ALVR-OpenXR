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
use std::{
    collections::HashMap,
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
/// Placeholder per-eye render resolution until the negotiated config is wired in.
const RECOMMENDED_VIEW_WIDTH: u32 = 2016;
const RECOMMENDED_VIEW_HEIGHT: u32 = 2240;

unsafe extern "C" {
    /// Submit a stereo frame to the reused C++ encoder (defined in glue.cpp).
    /// Safe no-op until the encoder is started.
    fn alvr_openxr_encoder_submit(
        shared_handle_left: *mut std::ffi::c_void,
        shared_handle_right: *mut std::ffi::c_void,
        fov_left: *const f32,
        fov_right: *const f32,
        eye_to_head_left: *const f32,
        eye_to_head_right: *const f32,
        // Per-eye normalized UV crop [u_min, u_max, v_min, v_max] in the swapchain
        // texture, derived from each view's sub_image.image_rect. Lets the encoder
        // sample the correct region for double-wide / sub-rect / atlas swapchains.
        bounds_left: *const f32,
        bounds_right: *const f32,
        timestamp_ns: u64,
        idr: bool,
        flip_v: bool,
    );
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
    // ponytail: single-instance runtime, nothing to free yet; real teardown
    // lands with the session/streaming lifecycle.
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

    unsafe { *session = sys::Session::from_raw(ALVR_SESSION_HANDLE) };
    sys::Result::SUCCESS
}

pub unsafe extern "system" fn destroy_session(_session: sys::Session) -> sys::Result {
    let mut state = crate::state::SESSION.lock();
    *state = crate::state::SessionState::default();
    crate::actions::clear_buttons();
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
    crate::streaming::ensure_started();
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
        None => sys::platform::LUID {
            LowPart: 0,
            HighPart: 0,
        },
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
    let luid = crate::vulkan::default_adapter_luid_bytes().unwrap_or([0u8; 8]);
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
    let luid = crate::vulkan::default_adapter_luid_bytes().unwrap_or([0u8; 8]);
    match unsafe {
        crate::vulkan::physical_device_for_luid(info.vulkan_instance as usize, luid)
    } {
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
            info.vulkan_create_info.cast(),
            info.vulkan_allocator.cast(),
            &mut out_instance,
            &mut out_result,
        )
    };
    unsafe {
        *vulkan_instance = out_instance as sys::platform::VkInstance;
        *vulkan_result = out_result;
    }
    if ok {
        sys::Result::SUCCESS
    } else {
        sys::Result::ERROR_RUNTIME_FAILURE
    }
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
            info.vulkan_create_info.cast(),
            info.vulkan_allocator.cast(),
            &mut out_device,
            &mut out_result,
        )
    };
    unsafe {
        *vulkan_device = out_device as sys::platform::VkDevice;
        *vulkan_result = out_result;
    }
    if ok {
        sys::Result::SUCCESS
    } else {
        sys::Result::ERROR_RUNTIME_FAILURE
    }
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

pub unsafe extern "system" fn create_swapchain(
    _session: sys::Session,
    create_info: *const sys::SwapchainCreateInfo,
    swapchain: *mut sys::Swapchain,
) -> sys::Result {
    if create_info.is_null() || swapchain.is_null() {
        return sys::Result::ERROR_VALIDATION_FAILURE;
    }
    let info = unsafe { &*create_info };

    // OpenGL and Vulkan sessions have no app D3D11 device; create a
    // runtime-owned one the encoder and interop share.
    {
        let mut graphics = crate::state::GRAPHICS.lock();
        let needs_device = graphics.api == crate::state::GraphicsApi::OpenGL
            || graphics.api == crate::state::GraphicsApi::Vulkan;
        if graphics.d3d11_device == 0 && needs_device {
            match unsafe { crate::d3d11::create_device() } {
                Ok(dev) => graphics.d3d11_device = dev,
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
    let session_api = crate::state::GRAPHICS.lock().api;
    let is_gl_session = session_api == crate::state::GraphicsApi::OpenGL;
    let is_vk_session = session_api == crate::state::GraphicsApi::Vulkan;
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
    let (vk_images, nt_handles, vk_cpu_copy) = if graphics.api
        == crate::state::GraphicsApi::Vulkan
    {
        match unsafe {
            import_vulkan_images(
                &graphics,
                &textures,
                info.format,
                info.width,
                info.height,
                info.array_size,
            )
        } {
            Ok((imgs, handles)) => (imgs, handles, false),
            Err(_) => {
                // Zero-copy import unsupported — rebuild as a CPU-copy swapchain.
                unsafe { crate::d3d11::release_textures(&textures) };
                let typed_format =
                    crate::vulkan::vk_format_to_dxgi_typed(info.format) as i64;
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
                    )
                } {
                    Ok(imgs) => {
                        eprintln!(
                            "[alvr] create_swapchain: Vulkan CPU-copy fallback ({} images) \
                             — universal GPU compatibility",
                            imgs.len()
                        );
                        (imgs, Vec::new(), true)
                    }
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

    graphics.swapchains.push(crate::state::Swapchain {
        textures,
        gl_textures,
        vk_images,
        nt_handles,
        vk_cpu_copy,
        gl_interop_locked: vec![true; gl_interop_handles.len()],
        gl_interop_handles,
        cpu_copy,
        next_index: 0,
        last_acquired_index: 0,
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
) -> Result<(Vec<(u64, u64)>, Vec<usize>), sys::Result> {
    let Some(vk) = (unsafe {
        crate::vulkan::VulkanFns::load(
            graphics.vk_instance,
            graphics.vk_physical_device,
            graphics.vk_device,
        )
    }) else {
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
            return Err(sys::Result::ERROR_RUNTIME_FAILURE);
        };
        let Some(pair) = (unsafe {
            vk.import_shared_texture(nt, vk_format, width, height, array_size)
        }) else {
            eprintln!("[alvr] import_vulkan_images FAIL: import_shared_texture failed");
            unsafe { free_vulkan_images(&vk, &images) };
            return Err(sys::Result::ERROR_RUNTIME_FAILURE);
        };
        images.push(pair);
        handles.push(nt);
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
) -> Result<Vec<(u64, u64)>, sys::Result> {
    let Some(vk) = (unsafe {
        crate::vulkan::VulkanFns::load(
            graphics.vk_instance,
            graphics.vk_physical_device,
            graphics.vk_device,
        )
    }) else {
        return Err(sys::Result::ERROR_RUNTIME_FAILURE);
    };
    let mut images = Vec::with_capacity(count);
    for _ in 0..count {
        let Some(pair) =
            (unsafe { vk.create_owned_image(vk_format, width, height, array_size) })
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
            unsafe {
                crate::vulkan::VulkanFns::load(
                    graphics.vk_instance,
                    graphics.vk_physical_device,
                    graphics.vk_device,
                )
            }
        } else {
            None
        };
        if let Some(sc) = graphics.swapchains.get(i) {
            if let Some(vk) = &vk {
                for &(image, memory) in &sc.vk_images {
                    unsafe { vk.destroy_image(image, memory) };
                }
            }
            unsafe { crate::d3d11::release_textures(&sc.textures) };
            for &nt in &sc.nt_handles {
                unsafe { crate::d3d11::close_handle(nt) };
            }
            // Leave a tombstone (empty) so later handles stay valid.
            graphics.swapchains[i].textures.clear();
            graphics.swapchains[i].vk_images.clear();
            graphics.swapchains[i].nt_handles.clear();
        }
    }
    sys::Result::SUCCESS
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
    if index.is_null() {
        return sys::Result::ERROR_VALIDATION_FAILURE;
    }
    let Some(i) = swapchain_index(swapchain) else {
        return sys::Result::ERROR_HANDLE_INVALID;
    };
    let mut graphics = crate::state::GRAPHICS.lock();
    let is_gl = graphics.api == crate::state::GraphicsApi::OpenGL;
    let interop_device = graphics.gl_interop_device as *mut std::ffi::c_void;
    let Some(sc) = graphics.swapchains.get_mut(i) else {
        return sys::Result::ERROR_HANDLE_INVALID;
    };
    if sc.textures.is_empty() {
        return sys::Result::ERROR_HANDLE_INVALID;
    }

    let acquired = sc.next_index;
    sc.next_index = (sc.next_index + 1) % sc.textures.len() as u32;
    sc.last_acquired_index = acquired;

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
    unsafe { *index = acquired };
    sys::Result::SUCCESS
}

pub unsafe extern "system" fn wait_swapchain_image(
    _swapchain: sys::Swapchain,
    _wait_info: *const sys::SwapchainImageWaitInfo,
) -> sys::Result {
    // Images are CPU-ready immediately (double/triple buffered); no GPU fence yet.
    // ponytail: add a real fence wait when the encoder consumes in parallel.
    sys::Result::SUCCESS
}

pub unsafe extern "system" fn release_swapchain_image(
    swapchain: sys::Swapchain,
    _release_info: *const sys::SwapchainImageReleaseInfo,
) -> sys::Result {
    let Some(i) = swapchain_index(swapchain) else {
        return sys::Result::ERROR_HANDLE_INVALID;
    };
    let mut graphics = crate::state::GRAPHICS.lock();
    let is_gl = graphics.api == crate::state::GraphicsApi::OpenGL;
    let interop_device = graphics.gl_interop_device as *mut std::ffi::c_void;

    if is_gl {
        let Some(sc) = graphics.swapchains.get_mut(i) else {
            return sys::Result::ERROR_HANDLE_INVALID;
        };
        let index = sc.last_acquired_index as usize;
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
        if graphics.d3d11_device != 0 {
            unsafe { crate::d3d11::flush_device(graphics.d3d11_device) };
        }
    }

    if graphics.api == crate::state::GraphicsApi::D3D11 && graphics.d3d11_device != 0 {
        unsafe { crate::d3d11::flush_device(graphics.d3d11_device) };
    }

    // Vulkan sessions: the app rendered into a VkImage aliased onto the shared
    // D3D11 texture. There is no cross-API fence, so wait for the app's GPU work
    // to finish before the encoder opens the texture on its own D3D11 device,
    // then flush the runtime device that owns the shared texture.
    if graphics.api == crate::state::GraphicsApi::Vulkan {
        if let Some(vk) = unsafe {
            crate::vulkan::VulkanFns::load(
                graphics.vk_instance,
                graphics.vk_physical_device,
                graphics.vk_device,
            )
        } {
            unsafe { vk.wait_idle() };
        }
        if graphics.d3d11_device != 0 {
            unsafe { crate::d3d11::flush_device(graphics.d3d11_device) };
        }
    }
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

    // Pace to the nominal refresh period so apps do not free-run thousands of
    // frames per second. This removes the largest source of reprojection jitter.
    let (sleep_duration, display_time, period) = {
        let mut frame = crate::state::FRAME_LOOP.lock();
        let now = Instant::now();
        let period_duration = Duration::from_nanos(frame.frame_period_ns.max(1) as u64);
        let origin = *frame.time_origin_instant.get_or_insert(now);

        let target = match frame.next_wake_instant {
            Some(t) => t,
            None => now + period_duration,
        };
        frame.next_wake_instant = Some(target + period_duration);
        frame.predicted_display_time_ns = target.duration_since(origin).as_nanos() as i64;

        (
            target.saturating_duration_since(now),
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

    // VDXR all-or-nothing: until a real client head sample exists, report the
    // views unavailable. Desktop2Stereo does not inspect `view_state_flags`; it
    // consumes returned views directly and latches their Y on the first frame.
    // Returning a default Y=0 would pin its Y-reset to the floor, so fail the
    // call until a real sample arrives (the app already catches this and retries).
    let display_time_ns = unsafe { (*view_locate_info).display_time.as_nanos() };
    let sample = sample_head_pose(display_time_ns);
    if !view_state.is_null() {
        let vs = unsafe { &mut *view_state };
        vs.ty = sys::StructureType::VIEW_STATE;
        vs.view_state_flags = sys::ViewStateFlags::EMPTY;
    }
    let Some((head_pose, tracking_ts)) = sample else {
        return sys::Result::ERROR_RUNTIME_FAILURE;
    };
    if !view_state.is_null() {
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
    let base_offset_inv = reference_space_pose(unsafe { (*view_locate_info).space }, head_pose)
        .map(|pose| pose.inverse());

    // Remember which real client tracking timestamp produced this frame's render
    // pose so end_frame can tag the encoded video with it (correct client ATW).
    {
        let mut map = crate::state::RENDER_TRACKING_TS.lock();
        map.push_back((display_time_ns, tracking_ts));
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
        let eye_pose = head_pose * local[i].pose;
        let eye_pose = match base_offset_inv {
            Some(inv) => inv * eye_pose,
            None => eye_pose,
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
    if frame_end_info.is_null() {
        return sys::Result::ERROR_VALIDATION_FAILURE;
    }
    let info = unsafe { &*frame_end_info };
    let display_time_ns = info.display_time.as_nanos().max(0);

    use std::sync::atomic::{AtomicU32, Ordering};

    // Tag the encoded frame with the real client tracking timestamp whose pose
    // rendered it (recorded in locate_views), not the synthetic display time, so
    // the client's async timewarp reprojects from the correct pose. Match the
    // nearest recorded display time rather than requiring exact equality: some
    // apps pass a slightly adjusted predicted_display_time to xrEndFrame vs
    // xrLocateViews, and an exact-match miss would fall back to the synthetic
    // timestamp for that frame only, mixing two time domains and making the
    // client's reprojection jump (visible as flicker / eye desync).
    let timestamp_ns = {
        let mut map = crate::state::RENDER_TRACKING_TS.lock();
        let nearest = map
            .iter()
            .enumerate()
            .min_by_key(|(_, &(d, _))| (d - display_time_ns).abs())
            .map(|(i, &(_, t))| (i, t));

        static TS_LOG: AtomicU32 = AtomicU32::new(0);
        let m = TS_LOG.fetch_add(1, Ordering::Relaxed);
        match nearest {
            Some((i, tracking_ts)) => {
                map.remove(i);
                tracking_ts.max(0) as u64
            }
            None => {
                if m < 5 || m % 300 == 0 {
                    eprintln!(
                        "[alvr] end_frame: no tracking-ts mapping (miss #{m}); \
                         using display time {display_time_ns}"
                    );
                }
                display_time_ns.max(0) as u64
            }
        }
    };

    // Find the projection layer (the streamed content). Quad and other layer
    // types are accepted without error but not composited yet.
    for i in 0..info.layer_count {
        let layer_ptr = unsafe { *info.layers.add(i as usize) };
        if layer_ptr.is_null() {
            continue;
        }
        let ty = unsafe { (*layer_ptr).ty };
        if ty == sys::StructureType::COMPOSITION_LAYER_PROJECTION {
            let proj = layer_ptr.cast::<sys::CompositionLayerProjection>();
            // `timestamp_ns` is the real client tracking sample selected by
            // `locate_views()` for this rendered frame. Do not replace it with
            // an orientation-only history match: repeated orientations and pure
            // translations can select a different sample during fast movement,
            // making client reprojection start from the wrong pose.
            unsafe { submit_projection(&*proj, timestamp_ns) };
        }
        // ponytail: COMPOSITION_LAYER_QUAD accepted (no reject) — quad
        // compositing over the projection lands with FrameRender layer support.
    }
    sys::Result::SUCCESS
}

/// Extract the two per-eye shared textures from a projection layer and hand
/// them to the C++ encoder.
///
/// # Safety
/// `proj` must be a valid projection layer with at least `VIEW_COUNT` views.
unsafe fn submit_projection(proj: &sys::CompositionLayerProjection, timestamp_ns: u64) {
    if (proj.view_count as usize) < VIEW_COUNT || proj.views.is_null() {
        eprintln!(
            "[alvr] submit_projection SKIP: view_count={} views_null={}",
            proj.view_count,
            proj.views.is_null()
        );
        return;
    }

    let mut handles = [std::ptr::null_mut::<std::ffi::c_void>(); VIEW_COUNT];
    let mut fovs = [[0.0f32; 4]; VIEW_COUNT];
    let local_views = crate::state::LOCAL_VIEW_PARAMS.read();
    let eye_to_head = [
        local_views[0].pose.position.to_array(),
        local_views[1].pose.position.to_array(),
    ];
    // Per-eye normalized UV crop [u_min, u_max, v_min, v_max]; defaults to the
    // full texture and is narrowed below from each view's sub_image.image_rect.
    let mut bounds = [[0.0f32, 1.0, 0.0, 1.0]; VIEW_COUNT];

    let graphics = crate::state::GRAPHICS.lock();
    let is_gl = graphics.api == crate::state::GraphicsApi::OpenGL;

    for eye in 0..VIEW_COUNT {
        let view = unsafe { &*proj.views.add(eye) };
        fovs[eye] = [
            view.fov.angle_left,
            view.fov.angle_right,
            view.fov.angle_up,
            view.fov.angle_down,
        ];

        let Some(sc_index) = swapchain_index(view.sub_image.swapchain) else {
            eprintln!("[alvr] submit_projection SKIP: bad swapchain handle eye={eye}");
            return;
        };
        let Some(sc) = graphics.swapchains.get(sc_index) else {
            eprintln!("[alvr] submit_projection SKIP: swapchain index {sc_index} missing");
            return;
        };
        let Some(&texture) = sc.textures.get(sc.last_acquired_index as usize) else {
            eprintln!("[alvr] submit_projection SKIP: no texture at index");
            return;
        };

        // Honor the app's per-eye viewport: many engines render both eyes into a
        // single double-wide swapchain (or a sub-rect of a larger texture) and
        // select each eye via image_rect. Encoding the full texture then squashes
        // the content and warps geometry. Convert the rect to normalized UVs so
        // the encoder samples exactly the eye's region.
        let rect = view.sub_image.image_rect;
        let tex_w = sc.width.max(1) as f32;
        let tex_h = sc.height.max(1) as f32;
        let u_min = (rect.offset.x as f32 / tex_w).clamp(0.0, 1.0);
        let v_min = (rect.offset.y as f32 / tex_h).clamp(0.0, 1.0);
        let u_max = ((rect.offset.x + rect.extent.width) as f32 / tex_w).clamp(0.0, 1.0);
        let v_max = ((rect.offset.y + rect.extent.height) as f32 / tex_h).clamp(0.0, 1.0);
        // Guard against zero/inverted rects (some apps leave image_rect empty):
        // fall back to the full texture rather than collapsing the crop.
        if u_max > u_min && v_max > v_min {
            bounds[eye] = [u_min, u_max, v_min, v_max];
        }

        // CPU-copy fallback (universal GPU path): read back the app's GL texture
        // and upload it into the D3D11 texture the encoder reads.
        if is_gl && sc.cpu_copy {
            if let Some(&gl_name) = sc.gl_textures.get(sc.last_acquired_index as usize) {
                if let Some(copy) = unsafe { crate::gl_interop::GlCopyFns::load() } {
                    let size = (sc.width as usize) * (sc.height as usize) * 4;
                    let mut buf = vec![0u8; size];
                    unsafe { copy.read_pixels(gl_name, &mut buf) };
                    unsafe {
                        crate::d3d11::update_texture_rgba(
                            graphics.d3d11_device,
                            texture,
                            sc.width,
                            &buf,
                        );
                        // Consumer encoder opens this texture on another D3D11
                        // device; submit the CPU upload before sharing it.
                        crate::d3d11::flush_device(graphics.d3d11_device);
                    }
                }
            }
        }

        // Vulkan CPU-copy fallback (universal GPU path): read back the app's
        // owned VkImage and upload it into the D3D11 texture the encoder reads.
        if graphics.api == crate::state::GraphicsApi::Vulkan && sc.vk_cpu_copy {
            if let Some(&(vk_image, _)) = sc.vk_images.get(sc.last_acquired_index as usize) {
                if let Some(vk) = unsafe {
                    crate::vulkan::VulkanFns::load(
                        graphics.vk_instance,
                        graphics.vk_physical_device,
                        graphics.vk_device,
                    )
                } {
                    if let Some(buf) = unsafe {
                        vk.readback_image(
                            vk_image,
                            sc.width,
                            sc.height,
                            graphics.vk_queue_family_index,
                            graphics.vk_queue_index,
                        )
                    } {
                        unsafe {
                            crate::d3d11::update_texture_rgba(
                                graphics.d3d11_device,
                                texture,
                                sc.width,
                                &buf,
                            );
                            crate::d3d11::flush_device(graphics.d3d11_device);
                        }
                    }
                }
            }
        }

        // Vulkan zero-copy sessions back the swapchain with NT-shared textures
        // whose legacy KMT handle (GetSharedHandle) is unavailable; reuse the NT
        // handle created at import (the encoder opens it via
        // OpenSharedResource1). The Vulkan CPU-copy fallback and all other
        // sessions use the KMT shared handle of the runtime-owned texture.
        let shared = if graphics.api == crate::state::GraphicsApi::Vulkan && !sc.vk_cpu_copy {
            sc.nt_handles.get(sc.last_acquired_index as usize).copied()
        } else {
            unsafe { crate::d3d11::shared_handle(texture) }
        };
        let Some(shared) = shared else {
            eprintln!("[alvr] submit_projection SKIP: shared_handle failed eye={eye}");
            return;
        };
        handles[eye] = shared as *mut std::ffi::c_void;
    }

    unsafe {
        let idr = crate::state::REQUEST_IDR.swap(false, std::sync::atomic::Ordering::Relaxed);
        // OpenGL renders with a bottom-left origin, so the aliased D3D11 texture
        // is vertically flipped versus D3D11's top-left origin. Flip V for GL.
        alvr_openxr_encoder_submit(
            handles[0],
            handles[1],
            fovs[0].as_ptr(),
            fovs[1].as_ptr(),
            eye_to_head[0].as_ptr(),
            eye_to_head[1].as_ptr(),
            bounds[0].as_ptr(),
            bounds[1].as_ptr(),
            timestamp_ns,
            idr,
            is_gl,
        );
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
    fn locate_views_rejects_pretracking_default_pose() {
        crate::state::HEAD_POSE_QUEUE.lock().clear();
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

        assert_eq!(result, sys::Result::ERROR_RUNTIME_FAILURE);
        assert_eq!(state.view_state_flags, sys::ViewStateFlags::EMPTY);
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
