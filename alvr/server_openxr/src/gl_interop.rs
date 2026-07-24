//! OpenGL swapchain support via `WGL_NV_DX_interop2`.
//!
//! An `XR_KHR_opengl_enable` app renders into GL texture names. The ALVR encoder
//! consumes D3D11 textures, so each GL swapchain image is a GL texture *aliased*
//! onto a shared D3D11 texture through WGL_NV_DX_interop2 (blueprint Q6). Between
//! `xrAcquireSwapchainImage`/`xrReleaseSwapchainImage` the app owns the GL alias;
//! the runtime locks the D3D11 side before handing it to the encoder.
//!
//! Interop is NVIDIA-strongest and honored by most AMD drivers; on drivers that
//! reject it the caller falls back to a CPU copy (blueprint Q6 option (i)). That
//! fallback path lands with the frame loop.

#![allow(non_snake_case, non_upper_case_globals)]

use std::ffi::{c_void, CString};
use windows::core::PCSTR;
use windows::Win32::Graphics::OpenGL::wglGetProcAddress;

// WGL_NV_DX_interop access flags.
const WGL_ACCESS_READ_WRITE_NV: u32 = 0x0001;
// GL texture target for 2D textures.
const GL_TEXTURE_2D: u32 = 0x0DE1;

type PfnWglDXOpenDeviceNV = unsafe extern "system" fn(dx_device: *mut c_void) -> *mut c_void;
type PfnWglDXRegisterObjectNV = unsafe extern "system" fn(
    device: *mut c_void,
    dx_object: *mut c_void,
    name: u32,
    gl_type: u32,
    access: u32,
) -> *mut c_void;
type PfnWglDXLockObjectsNV =
    unsafe extern "system" fn(device: *mut c_void, count: i32, objects: *mut *mut c_void) -> i32;
type PfnWglDXUnlockObjectsNV =
    unsafe extern "system" fn(device: *mut c_void, count: i32, objects: *mut *mut c_void) -> i32;
type PfnGlGenTextures = unsafe extern "system" fn(n: i32, textures: *mut u32);
type PfnGlBindTexture = unsafe extern "system" fn(target: u32, texture: u32);
type PfnGlTexImage2D = unsafe extern "system" fn(
    target: u32,
    level: i32,
    internalformat: i32,
    width: i32,
    height: i32,
    border: i32,
    format: u32,
    ty: u32,
    pixels: *const c_void,
);
type PfnGlGetTexImage =
    unsafe extern "system" fn(target: u32, level: i32, format: u32, ty: u32, pixels: *mut c_void);
type PfnGlTexParameteri = unsafe extern "system" fn(target: u32, pname: u32, param: i32);
type PfnGlFinish = unsafe extern "system" fn();

// GL enums for the CPU-copy fallback.
const GL_RGBA: u32 = 0x1908;
const GL_UNSIGNED_BYTE: u32 = 0x1401;
const GL_TEXTURE_MIN_FILTER: u32 = 0x2801;
const GL_TEXTURE_MAG_FILTER: u32 = 0x2800;
const GL_LINEAR: i32 = 0x2601;

/// Core-GL entry points for the universal CPU-copy OpenGL fallback (used when
/// `WGL_NV_DX_interop2` is unavailable). Standalone GL textures back the
/// swapchain; their pixels are read back and uploaded to D3D11 at submit time.
pub struct GlCopyFns {
    gen_textures: PfnGlGenTextures,
    bind_texture: PfnGlBindTexture,
    tex_image_2d: PfnGlTexImage2D,
    get_tex_image: PfnGlGetTexImage,
    tex_parameteri: PfnGlTexParameteri,
}

