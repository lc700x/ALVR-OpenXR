//! OpenXR runtime registration (Windows).
//!
//! An OpenXR app finds its runtime via `HKLM\SOFTWARE\Khronos\OpenXR\1\
//! ActiveRuntime`, which points at a manifest JSON that in turn points at this
//! runtime DLL. Per blueprint A7a the runtime hijacks the global ActiveRuntime,
//! saving the previous value so it can be restored on unregister.
//!
//! These helpers are exported for the installer / dashboard to call; they need
//! administrator rights to write `HKLM`.

use std::path::Path;
use windows::core::PCWSTR;
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyW, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
    HKEY, HKEY_LOCAL_MACHINE, KEY_READ, KEY_WRITE, REG_SZ,
};

const OPENXR_KEY: &str = "SOFTWARE\\Khronos\\OpenXR\\1";
const ACTIVE_RUNTIME: &str = "ActiveRuntime";
const SAVED_RUNTIME: &str = "ActiveRuntime_AlvrBackup";
const MANIFEST_FILE_NAME: &str = "alvr_openxr_runtime.json";

/// Serialize the runtime manifest JSON pointing at `runtime_dll_path`.
pub fn manifest_json(runtime_dll_path: &Path) -> String {
    // The library_path may be absolute; JSON needs escaped backslashes.
    let escaped = runtime_dll_path.to_string_lossy().replace('\\', "\\\\");
    format!(
        r#"{{
    "file_format_version": "1.0.0",
    "runtime": {{
        "name": "ALVR",
        "library_path": "{escaped}"
    }}
}}"#
    )
}

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Read the current `ActiveRuntime` value, if any.
fn read_active_runtime(key: HKEY) -> Option<String> {
    let name = to_wide(ACTIVE_RUNTIME);
    let mut size: u32 = 0;
    // First call sizes the buffer.
    let status = unsafe {
        RegQueryValueExW(
            key,
            PCWSTR(name.as_ptr()),
            None,
            None,
            None,
            Some(&mut size),
        )
    };
    if status.is_err() || size == 0 {
        return None;
    }
    let mut buf = vec![0u8; size as usize];
    let status = unsafe {
        RegQueryValueExW(
            key,
            PCWSTR(name.as_ptr()),
            None,
            None,
            Some(buf.as_mut_ptr()),
            Some(&mut size),
        )
    };
    if status.is_err() {
        return None;
    }
    // Value is a wide string; drop the trailing NUL.
    let wide: &[u16] = unsafe { std::slice::from_raw_parts(buf.as_ptr().cast(), buf.len() / 2) };
    let end = wide.iter().position(|&c| c == 0).unwrap_or(wide.len());
    Some(String::from_utf16_lossy(&wide[..end]))
}

fn set_value(key: HKEY, name: &str, value: &str) -> Result<(), windows::core::Error> {
    let name = to_wide(name);
    let value_wide = to_wide(value);
    let bytes = unsafe {
        std::slice::from_raw_parts(value_wide.as_ptr().cast::<u8>(), value_wide.len() * 2)
    };
    unsafe { RegSetValueExW(key, PCWSTR(name.as_ptr()), None, REG_SZ, Some(bytes)) }.ok()
}

/// Write the manifest next to the DLL and hijack `HKLM` ActiveRuntime, saving
/// the previous value for [`unregister`]. Requires administrator rights.
pub fn register(runtime_dll_path: &Path) -> Result<(), windows::core::Error> {
    let manifest_path = runtime_dll_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(MANIFEST_FILE_NAME);
    std::fs::write(&manifest_path, manifest_json(runtime_dll_path))
        .map_err(|e| windows::core::Error::new(windows::core::HRESULT(-1), e.to_string()))?;

    // Packaging recreates this directory and may delete only the manifest while
    // HKLM still points to the same path. Restoring that file needs no elevation;
    // avoid a needless registry write/UAC prompt in that common case.
    if is_active(runtime_dll_path) {
        return Ok(());
    }

    let subkey = to_wide(OPENXR_KEY);
    let mut key = HKEY::default();
    unsafe { RegCreateKeyW(HKEY_LOCAL_MACHINE, PCWSTR(subkey.as_ptr()), &mut key) }.ok()?;

    // Save the current runtime once, so repeated registers don't clobber it.
    if read_active_runtime(key).is_some() && read_saved_runtime(key).is_none() {
        if let Some(current) = read_active_runtime(key) {
            set_value(key, SAVED_RUNTIME, &current)?;
        }
    }

    let result = set_value(key, ACTIVE_RUNTIME, &manifest_path.to_string_lossy());
    let _ = unsafe { RegCloseKey(key) };
    result
}

