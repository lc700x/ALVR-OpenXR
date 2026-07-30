//! Vulkan swapchain support via D3D11<->Vulkan external-memory interop.
//!
//! An `XR_KHR_vulkan_enable` / `XR_KHR_vulkan_enable2` app renders into
//! `VkImage`s. The ALVR encoder consumes D3D11 textures, so each Vulkan
//! swapchain image is a `VkImage` *aliased* onto a shared D3D11 texture through
//! `VK_KHR_external_memory_win32` (importing the D3D11 KMT shared handle,
//! mirroring the OpenGL `WGL_NV_DX_interop2` path). The app renders Vulkan while
//! the encoder reads the very same underlying texture as D3D11 — zero-copy.
//!
//! Everything here is resolved dynamically from `vulkan-1.dll`; the crate does
//! not link against Vulkan, so a machine without a Vulkan loader simply reports
//! the extension paths as unavailable and the app falls back to D3D11/OpenGL.

#![allow(non_snake_case, non_upper_case_globals)]

use std::ffi::{c_char, c_void, CStr};

use windows::core::PCSTR;
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryA};

// --- Vulkan handles ---------------------------------------------------------
// Dispatchable handles are pointer-sized; non-dispatchable are 64-bit. We keep
// them as raw integers and only reinterpret at FFI boundaries.
type VkInstance = *mut c_void;
type VkPhysicalDevice = *mut c_void;
type VkDevice = *mut c_void;
type VkImage = u64;
type VkDeviceMemory = u64;
type VkResult = i32;

const VK_SUCCESS: VkResult = 0;

// --- Structure types --------------------------------------------------------
const VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO: u32 = 14;
const VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO: u32 = 5;
const VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_IMAGE_CREATE_INFO: u32 = 1_000_072_001;
const VK_STRUCTURE_TYPE_IMPORT_MEMORY_WIN32_HANDLE_INFO_KHR: u32 = 1_000_073_000;
const VK_STRUCTURE_TYPE_MEMORY_DEDICATED_ALLOCATE_INFO: u32 = 1_000_127_001;
const VK_STRUCTURE_TYPE_MEMORY_WIN32_HANDLE_PROPERTIES_KHR: u32 = 1_000_073_001;
const VK_STRUCTURE_TYPE_IMAGE_FORMAT_LIST_CREATE_INFO: u32 = 1_000_147_000;
const VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_ID_PROPERTIES: u32 = 1_000_071_004;
const VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_PROPERTIES_2: u32 = 1_000_059_001;

// --- Enums / flags ----------------------------------------------------------
const VK_IMAGE_TYPE_2D: u32 = 1;
const VK_IMAGE_TILING_OPTIMAL: u32 = 0;
const VK_SHARING_MODE_EXCLUSIVE: u32 = 0;
const VK_IMAGE_LAYOUT_UNDEFINED: u32 = 0;
const VK_SAMPLE_COUNT_1_BIT: u32 = 1;

/// The imported D3D11 texture is TYPELESS; the VkImage carries the concrete
/// (sRGB/linear) format. `MUTABLE_FORMAT` + an explicit format list is required
/// for every desktop driver (AMD/Intel/NVIDIA) to bind the shared memory.
const VK_IMAGE_CREATE_MUTABLE_FORMAT_BIT: u32 = 0x0000_0040;

const VK_IMAGE_USAGE_TRANSFER_SRC_BIT: u32 = 0x0000_0001;
const VK_IMAGE_USAGE_TRANSFER_DST_BIT: u32 = 0x0000_0002;
const VK_IMAGE_USAGE_SAMPLED_BIT: u32 = 0x0000_0004;
const VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT: u32 = 0x0000_0010;

/// Import a D3D11 texture opened via its NT shared handle
/// (`IDXGIResource1::CreateSharedHandle`). The NT handle type is what Vulkan
/// drivers accept; the legacy KMT handle is rejected with
/// `VK_ERROR_INVALID_EXTERNAL_HANDLE`.
const VK_EXTERNAL_MEMORY_HANDLE_TYPE_D3D11_TEXTURE_BIT: u32 = 0x0000_0020;
const VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT: u32 = 0x0000_0001;

// --- CPU-copy fallback constants (universal readback path) ------------------
const VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO: u32 = 12;
const VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO: u32 = 39;
const VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO: u32 = 40;
const VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO: u32 = 42;
const VK_STRUCTURE_TYPE_SUBMIT_INFO: u32 = 4;
const VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER: u32 = 45;

const VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT: u32 = 0x0000_0002;
const VK_MEMORY_PROPERTY_HOST_COHERENT_BIT: u32 = 0x0000_0004;
const VK_BUFFER_USAGE_TRANSFER_DST_BIT: u32 = 0x0000_0002;
const VK_COMMAND_BUFFER_LEVEL_PRIMARY: u32 = 0;
const VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT: u32 = 0x0000_0001;
const VK_IMAGE_ASPECT_COLOR_BIT: u32 = 0x0000_0001;
const VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL: u32 = 2;
const VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL: u32 = 6;
const VK_PIPELINE_STAGE_ALL_COMMANDS_BIT: u32 = 0x0001_0000;
const VK_PIPELINE_STAGE_TRANSFER_BIT: u32 = 0x0000_1000;
const VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT: u32 = 0x0000_0100;
const VK_ACCESS_TRANSFER_READ_BIT: u32 = 0x0000_0800;
const VK_QUEUE_FAMILY_IGNORED: u32 = u32::MAX;
/// Extra usage the owned CPU-copy image needs so its pixels can be read back.
const VK_IMAGE_USAGE_TRANSFER_SRC_BIT_FLAG: u32 = 0x0000_0001;

// --- Vulkan formats we accept for swapchains --------------------------------
pub const VK_FORMAT_R8G8B8A8_UNORM: i64 = 37;
pub const VK_FORMAT_R8G8B8A8_SRGB: i64 = 43;
pub const VK_FORMAT_B8G8R8A8_UNORM: i64 = 44;
pub const VK_FORMAT_B8G8R8A8_SRGB: i64 = 50;

/// Map a Vulkan format to the equivalent *typeless* DXGI format used for the
/// backing shared D3D11 texture. The concrete (sRGB/linear) format is applied
/// on the Vulkan `VkImage`; a typeless D3D11 backing is what lets every desktop
/// driver import + bind the shared memory. Falls back to R8G8B8A8_TYPELESS.
pub fn vk_format_to_dxgi(vk_format: i64) -> i32 {
    match vk_format {
        VK_FORMAT_R8G8B8A8_UNORM | VK_FORMAT_R8G8B8A8_SRGB => 27, // DXGI_FORMAT_R8G8B8A8_TYPELESS
        VK_FORMAT_B8G8R8A8_UNORM | VK_FORMAT_B8G8R8A8_SRGB => 90, // DXGI_FORMAT_B8G8R8A8_TYPELESS
        _ => 27,
    }
}

