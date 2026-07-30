//! D3D11 resource creation for the OpenXR swapchain path.
//!
//! The app supplies its own `ID3D11Device` through `GraphicsBindingD3D11KHR`;
//! the runtime creates swapchain textures on that device and hands the app the
//! `ID3D11Texture2D` pointers. Those same textures are later fed to the reused
//! C++ `CEncoder` (blueprint step 4, increment 4).

use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11Texture2D, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE,
    D3D11_RESOURCE_MISC_SHARED, D3D11_RESOURCE_MISC_SHARED_NTHANDLE, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::IDXGIResource;

/// Number of images per swapchain. Double-buffering is enough for the encoder
/// pipeline; the app round-robins through them.
pub const SWAPCHAIN_IMAGE_COUNT: usize = 3;

/// Return the LUID of the default (first) DXGI hardware adapter as `(low, high)`.
///
/// OpenXR apps use this to pick the D3D11 adapter matching the runtime. Some
/// apps (e.g. hello_xr) strictly call `EnumAdapterByLuid` and fail with
/// `DXGI_ERROR_NOT_FOUND` if the runtime reports a zero LUID, so we must return
/// a real one.
pub fn default_adapter_luid() -> Option<(u32, i32)> {
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1};

    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().ok()?;
        let adapter = factory.EnumAdapters(0).ok()?;
        let desc = adapter.GetDesc().ok()?;
        Some((desc.AdapterLuid.LowPart, desc.AdapterLuid.HighPart))
    }
}

/// Create a runtime-owned D3D11 device (used for OpenGL sessions, where the app
/// supplies only a GL context). Returns the raw `ID3D11Device` pointer as usize.
///
/// # Safety
/// Calls into D3D11; no external invariants, but returns a leaked COM ref that
/// lives for the process (single device).
pub unsafe fn create_device() -> Result<usize, windows::core::Error> {
    use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0};
    use windows::Win32::Graphics::Direct3D11::{
        D3D11CreateDevice, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION,
    };

    let mut device: Option<ID3D11Device> = None;
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            windows::Win32::Foundation::HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            None,
        )?
    };
    let device = device.ok_or_else(windows::core::Error::from_thread)?;
    Ok(device.into_raw() as usize)
}

/// Create `SWAPCHAIN_IMAGE_COUNT` textures on the app's device.
///
/// Returns the raw `ID3D11Texture2D` pointers (as usize). Ownership is held by
/// the returned COM references being forgotten into raw pointers; they are
/// released in [`release_textures`].
///
/// # Safety
/// `device_ptr` must be a valid `*mut ID3D11Device` for the lifetime of the call.
pub unsafe fn create_swapchain_textures(
    device_ptr: usize,
    format: i64,
    width: u32,
    height: u32,
    array_size: u32,
    sample_count: u32,
) -> Result<Vec<usize>, windows::core::Error> {
    unsafe { create_swapchain_textures_impl(device_ptr, format, width, height, array_size, sample_count, false) }
}

/// Like [`create_swapchain_textures`] but creates the textures with
/// `D3D11_RESOURCE_MISC_SHARED_NTHANDLE` so they can be shared via a modern NT
/// handle (`IDXGIResource1::CreateSharedHandle`). Vulkan external-memory import
/// requires the NT handle type; the legacy KMT handle from `GetSharedHandle`
/// is rejected by Vulkan drivers with `VK_ERROR_INVALID_EXTERNAL_HANDLE`.
///
/// # Safety
/// As [`create_swapchain_textures`].
pub unsafe fn create_swapchain_textures_nt(
    device_ptr: usize,
    format: i64,
    width: u32,
    height: u32,
    array_size: u32,
    sample_count: u32,
) -> Result<Vec<usize>, windows::core::Error> {
    unsafe { create_swapchain_textures_impl(device_ptr, format, width, height, array_size, sample_count, true) }
}

unsafe fn create_swapchain_textures_impl(
    device_ptr: usize,
    format: i64,
    width: u32,
    height: u32,
    array_size: u32,
    sample_count: u32,
    nt_handle: bool,
) -> Result<Vec<usize>, windows::core::Error> {
    // # Safety: caller guarantees a valid device pointer; we borrow without
    // taking ownership.
    let device_raw = device_ptr as *mut _;
    let device = unsafe { ID3D11Device::from_raw_borrowed(&device_raw) }
        .ok_or_else(windows::core::Error::from_thread)?;

    let misc_flags = if nt_handle {
        // NT-shared (CreateSharedHandle) — the form Vulkan external-memory
        // import accepts on AMD/Intel. NVIDIA rejects it at vkBindImageMemory,
        // so the Vulkan path falls back to a CPU copy there (universal).
        D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0 | D3D11_RESOURCE_MISC_SHARED.0
    } else {
        D3D11_RESOURCE_MISC_SHARED.0
    };

    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: array_size.max(1),
        Format: DXGI_FORMAT(format as i32),
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: sample_count.max(1),
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
        CPUAccessFlags: 0,
        // Shared so the encoder's separate D3D11 device can open these textures
        // by handle (ALVR's cross-device design).
        MiscFlags: misc_flags as u32,
    };

    let mut textures = Vec::with_capacity(SWAPCHAIN_IMAGE_COUNT);
    for _ in 0..SWAPCHAIN_IMAGE_COUNT {
        let mut texture: Option<ID3D11Texture2D> = None;
        // # Safety: desc is valid; out-param initialized on success.
        unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture))? };
        let texture = texture.ok_or_else(windows::core::Error::from_thread)?;
        // Leak the COM ref into a raw pointer the app/encoder will use.
        textures.push(texture.into_raw() as usize);
    }
    Ok(textures)
}

