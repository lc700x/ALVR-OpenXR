//! ALVR OpenXR runtime — a headless OpenXR runtime that streams an OpenXR app
//! from the PC to an ALVR client, with no SteamVR/OpenVR involved.
//!
//! This crate is loaded directly by the OpenXR loader as the active runtime
//! (see [`loader`]). The app's `xrEndFrame` swapchain images are handed to the
//! ALVR video encoder (reused in place from `server_openvr`) and streamed via
//! `alvr_server_core`.
//!
//! Status: skeleton. The loader negotiation entry point and the encoder FFI
//! bridge are in place; the `xr*` dispatch table and frame loop land later.

pub mod ipc;
mod loader;

pub use loader::xrNegotiateLoaderRuntimeInterface;

// The C++ encoder / FrameRender pipeline is Windows-only for now (blueprint Q3).
#[cfg(windows)]
mod actions;
#[cfg(windows)]
mod bindings;
#[cfg(windows)]
mod d3d11;
#[cfg(windows)]
mod encoder_ffi;
#[cfg(windows)]
mod gl_interop;
#[cfg(windows)]
mod graphics;
#[cfg(windows)]
pub mod registration;
#[cfg(windows)]
mod runtime;
#[cfg(windows)]
mod state;
#[cfg(windows)]
mod streaming;
#[cfg(windows)]
mod vulkan;

#[cfg(windows)]
pub use graphics::initialize_shaders;

/// Register ALVR as the active OpenXR runtime (hijacks `HKLM` ActiveRuntime).
/// Returns 0 on success. Requires administrator rights. Callable by the
/// installer / dashboard.
#[cfg(windows)]
#[unsafe(no_mangle)]
pub extern "C" fn alvr_openxr_register_runtime() -> i32 {
    let path = registration::default_dll_path();
    match registration::register(&path) {
        Ok(()) => 0,
        Err(_) => -1,
    }
}

/// Restore the previously-active OpenXR runtime. Returns 0 on success.
#[cfg(windows)]
#[unsafe(no_mangle)]
pub extern "C" fn alvr_openxr_unregister_runtime() -> i32 {
    match registration::unregister() {
        Ok(()) => 0,
        Err(_) => -1,
    }
}