/// The UNORM sibling of an sRGB VkFormat (identity for non-sRGB). Both are put
/// in the mutable-format image's view-format list so the app can render sRGB
/// while the encoder samples the same memory.
fn unorm_sibling(vk_format: i64) -> i64 {
    match vk_format {
        VK_FORMAT_R8G8B8A8_SRGB => VK_FORMAT_R8G8B8A8_UNORM,
        VK_FORMAT_B8G8R8A8_SRGB => VK_FORMAT_B8G8R8A8_UNORM,
        other => other,
    }
}

/// Map a Vulkan format to the equivalent *typed* DXGI format, used for the
/// CPU-copy fallback where the runtime owns the D3D11 texture and the encoder
/// creates a typed SRV over it. Falls back to R8G8B8A8_UNORM_SRGB (29).
pub fn vk_format_to_dxgi_typed(vk_format: i64) -> i32 {
    match vk_format {
        VK_FORMAT_R8G8B8A8_UNORM => 28, // DXGI_FORMAT_R8G8B8A8_UNORM
        VK_FORMAT_R8G8B8A8_SRGB => 29,  // DXGI_FORMAT_R8G8B8A8_UNORM_SRGB
        VK_FORMAT_B8G8R8A8_UNORM => 87, // DXGI_FORMAT_B8G8R8A8_UNORM
        VK_FORMAT_B8G8R8A8_SRGB => 91,  // DXGI_FORMAT_B8G8R8A8_UNORM_SRGB
        _ => 29,
    }
}

// --- Vulkan structs (repr(C), matching the Vulkan ABI) ----------------------

#[repr(C)]
struct VkExtent3D {
    width: u32,
    height: u32,
    depth: u32,
}

#[repr(C)]
struct VkExternalMemoryImageCreateInfo {
    s_type: u32,
    p_next: *const c_void,
    handle_types: u32,
}

#[repr(C)]
struct VkImageFormatListCreateInfo {
    s_type: u32,
    p_next: *const c_void,
    view_format_count: u32,
    p_view_formats: *const u32,
}

#[repr(C)]
struct VkImageCreateInfo {
    s_type: u32,
    p_next: *const c_void,
    flags: u32,
    image_type: u32,
    format: u32,
    extent: VkExtent3D,
    mip_levels: u32,
    array_layers: u32,
    samples: u32,
    tiling: u32,
    usage: u32,
    sharing_mode: u32,
    queue_family_index_count: u32,
    p_queue_family_indices: *const u32,
    initial_layout: u32,
}

#[repr(C)]
struct VkMemoryRequirements {
    size: u64,
    alignment: u64,
    memory_type_bits: u32,
}

#[repr(C)]
struct VkImportMemoryWin32HandleInfoKHR {
    s_type: u32,
    p_next: *const c_void,
    handle_type: u32,
    handle: *mut c_void,
    name: *const u16,
}

#[repr(C)]
struct VkMemoryDedicatedAllocateInfo {
    s_type: u32,
    p_next: *const c_void,
    image: VkImage,
    buffer: u64,
}

#[repr(C)]
struct VkMemoryWin32HandlePropertiesKHR {
    s_type: u32,
    p_next: *mut c_void,
    memory_type_bits: u32,
}