/// Create an NT shared handle for a texture created with
/// `D3D11_RESOURCE_MISC_SHARED_NTHANDLE`, usable both by Vulkan external-memory
/// import and by the encoder's `OpenSharedResource1`.
///
/// # Safety
/// `texture_ptr` must be a valid NT-shareable `ID3D11Texture2D`.
pub unsafe fn create_nt_shared_handle(texture_ptr: usize) -> Option<usize> {
    use windows::Win32::Graphics::Dxgi::IDXGIResource1;

    // DXGI_SHARED_RESOURCE_READ | DXGI_SHARED_RESOURCE_WRITE
    const DXGI_SHARED_RESOURCE_READ: u32 = 0x8000_0000;
    const DXGI_SHARED_RESOURCE_WRITE: u32 = 0x0000_0001;

    let texture_raw = texture_ptr as *mut _;
    let texture = unsafe { ID3D11Texture2D::from_raw_borrowed(&texture_raw) }?;
    let resource: IDXGIResource1 = texture.cast().ok()?;
    let handle = unsafe {
        resource.CreateSharedHandle(
            None,
            DXGI_SHARED_RESOURCE_READ | DXGI_SHARED_RESOURCE_WRITE,
            windows::core::PCWSTR::null(),
        )
    }
    .ok()?;
    Some(handle.0 as usize)
}

/// Close an NT handle created by [`create_nt_shared_handle`].
///
/// # Safety
/// `handle` must be a valid NT handle not yet closed.
pub unsafe fn close_handle(handle: usize) {
    if handle != 0 {
        let _ = unsafe {
            windows::Win32::Foundation::CloseHandle(windows::Win32::Foundation::HANDLE(
                handle as *mut _,
            ))
        };
    }
}

/// Release textures previously created by [`create_swapchain_textures`].
///
/// # Safety
/// Each pointer must have come from `create_swapchain_textures` and not been
/// released already.
pub unsafe fn release_textures(textures: &[usize]) {
    for &ptr in textures {
        if ptr != 0 {
            // # Safety: reconstitute the leaked COM ref so it drops (Release).
            drop(unsafe { ID3D11Texture2D::from_raw(ptr as *mut _) });
        }
    }
}

/// Submit pending commands on the app-provided D3D11 device before another
/// device opens and reads its shared swapchain texture.
///
/// # Safety
/// `device_ptr` must be a valid `ID3D11Device`.
pub unsafe fn flush_device(device_ptr: usize) {
    let device_raw = device_ptr as *mut _;
    let Some(device) = (unsafe { ID3D11Device::from_raw_borrowed(&device_raw) }) else {
        return;
    };
    if let Ok(context) = unsafe { device.GetImmediateContext() } {
        unsafe { context.Flush() };
    }
}

/// Get the DXGI shared handle for a texture, so the encoder's device can open
/// it via `CD3DRender::GetSharedTexture`.
///
/// # Safety
/// `texture_ptr` must be a valid shared `ID3D11Texture2D`.
pub unsafe fn shared_handle(texture_ptr: usize) -> Option<usize> {
    let texture_raw = texture_ptr as *mut _;
    let texture = unsafe { ID3D11Texture2D::from_raw_borrowed(&texture_raw) }?;
    let resource: IDXGIResource = texture.cast().ok()?;
    let handle = unsafe { resource.GetSharedHandle() }.ok()?;
    Some(handle.0 as usize)
}

/// Upload CPU-side RGBA8 pixels into a D3D11 texture via the device's immediate
/// context (used by the universal OpenGL CPU-copy fallback).
///
/// # Safety
/// `device_ptr`/`texture_ptr` valid; `data` at least `width*height*4` bytes.
pub unsafe fn update_texture_rgba(device_ptr: usize, texture_ptr: usize, width: u32, data: &[u8]) {
    let device_raw = device_ptr as *mut _;
    let Some(device) = (unsafe { ID3D11Device::from_raw_borrowed(&device_raw) }) else {
        return;
    };
    let texture_raw = texture_ptr as *mut _;
    let Some(texture) = (unsafe { ID3D11Texture2D::from_raw_borrowed(&texture_raw) }) else {
        return;
    };
    let resource: windows::Win32::Graphics::Direct3D11::ID3D11Resource = match texture.cast() {
        Ok(r) => r,
        Err(_) => return,
    };

    let context = match unsafe { device.GetImmediateContext() } {
        Ok(c) => c,
        Err(_) => return,
    };
    let row_pitch = width * 4;
    unsafe {
        context.UpdateSubresource(&resource, 0, None, data.as_ptr().cast(), row_pitch, 0);
    }
}