impl GlCopyFns {
    /// Resolve the core-GL entry points from opengl32.dll. Always available on
    /// any OpenGL context (GL 1.1+), so this works on every GPU/driver.
    ///
    /// # Safety
    /// A GL context must be current on the calling thread.
    pub unsafe fn load() -> Option<Self> {
        unsafe {
            Some(Self {
                gen_textures: std::mem::transmute::<_, PfnGlGenTextures>(load_gl("glGenTextures")?),
                bind_texture: std::mem::transmute::<_, PfnGlBindTexture>(load_gl("glBindTexture")?),
                tex_image_2d: std::mem::transmute::<_, PfnGlTexImage2D>(load_gl("glTexImage2D")?),
                get_tex_image: std::mem::transmute::<_, PfnGlGetTexImage>(load_gl(
                    "glGetTexImage",
                )?),
                tex_parameteri: std::mem::transmute::<_, PfnGlTexParameteri>(load_gl(
                    "glTexParameteri",
                )?),
            })
        }
    }

    /// Create a standalone RGBA8 GL texture of the given size the app can render
    /// into. Returns the GL texture name.
    ///
    /// # Safety
    /// A GL context must be current on the calling thread.
    pub unsafe fn create_texture(
        &self,
        width: u32,
        height: u32,
        internal_format: i32,
    ) -> Option<u32> {
        let mut name: u32 = 0;
        unsafe {
            (self.gen_textures)(1, &mut name);
            if name == 0 {
                return None;
            }
            (self.bind_texture)(GL_TEXTURE_2D, name);
            (self.tex_parameteri)(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_LINEAR);
            (self.tex_parameteri)(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_LINEAR);
            (self.tex_image_2d)(
                GL_TEXTURE_2D,
                0,
                internal_format,
                width as i32,
                height as i32,
                0,
                GL_RGBA,
                GL_UNSIGNED_BYTE,
                std::ptr::null(),
            );
            (self.bind_texture)(GL_TEXTURE_2D, 0);
        }
        Some(name)
    }

    /// Read back an RGBA8 GL texture's pixels into `out` (must be width*height*4).
    ///
    /// # Safety
    /// A GL context must be current; `gl_name` a valid texture; `out` sized.
    pub unsafe fn read_pixels(&self, gl_name: u32, out: &mut [u8]) {
        unsafe {
            (self.bind_texture)(GL_TEXTURE_2D, gl_name);
            (self.get_tex_image)(
                GL_TEXTURE_2D,
                0,
                GL_RGBA,
                GL_UNSIGNED_BYTE,
                out.as_mut_ptr().cast(),
            );
            (self.bind_texture)(GL_TEXTURE_2D, 0);
        }
    }
}

/// Resolved WGL/GL interop entry points. Valid only while the app's GL context
/// is current on the calling thread.
pub struct InteropFns {
    open_device: PfnWglDXOpenDeviceNV,
    register_object: PfnWglDXRegisterObjectNV,
    lock_objects: PfnWglDXLockObjectsNV,
    unlock_objects: PfnWglDXUnlockObjectsNV,
    gen_textures: PfnGlGenTextures,
    finish: PfnGlFinish,
}

/// Load a WGL extension function by name via `wglGetProcAddress`.
///
/// # Safety
/// A GL context must be current on the calling thread.
unsafe fn load(name: &str) -> Option<*const c_void> {
    let cname = CString::new(name).ok()?;
    let proc = unsafe { wglGetProcAddress(PCSTR(cname.as_ptr().cast())) };
    match proc {
        Some(p) => Some(p as *const c_void),
        None => {
            eprintln!("[alvr] gl_interop: wglGetProcAddress('{name}') returned null");
            None
        }
    }
}

impl InteropFns {
    /// Resolve all interop entry points. Returns `None` if the driver does not
    /// expose `WGL_NV_DX_interop2` (caller then uses the CPU-copy fallback).
    ///
    /// # Safety
    /// The app's GL context must be current on the calling thread.
    pub unsafe fn load() -> Option<Self> {
        unsafe {
            Some(Self {
                open_device: std::mem::transmute::<_, PfnWglDXOpenDeviceNV>(load(
                    "wglDXOpenDeviceNV",
                )?),
                register_object: std::mem::transmute::<_, PfnWglDXRegisterObjectNV>(load(
                    "wglDXRegisterObjectNV",
                )?),
                lock_objects: std::mem::transmute::<_, PfnWglDXLockObjectsNV>(load(
                    "wglDXLockObjectsNV",
                )?),
                unlock_objects: std::mem::transmute::<_, PfnWglDXUnlockObjectsNV>(load(
                    "wglDXUnlockObjectsNV",
                )?),
                // Core GL 1.1 export lives in opengl32.dll; resolve via the OS loader.
                gen_textures: std::mem::transmute::<_, PfnGlGenTextures>(load_gl("glGenTextures")?),
                finish: std::mem::transmute::<_, PfnGlFinish>(load_gl("glFinish")?),
            })
        }
    }