#[repr(C)]
struct VkMemoryAllocateInfo {
    s_type: u32,
    p_next: *const c_void,
    allocation_size: u64,
    memory_type_index: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct VkMemoryType {
    property_flags: u32,
    heap_index: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct VkMemoryHeap {
    size: u64,
    flags: u32,
}

#[repr(C)]
struct VkPhysicalDeviceMemoryProperties {
    memory_type_count: u32,
    memory_types: [VkMemoryType; 32],
    memory_heap_count: u32,
    memory_heaps: [VkMemoryHeap; 16],
}

#[repr(C)]
struct VkPhysicalDeviceIDProperties {
    s_type: u32,
    p_next: *mut c_void,
    device_uuid: [u8; 16],
    driver_uuid: [u8; 16],
    device_luid: [u8; 8],
    device_node_mask: u32,
    device_luid_valid: u32,
}

#[repr(C)]
struct VkPhysicalDeviceProperties2 {
    s_type: u32,
    p_next: *mut c_void,
    // The base VkPhysicalDeviceProperties payload is large; we never read it, so
    // reserve the exact byte size to keep the ABI correct for pNext walking.
    properties: [u8; 824],
}

// --- CPU-copy fallback structs ----------------------------------------------

#[repr(C)]
struct VkBufferCreateInfo {
    s_type: u32,
    p_next: *const c_void,
    flags: u32,
    size: u64,
    usage: u32,
    sharing_mode: u32,
    queue_family_index_count: u32,
    p_queue_family_indices: *const u32,
}

#[repr(C)]
struct VkCommandPoolCreateInfo {
    s_type: u32,
    p_next: *const c_void,
    flags: u32,
    queue_family_index: u32,
}

#[repr(C)]
struct VkCommandBufferAllocateInfo {
    s_type: u32,
    p_next: *const c_void,
    command_pool: u64,
    level: u32,
    command_buffer_count: u32,
}

#[repr(C)]
struct VkCommandBufferBeginInfo {
    s_type: u32,
    p_next: *const c_void,
    flags: u32,
    p_inheritance_info: *const c_void,
}

#[repr(C)]
struct VkSubmitInfo {
    s_type: u32,
    p_next: *const c_void,
    wait_semaphore_count: u32,
    p_wait_semaphores: *const u64,
    p_wait_dst_stage_mask: *const u32,
    command_buffer_count: u32,
    p_command_buffers: *const *mut c_void,
    signal_semaphore_count: u32,
    p_signal_semaphores: *const u64,
}

#[repr(C)]
struct VkImageSubresourceRange {
    aspect_mask: u32,
    base_mip_level: u32,
    level_count: u32,
    base_array_layer: u32,
    layer_count: u32,
}

#[repr(C)]
struct VkImageMemoryBarrier {
    s_type: u32,
    p_next: *const c_void,
    src_access_mask: u32,
    dst_access_mask: u32,
    old_layout: u32,
    new_layout: u32,
    src_queue_family_index: u32,
    dst_queue_family_index: u32,
    image: VkImage,
    subresource_range: VkImageSubresourceRange,
}

#[repr(C)]
struct VkImageSubresourceLayers {
    aspect_mask: u32,
    mip_level: u32,
    base_array_layer: u32,
    layer_count: u32,
}

#[repr(C)]
struct VkOffset3D {
    x: i32,
    y: i32,
    z: i32,
}

#[repr(C)]
struct VkBufferImageCopy {
    buffer_offset: u64,
    buffer_row_length: u32,
    buffer_image_height: u32,
    image_subresource: VkImageSubresourceLayers,
    image_offset: VkOffset3D,
    image_extent: VkExtent3D,
}

// --- Function pointer types -------------------------------------------------
type PfnVoid = unsafe extern "system" fn();
type PfnGetInstanceProcAddr =
    unsafe extern "system" fn(VkInstance, *const c_char) -> Option<PfnVoid>;
type PfnGetDeviceProcAddr = unsafe extern "system" fn(VkDevice, *const c_char) -> Option<PfnVoid>;
type PfnCreateImage = unsafe extern "system" fn(
    VkDevice,
    *const VkImageCreateInfo,
    *const c_void,
    *mut VkImage,
) -> VkResult;
type PfnDestroyImage = unsafe extern "system" fn(VkDevice, VkImage, *const c_void);
type PfnGetImageMemoryRequirements =
    unsafe extern "system" fn(VkDevice, VkImage, *mut VkMemoryRequirements);
type PfnAllocateMemory = unsafe extern "system" fn(
    VkDevice,
    *const VkMemoryAllocateInfo,
    *const c_void,
    *mut VkDeviceMemory,
) -> VkResult;
type PfnFreeMemory = unsafe extern "system" fn(VkDevice, VkDeviceMemory, *const c_void);
type PfnBindImageMemory =
    unsafe extern "system" fn(VkDevice, VkImage, VkDeviceMemory, u64) -> VkResult;
type PfnDeviceWaitIdle = unsafe extern "system" fn(VkDevice) -> VkResult;
type PfnGetMemoryWin32HandlePropertiesKHR = unsafe extern "system" fn(
    VkDevice,
    u32,
    *mut c_void,
    *mut VkMemoryWin32HandlePropertiesKHR,
) -> VkResult;
type PfnGetPhysicalDeviceMemoryProperties =
    unsafe extern "system" fn(VkPhysicalDevice, *mut VkPhysicalDeviceMemoryProperties);
type PfnEnumeratePhysicalDevices =
    unsafe extern "system" fn(VkInstance, *mut u32, *mut VkPhysicalDevice) -> VkResult;
type PfnGetPhysicalDeviceProperties2 =
    unsafe extern "system" fn(VkPhysicalDevice, *mut VkPhysicalDeviceProperties2);

/// Device-level Vulkan entry points needed for external-memory image import.
pub struct VulkanFns {
    device: VkDevice,
    physical_device: VkPhysicalDevice,
    get_device_proc_addr: PfnGetDeviceProcAddr,
    create_image: PfnCreateImage,
    destroy_image: PfnDestroyImage,
    get_image_memory_requirements: PfnGetImageMemoryRequirements,
    allocate_memory: PfnAllocateMemory,
    free_memory: PfnFreeMemory,
    bind_image_memory: PfnBindImageMemory,
    device_wait_idle: PfnDeviceWaitIdle,
    get_memory_win32_handle_properties: Option<PfnGetMemoryWin32HandlePropertiesKHR>,
    get_physical_device_memory_properties: PfnGetPhysicalDeviceMemoryProperties,
}

/// Load `vkGetInstanceProcAddr` from `vulkan-1.dll`, if a Vulkan loader exists.
fn get_instance_proc_addr() -> Option<PfnGetInstanceProcAddr> {
    unsafe {
        let module = LoadLibraryA(PCSTR(c"vulkan-1.dll".as_ptr().cast())).ok()?;
        let proc = GetProcAddress(module, PCSTR(c"vkGetInstanceProcAddr".as_ptr().cast()))?;
        Some(std::mem::transmute::<_, PfnGetInstanceProcAddr>(proc))
    }
}

/// Resolve an instance-level entry point by name.
unsafe fn instance_proc(
    gipa: PfnGetInstanceProcAddr,
    instance: VkInstance,
    name: &CStr,
) -> Option<PfnVoid> {
    unsafe { gipa(instance, name.as_ptr()) }
}

impl VulkanFns {
    /// Resolve the device-level functions from an app-provided `VkInstance` /
    /// `VkDevice` pair (from `GraphicsBindingVulkanKHR`).
    ///
    /// # Safety
    /// `instance`, `physical_device` and `device` must be valid live handles.
    pub unsafe fn load(
        instance: usize,
        physical_device: usize,
        device: usize,
    ) -> Option<Self> {
        let gipa = get_instance_proc_addr()?;
        let instance = instance as VkInstance;
        let device = device as VkDevice;
        let physical_device = physical_device as VkPhysicalDevice;

        let get_device_proc_addr: PfnGetDeviceProcAddr = unsafe {
            std::mem::transmute(instance_proc(gipa, instance, c"vkGetDeviceProcAddr")?)
        };
        let get_physical_device_memory_properties: PfnGetPhysicalDeviceMemoryProperties = unsafe {
            std::mem::transmute(instance_proc(
                gipa,
                instance,
                c"vkGetPhysicalDeviceMemoryProperties",
            )?)
        };

        macro_rules! dev {
            ($name:literal, $ty:ty) => {
                unsafe {
                    std::mem::transmute::<PfnVoid, $ty>(get_device_proc_addr(
                        device,
                        $name.as_ptr(),
                    )?)
                }
            };
        }

        Some(Self {
            device,
            physical_device,
            get_device_proc_addr,
            create_image: dev!(c"vkCreateImage", PfnCreateImage),
            destroy_image: dev!(c"vkDestroyImage", PfnDestroyImage),
            get_image_memory_requirements: dev!(
                c"vkGetImageMemoryRequirements",
                PfnGetImageMemoryRequirements
            ),
            allocate_memory: dev!(c"vkAllocateMemory", PfnAllocateMemory),
            free_memory: dev!(c"vkFreeMemory", PfnFreeMemory),
            bind_image_memory: dev!(c"vkBindImageMemory", PfnBindImageMemory),
            device_wait_idle: dev!(c"vkDeviceWaitIdle", PfnDeviceWaitIdle),
            get_memory_win32_handle_properties: unsafe {
                get_device_proc_addr(device, c"vkGetMemoryWin32HandlePropertiesKHR".as_ptr())
                    .map(|f| std::mem::transmute::<PfnVoid, PfnGetMemoryWin32HandlePropertiesKHR>(f))
            },
            get_physical_device_memory_properties,
        })
    }

    /// Pick a memory type index satisfying `type_bits`, preferring device-local.
    unsafe fn memory_type_index(&self, type_bits: u32) -> Option<u32> {
        let mut props: VkPhysicalDeviceMemoryProperties = unsafe { std::mem::zeroed() };
        unsafe { (self.get_physical_device_memory_properties)(self.physical_device, &mut props) };

        let mut fallback = None;
        for i in 0..props.memory_type_count {
            if type_bits & (1 << i) == 0 {
                continue;
            }
            let flags = props.memory_types[i as usize].property_flags;
            if flags & VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT != 0 {
                return Some(i);
            }
            fallback.get_or_insert(i);
        }
        fallback
    }

    /// Import a D3D11 shared texture (opened by its KMT shared handle) as a
    /// `VkImage` aliased onto the same GPU memory. Returns `(image, memory)`;
    /// both must be released with [`destroy_image`].
    ///
    /// # Safety
    /// `shared_handle` must be a valid D3D11 KMT shared handle for a texture
    /// matching `format`/`width`/`height`/`array_layers`, created on the same
    /// adapter as this Vulkan device.
    pub unsafe fn import_shared_texture(
        &self,
        shared_handle: usize,
        vk_format: i64,
        width: u32,
        height: u32,
        array_layers: u32,
    ) -> Option<(u64, u64)> {
        let external_info = VkExternalMemoryImageCreateInfo {
            s_type: VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_IMAGE_CREATE_INFO,
            p_next: std::ptr::null(),
            handle_types: VK_EXTERNAL_MEMORY_HANDLE_TYPE_D3D11_TEXTURE_BIT,
        };
        // The backing D3D11 texture is TYPELESS; declare the concrete VkImage
        // format plus its UNORM sibling in a format list so a mutable-format
        // image can be bound to the imported typeless memory on any driver.
        let view_formats = [vk_format as u32, unorm_sibling(vk_format) as u32];
        let format_list = VkImageFormatListCreateInfo {
            s_type: VK_STRUCTURE_TYPE_IMAGE_FORMAT_LIST_CREATE_INFO,
            p_next: (&external_info as *const VkExternalMemoryImageCreateInfo).cast(),
            view_format_count: view_formats.len() as u32,
            p_view_formats: view_formats.as_ptr(),
        };
        let image_info = VkImageCreateInfo {
            s_type: VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO,
            p_next: (&format_list as *const VkImageFormatListCreateInfo).cast(),
            flags: VK_IMAGE_CREATE_MUTABLE_FORMAT_BIT,
            image_type: VK_IMAGE_TYPE_2D,
            format: vk_format as u32,
            extent: VkExtent3D {
                width,
                height,
                depth: 1,
            },
            mip_levels: 1,
            array_layers: array_layers.max(1),
            samples: VK_SAMPLE_COUNT_1_BIT,
            tiling: VK_IMAGE_TILING_OPTIMAL,
            // Match the backing D3D11 texture's bind flags exactly
            // (RENDER_TARGET | SHADER_RESOURCE). Adding TRANSFER usage the D3D11
            // resource wasn't created with changes the required allocation size
            // and makes the dedicated external-memory import fail with
            // VK_ERROR_INVALID_EXTERNAL_HANDLE.
            usage: VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT | VK_IMAGE_USAGE_SAMPLED_BIT,
            sharing_mode: VK_SHARING_MODE_EXCLUSIVE,
            queue_family_index_count: 0,
            p_queue_family_indices: std::ptr::null(),
            initial_layout: VK_IMAGE_LAYOUT_UNDEFINED,
        };

        let mut image: VkImage = 0;
        let create_res = unsafe {
            (self.create_image)(self.device, &image_info, std::ptr::null(), &mut image)
        };
        if create_res != VK_SUCCESS {
            eprintln!("[alvr] vk import: vkCreateImage failed (VkResult={create_res})");
            return None;
        }

        let mut mem_req: VkMemoryRequirements = unsafe { std::mem::zeroed() };
        unsafe {
            (self.get_image_memory_requirements)(self.device, image, &mut mem_req);
        }

        // For imported handles the valid memory types come from the handle
        // itself; intersect with the image's requirements when available.
        let mut type_bits = mem_req.memory_type_bits;
        if let Some(get_props) = self.get_memory_win32_handle_properties {
            let mut props = VkMemoryWin32HandlePropertiesKHR {
                s_type: VK_STRUCTURE_TYPE_MEMORY_WIN32_HANDLE_PROPERTIES_KHR,
                p_next: std::ptr::null_mut(),
                memory_type_bits: 0,
            };
            let res = unsafe {
                get_props(
                    self.device,
                    VK_EXTERNAL_MEMORY_HANDLE_TYPE_D3D11_TEXTURE_BIT,
                    shared_handle as *mut c_void,
                    &mut props,
                )
            };
            if res == VK_SUCCESS && props.memory_type_bits != 0 {
                let intersection = type_bits & props.memory_type_bits;
                type_bits = if intersection != 0 {
                    intersection
                } else {
                    props.memory_type_bits
                };
            } else {
                eprintln!(
                    "[alvr] vk import: vkGetMemoryWin32HandlePropertiesKHR VkResult={res} bits={}",
                    props.memory_type_bits
                );
            }
        }

        let Some(type_index) = (unsafe { self.memory_type_index(type_bits) }) else {
            eprintln!("[alvr] vk import: no compatible memory type (bits={type_bits:#x})");
            unsafe { (self.destroy_image)(self.device, image, std::ptr::null()) };
            return None;
        };

        // Import the D3D11 NT shared handle, dedicated to this image. NVIDIA is
        // sensitive to the pNext order: the import struct must be the direct
        // child of VkMemoryAllocateInfo, with the dedicated-allocate struct
        // chained after it.
        let dedicated_info = VkMemoryDedicatedAllocateInfo {
            s_type: VK_STRUCTURE_TYPE_MEMORY_DEDICATED_ALLOCATE_INFO,
            p_next: std::ptr::null(),
            image,
            buffer: 0,
        };
        let import_info = VkImportMemoryWin32HandleInfoKHR {
            s_type: VK_STRUCTURE_TYPE_IMPORT_MEMORY_WIN32_HANDLE_INFO_KHR,
            p_next: (&dedicated_info as *const VkMemoryDedicatedAllocateInfo).cast(),
            handle_type: VK_EXTERNAL_MEMORY_HANDLE_TYPE_D3D11_TEXTURE_BIT,
            handle: shared_handle as *mut c_void,
            name: std::ptr::null(),
        };
        let alloc_info = VkMemoryAllocateInfo {
            s_type: VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
            p_next: (&import_info as *const VkImportMemoryWin32HandleInfoKHR).cast(),
            allocation_size: mem_req.size,
            memory_type_index: type_index,
        };

        let mut memory: VkDeviceMemory = 0;
        let alloc_res = unsafe {
            (self.allocate_memory)(self.device, &alloc_info, std::ptr::null(), &mut memory)
        };
        if alloc_res != VK_SUCCESS {
            eprintln!("[alvr] vk import: vkAllocateMemory failed (VkResult={alloc_res})");
            unsafe { (self.destroy_image)(self.device, image, std::ptr::null()) };
            return None;
        }

        let bind_res = unsafe { (self.bind_image_memory)(self.device, image, memory, 0) };
        if bind_res != VK_SUCCESS {
            eprintln!("[alvr] vk import: vkBindImageMemory failed (VkResult={bind_res})");
            unsafe {
                (self.free_memory)(self.device, memory, std::ptr::null());
                (self.destroy_image)(self.device, image, std::ptr::null());
            }
            return None;
        }

        Some((image, memory))
    }

    /// Wait until the device is idle so the app's Vulkan rendering completes
    /// before the encoder opens the shared texture on its own D3D11 device. The
    /// shared texture carries no cross-API fence, so this ordering barrier
    /// prevents the encoder reading a half-rendered frame.
    ///
    /// # Safety
    /// The device must be valid.
    pub unsafe fn wait_idle(&self) {
        unsafe { (self.device_wait_idle)(self.device) };
    }

    /// Resolve a device-level entry point by name.
    unsafe fn dev(&self, name: &CStr) -> Option<PfnVoid> {
        unsafe { (self.get_device_proc_addr)(self.device, name.as_ptr()) }
    }

    /// Create a runtime-owned (non-imported) `VkImage` the app renders into, for
    /// the universal CPU-copy fallback. The image adds `TRANSFER_SRC` usage so
    /// its pixels can be read back at submit time and uploaded into the encoder
    /// D3D11 texture. Returns `(image, memory)`; free with [`destroy_image`].
    ///
    /// # Safety
    /// The device must be valid.
    pub unsafe fn create_owned_image(
        &self,
        vk_format: i64,
        width: u32,
        height: u32,
        array_layers: u32,
    ) -> Option<(u64, u64)> {
        let image_info = VkImageCreateInfo {
            s_type: VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO,
            p_next: std::ptr::null(),
            flags: 0,
            image_type: VK_IMAGE_TYPE_2D,
            format: vk_format as u32,
            extent: VkExtent3D {
                width,
                height,
                depth: 1,
            },
            mip_levels: 1,
            array_layers: array_layers.max(1),
            samples: VK_SAMPLE_COUNT_1_BIT,
            tiling: VK_IMAGE_TILING_OPTIMAL,
            usage: VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT
                | VK_IMAGE_USAGE_SAMPLED_BIT
                | VK_IMAGE_USAGE_TRANSFER_SRC_BIT_FLAG,
            sharing_mode: VK_SHARING_MODE_EXCLUSIVE,
            queue_family_index_count: 0,
            p_queue_family_indices: std::ptr::null(),
            initial_layout: VK_IMAGE_LAYOUT_UNDEFINED,
        };

        let mut image: VkImage = 0;
        if unsafe {
            (self.create_image)(self.device, &image_info, std::ptr::null(), &mut image)
        } != VK_SUCCESS
        {
            return None;
        }

        let mut mem_req: VkMemoryRequirements = unsafe { std::mem::zeroed() };
        unsafe { (self.get_image_memory_requirements)(self.device, image, &mut mem_req) };

        let Some(type_index) =
            (unsafe { self.memory_type_index_with(mem_req.memory_type_bits, VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT) })
        else {
            unsafe { (self.destroy_image)(self.device, image, std::ptr::null()) };
            return None;
        };
        let alloc_info = VkMemoryAllocateInfo {
            s_type: VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
            p_next: std::ptr::null(),
            allocation_size: mem_req.size,
            memory_type_index: type_index,
        };
        let mut memory: VkDeviceMemory = 0;
        if unsafe {
            (self.allocate_memory)(self.device, &alloc_info, std::ptr::null(), &mut memory)
        } != VK_SUCCESS
        {
            unsafe { (self.destroy_image)(self.device, image, std::ptr::null()) };
            return None;
        }
        if unsafe { (self.bind_image_memory)(self.device, image, memory, 0) } != VK_SUCCESS {
            unsafe {
                (self.free_memory)(self.device, memory, std::ptr::null());
                (self.destroy_image)(self.device, image, std::ptr::null());
            }
            return None;
        }
        Some((image, memory))
    }

    /// Read back an owned image's pixels into a host RGBA8 buffer (universal
    /// CPU-copy path). The app leaves color images in
    /// `COLOR_ATTACHMENT_OPTIMAL`; this transitions to `TRANSFER_SRC_OPTIMAL`,
    /// copies to a host-visible buffer, waits, and returns the bytes.
    ///
    /// # Safety
    /// `image` must be an owned image of `width`x`height`; queue indices valid.
    pub unsafe fn readback_image(
        &self,
        image: u64,
        width: u32,
        height: u32,
        queue_family_index: u32,
        queue_index: u32,
    ) -> Option<Vec<u8>> {
        type PfnGetDeviceQueue = unsafe extern "system" fn(VkDevice, u32, u32, *mut *mut c_void);
        type PfnCreateCommandPool = unsafe extern "system" fn(
            VkDevice,
            *const VkCommandPoolCreateInfo,
            *const c_void,
            *mut u64,
        ) -> VkResult;
        type PfnDestroyCommandPool = unsafe extern "system" fn(VkDevice, u64, *const c_void);
        type PfnAllocateCommandBuffers = unsafe extern "system" fn(
            VkDevice,
            *const VkCommandBufferAllocateInfo,
            *mut *mut c_void,
        ) -> VkResult;
        type PfnBeginCommandBuffer =
            unsafe extern "system" fn(*mut c_void, *const VkCommandBufferBeginInfo) -> VkResult;
        type PfnEndCommandBuffer = unsafe extern "system" fn(*mut c_void) -> VkResult;
        type PfnCmdPipelineBarrier = unsafe extern "system" fn(
            *mut c_void,
            u32,
            u32,
            u32,
            u32,
            *const c_void,
            u32,
            *const c_void,
            u32,
            *const VkImageMemoryBarrier,
        );
        type PfnCmdCopyImageToBuffer = unsafe extern "system" fn(
            *mut c_void,
            VkImage,
            u32,
            u64,
            u32,
            *const VkBufferImageCopy,
        );
        type PfnQueueSubmit =
            unsafe extern "system" fn(*mut c_void, u32, *const VkSubmitInfo, u64) -> VkResult;
        type PfnQueueWaitIdle = unsafe extern "system" fn(*mut c_void) -> VkResult;
        type PfnCreateBuffer = unsafe extern "system" fn(
            VkDevice,
            *const VkBufferCreateInfo,
            *const c_void,
            *mut u64,
        ) -> VkResult;
        type PfnDestroyBuffer = unsafe extern "system" fn(VkDevice, u64, *const c_void);
        type PfnGetBufferMemoryRequirements =
            unsafe extern "system" fn(VkDevice, u64, *mut VkMemoryRequirements);
        type PfnBindBufferMemory =
            unsafe extern "system" fn(VkDevice, u64, VkDeviceMemory, u64) -> VkResult;
        type PfnMapMemory = unsafe extern "system" fn(
            VkDevice,
            VkDeviceMemory,
            u64,
            u64,
            u32,
            *mut *mut c_void,
        ) -> VkResult;
        type PfnUnmapMemory = unsafe extern "system" fn(VkDevice, VkDeviceMemory);

        macro_rules! f {
            ($name:literal, $ty:ty) => {
                unsafe { std::mem::transmute::<PfnVoid, $ty>(self.dev($name)?) }
            };
        }

        let get_queue = f!(c"vkGetDeviceQueue", PfnGetDeviceQueue);
        let create_pool = f!(c"vkCreateCommandPool", PfnCreateCommandPool);
        let destroy_pool = f!(c"vkDestroyCommandPool", PfnDestroyCommandPool);
        let alloc_cb = f!(c"vkAllocateCommandBuffers", PfnAllocateCommandBuffers);
        let begin_cb = f!(c"vkBeginCommandBuffer", PfnBeginCommandBuffer);
        let end_cb = f!(c"vkEndCommandBuffer", PfnEndCommandBuffer);
        let barrier = f!(c"vkCmdPipelineBarrier", PfnCmdPipelineBarrier);
        let copy_to_buf = f!(c"vkCmdCopyImageToBuffer", PfnCmdCopyImageToBuffer);
        let submit = f!(c"vkQueueSubmit", PfnQueueSubmit);
        let wait_queue = f!(c"vkQueueWaitIdle", PfnQueueWaitIdle);
        let create_buffer = f!(c"vkCreateBuffer", PfnCreateBuffer);
        let destroy_buffer = f!(c"vkDestroyBuffer", PfnDestroyBuffer);
        let buf_mem_req = f!(c"vkGetBufferMemoryRequirements", PfnGetBufferMemoryRequirements);
        let bind_buffer = f!(c"vkBindBufferMemory", PfnBindBufferMemory);
        let map_memory = f!(c"vkMapMemory", PfnMapMemory);
        let unmap_memory = f!(c"vkUnmapMemory", PfnUnmapMemory);

        let byte_size = (width as u64) * (height as u64) * 4;

        // Host-visible staging buffer to receive the copied pixels.
        let buf_info = VkBufferCreateInfo {
            s_type: VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
            p_next: std::ptr::null(),
            flags: 0,
            size: byte_size,
            usage: VK_BUFFER_USAGE_TRANSFER_DST_BIT,
            sharing_mode: VK_SHARING_MODE_EXCLUSIVE,
            queue_family_index_count: 0,
            p_queue_family_indices: std::ptr::null(),
        };
        let mut buffer: u64 = 0;
        if unsafe { create_buffer(self.device, &buf_info, std::ptr::null(), &mut buffer) }
            != VK_SUCCESS
        {
            return None;
        }
        let mut buf_req: VkMemoryRequirements = unsafe { std::mem::zeroed() };
        unsafe { buf_mem_req(self.device, buffer, &mut buf_req) };
        let Some(host_type) = (unsafe {
            self.memory_type_index_with(
                buf_req.memory_type_bits,
                VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT | VK_MEMORY_PROPERTY_HOST_COHERENT_BIT,
            )
        }) else {
            unsafe { destroy_buffer(self.device, buffer, std::ptr::null()) };
            return None;
        };
        let alloc_info = VkMemoryAllocateInfo {
            s_type: VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
            p_next: std::ptr::null(),
            allocation_size: buf_req.size,
            memory_type_index: host_type,
        };
        let mut buf_memory: VkDeviceMemory = 0;
        if unsafe { (self.allocate_memory)(self.device, &alloc_info, std::ptr::null(), &mut buf_memory) }
            != VK_SUCCESS
        {
            unsafe { destroy_buffer(self.device, buffer, std::ptr::null()) };
            return None;
        }
        unsafe { bind_buffer(self.device, buffer, buf_memory, 0) };

        // Record a one-time command buffer: transition -> copy image to buffer.
        let pool_info = VkCommandPoolCreateInfo {
            s_type: VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
            p_next: std::ptr::null(),
            flags: 0,
            queue_family_index,
        };
        let mut pool: u64 = 0;
        let cleanup_buf = |device: VkDevice| unsafe {
            destroy_buffer(device, buffer, std::ptr::null());
            (self.free_memory)(device, buf_memory, std::ptr::null());
        };
        if unsafe { create_pool(self.device, &pool_info, std::ptr::null(), &mut pool) } != VK_SUCCESS
        {
            cleanup_buf(self.device);
            return None;
        }
        let cb_info = VkCommandBufferAllocateInfo {
            s_type: VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
            p_next: std::ptr::null(),
            command_pool: pool,
            level: VK_COMMAND_BUFFER_LEVEL_PRIMARY,
            command_buffer_count: 1,
        };
        let mut cmd: *mut c_void = std::ptr::null_mut();
        if unsafe { alloc_cb(self.device, &cb_info, &mut cmd) } != VK_SUCCESS {
            unsafe { destroy_pool(self.device, pool, std::ptr::null()) };
            cleanup_buf(self.device);
            return None;
        }

        let begin_info = VkCommandBufferBeginInfo {
            s_type: VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
            p_next: std::ptr::null(),
            flags: VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT,
            p_inheritance_info: std::ptr::null(),
        };
        unsafe { begin_cb(cmd, &begin_info) };

        let to_transfer = VkImageMemoryBarrier {
            s_type: VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
            p_next: std::ptr::null(),
            src_access_mask: VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT,
            dst_access_mask: VK_ACCESS_TRANSFER_READ_BIT,
            old_layout: VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL,
            new_layout: VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL,
            src_queue_family_index: VK_QUEUE_FAMILY_IGNORED,
            dst_queue_family_index: VK_QUEUE_FAMILY_IGNORED,
            image,
            subresource_range: VkImageSubresourceRange {
                aspect_mask: VK_IMAGE_ASPECT_COLOR_BIT,
                base_mip_level: 0,
                level_count: 1,
                base_array_layer: 0,
                layer_count: 1,
            },
        };
        unsafe {
            barrier(
                cmd,
                VK_PIPELINE_STAGE_ALL_COMMANDS_BIT,
                VK_PIPELINE_STAGE_TRANSFER_BIT,
                0,
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                1,
                &to_transfer,
            );
        }

        let region = VkBufferImageCopy {
            buffer_offset: 0,
            buffer_row_length: 0,
            buffer_image_height: 0,
            image_subresource: VkImageSubresourceLayers {
                aspect_mask: VK_IMAGE_ASPECT_COLOR_BIT,
                mip_level: 0,
                base_array_layer: 0,
                layer_count: 1,
            },
            image_offset: VkOffset3D { x: 0, y: 0, z: 0 },
            image_extent: VkExtent3D {
                width,
                height,
                depth: 1,
            },
        };
        unsafe {
            copy_to_buf(
                cmd,
                image,
                VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL,
                buffer,
                1,
                &region,
            );
        }
        unsafe { end_cb(cmd) };

        let mut queue: *mut c_void = std::ptr::null_mut();
        unsafe { get_queue(self.device, queue_family_index, queue_index, &mut queue) };
        let submit_info = VkSubmitInfo {
            s_type: VK_STRUCTURE_TYPE_SUBMIT_INFO,
            p_next: std::ptr::null(),
            wait_semaphore_count: 0,
            p_wait_semaphores: std::ptr::null(),
            p_wait_dst_stage_mask: std::ptr::null(),
            command_buffer_count: 1,
            p_command_buffers: &cmd,
            signal_semaphore_count: 0,
            p_signal_semaphores: std::ptr::null(),
        };
        let ok = unsafe { submit(queue, 1, &submit_info, 0) } == VK_SUCCESS;
        unsafe { wait_queue(queue) };

        let mut out = None;
        if ok {
            let mut mapped: *mut c_void = std::ptr::null_mut();
            if unsafe { map_memory(self.device, buf_memory, 0, byte_size, 0, &mut mapped) }
                == VK_SUCCESS
                && !mapped.is_null()
            {
                let mut buf = vec![0u8; byte_size as usize];
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        mapped as *const u8,
                        buf.as_mut_ptr(),
                        byte_size as usize,
                    );
                    unmap_memory(self.device, buf_memory);
                }
                out = Some(buf);
            }
        }

        unsafe {
            destroy_pool(self.device, pool, std::ptr::null());
        }
        cleanup_buf(self.device);
        out
    }

