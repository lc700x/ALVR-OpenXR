//! ALVR OpenXR runtime registration helper.
//!
//! Registers (or unregisters) the ALVR runtime as the active OpenXR runtime by
//! writing `HKLM\SOFTWARE\Khronos\OpenXR\1\ActiveRuntime` — which needs admin.
//! If not elevated, the tool relaunches itself elevated (one UAC prompt).
//!
//! Usage:
//!   alvr_openxr_register            register (default)
//!   alvr_openxr_register unregister restore the previous runtime
//!   alvr_openxr_register --auto     register only if not already active (no
//!                                   prompt when already set) — for auto-run
//!                                   at dashboard startup
//!   alvr_openxr_register --watch PID register while PID lives, then restore

#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
fn main() {
    use alvr_server_openxr::registration;

    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let arg = args.first().map(String::as_str).unwrap_or_default();
    let dll_path = registration::default_dll_path();

    match arg {
        "--watch" => {
            let Some(pid) = args.get(1).and_then(|pid| pid.parse::<u32>().ok()) else {
                std::process::exit(2);
            };

            // A system-wide runtime switch needs HKLM write access. Keep one
            // elevated helper alive for the dashboard session, then restore.
            if !registration::can_write_active_runtime() {
                let _ = relaunch_elevated(&format!("--watch {pid}"));
                return;
            }
            if registration::register(&dll_path).is_err() {
                std::process::exit(1);
            }
            wait_for_process_exit(pid);
            registration::unregister().ok();
        }
        "unregister" => match registration::unregister() {
            Ok(()) => println!("ALVR OpenXR runtime unregistered."),
            Err(_) => relaunch_elevated_or_fail("unregister"),
        },
        "--auto" => {
            // Silent no-op if already the active runtime (avoids UAC spam).
            if registration::is_active(&dll_path) {
                return;
            }
            if registration::register(&dll_path).is_err() {
                // Best-effort elevation; auto mode must never block startup.
                let _ = relaunch_elevated("register");
            }
        }
        _ => match registration::register(&dll_path) {
            Ok(()) => println!("ALVR is now the active OpenXR runtime."),
            Err(_) => relaunch_elevated_or_fail("register"),
        },
    }
}

#[cfg(windows)]
fn wait_for_process_exit(pid: u32) {
    use windows::Win32::{
        Foundation::CloseHandle,
        System::Threading::{OpenProcess, WaitForSingleObject, INFINITE, PROCESS_SYNCHRONIZE},
    };

    if let Ok(process) = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) } {
        unsafe { WaitForSingleObject(process, INFINITE) };
        let _ = unsafe { CloseHandle(process) };
    }
}

/// Relaunch this exe elevated with the given verb, then exit.
#[cfg(windows)]
fn relaunch_elevated_or_fail(args: &str) {
    if relaunch_elevated(args).is_err() {
        eprintln!("Failed to register: administrator rights are required.");
        std::process::exit(1);
    }
}

#[cfg(windows)]
fn relaunch_elevated(args: &str) -> Result<(), ()> {
    use windows::core::{HSTRING, PCWSTR};
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;

    // Guard against an infinite relaunch loop.
    if std::env::var("ALVR_OPENXR_ELEVATED").is_ok() {
        return Err(());
    }

    let exe = std::env::current_exe().map_err(|_| ())?;
    let runas = HSTRING::from("runas");
    let file = HSTRING::from(exe.as_os_str());
    let params = HSTRING::from(args);

    // # Safety: all strings are valid, NUL-terminated HSTRINGs.
    let result = unsafe {
        ShellExecuteW(
            None,
            PCWSTR(runas.as_ptr()),
            PCWSTR(file.as_ptr()),
            PCWSTR(params.as_ptr()),
            PCWSTR::null(),
            SW_HIDE,
        )
    };

    // ShellExecuteW returns >32 on success.
    if result.0 as usize > 32 {
        Ok(())
    } else {
        Err(())
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("The ALVR OpenXR runtime is Windows-only.");
    std::process::exit(1);
}