    /// Open an interop device handle over the runtime's D3D11 device.
    ///
    /// # Safety
    /// `d3d11_device` must be a valid `ID3D11Device` pointer.
    pub unsafe fn open_device(&self, d3d11_device: usize) -> *mut c_void {
        let dev = unsafe { (self.open_device)(d3d11_device as *mut c_void) };
        if dev.is_null() {
            let err = unsafe { windows::Win32::Foundation::GetLastError() };
            eprintln!(
                "[alvr] gl_interop: wglDXOpenDeviceNV failed (GetLastError=0x{:08x})",
                err.0
            );
        }
        dev
    }

    /// Create a GL texture name aliased onto a shared D3D11 texture.
    ///
    /// Returns `(gl_name, interop_handle)`. The handle is passed to
    /// `lock`/`unlock` around encoder access.
    ///
    /// # Safety
    /// `interop_device` from [`open_device`]; `d3d11_texture` a valid texture.
    pub unsafe fn register_texture(
        &self,
        interop_device: *mut c_void,
        d3d11_texture: usize,
    ) -> Option<(u32, *mut c_void)> {
        let mut gl_name: u32 = 0;
        unsafe { (self.gen_textures)(1, &mut gl_name) };
        if gl_name == 0 {
            eprintln!("[alvr] gl_interop: glGenTextures returned 0");
            return None;
        }
        let handle = unsafe {
            (self.register_object)(
                interop_device,
                d3d11_texture as *mut c_void,
                gl_name,
                GL_TEXTURE_2D,
                WGL_ACCESS_READ_WRITE_NV,
            )
        };
        if handle.is_null() {
            let err = unsafe { windows::Win32::Foundation::GetLastError() };
            eprintln!(
                "[alvr] gl_interop: wglDXRegisterObjectNV failed (GetLastError=0x{:08x})",
                err.0
            );
            return None;
        }
        Some((gl_name, handle))
    }

    /// Lock a registered object so the D3D11 side can be read by the encoder.
    ///
    /// # Safety
    /// `device` and `handle` from this interop session.
    pub unsafe fn lock(&self, device: *mut c_void, handle: &mut *mut c_void) -> bool {
        unsafe { (self.lock_objects)(device, 1, handle) != 0 }
    }

    /// Unlock a registered object, returning it to the app's GL side.
    ///
    /// # Safety
    /// `device` and `handle` from this interop session.
    pub unsafe fn unlock(&self, device: *mut c_void, handle: &mut *mut c_void) -> bool {
        unsafe { (self.unlock_objects)(device, 1, handle) != 0 }
    }

    /// Complete all GL commands issued for the current context before handing a
    /// texture to D3D11 through WGL interop.
    ///
    /// # Safety
    /// A GL context must be current on the calling thread.
    pub unsafe fn finish(&self) {
        unsafe { (self.finish)() };
    }
}

/// Resolve a core GL function from opengl32.dll (not all are reachable via
/// `wglGetProcAddress` across drivers).
///
/// # Safety
/// A GL context must be current on the calling thread.
unsafe fn load_gl(name: &str) -> Option<*const c_void> {
    use windows::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};
    let module = unsafe { GetModuleHandleA(PCSTR(c"opengl32.dll".as_ptr().cast())) }.ok()?;
    let cname = CString::new(name).ok()?;
    let proc = unsafe { GetProcAddress(module, PCSTR(cname.as_ptr().cast())) };
    proc.map(|p| p as *const c_void)
}