    /// Pick a memory type index satisfying `type_bits` and all of `flags`.
    unsafe fn memory_type_index_with(&self, type_bits: u32, flags: u32) -> Option<u32> {
        let mut props: VkPhysicalDeviceMemoryProperties = unsafe { std::mem::zeroed() };
        unsafe { (self.get_physical_device_memory_properties)(self.physical_device, &mut props) };
        for i in 0..props.memory_type_count {
            if type_bits & (1 << i) == 0 {
                continue;
            }
            if props.memory_types[i as usize].property_flags & flags == flags {
                return Some(i);
            }
        }
        None
    }

    /// Destroy an imported image and free its imported memory.
    ///
    /// # Safety
    /// `image`/`memory` must come from [`import_shared_texture`] on this device
    /// and not have been destroyed already.
    pub unsafe fn destroy_image(&self, image: u64, memory: u64) {
        unsafe {
            if image != 0 {
                (self.destroy_image)(self.device, image, std::ptr::null());
            }
            if memory != 0 {
                (self.free_memory)(self.device, memory, std::ptr::null());
            }
        }
    }
}

/// Enumerate the physical devices of an app-provided `VkInstance` and return the
/// one whose device LUID matches `target_luid` (the D3D11 adapter the encoder
/// textures live on), so external-memory sharing targets the correct GPU.
/// Falls back to the first physical device when no LUID matches.
///
/// # Safety
/// `instance` must be a valid live `VkInstance`.
pub unsafe fn physical_device_for_luid(instance: usize, target_luid: [u8; 8]) -> Option<usize> {
    let gipa = get_instance_proc_addr()?;
    let instance = instance as VkInstance;

    let enumerate: PfnEnumeratePhysicalDevices = unsafe {
        std::mem::transmute(instance_proc(gipa, instance, c"vkEnumeratePhysicalDevices")?)
    };
    let get_props2: PfnGetPhysicalDeviceProperties2 = unsafe {
        std::mem::transmute(instance_proc(
            gipa,
            instance,
            c"vkGetPhysicalDeviceProperties2",
        )?)
    };

    let mut count = 0u32;
    if unsafe { enumerate(instance, &mut count, std::ptr::null_mut()) } != VK_SUCCESS || count == 0 {
        return None;
    }
    let mut devices = vec![std::ptr::null_mut::<c_void>(); count as usize];
    if unsafe { enumerate(instance, &mut count, devices.as_mut_ptr()) } != VK_SUCCESS {
        return None;
    }

    let first = devices.first().copied();
    for &device in &devices {
        let mut id_props: VkPhysicalDeviceIDProperties = unsafe { std::mem::zeroed() };
        id_props.s_type = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_ID_PROPERTIES;
        let mut props2: VkPhysicalDeviceProperties2 = unsafe { std::mem::zeroed() };
        props2.s_type = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_PROPERTIES_2;
        props2.p_next = (&mut id_props as *mut VkPhysicalDeviceIDProperties).cast();

        unsafe { get_props2(device, &mut props2) };

        if id_props.device_luid_valid != 0 && id_props.device_luid == target_luid {
            return Some(device as usize);
        }
    }

    first.map(|d| d as usize)
}

