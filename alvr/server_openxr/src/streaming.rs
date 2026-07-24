//! In-app adapter for the persistent OpenXR streamer host.

use crate::{
    ipc::{self, HostEvent, RuntimeEvent},
    state::{CONTROLLER_POSES, CONTROLLER_TRACKED, FRAME_LOOP, HEAD_POSE_QUEUE, LOCAL_VIEW_PARAMS},
};
use alvr_common::{parking_lot::Mutex, ViewParams};
use alvr_filesystem as afs;
use std::{
    ffi::CString,
    net::TcpStream,
    sync::{
        atomic::{AtomicBool, Ordering},
        Once, OnceLock,
    },
    thread,
    time::Duration,
};

static INIT: Once = Once::new();
static PATHS_INIT: Once = Once::new();
static SETTINGS_INIT: Once = Once::new();
static RECOMMENDED_VIEW_SIZE: OnceLock<(u32, u32)> = OnceLock::new();
static ENCODER_STARTED: AtomicBool = AtomicBool::new(false);
static WRITER: Mutex<Option<TcpStream>> = Mutex::new(None);
static SESSION_PATH_CSTR: OnceLock<CString> = OnceLock::new();
static DRIVER_ROOT_CSTR: OnceLock<CString> = OnceLock::new();

unsafe extern "C" {
    fn alvr_openxr_set_paths(
        session_path: *const std::ffi::c_char,
        driver_root: *const std::ffi::c_char,
    );
    fn alvr_openxr_load_settings();
    fn alvr_openxr_adapter_index() -> i32;
    fn alvr_openxr_refresh_rate() -> u32;
    fn alvr_openxr_render_width() -> u32;
    fn alvr_openxr_render_height() -> u32;
    fn alvr_openxr_encoder_start(adapter_index: u32);
    fn alvr_openxr_encoder_stop();
}

/// Point the reused C++ `Settings` singleton at the shared ALVR `session.json`.
/// Idempotent: safe to call from both view-config enumeration and session begin.
fn configure_paths() {
    PATHS_INIT.call_once(|| {
        let install_dir = crate::registration::install_dir().unwrap_or_default();
        let layout = if install_dir.as_os_str().is_empty() {
            afs::filesystem_layout_invalid()
        } else {
            afs::Layout::new(&install_dir)
        };

        let session = CString::new(layout.session().to_string_lossy().as_ref()).unwrap_or_default();
        let root =
            CString::new(layout.executables_dir.to_string_lossy().as_ref()).unwrap_or_default();
        let session = SESSION_PATH_CSTR.get_or_init(|| session).as_ptr();
        let root = DRIVER_ROOT_CSTR.get_or_init(|| root).as_ptr();
        unsafe { alvr_openxr_set_paths(session, root) };
    });
}

/// Per-eye recommended swapchain size derived from the ALVR-configured stereo
/// render resolution (`width / 2` by `height`). Loaded once, before the app
/// creates its swapchains, so the OpenXR views match the ALVR dashboard setting
/// instead of a hardcoded default. Returns `None` if settings are unavailable,
/// letting the caller fall back to its placeholder.
pub fn recommended_view_size() -> Option<(u32, u32)> {
    SETTINGS_INIT.call_once(|| {
        configure_paths();
        unsafe { alvr_openxr_load_settings() };
        let width = unsafe { alvr_openxr_render_width() };
        let height = unsafe { alvr_openxr_render_height() };
        if width >= 2 && height > 0 {
            let _ = RECOMMENDED_VIEW_SIZE.set((width / 2, height));
        }
    });
    RECOMMENDED_VIEW_SIZE.get().copied()
}

pub fn ensure_started() {
    INIT.call_once(|| {
        configure_paths();

        crate::encoder_ffi::initialize_callbacks();

        thread::spawn(connection_loop);
    });
}

/// Send one event to the persistent host. Dropped while host/runtime reconnects;
/// the next encoded frame is enough to resume streaming.
pub fn send(event: RuntimeEvent) {
    let mut writer = WRITER.lock();
    if let Some(stream) = writer.as_mut() {
        if ipc::send(stream, &event).is_err() {
            *writer = None;
        }
    }
}

fn connection_loop() {
    loop {
        match TcpStream::connect(ipc::HOST_ADDR) {
            Ok(stream) => {
                stream.set_nodelay(true).ok();
                let Ok(writer) = stream.try_clone() else {
                    thread::sleep(Duration::from_millis(250));
                    continue;
                };
                *WRITER.lock() = Some(writer);
                eprintln!("[alvr] connected to persistent streamer host");
                receive_loop(stream);
                *WRITER.lock() = None;
                if ENCODER_STARTED.swap(false, Ordering::SeqCst) {
                    unsafe { alvr_openxr_encoder_stop() };
                }
            }
            Err(_) => {}
        }
        thread::sleep(Duration::from_millis(250));
    }
}

