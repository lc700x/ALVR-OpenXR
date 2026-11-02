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

#[cfg(any(target_os = "android", test))]
fn duration_to_media_codec_timestamp_us(timestamp: Duration) -> u64 {
    timestamp.as_micros() as u64
}

#[cfg(test)]
fn media_codec_timestamp_us_to_render_ns(timestamp_us: i64) -> i64 {
    timestamp_us.saturating_mul(1_000)
}

#[cfg(any(target_os = "android", test))]
#[derive(Default)]
struct DecoderTimestamps {
    last_pts_us: u64,
    pending: std::collections::BTreeMap<u64, Duration>,
}

#[cfg(any(target_os = "android", test))]
impl DecoderTimestamps {
    fn reserve(&mut self, timestamp: Duration) -> Option<u64> {
        let pts =
            duration_to_media_codec_timestamp_us(timestamp).max(self.last_pts_us.checked_add(1)?);
        self.last_pts_us = pts;
        if self.pending.len() >= 1024 {
            self.pending.pop_first();
        }
        self.pending.insert(pts, timestamp);
        Some(pts)
    }

    fn take(&mut self, pts_us: u64) -> Option<Duration> {
        self.pending.remove(&pts_us)
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
    fn decoder_pts_preserve_frame_identity_and_reordered_output() {
        let mut timestamps = DecoderTimestamps::default();
        let a = Duration::from_nanos(1_826_299_205_123);
        let b = a + Duration::from_nanos(1);
        let pa = timestamps.reserve(a).unwrap();
        let pb = timestamps.reserve(b).unwrap();
        let duplicate = timestamps.reserve(a).unwrap();
        assert!(pa < pb && pb < duplicate);
        assert_eq!(timestamps.take(pb), Some(b));
        assert_eq!(timestamps.take(pa), Some(a));
        assert_eq!(timestamps.take(duplicate), Some(a));
        assert_eq!(timestamps.take(pa), None);
    }

    #[test]
    fn decoded_identity_matches_statistics_history_exactly() {
        let original = Duration::from_nanos(1_826_299_205_123);
        let mut timestamps = DecoderTimestamps::default();
        let mut stats =
            crate::statistics::StatisticsManager::new(32, Duration::from_millis(14), 0.0);
        stats.report_input_acquired(original);
        stats.report_video_packet_received(original);
        let pts = timestamps.reserve(original).unwrap();
        let restored = timestamps.take(pts).unwrap();
        stats.report_frame_decoded(restored);
        stats.report_compositor_start(restored);
        stats.report_submit(restored, Duration::ZERO);
        assert_eq!(stats.summary(restored).unwrap().target_timestamp, original);
        assert!(stats.summary(Duration::from_micros(pts)).is_none());
    }

    #[test]
    fn decoder_pts_bound_pending_history_and_cancel_failed_enqueue() {
        let mut timestamps = DecoderTimestamps::default();
        let first = timestamps.reserve(Duration::from_nanos(123)).unwrap();
        for n in 1..=1024 {
            timestamps.reserve(Duration::from_millis(n)).unwrap();
        }
        assert_eq!(timestamps.pending.len(), 1024);
        assert_eq!(timestamps.take(first), None);
        let failed = timestamps.reserve(Duration::from_secs(2)).unwrap();
        assert!(timestamps.take(failed).is_some());
        assert_eq!(timestamps.take(failed), None);
        timestamps = DecoderTimestamps::default();
        assert!(timestamps.pending.is_empty());
        assert_eq!(timestamps.last_pts_us, 0);
    }

    #[test]
    fn hvr_uses_triple_buffered_decoder_surface() {
        assert_eq!(decoder_max_images_for_platform(Platform::Hvr), 3);
        assert_eq!(decoder_max_images_for_platform(Platform::Quest2), 10);
        assert_eq!(decoder_max_images_for_platform(Platform::Pico4), 10);
        assert_eq!(decoder_max_images_for_platform(Platform::Qiyu), 10);
    }

    #[test]
    fn media_codec_timestamps_convert_between_microseconds_and_nanoseconds() {
        let timestamp = Duration::from_nanos(1_826_299_205_000);
        let timestamp_us = duration_to_media_codec_timestamp_us(timestamp);

        assert_eq!(timestamp_us, 1_826_299_205);
        assert_eq!(
            media_codec_timestamp_us_to_render_ns(timestamp_us as i64),
            timestamp.as_nanos() as i64
        );
    }
}

/// Diagnostic video counts are separate from the headset compositor refresh rate.
pub fn report_video_perf(presented: bool) {
    use std::sync::{Mutex, OnceLock};
    use std::time::Instant;
    static ENABLED: OnceLock<bool> = OnceLock::new();
    static COUNTS: OnceLock<Mutex<(Instant, u64, u64)>> = OnceLock::new();
    if !*ENABLED.get_or_init(|| std::env::var("ALVR_OPENXR_PERF").as_deref() == Ok("1")) {
        return;
    }
    let mut counts = COUNTS
        .get_or_init(|| Mutex::new((Instant::now(), 0, 0)))
        .lock()
        .unwrap();
    if presented {
        counts.2 += 1;
    } else {
        counts.1 += 1;
    }
    let seconds = counts.0.elapsed().as_secs_f64();
    if seconds >= 5.0 {
        alvr_common::info!(
            "[ALVR-PERF] video decoded={:.1}fps presented={:.1}fps",
            counts.1 as f64 / seconds,
            counts.2 as f64 / seconds
        );
        *counts = (Instant::now(), 0, 0);
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