/// The DXGI adapter LUID (as the 8-byte little-endian layout Vulkan reports) for
/// the default adapter the encoder textures are created on.
pub fn default_adapter_luid_bytes() -> Option<[u8; 8]> {
    let (low, high) = crate::d3d11::default_adapter_luid()?;
    let mut bytes = [0u8; 8];
    bytes[0..4].copy_from_slice(&low.to_ne_bytes());
    bytes[4..8].copy_from_slice(&high.to_ne_bytes());
    Some(bytes)
}

// --- Required extensions for D3D11<->Vulkan external-memory interop ----------

/// Instance extensions the app must enable for external-memory import.
pub const REQUIRED_INSTANCE_EXTENSIONS: &[&CStr] = &[
    c"VK_KHR_external_memory_capabilities",
    c"VK_KHR_get_physical_device_properties2",
];

/// Device extensions the app must enable for external-memory import.
pub const REQUIRED_DEVICE_EXTENSIONS: &[&CStr] = &[
    c"VK_KHR_external_memory",
    c"VK_KHR_external_memory_win32",
    c"VK_KHR_dedicated_allocation",
    c"VK_KHR_get_memory_requirements2",
];

// --- enable2 instance/device creation --------------------------------------

const VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO: u32 = 1;
const VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO: u32 = 3;

