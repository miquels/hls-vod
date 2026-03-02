//! Variant playlist generator
//!
//! Generates HLS variant playlists for video, audio, and subtitles.

use super::codec::*;
use crate::media::StreamIndex;

/// Generate video variant playlist
///
/// Creates video.m3u8 with segment references
pub(crate) fn generate_video_playlist(index: &StreamIndex) -> String {
    let mut output = String::new();

    // Calculate target duration
    let target_duration = calculate_target_duration(&index.segments);

    // Header
    output.push_str("#EXTM3U\n");
    output.push_str("#EXT-X-VERSION:7\n");
    output.push_str(&format!("#EXT-X-TARGETDURATION:{}\n", target_duration));
    output.push_str("#EXT-X-MEDIA-SEQUENCE:0\n");
    output.push_str("#EXT-X-PLAYLIST-TYPE:VOD\n");
    output.push_str("#EXT-X-INDEPENDENT-SEGMENTS\n");
    let video_index = index.primary_video().map(|v| v.stream_index).unwrap_or(0);
    let init_seg = crate::params::VideoSegment {
        track_id: video_index,
        audio_track_id: None,
        audio_transcode_to: None,
        segment_id: None,
    };
    // EXT-X-MAP points to video init segment
    output.push_str(&format!("#EXT-X-MAP:URI=\"{}\"\n", init_seg));
    output.push('\n');

    // Generate segment entries
    for segment in &index.segments {
        let seg = crate::params::VideoSegment {
            track_id: video_index,
            audio_track_id: None,
            audio_transcode_to: None,
            segment_id: Some(segment.sequence),
        };
        output.push_str(&format!("#EXTINF:{:.3},\n", segment.duration_secs));
        output.push_str(&format!("{}\n", seg));
    }

    // End list
    output.push_str("#EXT-X-ENDLIST\n");

    output
}

/// Generate audio variant playlist
///
/// Creates a/<track_index>.m3u8 with segment references
pub(crate) fn generate_audio_playlist(
    index: &StreamIndex,
    track_index: usize,
    requested_transcode: Option<&str>,
) -> String {
    let mut output = String::new();

    // Calculate target duration
    let target_duration = calculate_target_duration(&index.segments);

    // Header
    output.push_str("#EXTM3U\n");
    output.push_str("#EXT-X-VERSION:7\n");
    output.push_str(&format!("#EXT-X-TARGETDURATION:{}\n", target_duration));
    output.push_str("#EXT-X-MEDIA-SEQUENCE:0\n");
    output.push_str("#EXT-X-PLAYLIST-TYPE:VOD\n");
    output.push_str("#EXT-X-INDEPENDENT-SEGMENTS\n");

    let transcode_to = requested_transcode.map(String::from).or_else(|| {
        index
            .get_audio_stream(track_index)
            .ok()
            .and_then(|s| s.transcode_to)
            .and_then(codec_name_short)
            .map(String::from)
    });

    let init_seg = crate::params::AudioSegment {
        track_id: track_index,
        transcode_to: transcode_to.clone(),
        segment_id: None,
    };

    // EXT-X-MAP points to init segment for CMAF-style HLS
    output.push_str(&format!("#EXT-X-MAP:URI=\"{}\"\n", init_seg));
    output.push('\n');

    // Generate segment entries
    for segment in &index.segments {
        let seg = crate::params::AudioSegment {
            track_id: track_index,
            transcode_to: transcode_to.clone(),
            segment_id: Some(segment.sequence),
        };
        output.push_str(&format!("#EXTINF:{:.3},\n", segment.duration_secs));
        output.push_str(&format!("{}\n", seg));
    }

    // End list
    output.push_str("#EXT-X-ENDLIST\n");

    output
}

