//! Master playlist generator
//!
//! Generates the master.m3u8 playlist that references all variant playlists.

use std::collections::{HashMap, HashSet};

use ffmpeg_next as ffmpeg;

use super::codec::*;
use crate::media::StreamIndex;

/// Generate master playlist content
///
/// The master playlist contains:
/// - One `#EXT-X-MEDIA` per audio track, grouped by codec family
///   (`GROUP-ID="audio-aac"`, `GROUP-ID="audio-ac3"`, etc.)
/// - One `#EXT-X-STREAM-INF` per audio codec group, all referencing the
///   same video variant playlist but differing in `AUDIO=` and `CODECS=`
/// - Subtitle MEDIA entries for text tracks
///
/// When `interleaved` is true and there's exactly one video and one audio track,
/// generates a single muxed audio-video playlist instead of separate tracks.
/// When `force_aac` is also true, the audio will be transcoded to AAC.
pub fn generate_master_playlist(
    index: &StreamIndex,
    video_url: &str,
    session_id: Option<&str>,
    codecs: &[String],
    tracks_enabled: &HashSet<usize>,
    transcode: &HashMap<usize, String>,
    interleaved: bool,
) -> String {
    let mut output = String::new();

    // Header
    output.push_str("#EXTM3U\n");
    output.push_str("#EXT-X-VERSION:7\n");
    output.push('\n');

    // Remove tracks that aren't enabled.
    let orig_index = index;
    let mut index = index.clone();
    index
        .audio_streams
        .retain(|a| tracks_enabled.contains(&a.stream_index));
    index
        .video_streams
        .retain(|v| tracks_enabled.contains(&v.stream_index));
    index
        .subtitle_streams
        .retain(|s| tracks_enabled.contains(&s.stream_index));

    // Mark tracks to be transcoded (audio only for now).
    for (idx, codec) in transcode.iter() {
        if let Some(t) = index.get_audio_stream_mut(*idx) {
            t.transcode_to = codec_id(codec);
        }
    }

    // Filter out unsupported codecs (only when a codec list was supplied).
    // When codecs is empty (no ?codecs= query param), keep all audio streams.
    let mut index = index.clone();
    if !codecs.is_empty() {
        index.audio_streams.retain(|s| {
            for codec in codecs {
                if let Some(codec_id) = codec_id(codec) {
                    if s.codec_id == codec_id || s.transcode_to == Some(codec_id) {
                        return true;
                    }
                }
            }
            false
        });
    }

    // Now, if we have no audio streams left, but 'aac' was
    // in the supported list, add transcoded streams.
    if index.audio_streams.is_empty() && !orig_index.audio_streams.is_empty() {
        let has_aac = codecs
            .iter()
            .filter_map(|c| codec_id(c))
            .any(|id| id == ffmpeg::codec::Id::AAC);
        if has_aac {
            let mut src_codec = None;
            for s in orig_index
                .audio_streams
                .iter()
                .filter(|a| tracks_enabled.contains(&a.stream_index))
            {
                if src_codec.is_none() {
                    src_codec = Some(s.codec_id);
                }
                if Some(s.codec_id) == src_codec {
                    let mut s = s.clone();
                    s.transcode_to = Some(ffmpeg::codec::Id::AAC);
                    index.audio_streams.push(s);
                }
            }
        }
    }

    /// Return the codec-family GROUP-ID for a given stream.
    // FIXME: codec_name_short can fail, not sure about the fallback to aac.
    // Probably better to filter out unknown codecs.
    fn group_id_for_stream(stream: &crate::media::AudioStreamInfo) -> String {
        let codec = stream.transcode_to.unwrap_or(stream.codec_id);
        format!("audio-{}", codec_name_short(codec).unwrap_or("aac"))
    }

    /// HLS codec string we advertise for a given group.
    fn codec_str_for_group(group_id: &str) -> String {
        let name = group_id.strip_prefix("audio-").unwrap();
        codec_name_normalized(name).unwrap_or(name.to_string())
    }

    // Skip separate audio tracks section when using interleaved mode
    // (audio is already muxed into the video stream)
    let skip_audio_section =
        interleaved && index.video_streams.len() == 1 && index.audio_streams.len() == 1;

    if !index.audio_streams.is_empty() && !skip_audio_section {
        output.push_str("# Audio Tracks\n");

        // Sort variants for stable output: by group_id then stream_index
        let mut streams_sorted = index.audio_streams.clone();
        streams_sorted.sort_by(|a, b| {
            let ga = group_id_for_stream(a);
            let gb = group_id_for_stream(b);
            ga.cmp(&gb).then(a.stream_index.cmp(&b.stream_index))
        });

        // Track which group_ids we've seen so we can mark the first of each as DEFAULT
        let mut seen_groups: std::collections::HashSet<String> = std::collections::HashSet::new();

        for variant in &streams_sorted {
            let group_id = group_id_for_stream(variant);
            let language = variant.language.as_deref().unwrap_or("und");
            let language_rfc = to_rfc5646(language);
            let codec = variant.transcode_to.unwrap_or(variant.codec_id);
            let label = codec_label(codec);

            let name = if language == "und" {
                label.to_string()
            } else {
                format!("{} {}", language.to_uppercase(), label)
            };

            let is_first_in_group = seen_groups.insert(group_id.clone());
            let default = if is_first_in_group { "YES" } else { "NO" };

            let audio_transcode_to = variant
                .transcode_to
                .and_then(|c| codec_name_short(c))
                .map(String::from);

            let uri = crate::params::HlsParams {
                video_url: video_url.to_string(),
                session_id: session_id.map(|s| s.to_string()),
                url_type: crate::params::UrlType::Playlist(crate::params::Playlist {
                    track_id: variant.stream_index,
                    audio_track_id: None,
                    audio_transcode_to,
                }),
            };

            output.push_str(&format!(
                "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"{}\",LANGUAGE=\"{}\",NAME=\"{}\",DEFAULT={},AUTOSELECT=YES,URI=\"{}\"\n",
                group_id, language_rfc, name, default, uri.encode_url()
            ));
        }
        output.push('\n');
    }

    // ── Subtitle MEDIA groups ──────────────────────────────────────────────
    if !index.subtitle_streams.is_empty() {
        output.push_str("# Subtitle Tracks\n");
        for (i, sub) in index.subtitle_streams.iter().enumerate() {
            let language = sub.language.as_deref().unwrap_or("und");
            let language_rfc = to_rfc5646(language);
            let group_id = "subs";
            let name = format!("{} Subtitles", language.to_uppercase());
            let default = if i == 0 { "YES" } else { "NO" };
            let uri = crate::params::HlsParams {
                video_url: video_url.to_string(),
                session_id: session_id.map(|s| s.to_string()),
                url_type: crate::params::UrlType::Playlist(crate::params::Playlist {
                    track_id: sub.stream_index,
                    audio_track_id: None,
                    audio_transcode_to: None,
                }),
            };

            output.push_str(&format!(
                "#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"{}\",LANGUAGE=\"{}\",NAME=\"{}\",DEFAULT={},AUTOSELECT={},FORCED=NO,URI=\"{}\"\n",
                group_id, language_rfc, name, default, default, uri.encode_url()
            ));
        }
        output.push('\n');
    }

    // ── Video Variants ─────────────────────────────────────────────────────
    // Emit one EXT-X-STREAM-INF per unique audio codec group so that clients
    // see all available codec combinations (e.g. AAC + AC-3).
    output.push_str("# Video Variants\n");
    if let Some(video) = index.primary_video() {
        let resolution = format!("{}x{}", video.width, video.height);

        // Subtitle group attribute (same for all variants)
        let subtitle_attr = if !index.subtitle_streams.is_empty() {
            ",SUBTITLES=\"subs\"".to_string()
        } else {
            String::new()
        };

        // Collect distinct audio codec groups (preserving first-seen order)
        let audio_groups: Vec<String> = {
            let mut seen = std::collections::HashSet::new();
            let mut groups = Vec::new();
            for s in &index.audio_streams {
                let g = group_id_for_stream(s);
                if seen.insert(g.clone()) {
                    groups.push(g);
                }
            }
            groups
        };

        // Check if we should use interleaved mode (single muxed A/V playlist)
        // Subtitles are allowed as separate text tracks
        let use_interleaved =
            interleaved && index.video_streams.len() == 1 && index.audio_streams.len() == 1;

        if use_interleaved {
            // Single interleaved audio-video playlist
            // Subtitles are handled as a separate MEDIA group
            let audio = &index.audio_streams[0];
            let video_idx = video.stream_index;
            let audio_idx = audio.stream_index;

            // Get codec name.
            let audio_codec = audio.transcode_to.unwrap_or(audio.codec_id);
            let audio_codec_str = codec_name(audio_codec);

            let has_subs = !index.subtitle_streams.is_empty();
            let video_codec_str = build_codec_attribute(
                Some(video.codec_id),
                video.width,
                video.height,
                video.bitrate,
                video.profile,
                video.level,
                &[],
                false,
            );

            let mut codec_list = Vec::new();
            if let Some(vc) = video_codec_str {
                codec_list.push(vc);
            }
            codec_list.push(audio_codec_str.to_string());
            if has_subs {
                codec_list.push("wvtt".to_string());
            }
            let codecs = codec_list.join(",");

            let bandwidth =
                calculate_bandwidth(video.bitrate.max(100_000), &[audio.bitrate as u32]);

            let subtitle_attr = if has_subs {
                ",SUBTITLES=\"subs\"".to_string()
            } else {
                String::new()
            };

            let audio_transcode_to = audio
                .transcode_to
                .and_then(|c| codec_name_short(c))
                .map(String::from);

            let uri = crate::params::HlsParams {
                video_url: video_url.to_string(),
                session_id: session_id.map(|s| s.to_string()),
                url_type: crate::params::UrlType::Playlist(crate::params::Playlist {
                    track_id: video_idx,
                    audio_track_id: Some(audio_idx),
                    audio_transcode_to,
                }),
            };

            output.push_str(&format!(
                "#EXT-X-STREAM-INF:BANDWIDTH={},RESOLUTION={},CODECS=\"{}\"{}\n",
                bandwidth, resolution, codecs, subtitle_attr
            ));
            output.push_str(&format!("{}\n", uri.encode_url()));
        } else if audio_groups.is_empty() {
            // No audio: single variant with only video codec
            let codecs = build_codec_attribute(
                Some(video.codec_id),
                video.width,
                video.height,
                video.bitrate,
                video.profile,
                video.level,
                &[],
                !index.subtitle_streams.is_empty(),
            );
            let bandwidth = calculate_bandwidth(video.bitrate.max(100000), &[]);
            let codec_attr = codecs
                .map(|c| format!(",CODECS=\"{}\"", c))
                .unwrap_or_default();

            let uri = crate::params::HlsParams {
                video_url: video_url.to_string(),
                session_id: session_id.map(|s| s.to_string()),
                url_type: crate::params::UrlType::Playlist(crate::params::Playlist {
                    track_id: video.stream_index,
                    audio_track_id: None,
                    audio_transcode_to: None,
                }),
            };

            output.push_str(&format!(
                "#EXT-X-STREAM-INF:BANDWIDTH={},RESOLUTION={}{}{}\n",
                bandwidth, resolution, subtitle_attr, codec_attr
            ));
            output.push_str(&format!("{}\n", uri.encode_url()));
        } else {
            // One variant per audio codec group
            for group_id in &audio_groups {
                let audio_codec_str = codec_str_for_group(group_id);

                // Build full codec string: video + this audio group's codec
                // Build full codec string: video + audio + subtitles
                let has_subs = !index.subtitle_streams.is_empty();
                let video_codec_str = build_codec_attribute(
                    Some(video.codec_id),
                    video.width,
                    video.height,
                    video.bitrate,
                    video.profile,
                    video.level,
                    &[],
                    false,
                );

                let mut codec_list = Vec::new();
                if let Some(vc) = video_codec_str {
                    codec_list.push(vc);
                }
                codec_list.push(audio_codec_str.to_string());
                if has_subs {
                    codec_list.push("wvtt".to_string());
                }
                let codecs = codec_list.join(",");

                // Bandwidth: video + all audio streams in this group
                let group_audio_bitrates: Vec<u32> = index
                    .audio_streams
                    .iter()
                    .filter(|s| group_id_for_stream(s) == *group_id)
                    .map(|s| s.bitrate as u32)
                    .collect();
                let bandwidth =
                    calculate_bandwidth(video.bitrate.max(100_000), &group_audio_bitrates);

                let uri = crate::params::HlsParams {
                    video_url: video_url.to_string(),
                    session_id: session_id.map(|s| s.to_string()),
                    url_type: crate::params::UrlType::Playlist(crate::params::Playlist {
                        track_id: video.stream_index,
                        audio_track_id: None,
                        audio_transcode_to: None,
                    }),
                };

                output.push_str(&format!(
                    "#EXT-X-STREAM-INF:BANDWIDTH={},RESOLUTION={},AUDIO=\"{}\",CODECS=\"{}\"{}\n",
                    bandwidth, resolution, group_id, codecs, subtitle_attr
                ));
                output.push_str(&format!("{}\n", uri.encode_url()));
            }
        }
    }

    output
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::{AudioStreamInfo, SubtitleFormat, SubtitleStreamInfo, VideoStreamInfo};
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

        index
    }

    #[test]
    fn test_generate_master_playlist() {
        let index = create_test_index();
        let tracks: HashSet<usize> = index
            .video_streams
            .iter()
            .map(|v| v.stream_index)
            .chain(index.audio_streams.iter().map(|a| a.stream_index))
            .chain(index.subtitle_streams.iter().map(|s| s.stream_index))
            .collect();
        let playlist = generate_master_playlist(
            &index,
            "video.mp4",
            None,
            &[],
            &tracks,
            &HashMap::new(),
            false,
        );

        assert!(playlist.contains("#EXTM3U"));
        assert!(playlist.contains("#EXT-X-VERSION:7"));
        assert!(playlist.contains("#EXT-X-STREAM-INF"));
        assert!(playlist.contains("BANDWIDTH="));
        assert!(playlist.contains("RESOLUTION=1920x1080"));
        assert!(playlist.contains("video.mp4/t.0.m3u8"));
    }

    #[test]
    fn test_generate_master_playlist_with_audio() {
        let index = create_test_index();
        let tracks: HashSet<usize> = index
            .video_streams
            .iter()
            .map(|v| v.stream_index)
            .chain(index.audio_streams.iter().map(|a| a.stream_index))
            .collect();
        let playlist = generate_master_playlist(
            &index,
            "video.mp4",
            None,
            &[],
            &tracks,
            &HashMap::new(),
            false,
        );

        assert!(playlist.contains("TYPE=AUDIO"));
        assert!(playlist.contains("LANGUAGE=\"en\""));
        assert!(playlist.contains("video.mp4/t.1.m3u8"));
    }

    #[test]
    fn test_generate_master_playlist_with_subtitles() {
        let mut index = create_test_index();
        index.subtitle_streams.push(SubtitleStreamInfo {
            stream_index: 2,
            codec_id: ffmpeg::codec::Id::SUBRIP,
            language: Some("en".to_string()),
            format: SubtitleFormat::SubRip,
            non_empty_sequences: Vec::new(),
            sample_index: Vec::new(),
            timebase: ffmpeg::Rational::new(1, 1000),
            start_time: 0,
        });

        let tracks: HashSet<usize> = index
            .video_streams
            .iter()
            .map(|v| v.stream_index)
            .chain(index.audio_streams.iter().map(|a| a.stream_index))
            .chain(index.subtitle_streams.iter().map(|s| s.stream_index))
            .collect();
        let playlist = generate_master_playlist(
            &index,
            "video.mp4",
            None,
            &[],
            &tracks,
            &HashMap::new(),
            false,
        );

        assert!(playlist.contains("TYPE=SUBTITLES"));
        assert!(playlist.contains("video.mp4/t.2.m3u8"));
        assert!(playlist.contains("CODECS=\"avc1.640028,mp4a.40.2,wvtt\""));
    }

    #[test]
    fn test_generate_master_playlist_interleaved() {
        let index = create_test_index();
        let tracks: HashSet<usize> = index
            .video_streams
            .iter()
            .map(|v| v.stream_index)
            .chain(index.audio_streams.iter().map(|a| a.stream_index))
            .collect();
        let playlist = generate_master_playlist(
            &index,
            "video.mp4",
            None,
            &[],
            &tracks,
            &HashMap::new(),
            true,
        );

        assert!(playlist.contains("#EXTM3U"));
        assert!(playlist.contains("#EXT-X-VERSION:7"));
        assert!(playlist.contains("#EXT-X-STREAM-INF"));
        assert!(playlist.contains("BANDWIDTH="));
        assert!(playlist.contains("RESOLUTION=1920x1080"));
        // Should use interleaved playlist (t.0+1.m3u8) instead of separate audio/video
        assert!(playlist.contains("video.mp4/t.0+1.m3u8"));
        assert!(!playlist.contains("TYPE=AUDIO")); // No separate audio entries
    }

    #[test]
    fn test_generate_master_playlist_interleaved_with_subtitles() {
        let mut index = create_test_index();
        index.subtitle_streams.push(SubtitleStreamInfo {
            stream_index: 2,
            codec_id: ffmpeg::codec::Id::SUBRIP,
            language: Some("en".to_string()),
            format: SubtitleFormat::SubRip,
            non_empty_sequences: Vec::new(),
            sample_index: Vec::new(),
            timebase: ffmpeg::Rational::new(1, 1000),
            start_time: 0,
        });

        let tracks: HashSet<usize> = index
            .video_streams
            .iter()
            .map(|v| v.stream_index)
            .chain(index.audio_streams.iter().map(|a| a.stream_index))
            .chain(index.subtitle_streams.iter().map(|s| s.stream_index))
            .collect();
        let playlist = generate_master_playlist(
            &index,
            "video.mp4",
            None,
            &[],
            &tracks,
            &HashMap::new(),
            true,
        );

        assert!(playlist.contains("#EXTM3U"));
        assert!(playlist.contains("video.mp4/t.0+1.m3u8"));
        assert!(!playlist.contains("TYPE=AUDIO")); // No separate audio entries
                                                   // Should have subtitles as separate MEDIA entries
        assert!(playlist.contains("TYPE=SUBTITLES"));
        assert!(playlist.contains("SUBTITLES=\"subs\"")); // Stream should reference subtitle group
        assert!(playlist.contains("CODECS=\"")); // Should include wvtt in codecs
    }

    #[test]
    fn test_generate_master_playlist_interleaved_force_aac() {
        let index = create_test_index();
        let tracks: HashSet<usize> = index
            .video_streams
            .iter()
            .map(|v| v.stream_index)
            .chain(index.audio_streams.iter().map(|a| a.stream_index))
            .collect();
        let transcode: HashMap<usize, String> = [(1, "aac".to_string())].into();
        let playlist =
            generate_master_playlist(&index, "video.mp4", None, &[], &tracks, &transcode, true);

        assert!(playlist.contains("#EXTM3U"));
        assert!(playlist.contains("#EXT-X-VERSION:7"));
        assert!(playlist.contains("#EXT-X-STREAM-INF"));
        // Should use interleaved playlist with -aac suffix
        assert!(playlist.contains("video.mp4/t.0+1-aac.m3u8"));
        // Should report AAC codec
        assert!(playlist.contains("CODECS=\"avc1.640028,mp4a.40.2\""));
        assert!(!playlist.contains("TYPE=AUDIO")); // No separate audio entries
    }
}