/// The `VkInstance` created via `xrCreateVulkanInstanceKHR`. `vkCreateDevice` is
/// an instance-level command that can only be resolved with a valid instance
/// handle (a NULL instance resolves only the global commands), and the
/// `XrVulkanDeviceCreateInfoKHR` the app hands to `xrCreateVulkanDeviceKHR`
/// carries no instance, so we remember it here between the two calls.
static CREATED_INSTANCE: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

#[repr(C)]
struct VkInstanceCreateInfo {
    s_type: u32,
    p_next: *const c_void,
    flags: u32,
    p_application_info: *const c_void,
    enabled_layer_count: u32,
    pp_enabled_layer_names: *const *const c_char,
    enabled_extension_count: u32,
    pp_enabled_extension_names: *const *const c_char,
}

#[repr(C)]
struct VkDeviceCreateInfo {
    s_type: u32,
    p_next: *const c_void,
    flags: u32,
    queue_create_info_count: u32,
    p_queue_create_infos: *const c_void,
    enabled_layer_count: u32,
    pp_enabled_layer_names: *const *const c_char,
    enabled_extension_count: u32,
    pp_enabled_extension_names: *const *const c_char,
    p_enabled_features: *const c_void,
}

type PfnCreateInstance = unsafe extern "system" fn(
    *const VkInstanceCreateInfo,
    *const c_void,
    *mut VkInstance,
) -> VkResult;
type PfnCreateDevice = unsafe extern "system" fn(
    VkPhysicalDevice,
    *const VkDeviceCreateInfo,
    *const c_void,
    *mut VkDevice,
) -> VkResult;

