//! Test fixtures for integration tests
//!
//! Provides mock media file information for testing without actual media files.

use crate::ffmpeg_utils::ffmpeg;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, AtomicU64};
// use std::sync::Arc; // Commented out as per instruction
// use MediaInfo; // Commented out as per instruction
// use crate::ffmpeg_utils::ffmpeg::Rational; // Commented out as per instruction
use crate::media::{
    AudioStreamInfo, SegmentInfo, StreamIndex, SubtitleFormat, SubtitleStreamInfo, VideoStreamInfo,
};

/// Test media file information
#[derive(Debug, Clone)]
pub struct TestMediaInfo {
    pub name: &'static str,
    #[allow(dead_code)]
    pub description: &'static str,
    pub has_video: bool,
    pub has_audio: bool,
    pub has_subtitles: bool,
    pub video_codec: Option<ffmpeg::codec::Id>,
    pub audio_codecs: Vec<ffmpeg::codec::Id>,
    pub subtitle_formats: Vec<ffmpeg::codec::Id>,
    pub duration_secs: f64,
    // pub expected_segments: usize, // Commented out as per instruction
}

impl TestMediaInfo {
    /// AAC audio only - no transcoding needed
    pub fn aac_only() -> Self {
        Self {
            name: "aac_only",
            description: "MP4 with AAC audio (no transcoding needed)",
            has_video: true,
            has_audio: true,
            has_subtitles: false,
            video_codec: Some(ffmpeg::codec::Id::H264),
            audio_codecs: vec![ffmpeg::codec::Id::AAC],
            subtitle_formats: vec![],
            duration_secs: 60.0,
            // expected_segments: 15, // 60s / 4s per segment // Commented out as per instruction
        }
    }

    /// AC-3 audio only - requires transcoding
    pub fn ac3_only() -> Self {
        Self {
            name: "ac3_only",
            description: "MP4 with AC-3 audio (requires AAC transcode)",
            has_video: true,
            has_audio: true,
            has_subtitles: false,
            video_codec: Some(ffmpeg::codec::Id::H264),
            audio_codecs: vec![ffmpeg::codec::Id::AC3],
            subtitle_formats: vec![],
            duration_secs: 60.0,
            // expected_segments: 15, // Commented out as per instruction
        }
    }

    /// Multiple audio tracks
    pub fn multi_audio() -> Self {
        Self {
            name: "multi_audio",
            description: "MP4 with multiple audio tracks (AAC + AC-3)",
            has_video: true,
            has_audio: true,
            has_subtitles: false,
            video_codec: Some(ffmpeg::codec::Id::H264),
            audio_codecs: vec![ffmpeg::codec::Id::AAC, ffmpeg::codec::Id::AC3],
            subtitle_formats: vec![],
            duration_secs: 60.0,
            // expected_segments: 15, // Commented out as per instruction
        }
    }

    /// With subtitles
    pub fn with_subtitles() -> Self {
        Self {
            name: "with_subtitles",
            description: "MP4 with SubRip subtitles",
            has_video: true,
            has_audio: true,
            has_subtitles: true,
            video_codec: Some(ffmpeg::codec::Id::H264),
            audio_codecs: vec![ffmpeg::codec::Id::AAC],
            subtitle_formats: vec![ffmpeg::codec::Id::SUBRIP],
            duration_secs: 60.0,
            // expected_segments: 15, // Commented out as per instruction
        }
    }

    /// Multi-language audio and subtitles
    pub fn multi_language() -> Self {
        Self {
            name: "multi_language",
            description: "MP4 with multiple audio languages and subtitles",
            has_video: true,
            has_audio: true,
            has_subtitles: true,
            video_codec: Some(ffmpeg::codec::Id::H264),
            audio_codecs: vec![
                ffmpeg::codec::Id::AAC, // English
                ffmpeg::codec::Id::AAC, // Spanish
            ],
            subtitle_formats: vec![
                ffmpeg::codec::Id::SUBRIP, // English
                ffmpeg::codec::Id::SUBRIP, // Spanish
            ],
            duration_secs: 60.0,
            // expected_segments: 15, // Commented out as per instruction
        }
    }

