//! extern-C shims the reused C++ encoder / FrameRender translation units call
//! back into. These mirror the symbols normally provided by `server_openvr`,
//! but route the encoded video into `alvr_server_core` with no SteamVR driver.

use crate::bindings::FfiDynamicEncoderParams;
use crate::ipc::RuntimeEvent;
use alvr_common::parking_lot::RwLock;
use std::{
    ffi::{c_char, CStr},
    ptr,
};

static DYNAMIC_PARAMS: RwLock<FfiDynamicEncoderParams> = RwLock::new(FfiDynamicEncoderParams {
    updated: 0,
    bitrate_bps: 0,
    framerate: 0.0,
});

// --- Logging ---------------------------------------------------------------

unsafe extern "C" fn log_error(string_ptr: *const c_char) {
    eprintln!(
        "[alvr] {}",
        unsafe { CStr::from_ptr(string_ptr) }.to_string_lossy()
    );
}

unsafe extern "C" fn log_warn(string_ptr: *const c_char) {
    eprintln!(
        "[alvr] {}",
        unsafe { CStr::from_ptr(string_ptr) }.to_string_lossy()
    );
}

unsafe extern "C" fn log_info(string_ptr: *const c_char) {
    eprintln!(
        "[alvr] {}",
        unsafe { CStr::from_ptr(string_ptr) }.to_string_lossy()
    );
}

unsafe extern "C" fn log_debug(string_ptr: *const c_char) {
    eprintln!(
        "[alvr] {}",
        unsafe { CStr::from_ptr(string_ptr) }.to_string_lossy()
    );
}

unsafe extern "C" fn log_encoder(string_ptr: *const c_char) {
    eprintln!(
        "[alvr] {}",
        unsafe { CStr::from_ptr(string_ptr) }.to_string_lossy()
    );
}

unsafe extern "C" fn log_periodically(tag_ptr: *const c_char, string_ptr: *const c_char) {
    eprintln!(
        "[alvr] {}: {}",
        unsafe { CStr::from_ptr(tag_ptr) }.to_string_lossy(),
        unsafe { CStr::from_ptr(string_ptr) }.to_string_lossy()
    );
}

// --- Video -----------------------------------------------------------------

extern "C" fn set_video_config_nals(buffer_ptr: *const u8, len: i32, codec: i32) {
    let mut config_buffer = vec![0; len as usize];
    unsafe { ptr::copy_nonoverlapping(buffer_ptr, config_buffer.as_mut_ptr(), len as usize) };

    crate::streaming::send(RuntimeEvent::VideoConfig {
        buffer: config_buffer,
        codec,
    });
}

extern "C" fn send_video(timestamp_ns: u64, buffer_ptr: *mut u8, len: i32, is_idr: bool) {
    let buffer = unsafe { std::slice::from_raw_parts(buffer_ptr, len as usize) };

    crate::streaming::send(RuntimeEvent::VideoNal {
        timestamp_ns,
        buffer: buffer.to_vec(),
        is_idr,
    });
}

extern "C" fn get_dynamic_encoder_params() -> FfiDynamicEncoderParams {
    let mut params = DYNAMIC_PARAMS.write();
    let result = *params;
    params.updated = 0;
    result
}

extern "C" fn report_composed(timestamp_ns: u64, offset_ns: u64) {
    crate::streaming::send(RuntimeEvent::ReportComposed {
        timestamp_ns,
        offset_ns,
    });
}

extern "C" fn report_present(timestamp_ns: u64, offset_ns: u64) {
    crate::streaming::send(RuntimeEvent::ReportPresent {
        timestamp_ns,
        offset_ns,
    });
}

pub fn set_dynamic_encoder_params(updated: bool, bitrate_bps: u64, framerate: f32) {
    *DYNAMIC_PARAMS.write() = FfiDynamicEncoderParams {
        updated: updated as u32,
        bitrate_bps,
        framerate,
    };
}

/// Wire reused C++ callback-pointer globals exactly as v20 server_openvr does.
pub fn initialize_callbacks() {
    unsafe {
        crate::bindings::LogError = Some(log_error);
        crate::bindings::LogWarn = Some(log_warn);
        crate::bindings::LogInfo = Some(log_info);
        crate::bindings::LogDebug = Some(log_debug);
        crate::bindings::LogEncoder = Some(log_encoder);
        crate::bindings::LogPeriodically = Some(log_periodically);
        crate::bindings::SetVideoConfigNals = Some(set_video_config_nals);
        crate::bindings::VideoSend = Some(send_video);
        crate::bindings::GetDynamicEncoderParams = Some(get_dynamic_encoder_params);
        crate::bindings::ReportComposed = Some(report_composed);
        crate::bindings::ReportPresent = Some(report_present);
    }
}