/// Merge `required` C-string extension names into the app-supplied list,
/// skipping duplicates. Returns the merged pointer vector (kept alive by the
/// caller) so the pointers stay valid across the create call.
unsafe fn merge_extensions(
    existing: *const *const c_char,
    existing_count: u32,
    required: &[&CStr],
) -> Vec<*const c_char> {
    let mut names: Vec<*const c_char> = Vec::new();
    let mut seen: Vec<Vec<u8>> = Vec::new();
    for i in 0..existing_count as usize {
        let ptr = unsafe { *existing.add(i) };
        if !ptr.is_null() {
            seen.push(unsafe { CStr::from_ptr(ptr) }.to_bytes().to_vec());
            names.push(ptr);
        }
    }
    for ext in required {
        let bytes = ext.to_bytes().to_vec();
        if !seen.iter().any(|s| s == &bytes) {
            names.push(ext.as_ptr());
        }
    }
    names
}

/// Create a `VkInstance` on the app's behalf, injecting the interop instance
/// extensions (implements `xrCreateVulkanInstanceKHR`).
///
/// # Safety
/// `gipa` is a valid `vkGetInstanceProcAddr`; `app_create_info` a valid
/// `VkInstanceCreateInfo`; `out_instance`/`out_result` valid out-params.
pub unsafe fn create_instance(
    gipa: usize,
    app_create_info: *const c_void,
    allocator: *const c_void,
    out_instance: *mut usize,
    out_result: *mut i32,
) -> bool {
    let gipa: PfnGetInstanceProcAddr = unsafe { std::mem::transmute(gipa) };
    let Some(create_fn) =
        (unsafe { instance_proc(gipa, std::ptr::null_mut(), c"vkCreateInstance") })
    else {
        return false;
    };
    let create_instance: PfnCreateInstance = unsafe { std::mem::transmute(create_fn) };

    let app_info = unsafe { &*(app_create_info as *const VkInstanceCreateInfo) };
    let merged = unsafe {
        merge_extensions(
            app_info.pp_enabled_extension_names,
            app_info.enabled_extension_count,
            REQUIRED_INSTANCE_EXTENSIONS,
        )
    };
    let info = VkInstanceCreateInfo {
        s_type: VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
        p_next: app_info.p_next,
        flags: app_info.flags,
        p_application_info: app_info.p_application_info,
        enabled_layer_count: app_info.enabled_layer_count,
        pp_enabled_layer_names: app_info.pp_enabled_layer_names,
        enabled_extension_count: merged.len() as u32,
        pp_enabled_extension_names: merged.as_ptr(),
    };

    let mut instance: VkInstance = std::ptr::null_mut();
    let result = unsafe { create_instance(&info, allocator, &mut instance) };
    unsafe {
        *out_result = result;
        *out_instance = instance as usize;
    }
    if result == VK_SUCCESS {
        CREATED_INSTANCE.store(instance as usize, std::sync::atomic::Ordering::Relaxed);
    }
    result == VK_SUCCESS
}