    /// Create a mock StreamIndex for testing
    pub fn create_mock_index(&self) -> StreamIndex {
        let mut index = StreamIndex {
            stream_id: uuid::Uuid::new_v4().to_string(),
            source_path: PathBuf::from(format!("/test/{}.mp4", self.name)),
            duration_secs: self.duration_secs,
            video_timebase: ffmpeg::Rational::new(1, 90000),
            video_streams: Vec::new(),
            audio_streams: Vec::new(),
            subtitle_streams: Vec::new(),
            segments: Vec::new(),
            indexed_at: std::time::SystemTime::now(),
            last_accessed: AtomicU64::new(0),
            segment_first_pts: std::sync::Arc::new(Vec::new()),
            cached_context: None,
            cache_enabled: true,
            last_requested_segment: std::sync::atomic::AtomicI64::new(-1),
            lookahead_queue: std::sync::Mutex::new(std::collections::VecDeque::new()),
        };

        // Add video stream
        if self.has_video {
            if self.video_codec.is_some() {
                index.video_streams.push(VideoStreamInfo {
                    stream_index: 0,
                    codec_id: ffmpeg::codec::Id::H264,
                    width: 1920,
                    height: 1080,
                    bitrate: 5000000,
                    framerate: ffmpeg::Rational::new(24, 1),
                    language: Some("eng".to_string()),
                    profile: None,
                    level: None,
                });
            }
        }

        // Add audio streams
        let mut audio_index = 1;
        for (i, &codec) in self.audio_codecs.iter().enumerate() {
            let language = match i {
                0 => Some("en".to_string()),
                1 => Some("es".to_string()),
                _ => Some("und".to_string()),
            };

            index.audio_streams.push(AudioStreamInfo {
                stream_index: audio_index,
                codec_id: codec,
                sample_rate: 48000,
                channels: 2,
                bitrate: 128000,
                language,
                transcode_to: None,
                encoder_delay: 0,
            });
            audio_index += 1;
        }

        // Add segments
        let segment_duration = 4.0;
        let num_segments = (self.duration_secs / segment_duration).ceil() as usize;

        // Add subtitle streams
        let mut sub_index = audio_index;
        for (i, &codec) in self.subtitle_formats.iter().enumerate() {
            let language = match i {
                0 => Some("en".to_string()),
                1 => Some("es".to_string()),
                _ => Some("und".to_string()),
            };

            index.subtitle_streams.push(SubtitleStreamInfo {
                stream_index: sub_index,
                codec_id: codec,
                language,
                format: SubtitleFormat::SubRip,
                non_empty_sequences: (0..num_segments).collect(),
                sample_index: Vec::new(),
                timebase: ffmpeg::Rational::new(1, 1000),
                start_time: 0,
            });
            sub_index += 1;
        }

        for i in 0..num_segments {
            let start_pts = (i as f64 * segment_duration * 90000.0) as i64;
            let end_pts = ((i + 1) as f64 * segment_duration * 90000.0) as i64;

            index.segments.push(SegmentInfo {
                sequence: i,
                start_pts,
                end_pts,
                duration_secs: segment_duration,
                is_keyframe: true,
                video_byte_offset: (i * 100000) as u64,
            });
        }

        // Initialize segment_first_pts
        let n = index.segments.len();
        let v: Vec<AtomicI64> = (0..n).map(|_| AtomicI64::new(i64::MIN)).collect();
        index.segment_first_pts = std::sync::Arc::new(v);

        index
    }

    pub fn create_mock_media(&self) -> crate::media::StreamIndex {
        let index = self.create_mock_index();

        crate::media::register_test_stream(std::sync::Arc::new(index.clone()));
        index
    }
}

/// Create test fixture for AAC-only media
pub fn fixture_aac_only() -> TestMediaInfo {
    TestMediaInfo::aac_only()
}

/// Create test fixture for AC-3 media
pub fn fixture_ac3_only() -> TestMediaInfo {
    TestMediaInfo::ac3_only()
}

/// Create test fixture for multi-audio media
pub fn fixture_multi_audio() -> TestMediaInfo {
    TestMediaInfo::multi_audio()
}

/// Create test fixture for media with subtitles
#[allow(dead_code)] // Added as per instruction
pub fn fixture_with_subtitles() -> TestMediaInfo {
    TestMediaInfo::with_subtitles()
}

/// Create test fixture for multi-language media
pub fn fixture_multi_language() -> TestMediaInfo {
    TestMediaInfo::multi_language()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fixture_aac_only() {
        let fixture = fixture_aac_only();
        assert_eq!(fixture.name, "aac_only");
        assert!(fixture.has_video);
        assert!(fixture.has_audio);
        assert!(!fixture.has_subtitles);
        assert_eq!(fixture.audio_codecs.len(), 1);
        assert_eq!(fixture.audio_codecs[0], ffmpeg::codec::Id::AAC);
    }

    #[test]
    fn test_fixture_ac3_only() {
        let fixture = fixture_ac3_only();
        assert_eq!(fixture.name, "ac3_only");
        assert_eq!(fixture.audio_codecs[0], ffmpeg::codec::Id::AC3);
    }

    #[test]
    fn test_fixture_multi_audio() {
        let fixture = fixture_multi_audio();
        assert_eq!(fixture.audio_codecs.len(), 2);
    }

    #[test]
    fn test_create_mock_index() {
        let fixture = fixture_aac_only();
        let index = fixture.create_mock_index();

        assert_eq!(index.video_streams.len(), 1);
        assert_eq!(index.audio_streams.len(), 1);
        assert_eq!(index.subtitle_streams.len(), 0);
        assert!(index.segments.len() > 0);
        assert!((index.duration_secs - 60.0).abs() < 0.1);
    }

    #[test]
    fn test_mock_index_multi_language() {
        let fixture = fixture_multi_language();
        let index = fixture.create_mock_index();

        assert_eq!(index.audio_streams.len(), 2);
        assert_eq!(index.subtitle_streams.len(), 2);
        assert_eq!(index.audio_streams[0].language, Some("en".to_string()));
        assert_eq!(index.audio_streams[1].language, Some("es".to_string()));
    }
}
