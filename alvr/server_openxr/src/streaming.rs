//! In-app adapter for the persistent OpenXR streamer host.

use crate::{
    ipc::{self, HostEvent, RuntimeEvent},
    state::{CONTROLLER_POSES, CONTROLLER_TRACKED, FRAME_LOOP, HEAD_POSE_QUEUE, LOCAL_VIEW_PARAMS},
};
use alvr_common::{parking_lot::Mutex, ViewParams};
use alvr_filesystem as afs;
use std::{
    ffi::CString,
    net::{Shutdown, TcpStream},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Once, OnceLock,
    },
    thread,
    time::Duration,
};

const RECONNECT_DELAY: Duration = Duration::from_millis(250);
static CALLBACKS_INIT: Once = Once::new();
static PATHS_INIT: Once = Once::new();
static SETTINGS_INIT: Once = Once::new();
static RECOMMENDED_VIEW_SIZE: OnceLock<(u32, u32)> = OnceLock::new();
static ENCODER_STATE: Mutex<EncoderState> = Mutex::new(EncoderState {
    client_connected: false,
    session_active: false,
    started: false,
});
static WRITER: Mutex<Option<TcpStream>> = Mutex::new(None);
static SESSION_PATH_CSTR: OnceLock<CString> = OnceLock::new();
static DRIVER_ROOT_CSTR: OnceLock<CString> = OnceLock::new();
static CONNECTION_WORKER: Mutex<Option<ConnectionWorker>> = Mutex::new(None);

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
    fn alvr_openxr_encoder_start(adapter_index: u32) -> bool;
    fn alvr_openxr_encoder_stop();
}

#[derive(Default)]
struct EncoderState {
    client_connected: bool,
    session_active: bool,
    started: bool,
}

struct ConnectionWorker {
    stop: Arc<AtomicBool>,
    socket: Arc<Mutex<Option<TcpStream>>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl ConnectionWorker {
    fn start() -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let socket = Arc::new(Mutex::new(None));
        let thread = {
            let stop = Arc::clone(&stop);
            let socket = Arc::clone(&socket);
            thread::spawn(move || connection_loop(&stop, &socket))
        };

        Self {
            stop,
            socket,
            thread: Some(thread),
        }
    }

    fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(socket) = self.socket.lock().take() {
            // Shutdown also wakes writes through the encoder's cloned socket.
            let _ = socket.shutdown(Shutdown::Both);
        }
        if let Some(thread) = self.thread.take() {
            thread.thread().unpark();
            let _ = thread.join();
        }
    }
}

impl EncoderState {
    fn synchronize(&mut self, start: impl FnOnce() -> bool, stop: impl FnOnce()) {
        if self.client_connected && self.session_active {
            if !self.started {
                self.started = start();
            }
        } else if self.started {
            stop();
            self.started = false;
        }
    }
}

/// Point the reused C++ `Settings` singleton at the shared ALVR `session.json`.
/// Idempotent: safe to call from both view-config enumeration and session begin.
/// Guards the eye parameters against degenerate values. Only rejects what is already broken, so
/// valid parameters pass through untouched for every client.
fn view_params_are_sane(view: &ViewParams) -> bool {
    let position = view.pose.position;
    let orientation = view.pose.orientation;
    let fov = view.fov;

    position.x.is_finite()
        && position.y.is_finite()
        && position.z.is_finite()
        && position.length() < 10.0
        && orientation.x.is_finite()
        && orientation.y.is_finite()
        && orientation.z.is_finite()
        && orientation.w.is_finite()
        && fov.left.is_finite()
        && fov.right.is_finite()
        && fov.up.is_finite()
        && fov.down.is_finite()
        && (fov.right - fov.left) > 0.1
        && (fov.up - fov.down) > 0.1
}

