use android_activity::{AndroidApp, InputStatus, MainEvent, PollEvent};
use std::{ffi::c_void, time::Duration};

unsafe extern "C" {
    fn alvr_qiyu_initialize(activity: *mut c_void, java_vm: *mut c_void) -> bool;
    fn alvr_qiyu_start(native_window: *mut c_void) -> bool;
    fn alvr_qiyu_stop();
    fn alvr_qiyu_render_frame();
    fn alvr_qiyu_destroy();
}

pub fn entry_point(app: AndroidApp) {
    alvr_client_core::init_logging();

    if !unsafe { alvr_qiyu_initialize(app.activity_as_ptr(), app.vm_as_ptr()) } {
        alvr_common::error!("QiYu SDK initialization failed");
        return;
    }

    let mut resumed = false;
    let mut running = false;
    let mut should_quit = false;

    while !should_quit {
        app.poll_events(
            Some(if running {
                Duration::ZERO
            } else {
                Duration::from_millis(100)
            }),
            |event| match event {
                PollEvent::Main(MainEvent::Resume { .. }) => resumed = true,
                PollEvent::Main(MainEvent::Pause) => {
                    if running {
                        unsafe { alvr_qiyu_stop() };
                        running = false;
                    }
                    resumed = false;
                }
                PollEvent::Main(MainEvent::InitWindow { .. }) => {
                    if resumed && !running {
                        if let Some(window) = app.native_window() {
                            running =
                                unsafe { alvr_qiyu_start(window.ptr().as_ptr().cast::<c_void>()) };
                        }
                    }
                }
                PollEvent::Main(MainEvent::TerminateWindow { .. }) => {
                    if running {
                        unsafe { alvr_qiyu_stop() };
                        running = false;
                    }
                }
                PollEvent::Main(MainEvent::InputAvailable) => {
                    if let Ok(mut iter) = app.input_events_iter() {
                        while iter.next(|_| InputStatus::Unhandled) {}
                    }
                }
                PollEvent::Main(MainEvent::Destroy) => should_quit = true,
                _ => (),
            },
        );

        if resumed && !running {
            if let Some(window) = app.native_window() {
                running = unsafe { alvr_qiyu_start(window.ptr().as_ptr().cast::<c_void>()) };
            }
        }

        if running {
            unsafe { alvr_qiyu_render_frame() };
        }
    }

    if running {
        unsafe { alvr_qiyu_stop() };
    }
    unsafe { alvr_qiyu_destroy() };
}
