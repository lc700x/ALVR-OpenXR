#[cfg(target_os = "android")]
mod android;

use alvr_common::anyhow::Result;
use alvr_session::{CodecType, MediacodecProperty};
#[cfg(any(target_os = "android", test))]
use alvr_system_info::Platform;
use std::time::Duration;

#[cfg(any(target_os = "android", test))]
const DEFAULT_DECODER_MAX_IMAGES: i32 = 10;
#[cfg(any(target_os = "android", test))]
const HVR_DECODER_MAX_IMAGES: i32 = 3;

#[cfg(any(target_os = "android", test))]
fn decoder_max_images_for_platform(platform: Platform) -> i32 {
    if platform == Platform::Hvr {
        HVR_DECODER_MAX_IMAGES
    } else {
        DEFAULT_DECODER_MAX_IMAGES
    }
}

#[cfg(target_os = "android")]
fn decoder_max_images() -> i32 {
    decoder_max_images_for_platform(alvr_system_info::platform())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hvr_uses_triple_buffered_decoder_surface() {
        assert_eq!(decoder_max_images_for_platform(Platform::Hvr), 3);
        assert_eq!(decoder_max_images_for_platform(Platform::Quest2), 10);
        assert_eq!(decoder_max_images_for_platform(Platform::Pico4), 10);
        assert_eq!(decoder_max_images_for_platform(Platform::Qiyu), 10);
    }
}

#[derive(Clone, Default, PartialEq)]
pub struct VideoDecoderConfig {
    pub codec: CodecType,
    pub force_software_decoder: bool,
    pub max_buffering_frames: f32,
    pub buffering_history_weight: f32,
    pub options: Vec<(String, MediacodecProperty)>,
    pub config_buffer: Vec<u8>,
}

pub struct VideoDecoderSink {
    #[cfg(target_os = "android")]
    inner: android::VideoDecoderSink,
}

impl VideoDecoderSink {
    // returns true if frame has been successfully enqueued
    #[allow(unused_variables)]
    pub fn push_nal(&mut self, timestamp: Duration, nal: &[u8]) -> bool {
        #[cfg(target_os = "android")]
        {
            alvr_common::show_err(self.inner.push_frame_nal(timestamp, nal)).unwrap_or(false)
        }
        #[cfg(not(target_os = "android"))]
        false
    }
}

pub struct VideoDecoderSource {
    #[cfg(target_os = "android")]
    inner: android::VideoDecoderSource,
}

impl VideoDecoderSource {
    /// If a frame is available, return the timestamp and the AHardwareBuffer.
    pub fn get_frame(&mut self) -> Option<(Duration, *mut std::ffi::c_void)> {
        #[cfg(target_os = "android")]
        {
            self.inner.dequeue_frame()
        }
        #[cfg(not(target_os = "android"))]
        None
    }
}

// report_frame_decoded: (target_timestamp: Duration) -> ()
#[allow(unused_variables)]
pub fn create_decoder(
    config: VideoDecoderConfig,
    report_frame_decoded: impl Fn(Result<Duration>) + Send + Sync + 'static,
) -> (VideoDecoderSink, VideoDecoderSource) {
    #[cfg(target_os = "android")]
    {
        let (sink, source) = android::video_decoder_split(
            config.clone(),
            config.config_buffer,
            report_frame_decoded,
        )
        .unwrap();

        (
            VideoDecoderSink { inner: sink },
            VideoDecoderSource { inner: source },
        )
    }
    #[cfg(not(target_os = "android"))]
    (VideoDecoderSink {}, VideoDecoderSource {})
}