fn configure_paths() {    PATHS_INIT.call_once(|| {
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
    ensure_settings_loaded();
    RECOMMENDED_VIEW_SIZE.get().copied()
}

pub fn selected_adapter_index() -> u32 {
    ensure_settings_loaded();
    unsafe { alvr_openxr_adapter_index().max(0) as u32 }
}

fn ensure_settings_loaded() {
    SETTINGS_INIT.call_once(|| {
        configure_paths();
        unsafe { alvr_openxr_load_settings() };
        let width = unsafe { alvr_openxr_render_width() };
        let height = unsafe { alvr_openxr_render_height() };
        if width >= 2 && height > 0 {
            let _ = RECOMMENDED_VIEW_SIZE.set((width / 2, height));
        }
    });
}

pub fn ensure_started() {
    configure_paths();
    CALLBACKS_INIT.call_once(crate::encoder_ffi::initialize_callbacks);
    let mut worker = CONNECTION_WORKER.lock();
    if worker.is_none() {
        *worker = Some(ConnectionWorker::start());
    }
}

/// Join all runtime IPC work before the OpenXR loader unloads this DLL.
pub fn shutdown() {
    let mut worker = CONNECTION_WORKER.lock();
    if let Some(mut worker) = worker.take() {
        worker.stop();
    }
    *WRITER.lock() = None;
    let mut state = ENCODER_STATE.lock();
    state.client_connected = false;
    state.session_active = false;
    state.synchronize(start_encoder, stop_encoder);
}

/// Keep the encoder lifecycle synchronized with session destruction and host events.
pub fn set_session_active(active: bool) {
    if active {
        ensure_started();
    }
    let mut state = ENCODER_STATE.lock();
    state.session_active = active;
    state.synchronize(start_encoder, stop_encoder);
}

fn set_client_connected(connected: bool) {
    let mut state = ENCODER_STATE.lock();
    state.client_connected = connected;
    state.synchronize(start_encoder, stop_encoder);
}

fn start_encoder() -> bool {
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
        alvr_openxr_encoder_start(adapter)
    }
}

fn stop_encoder() {
    unsafe { alvr_openxr_encoder_stop() };
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

fn connection_loop(stop: &AtomicBool, socket: &Mutex<Option<TcpStream>>) {
    // # Safety: HOST_ADDR is a fixed numeric localhost socket address.
    let address = ipc::HOST_ADDR.parse().unwrap();
    while !stop.load(Ordering::Acquire) {
        match TcpStream::connect_timeout(&address, RECONNECT_DELAY) {
            Ok(stream) => {
                stream.set_nodelay(true).ok();
                if stream.set_read_timeout(Some(RECONNECT_DELAY)).is_err() {
                    thread::park_timeout(RECONNECT_DELAY);
                    continue;
                }
                let Ok(writer) = stream.try_clone() else {
                    thread::park_timeout(RECONNECT_DELAY);
                    continue;
                };
                let Ok(wakeup) = stream.try_clone() else {
                    thread::park_timeout(RECONNECT_DELAY);
                    continue;
                };
                *socket.lock() = Some(wakeup);
                // Shutdown can race the connect or publication of the wakeup socket.
                if stop.load(Ordering::Acquire) {
                    if let Some(socket) = socket.lock().take() {
                        let _ = socket.shutdown(Shutdown::Both);
                    }
                    break;
                }
                *WRITER.lock() = Some(writer);
                eprintln!("[alvr] connected to persistent streamer host");
                receive_loop(stream, stop);
                *WRITER.lock() = None;
                set_client_connected(false);
                *socket.lock() = None;
            }
            Err(_) => {}
        }
        thread::park_timeout(RECONNECT_DELAY);
    }
}

fn receive_loop(mut stream: TcpStream, stop: &AtomicBool) {
    while !stop.load(Ordering::Acquire) {
        let Ok(event) = ipc::receive_interruptible::<HostEvent>(&mut stream, stop) else {
            break;
        };
        if stop.load(Ordering::Acquire) {
            break;
        }
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
                // corrupts the decoder. Cache the connection across sessions.
                set_client_connected(true);
            }
            HostEvent::ClientDisconnected => {
                crate::actions::clear_buttons();
                *CONTROLLER_TRACKED.write() = [false; 2];
                HEAD_POSE_QUEUE.lock().clear();
                crate::state::RENDER_TRACKING_TS.lock().clear();
                set_client_connected(false);
            }
            HostEvent::RequestIdr => {
                crate::state::REQUEST_IDR.store(true, std::sync::atomic::Ordering::Relaxed)
            }
            HostEvent::ViewsConfig {
                local_view_transforms,
                fov,
            } => {
                let incoming = [
                    ViewParams {
                        pose: local_view_transforms[0],
                        fov: fov[0],
                    },
                    ViewParams {
                        pose: local_view_transforms[1],
                        fov: fov[1],
                    },
                ];

                // The host echoes the client's view parameters back, and a degenerate value has
                // reached this point twice (a pose at 1e20, then a z of -3.6e35). Keeping the last
                // good pair is always better than projecting with a pose that is not a pose.
                if incoming.iter().all(view_params_are_sane) {
                    *LOCAL_VIEW_PARAMS.write() = incoming;
                } else {
                    eprintln!(
                        "[alvr] rejecting degenerate view params from the host: left={:?} right={:?}",
                        incoming[0].pose.position, incoming[1].pose.position
                    );
                }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::Cell, net::TcpListener, sync::mpsc, time::Instant};

    #[test]
    fn connection_worker_shutdown_wakes_blocked_receive_and_can_restart() {
        for _ in 0..2 {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let mut stream = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
            stream.set_read_timeout(Some(RECONNECT_DELAY)).unwrap();
            let (_peer, _) = listener.accept().unwrap();
            let stop = Arc::new(AtomicBool::new(false));
            let socket = Arc::new(Mutex::new(Some(stream.try_clone().unwrap())));
            let (ready_tx, ready_rx) = mpsc::channel();
            let (done_tx, done_rx) = mpsc::channel();
            let thread = {
                let stop = Arc::clone(&stop);
                thread::spawn(move || {
                    ready_tx.send(()).unwrap();
                    assert!(ipc::receive_interruptible::<HostEvent>(&mut stream, &stop).is_err());
                    assert!(stop.load(Ordering::Acquire));
                    done_tx.send(()).unwrap();
                })
            };
            let mut worker = ConnectionWorker {
                stop,
                socket,
                thread: Some(thread),
            };
            ready_rx.recv_timeout(Duration::from_secs(1)).unwrap();
            assert!(matches!(
                done_rx.recv_timeout(Duration::from_millis(25)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ));
            let started = Instant::now();
            worker.stop();
            worker.stop();
            assert!(worker.thread.is_none());
            done_rx.recv_timeout(Duration::from_secs(1)).unwrap();
            assert!(started.elapsed() < Duration::from_secs(1));
        }
    }

    #[test]
    fn encoder_restarts_for_a_recreated_session_without_a_client_event() {
        let starts = Cell::new(0);
        let stops = Cell::new(0);
        let start = || {
            starts.set(starts.get() + 1);
            true
        };
        let stop = || stops.set(stops.get() + 1);
        let mut state = EncoderState::default();

        state.client_connected = true;
        state.synchronize(start, stop);
        assert_eq!(starts.get(), 0);

        state.session_active = true;
        state.synchronize(start, stop);
        assert!(state.started);
        assert_eq!(starts.get(), 1);

        state.session_active = false;
        state.synchronize(start, stop);
        assert!(!state.started);
        assert!(state.client_connected);
        assert_eq!(stops.get(), 1);

        state.session_active = true;
        state.synchronize(start, stop);
        assert!(state.started);
        assert_eq!(starts.get(), 2);

        state.client_connected = true;
        state.synchronize(start, stop);
        assert_eq!(starts.get(), 2);

        state.client_connected = false;
        state.synchronize(start, stop);
        assert!(!state.started);
        assert_eq!(stops.get(), 2);
        state.synchronize(start, stop);
        assert_eq!(stops.get(), 2);
    }

    #[test]
    fn encoder_waits_for_a_client_and_retries_a_failed_start() {
        let attempts = Cell::new(0);
        let start = || {
            attempts.set(attempts.get() + 1);
            attempts.get() > 1
        };
        let mut state = EncoderState {
            session_active: true,
            ..Default::default()
        };

        state.synchronize(start, || panic!("encoder was not started"));
        assert_eq!(attempts.get(), 0);

        state.client_connected = true;
        state.synchronize(start, || panic!("encoder was not started"));
        assert!(!state.started);
        assert_eq!(attempts.get(), 1);

        state.session_active = false;
        state.synchronize(start, || panic!("failed start must not require a stop"));
        state.session_active = true;
        state.synchronize(start, || panic!("encoder is starting"));
        assert!(state.started);
        assert_eq!(attempts.get(), 2);

        state.synchronize(start, || panic!("encoder must stay started"));
        assert_eq!(attempts.get(), 2);
    }
}