/// Create a `VkDevice` on the app's behalf, injecting the interop device
/// extensions (implements `xrCreateVulkanDeviceKHR`).
///
/// # Safety
/// As [`create_instance`], plus `physical_device` a valid `VkPhysicalDevice`.
pub unsafe fn create_device(
    gipa: usize,
    physical_device: usize,
    app_create_info: *const c_void,
    allocator: *const c_void,
    out_device: *mut usize,
    out_result: *mut i32,
) -> bool {
    let gipa: PfnGetInstanceProcAddr = unsafe { std::mem::transmute(gipa) };
    // vkCreateDevice is instance-level: resolve it against the instance created
    // in xrCreateVulkanInstanceKHR (falls back to the app-provided instance if
    // set later). A NULL instance would only resolve global commands.
    let instance = CREATED_INSTANCE.load(std::sync::atomic::Ordering::Relaxed) as VkInstance;
    let Some(create_fn) = (unsafe { instance_proc(gipa, instance, c"vkCreateDevice") }) else {
        return false;
    };
    let create_device: PfnCreateDevice = unsafe { std::mem::transmute(create_fn) };

    let app_info = unsafe { &*(app_create_info as *const VkDeviceCreateInfo) };
    let merged = unsafe {
        merge_extensions(
            app_info.pp_enabled_extension_names,
            app_info.enabled_extension_count,
            REQUIRED_DEVICE_EXTENSIONS,
        )
    };
    let info = VkDeviceCreateInfo {
        s_type: VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
        p_next: app_info.p_next,
        flags: app_info.flags,
        queue_create_info_count: app_info.queue_create_info_count,
        p_queue_create_infos: app_info.p_queue_create_infos,
        enabled_layer_count: app_info.enabled_layer_count,
        pp_enabled_layer_names: app_info.pp_enabled_layer_names,
        enabled_extension_count: merged.len() as u32,
        pp_enabled_extension_names: merged.as_ptr(),
        p_enabled_features: app_info.p_enabled_features,
    };

    let mut device: VkDevice = std::ptr::null_mut();
    let result = unsafe { create_device(physical_device as VkPhysicalDevice, &info, allocator, &mut device) };
    unsafe {
        *out_result = result;
        *out_device = device as usize;
    }
    result == VK_SUCCESS
}