/// Generate interleaved audio-video variant playlist
///
/// Creates v/<video_idx>.<audio_idx>.media.m3u8 with references to muxed A/V segments
pub(crate) fn generate_interleaved_playlist(
    index: &StreamIndex,
    video_idx: usize,
    audio_idx: usize,
    requested_audio_transcode: Option<&str>,
) -> String {
    let mut output = String::new();

    // Calculate target duration
    let target_duration = calculate_target_duration(&index.segments);

    // Header
    output.push_str("#EXTM3U\n");
    output.push_str("#EXT-X-VERSION:7\n");
    output.push_str(&format!("#EXT-X-TARGETDURATION:{}\n", target_duration));
    output.push_str("#EXT-X-MEDIA-SEQUENCE:0\n");
    output.push_str("#EXT-X-PLAYLIST-TYPE:VOD\n");
    output.push_str("#EXT-X-INDEPENDENT-SEGMENTS\n");

    let audio_transcode_to = requested_audio_transcode.map(String::from).or_else(|| {
        index
            .get_audio_stream(audio_idx)
            .ok()
            .and_then(|s| s.transcode_to)
            .and_then(codec_name_short)
            .map(String::from)
    });

    let init_seg = crate::params::VideoSegment {
        track_id: video_idx,
        audio_track_id: Some(audio_idx),
        audio_transcode_to: audio_transcode_to.clone(),
        segment_id: None,
    };

    // EXT-X-MAP points to interleaved init segment
    output.push_str(&format!("#EXT-X-MAP:URI=\"{}\"\n", init_seg));
    output.push('\n');

    // Generate segment entries
    for segment in &index.segments {
        let seg = crate::params::VideoSegment {
            track_id: video_idx,
            audio_track_id: Some(audio_idx),
            audio_transcode_to: audio_transcode_to.clone(),
            segment_id: Some(segment.sequence),
        };
        output.push_str(&format!("#EXTINF:{:.3},\n", segment.duration_secs));
        output.push_str(&format!("{}\n", seg));
    }

    // End list
    output.push_str("#EXT-X-ENDLIST\n");

    output
}

/// Generate subtitle variant playlist
///
/// Creates s/<track_index>.m3u8 with WebVTT segment references
pub(crate) fn generate_subtitle_playlist(index: &StreamIndex, track_index: usize) -> String {
    let mut output = String::new();

    // Find the subtitle stream info to check for non-empty sequences
    let sub_info = index
        .subtitle_streams
        .iter()
        .find(|s| s.stream_index == track_index);

    let _is_non_empty = |sequence: usize| -> bool {
        if let Some(info) = sub_info {
            // If the list is empty, it means we haven't scanned (or there are no subtitles at all).
            // We'll be safe and include them if the list is completely empty (legacy behavior),
            // but the scanner now populates it, so if it's empty, we drop all.
            // Wait, if the scanner hasn't run, `non_empty_sequences` is empty.
            // But if it has run and there are no subtitles, it's also empty.
            // Since we populate it during indexing, it's safe to strictly require presence.
            info.non_empty_sequences.binary_search(&sequence).is_ok()
        } else {
            true // Fallback if stream info not found (shouldn't happen)
        }
    };

    // Generate segment entries merging durations of consecutive empty segments
    // to keep the timeline consistent without using EXT-X-GAP (fixes VLC compatibility).
    // The user requested: cap any merged segments at 30 seconds.
    let mut merged_segments = Vec::new();
    let mut accumulated_duration = 0.0;
    let mut accumulated_start_seq = None;

    for segment in &index.segments {
        let is_empty = if let Some(stream_info) = index
            .subtitle_streams
            .iter()
            .find(|s| s.stream_index == track_index)
        {
            stream_info
                .non_empty_sequences
                .binary_search(&segment.sequence)
                .is_err()
        } else {
            false
        };

        if accumulated_start_seq.is_none() {
            accumulated_start_seq = Some(segment.sequence);
        }

        // Check if adding this segment would exceed our 30-second cap
        if accumulated_duration > 0.0 && accumulated_duration + segment.duration_secs > 30.0 {
            let start_s = accumulated_start_seq.unwrap();
            let end_s = segment.sequence.saturating_sub(1);
            let end_s = std::cmp::max(start_s, end_s);

            merged_segments.push((start_s, end_s, accumulated_duration));
            accumulated_duration = 0.0;
            accumulated_start_seq = Some(segment.sequence);
        }

        accumulated_duration += segment.duration_secs;

        // Flush immediately if it is a non-empty segment so it doesn't get inappropriately swallowed by subsequent empty segments
        if !is_empty {
            let start_s = accumulated_start_seq.unwrap();
            merged_segments.push((start_s, segment.sequence, accumulated_duration));

            accumulated_duration = 0.0;
            accumulated_start_seq = None;
        }
    }

    if accumulated_duration > 0.0 {
        let start_s = accumulated_start_seq.unwrap_or(0);
        let last_s = index.segments.last().map(|s| s.sequence).unwrap_or(0);
        merged_segments.push((start_s, last_s, accumulated_duration));
    }

    // Calculate dynamic target duration from the merged segments (capped at 30)
    let mut max_duration = 0.0_f64;
    for &(_, _, dur) in &merged_segments {
        if dur > max_duration {
            max_duration = dur;
        }
    }
    let target_duration = std::cmp::max(
        max_duration.ceil() as u32,
        crate::playlist::variant::calculate_target_duration(&index.segments), // fallback to standard video target if smaller
    );

    // Header
    output.push_str("#EXTM3U\n");
    output.push_str("#EXT-X-VERSION:7\n");
    output.push_str(&format!("#EXT-X-TARGETDURATION:{}\n", target_duration));
    output.push_str("#EXT-X-MEDIA-SEQUENCE:0\n");
    output.push_str("#EXT-X-PLAYLIST-TYPE:VOD\n");
    output.push('\n');

    for (start_s, end_s, dur) in merged_segments {
        let seg = crate::params::VttSegment {
            track_id: track_index,
            start_cue: start_s,
            end_cue: end_s,
        };
        output.push_str(&format!("#EXTINF:{:.6},\n", dur));
        output.push_str(&format!("{}\n", seg));
    }

    // End list
    output.push_str("#EXT-X-ENDLIST\n");

    output
}

