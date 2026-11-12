//! Local IPC between the persistent streamer host and the in-app OpenXR runtime.
//!
//! One framed bincode stream in each direction. The host owns `server_core` and
//! the headset connection; the runtime owns graphics/encoding and may disappear
//! when an OpenXR app exits without disconnecting the headset.

use alvr_common::{Fov, Pose};
use alvr_packets::ButtonEntry;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::{
    io::{self, Read, Write},
    net::TcpStream,
    sync::atomic::{AtomicBool, Ordering},
};

pub const HOST_ADDR: &str = "127.0.0.1:9941";
const MAX_MESSAGE_SIZE: usize = 64 * 1024 * 1024;

#[derive(Serialize, Deserialize)]
pub enum RuntimeEvent {
    VideoConfig {
        buffer: Vec<u8>,
        codec: i32,
    },
    VideoNal {
        timestamp_ns: u64,
        buffer: Vec<u8>,
        is_idr: bool,
    },
    ReportComposed {
        timestamp_ns: u64,
        offset_ns: u64,
    },
    ReportPresent {
        timestamp_ns: u64,
        offset_ns: u64,
    },
}

#[derive(Serialize, Deserialize)]
pub enum HostEvent {
    /// A new runtime IPC consumer attached. Drop all cached input state before
    /// replaying the persistent headset connection snapshot.
    ResetInputs,
    ClientConnected,
    ClientDisconnected,
    RequestIdr,
    ViewsConfig {
        local_view_transforms: [Pose; 2],
        fov: [Fov; 2],
    },
    Tracking {
        sample_timestamp_ns: u64,
        head_pose: Pose,
        left_hand_pose: Option<Pose>,
        right_hand_pose: Option<Pose>,
    },
    Buttons(Vec<ButtonEntry>),
    DynamicEncoderParams {
        updated: bool,
        bitrate_bps: u64,
        framerate: f32,
    },
    /// The client requested a playspace recenter (e.g. long-press Home). The
    /// runtime turns this into an OpenXR `REFERENCE_SPACE_CHANGE_PENDING` event
    /// so apps like Desktop2Stereo re-seat their screen to the current gaze.
    Recenter,
}

pub fn send<T: Serialize>(stream: &mut TcpStream, message: &T) -> io::Result<()> {
    let payload = bincode::serde::encode_to_vec(message, bincode::config::standard())
        .map_err(io::Error::other)?;
    if payload.len() > MAX_MESSAGE_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "IPC message too large",
        ));
    }

    stream.write_all(&(payload.len() as u32).to_le_bytes())?;
    stream.write_all(&payload)
}

pub fn receive<T: DeserializeOwned>(stream: &mut TcpStream) -> io::Result<T> {
    receive_with(|buffer| stream.read_exact(buffer))
}

/// Preserve partial frames across read timeouts while allowing a worker to stop.
/// The stream must have a finite read timeout.
pub fn receive_interruptible<T: DeserializeOwned>(
    stream: &mut TcpStream,
    stop: &AtomicBool,
) -> io::Result<T> {
    receive_with(|mut buffer| {
        while !buffer.is_empty() {
            if stop.load(Ordering::Acquire) {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "IPC worker stopped",
                ));
            }
            match stream.read(buffer) {
                Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(count) => buffer = &mut buffer[count..],
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::Interrupted
                            | io::ErrorKind::WouldBlock
                            | io::ErrorKind::TimedOut
                    ) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    })
}

fn receive_with<T: DeserializeOwned>(
    mut read_exact: impl FnMut(&mut [u8]) -> io::Result<()>,
) -> io::Result<T> {
    let mut length = [0u8; 4];
    read_exact(&mut length)?;
    let length = u32::from_le_bytes(length) as usize;
    if length > MAX_MESSAGE_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "IPC message too large",
        ));
    }

    let mut payload = vec![0u8; length];
    read_exact(&mut payload)?;
    bincode::serde::decode_from_slice(&payload, bincode::config::standard())
        .map(|(message, _)| message)
        .map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn framed_message_round_trips() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let thread = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let RuntimeEvent::ReportPresent {
                timestamp_ns,
                offset_ns,
            } = receive(&mut stream).unwrap()
            else {
                panic!("wrong event")
            };
            assert_eq!((timestamp_ns, offset_ns), (12, 34));
        });

        let mut stream = TcpStream::connect(address).unwrap();
        send(
            &mut stream,
            &RuntimeEvent::ReportPresent {
                timestamp_ns: 12,
                offset_ns: 34,
            },
        )
        .unwrap();
        thread.join().unwrap();
    }
}