fn read_saved_runtime(key: HKEY) -> Option<String> {
    let name = to_wide(SAVED_RUNTIME);
    let mut size: u32 = 0;
    let status = unsafe {
        RegQueryValueExW(
            key,
            PCWSTR(name.as_ptr()),
            None,
            None,
            None,
            Some(&mut size),
        )
    };
    if status.is_err() || size == 0 {
        return None;
    }
    let mut buf = vec![0u8; size as usize];
    unsafe {
        RegQueryValueExW(
            key,
            PCWSTR(name.as_ptr()),
            None,
            None,
            Some(buf.as_mut_ptr()),
            Some(&mut size),
        )
    }
    .ok()
    .ok()?;
    let wide: &[u16] = unsafe { std::slice::from_raw_parts(buf.as_ptr().cast(), buf.len() / 2) };
    let end = wide.iter().position(|&c| c == 0).unwrap_or(wide.len());
    Some(String::from_utf16_lossy(&wide[..end]))
}

/// Restore the previously-saved ActiveRuntime (or clear ALVR's if none saved).
/// Requires administrator rights.
pub fn unregister() -> Result<(), windows::core::Error> {
    let subkey = to_wide(OPENXR_KEY);
    let mut key = HKEY::default();
    unsafe {
        RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(subkey.as_ptr()),
            None,
            KEY_READ | KEY_WRITE,
            &mut key,
        )
    }
    .ok()?;

    let result = if let Some(saved) = read_saved_runtime(key) {
        let r = set_value(key, ACTIVE_RUNTIME, &saved);
        let saved_name = to_wide(SAVED_RUNTIME);
        let _ = unsafe { RegDeleteValueW(key, PCWSTR(saved_name.as_ptr())) };
        r
    } else {
        let name = to_wide(ACTIVE_RUNTIME);
        let _ = unsafe { RegDeleteValueW(key, PCWSTR(name.as_ptr())) };
        Ok(())
    };

    let _ = unsafe { RegCloseKey(key) };
    result
}

/// Default install path for the runtime DLL (next to the current executable).
pub fn default_dll_path() -> std::path::PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("alvr_server_openxr.dll")))
        .unwrap_or_else(|| Path::new("alvr_server_openxr.dll").to_owned())
}

/// Directory this runtime DLL is installed in (not the host OpenXR app's dir).
/// Used to locate the shared ALVR `session.json` / config, so the runtime and
/// dashboard read the same file.
pub fn install_dir() -> Option<std::path::PathBuf> {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::System::LibraryLoader::{
        GetModuleFileNameW, GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
        GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
    };

    // Resolve the module that contains this function's address (i.e. our DLL).
    let mut module = HMODULE::default();
    let anchor = install_dir as *const () as *const u16;
    let ok = unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            PCWSTR(anchor),
            &mut module,
        )
    };
    ok.ok()?;

    let mut buf = [0u16; 1024];
    let len = unsafe { GetModuleFileNameW(Some(module.into()), &mut buf) };
    if len == 0 {
        return None;
    }
    let dll_path = std::path::PathBuf::from(String::from_utf16_lossy(&buf[..len as usize]));
    dll_path.parent().map(|p| p.to_owned())
}

/// True if the ALVR runtime manifest is already the active OpenXR runtime, so
/// auto-registration can skip a needless elevation prompt.
pub fn is_active(runtime_dll_path: &Path) -> bool {
    let manifest_path = runtime_dll_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(MANIFEST_FILE_NAME);

    if !manifest_path.exists() {
        return false;
    }

    let subkey = to_wide(OPENXR_KEY);
    let mut key = HKEY::default();
    let opened = unsafe {
        RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(subkey.as_ptr()),
            None,
            KEY_READ,
            &mut key,
        )
    };
    if opened.is_err() {
        return false;
    }
    let current = read_active_runtime(key);
    let _ = unsafe { RegCloseKey(key) };

    current.is_some_and(|c| Path::new(&c) == manifest_path)
}

/// Whether this process may write the system-wide OpenXR runtime key.
pub fn can_write_active_runtime() -> bool {
    let subkey = to_wide(OPENXR_KEY);
    let mut key = HKEY::default();
    let result = unsafe {
        RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(subkey.as_ptr()),
            None,
            KEY_WRITE,
            &mut key,
        )
    };
    if result.is_ok() {
        let _ = unsafe { RegCloseKey(key) };
        true
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_escapes_backslashes() {
        let json = manifest_json(Path::new(r"C:\alvr\alvr_server_openxr.dll"));
        assert!(json.contains(r"C:\\alvr\\alvr_server_openxr.dll"));
        assert!(json.contains("\"name\": \"ALVR\""));
    }
}
