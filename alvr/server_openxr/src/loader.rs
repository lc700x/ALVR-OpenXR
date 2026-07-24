//! OpenXR loader negotiation ABI.
//!
//! The OpenXR loader dlopen's the runtime and calls
//! `xrNegotiateLoaderRuntimeInterface` to exchange interface versions and obtain
//! the runtime's `xrGetInstanceProcAddr`. The negotiation structs are a stable,
//! self-contained ABI defined by `openxr_loader_negotiation.h`, so we declare
//! them here rather than depend on loader-private types.

#![allow(non_snake_case, non_upper_case_globals)]

use openxr_sys as sys;
use std::ffi::c_char;

/// `XR_CURRENT_LOADER_RUNTIME_VERSION`
const CURRENT_LOADER_RUNTIME_VERSION: u32 = 1;

/// `XrLoaderInterfaceStructs` discriminants used during negotiation.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LoaderInterfaceStruct {
    Uninitialized = 0,
    LoaderInfo = 1,
    ApiLayerRequest = 2,
    RuntimeRequest = 3,
    ApiLayerCreateInfo = 4,
    ApiLayerNextInfo = 5,
}

/// `XrNegotiateLoaderInfo` — filled by the loader, read by the runtime.
#[repr(C)]
pub struct XrNegotiateLoaderInfo {
    pub struct_type: LoaderInterfaceStruct,
    pub struct_version: u32,
    pub struct_size: usize,
    pub min_interface_version: u32,
    pub max_interface_version: u32,
    pub min_api_version: sys::Version,
    pub max_api_version: sys::Version,
}

/// `XrNegotiateRuntimeRequest` — filled by the runtime, read by the loader.
#[repr(C)]
pub struct XrNegotiateRuntimeRequest {
    pub struct_type: LoaderInterfaceStruct,
    pub struct_version: u32,
    pub struct_size: usize,
    pub runtime_interface_version: u32,
    pub runtime_api_version: sys::Version,
    pub get_instance_proc_addr: Option<sys::pfn::GetInstanceProcAddr>,
}

/// Entry point the OpenXR loader calls after loading the runtime library.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn xrNegotiateLoaderRuntimeInterface(
    loader_info: *const XrNegotiateLoaderInfo,
    runtime_request: *mut XrNegotiateRuntimeRequest,
) -> sys::Result {
    if loader_info.is_null() || runtime_request.is_null() {
        return sys::Result::ERROR_INITIALIZATION_FAILED;
    }

    // # Safety: pointers are non-null (checked above) and the loader guarantees
    // they point at correctly-sized negotiation structs.
    let loader_info = unsafe { &*loader_info };
    let runtime_request = unsafe { &mut *runtime_request };

    let info_ok = loader_info.struct_type == LoaderInterfaceStruct::LoaderInfo
        && loader_info.struct_size == size_of::<XrNegotiateLoaderInfo>();
    let request_ok = runtime_request.struct_type == LoaderInterfaceStruct::RuntimeRequest
        && runtime_request.struct_size == size_of::<XrNegotiateRuntimeRequest>();
    if !info_ok || !request_ok {
        return sys::Result::ERROR_INITIALIZATION_FAILED;
    }

    let interface_ok = CURRENT_LOADER_RUNTIME_VERSION >= loader_info.min_interface_version
        && CURRENT_LOADER_RUNTIME_VERSION <= loader_info.max_interface_version;
    let api_ok = sys::CURRENT_API_VERSION >= loader_info.min_api_version
        && sys::CURRENT_API_VERSION <= loader_info.max_api_version;
    if !interface_ok || !api_ok {
        return sys::Result::ERROR_API_VERSION_UNSUPPORTED;
    }

    runtime_request.runtime_interface_version = CURRENT_LOADER_RUNTIME_VERSION;
    runtime_request.runtime_api_version = sys::CURRENT_API_VERSION;
    runtime_request.get_instance_proc_addr = Some(get_instance_proc_addr);

    sys::Result::SUCCESS
}

/// Runtime dispatch entry. Returns a function pointer for a named `xr*` call.
unsafe extern "system" fn get_instance_proc_addr(
    _instance: sys::Instance,
    name: *const c_char,
    function: *mut Option<sys::pfn::VoidFunction>,
) -> sys::Result {
    if function.is_null() || name.is_null() {
        return sys::Result::ERROR_VALIDATION_FAILURE;
    }

    // # Safety: non-null checked; loader passes a valid NUL-terminated name and
    // a valid out-param.
    let name = unsafe { std::ffi::CStr::from_ptr(name) };

    #[cfg(windows)]
    let resolved = crate::runtime::resolve(name);
    #[cfg(not(windows))]
    let resolved: Option<sys::pfn::VoidFunction> = {
        let _ = name;
        None
    };

    match resolved {
        Some(func) => {
            unsafe { *function = Some(func) };
            sys::Result::SUCCESS
        }
        None => {
            unsafe { *function = None };
            sys::Result::ERROR_FUNCTION_UNSUPPORTED
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negotiate_fills_runtime_request() {
        let loader_info = XrNegotiateLoaderInfo {
            struct_type: LoaderInterfaceStruct::LoaderInfo,
            struct_version: 1,
            struct_size: size_of::<XrNegotiateLoaderInfo>(),
            min_interface_version: 1,
            max_interface_version: 1,
            min_api_version: sys::Version::new(1, 0, 0),
            max_api_version: sys::CURRENT_API_VERSION,
        };
        let mut request = XrNegotiateRuntimeRequest {
            struct_type: LoaderInterfaceStruct::RuntimeRequest,
            struct_version: 1,
            struct_size: size_of::<XrNegotiateRuntimeRequest>(),
            runtime_interface_version: 0,
            runtime_api_version: sys::Version::new(0, 0, 0),
            get_instance_proc_addr: None,
        };

        let result = unsafe { xrNegotiateLoaderRuntimeInterface(&loader_info, &mut request) };

        assert_eq!(result, sys::Result::SUCCESS);
        assert_eq!(
            request.runtime_interface_version,
            CURRENT_LOADER_RUNTIME_VERSION
        );
        assert!(request.get_instance_proc_addr.is_some());
    }

    #[test]
    fn negotiate_rejects_null() {
        let result =
            unsafe { xrNegotiateLoaderRuntimeInterface(std::ptr::null(), std::ptr::null_mut()) };
        assert_eq!(result, sys::Result::ERROR_INITIALIZATION_FAILED);
    }
}