/// Calculate target duration from segments
pub fn calculate_target_duration(segments: &[crate::media::SegmentInfo]) -> u32 {
    if segments.is_empty() {
        return 6; // Default
    }

    let max_duration = segments
        .iter()
        .map(|s| s.duration_secs)
        .fold(0.0f64, |a, b| a.max(b));

    // Round up to nearest integer, minimum 6
    (max_duration.ceil() as u32).max(6)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::{AudioStreamInfo, SegmentInfo, StreamIndex, VideoStreamInfo};
    use ffmpeg_next as ffmpeg;
    use std::path::PathBuf;

    fn create_test_index() -> StreamIndex {
        let mut index = StreamIndex::new(PathBuf::from("/test/video.mp4"));

        index.video_streams.push(VideoStreamInfo {
            stream_index: 0,
            codec_id: ffmpeg::codec::Id::H264,
            width: 1920,
            height: 1080,
            bitrate: 5000000,
            framerate: ffmpeg::Rational::new(30, 1),
            language: None,
            profile: None,
            level: None,
        });

        index.audio_streams.push(AudioStreamInfo {
            stream_index: 1,
            codec_id: ffmpeg::codec::Id::AAC,
            sample_rate: 48000,
            channels: 2,
            bitrate: 128000,
            language: Some("en".to_string()),
            transcode_to: None,
            encoder_delay: 0,
        });

        index.segments.push(SegmentInfo {
            sequence: 0,
            start_pts: 0,
            end_pts: 90000,
            duration_secs: 4.0,
            is_keyframe: true,
            video_byte_offset: 0,
        });
        index.segments.push(SegmentInfo {
            sequence: 1,
            start_pts: 90000,
            end_pts: 180000,
            duration_secs: 4.0,
            is_keyframe: true,
            video_byte_offset: 1000,
        });

        index
    }

    #[test]
    fn test_generate_video_playlist() {
        let index = create_test_index();
        let playlist = generate_video_playlist(&index);

        assert!(playlist.contains("#EXTM3U"));
        assert!(playlist.contains("#EXT-X-VERSION:7"));
        assert!(playlist.contains("#EXT-X-TARGETDURATION:6"));
        assert!(playlist.contains("#EXT-X-PLAYLIST-TYPE:VOD"));
        assert!(playlist.contains("#EXT-X-ENDLIST"));
        assert!(playlist.contains("0.0.m4s"));
        assert!(playlist.contains("0.1.m4s"));
    }

    #[test]
    fn test_generate_audio_playlist() {
        let index = create_test_index();
        let playlist = generate_audio_playlist(&index, 1, None);

        assert!(playlist.contains("#EXTM3U"));
        assert!(playlist.contains("#EXT-X-VERSION:7"));
        assert!(playlist.contains("1.0.m4s"));
        assert!(playlist.contains("1.1.m4s"));
        assert!(playlist.contains("#EXT-X-ENDLIST"));
    }

    #[test]
    fn test_generate_subtitle_playlist() {
        let index = create_test_index();
        let playlist = generate_subtitle_playlist(&index, 2);

        assert!(playlist.contains("#EXTM3U"));
        assert!(playlist.contains("#EXT-X-VERSION:7"));
        assert!(playlist.contains("2.0-0.vtt"));
        assert!(playlist.contains("2.1-1.vtt"));
        assert!(playlist.contains("#EXT-X-ENDLIST"));
    }

    #[test]
    fn test_calculate_target_duration() {
        let segments = vec![
            SegmentInfo {
                sequence: 0,
                start_pts: 0,
                end_pts: 90000,
                duration_secs: 4.0,
                is_keyframe: true,
                video_byte_offset: 0,
            },
            SegmentInfo {
                sequence: 1,
                start_pts: 90000,
                end_pts: 180000,
                duration_secs: 5.5,
                is_keyframe: true,
                video_byte_offset: 1000,
            },
        ];

        assert_eq!(calculate_target_duration(&segments), 6);

        let segments = vec![SegmentInfo {
            sequence: 0,
            start_pts: 0,
            end_pts: 90000,
            duration_secs: 10.0,
            is_keyframe: true,
            video_byte_offset: 0,
        }];

        assert_eq!(calculate_target_duration(&segments), 10);
    }
}