fn receive_loop(mut stream: TcpStream) {
    while let Ok(event) = ipc::receive::<HostEvent>(&mut stream) {
        match event {
            HostEvent::ResetInputs => {
                crate::actions::clear_buttons();
                *CONTROLLER_TRACKED.write() = [false; 2];
                HEAD_POSE_QUEUE.lock().clear();
                crate::state::RENDER_TRACKING_TS.lock().clear();
            }
            HostEvent::ClientConnected => {
                // The host replays ClientConnected on every reconnect/snapshot;
                // starting the encoder more than once churns the stream and
                // corrupts the decoder. Start only on the first transition.
                if !ENCODER_STARTED.swap(true, Ordering::SeqCst) {
                    crate::graphics::initialize_shaders();
                    unsafe {
                        alvr_openxr_load_settings();
                        let refresh_rate = alvr_openxr_refresh_rate();
                        if (30..=240).contains(&refresh_rate) {
                            let mut frame_loop = FRAME_LOOP.lock();
                            frame_loop.frame_period_ns = 1_000_000_000 / refresh_rate as i64;
                            // Restart the whole pacing clock, not just the next
                            // wake. The origin was set on the first pre-connect
                            // wait_frame at the default 90 Hz period; if only
                            // next_wake changes here, predicted_display_time
                            // (= target - origin) makes a one-time discontinuous
                            // jump when the period switches. That jump desyncs
                            // the display-time → tracking-ts map (RENDER_TRACKING_TS),
                            // so end_frame's nearest-match picks a wrong sample
                            // and the client reprojects from a stale pose —
                            // intermittent per-eye jitter. Reset origin +
                            // predicted time and drop the stale map so the clock
                            // is monotonic and continuous from connect.
                            frame_loop.next_wake_instant = None;
                            frame_loop.time_origin_instant = None;
                            frame_loop.predicted_display_time_ns = 0;
                            crate::state::RENDER_TRACKING_TS.lock().clear();
                            eprintln!("[alvr] frame pacing: {refresh_rate} Hz");
                        }
                        let adapter = alvr_openxr_adapter_index().max(0) as u32;
                        alvr_openxr_encoder_start(adapter);
                    }
                }
            }
            HostEvent::ClientDisconnected => {
                crate::actions::clear_buttons();
                *CONTROLLER_TRACKED.write() = [false; 2];
                HEAD_POSE_QUEUE.lock().clear();
                crate::state::RENDER_TRACKING_TS.lock().clear();
                if ENCODER_STARTED.swap(false, Ordering::SeqCst) {
                    unsafe { alvr_openxr_encoder_stop() };
                }
            }
            HostEvent::RequestIdr => {
                crate::state::REQUEST_IDR.store(true, std::sync::atomic::Ordering::Relaxed)
            }
            HostEvent::ViewsConfig {
                local_view_transforms,
                fov,
            } => {
                *LOCAL_VIEW_PARAMS.write() = [
                    ViewParams {
                        pose: local_view_transforms[0],
                        fov: fov[0],
                    },
                    ViewParams {
                        pose: local_view_transforms[1],
                        fov: fov[1],
                    },
                ];
            }
            HostEvent::Tracking {
                sample_timestamp_ns,
                head_pose,
                left_hand_pose,
                right_hand_pose,
            } => {
                let mut queue = HEAD_POSE_QUEUE.lock();
                queue.push_back((Duration::from_nanos(sample_timestamp_ns), head_pose));
                while queue.len() > 360 {
                    queue.pop_front();
                }

                // Keep this order in sync with `xrLocateSpace()` to avoid
                // a tracking-update thread and runtime call waiting on each other.
                let mut controller_tracked = CONTROLLER_TRACKED.write();
                let mut controllers = CONTROLLER_POSES.write();
                controller_tracked[0] = left_hand_pose.is_some();
                controller_tracked[1] = right_hand_pose.is_some();
                if let Some(pose) = left_hand_pose {
                    controllers[0] = pose;
                }
                if let Some(pose) = right_hand_pose {
                    controllers[1] = pose;
                }
            }
            HostEvent::Buttons(entries) => crate::actions::update_buttons(entries),
            HostEvent::DynamicEncoderParams {
                updated,
                bitrate_bps,
                framerate,
            } => crate::encoder_ffi::set_dynamic_encoder_params(updated, bitrate_bps, framerate),
            HostEvent::Recenter => {
                // Match VDXR: one physical recenter queues LOCAL then STAGE
                // reference-space-change events.
                crate::state::PENDING_RECENTER.store(2, std::sync::atomic::Ordering::Relaxed);
            }
        }
    }
}
