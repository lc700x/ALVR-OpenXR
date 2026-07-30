#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
fn main() {
    use alvr_common::{parking_lot::Mutex, Fov, Pose, HAND_LEFT_ID, HAND_RIGHT_ID, HEAD_ID};
    use alvr_server_core::{ServerCoreContext, ServerCoreEvent};
    use alvr_server_openxr::ipc::{self, HostEvent, RuntimeEvent};
    use alvr_session::CodecType;
    use std::{
        net::{TcpListener, TcpStream},
        sync::{
            atomic::{AtomicU64, Ordering},
            Arc,
        },
        thread,
    };

    type Writer = Arc<Mutex<Option<(u64, TcpStream)>>>;

    #[derive(Default)]
    struct Snapshot {
        connected: bool,
        views: Option<([Pose; 2], [Fov; 2])>,
        encoder_params: Option<(u64, f32)>,
    }

    fn send(writer: &Writer, event: &HostEvent) {
        let mut guard = writer.lock();
        if let Some((_, stream)) = guard.as_mut() {
            if ipc::send(stream, event).is_err() {
                *guard = None;
            }
        }
    }

    // On each headset connection, make sure ALVR is still the active OpenXR
    // runtime so the next OpenXR app launch loads our runtime. Another runtime
    // (SteamVR, VDXR, Oculus) may have hijacked `ActiveRuntime` since startup.
    // `--auto` is a silent no-op when already active and self-elevates only when
    // a registry write is actually needed.
    fn ensure_runtime_registered() {
        use alvr_server_openxr::registration;
        use std::os::windows::process::CommandExt;

        const CREATE_NO_WINDOW: u32 = 0x0800_0000;

        let dll_path = registration::default_dll_path();
        if registration::is_active(&dll_path) {
            return;
        }

        if let Some(register_exe) = std::env::current_exe()
            .ok()
            .and_then(|path| {
                path.parent()
                    .map(|dir| dir.join("alvr_openxr_register.exe"))
            })
            .filter(|path| path.exists())
        {
            std::process::Command::new(register_exe)
                .arg("--auto")
                .creation_flags(CREATE_NO_WINDOW)
                .spawn()
                .ok();
        }
    }

    if let Some(parent_pid) = std::env::args()
        .skip_while(|arg| arg != "--parent")
        .nth(1)
        .and_then(|pid| pid.parse::<u32>().ok())
    {
        thread::spawn(move || wait_for_process_exit(parent_pid));
    }

    let listener = match TcpListener::bind(ipc::HOST_ADDR) {
        Ok(listener) => listener,
        // Existing host owns the port. Dashboard may launch us repeatedly.
        Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => return,
        Err(error) => {
            eprintln!("[alvr-host] bind {} failed: {error}", ipc::HOST_ADDR);
            std::process::exit(1);
        }
    };

    let install_dir = std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(ToOwned::to_owned))
        .unwrap_or_default();
    let layout = alvr_filesystem::Layout::new(&install_dir);
    alvr_server_core::initialize_environment(layout.clone());
    alvr_server_core::init_logging(None, Some(layout.crash_log()));
    alvr_server_core::set_driverless_mode(true);

    let (context, events) = ServerCoreContext::new();
    let context = Arc::new(context);

    let writer: Writer = Arc::new(Mutex::new(None));
    let snapshot = Arc::new(Mutex::new(Snapshot::default()));
    {
        let writer = Arc::clone(&writer);
        let context = Arc::clone(&context);
        let snapshot = Arc::clone(&snapshot);
        thread::spawn(move || {
            while let Ok(event) = events.recv() {
                match event {
                    ServerCoreEvent::ClientConnected => {
                        ensure_runtime_registered();
                        snapshot.lock().connected = true;
                        send(&writer, &HostEvent::ClientConnected);
                    }
                    ServerCoreEvent::ClientDisconnected => {
                        snapshot.lock().connected = false;
                        send(&writer, &HostEvent::ClientDisconnected);
                    }
                    ServerCoreEvent::RequestIDR => send(&writer, &HostEvent::RequestIdr),
                    ServerCoreEvent::ViewsConfig(config) => {
                        snapshot.lock().views = Some((config.local_view_transforms, config.fov));
                        send(
                            &writer,
                            &HostEvent::ViewsConfig {
                                local_view_transforms: config.local_view_transforms,
                                fov: config.fov,
                            },
                        );
                    }
                    ServerCoreEvent::Tracking { sample_timestamp } => {
                        if let Some(motion) = context.get_device_motion(*HEAD_ID, sample_timestamp)
                        {
                            // Tag the frame with sample_timestamp and render from the
                            // pose AT that same instant (no motion-to-photon predict).
                            // The client's async timewarp already reprojects from this
                            // pose to the current head pose; predicting to photon here
                            // too would double-compensate and make the image overshoot
                            // and diverge between the eyes when the head moves.
                            let head_pose = motion.pose;
                            let left_hand_pose = context
                                .get_device_motion(*HAND_LEFT_ID, sample_timestamp)
                                .map(|m| m.pose);
                            let right_hand_pose = context
                                .get_device_motion(*HAND_RIGHT_ID, sample_timestamp)
                                .map(|m| m.pose);
                            send(
                                &writer,
                                &HostEvent::Tracking {
                                    sample_timestamp_ns: sample_timestamp.as_nanos() as u64,
                                    head_pose,
                                    left_hand_pose,
                                    right_hand_pose,
                                },
                            );
                        }
                        if let Some(params) = context.get_dynamic_encoder_params() {
                            snapshot.lock().encoder_params =
                                Some((params.bitrate_bps as u64, params.framerate));
                            send(
                                &writer,
                                &HostEvent::DynamicEncoderParams {
                                    updated: true,
                                    bitrate_bps: params.bitrate_bps as u64,
                                    framerate: params.framerate,
                                },
                            );
                        }
                    }
                    ServerCoreEvent::Buttons(entries) => {
                        send(&writer, &HostEvent::Buttons(entries))
                    }
                    ServerCoreEvent::PlayspaceSync(_) => {
                        // Long-press Home / guardian recenter: tell the runtime to
                        // emit an OpenXR reference-space-change event so the app
                        // re-seats to the current head gaze.
                        send(&writer, &HostEvent::Recenter);
                    }
                    _ => {}
                }
            }
        });
    }

    // Keep the headset/server-core connection alive independently of any XR app.
    // Starting and stopping it for every runtime IPC attachment forced a full
    // headset renegotiation on rapid app restarts and caused intermittent gaps.
    // This mirrors VDXR's persistent service/session model: XR processes attach
    // to current live state instead of tearing the transport down.
    context.start_connection();

    let next_id = AtomicU64::new(1);
    for incoming in listener.incoming() {
        let Ok(mut stream) = incoming else { continue };
        stream.set_nodelay(true).ok();
        let Ok(writer_stream) = stream.try_clone() else {
            continue;
        };
        let id = next_id.fetch_add(1, Ordering::Relaxed);
        *writer.lock() = Some((id, writer_stream));

        // Runtime process state is not authoritative across IPC reconnects.
        // Clear cached buttons/tracking before replaying the persistent live
        // connection snapshot, exactly as VDXR reports inactive inputs false.
        send(&writer, &HostEvent::ResetInputs);

        // A replacement IPC connection can arrive before the previous reader
        // observes EOF. Replay the live state in that narrow overlap; normally
        // a new XR app starts a fresh headset connection below.
        {
            let snapshot = snapshot.lock();
            if snapshot.connected {
                send(&writer, &HostEvent::ClientConnected);
            }
            if let Some((local_view_transforms, fov)) = snapshot.views {
                send(
                    &writer,
                    &HostEvent::ViewsConfig {
                        local_view_transforms,
                        fov,
                    },
                );
            }
            if let Some((bitrate_bps, framerate)) = snapshot.encoder_params {
                send(
                    &writer,
                    &HostEvent::DynamicEncoderParams {
                        updated: true,
                        bitrate_bps,
                        framerate,
                    },
                );
            }
        }

        let writer = Arc::clone(&writer);
        let context = Arc::clone(&context);
        thread::spawn(move || {
            while let Ok(event) = ipc::receive::<RuntimeEvent>(&mut stream) {
                if writer
                    .lock()
                    .as_ref()
                    .is_none_or(|(current_id, _)| *current_id != id)
                {
                    break;
                }
                match event {
                    RuntimeEvent::VideoConfig { buffer, codec } => {
                        let codec = match codec {
                            0 => CodecType::H264,
                            1 => CodecType::Hevc,
                            _ => CodecType::AV1,
                        };
                        context.set_video_config_nals(buffer, codec);
                    }
                    RuntimeEvent::VideoNal {
                        timestamp_ns,
                        buffer,
                        is_idr,
                    } => context.send_video_nal(
                        std::time::Duration::from_nanos(timestamp_ns),
                        buffer,
                        is_idr,
                    ),
                    RuntimeEvent::ReportComposed {
                        timestamp_ns,
                        offset_ns,
                    } => context.report_composed(
                        std::time::Duration::from_nanos(timestamp_ns),
                        std::time::Duration::from_nanos(offset_ns),
                    ),
                    RuntimeEvent::ReportPresent {
                        timestamp_ns,
                        offset_ns,
                    } => context.report_present(
                        std::time::Duration::from_nanos(timestamp_ns),
                        std::time::Duration::from_nanos(offset_ns),
                    ),
                }
            }

            {
                let mut guard = writer.lock();
                let is_current = guard
                    .as_ref()
                    .is_some_and(|(current_id, _)| *current_id == id);
                if is_current {
                    *guard = None;
                }
            }
            // Keep server-core/headset connected. The next XR process receives
            // ResetInputs followed by the live snapshot.
        });
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
    std::process::exit(0);
}

#[cfg(not(windows))]
fn main() {
    eprintln!("The persistent OpenXR streamer host is Windows-only.");
}
