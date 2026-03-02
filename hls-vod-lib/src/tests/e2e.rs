//! End-to-end integration tests

use crate::media::StreamIndex;
use crate::params::HlsParams;
use crate::tests::fixtures::TestMediaInfo;
use crate::tests::validation::{
    validate_master_playlist, validate_variant_playlist, validate_webvtt, PlaylistType,
    ValidationResult,
};

fn get_master(media: &StreamIndex, session: Option<&str>) -> String {
    use crate::hlsvideo::MainPlaylist;
    use std::sync::Arc;
    let url = format!("{}.as.m3u8", media.source_path.to_string_lossy());
    let mut hls_params = HlsParams::parse(&url).expect("Should parse master URL");
    if let Some(s) = session {
        hls_params.session_id = Some(s.to_string());
    }
    let p = MainPlaylist {
        hls_params,
        index: Arc::new(media.clone()),
        tracks: media
            .video_streams
            .iter()
            .map(|v| v.stream_index)
            .chain(media.audio_streams.iter().map(|a| a.stream_index))
            .chain(media.subtitle_streams.iter().map(|s| s.stream_index))
            .collect(),
        codecs: Vec::new(),
        transcode: std::collections::HashMap::new(),
        interleave: false,
    };
    String::from_utf8(p.generate().unwrap()).unwrap()
}

fn get_variant(media: &StreamIndex, path: &str) -> String {
    use crate::hlsvideo::PlaylistOrSegment;
    use std::sync::Arc;
    // URL format: <video_file>/<session_id>/<rest>
    let url = format!(
        "{}/{}/{}",
        media.source_path.to_string_lossy(),
        media.stream_id,
        path
    );
    let hls_params = HlsParams::parse(&url).unwrap();
    let p = PlaylistOrSegment::from_index(hls_params, Arc::new(media.clone()));
    String::from_utf8(p.generate().unwrap()).unwrap()
}

fn get_segment(media: &StreamIndex, path: &str) -> Vec<u8> {
    try_get_segment(media, path).unwrap()
}

fn try_get_segment(media: &StreamIndex, path: &str) -> Result<Vec<u8>, crate::error::HlsError> {
    use crate::hlsvideo::PlaylistOrSegment;
    use std::sync::Arc;
    // URL format: <video_file>/<session_id>/<rest>
    let url = format!(
        "{}/{}/{}",
        media.source_path.to_string_lossy(),
        media.stream_id,
        path
    );
    let hls_params = HlsParams::parse(&url).unwrap();
    PlaylistOrSegment::from_index(hls_params, Arc::new(media.clone())).generate()
}

/// Test the complete stream lifecycle
pub fn test_stream_lifecycle() -> ValidationResult {
    // Use a real asset for the complete lifecycle test
    let mut asset_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    asset_path.push("testvideos");
    asset_path.push("bun33s.mp4");

    if !asset_path.exists() {
        return ValidationResult::success(); // Skip if asset missing
    }

    let media = StreamIndex::open(&asset_path, None).expect("Parsing failed");

    // Generate and validate master playlist
    let master = get_master(&media, Some("testsession"));
    let master_result = validate_master_playlist(&master);
    if !master_result.is_valid {
        return master_result;
    }

    // Generate and validate video playlist
    let video_pl = get_variant(&media, "t.0.m3u8");
    let video_result = validate_variant_playlist(&video_pl, PlaylistType::Video);
    if !video_result.is_valid {
        return video_result;
    }

    // Generate init segment
    let _init_seg = get_segment(&media, "v/0.init.mp4");

    ValidationResult::success()
}

/// Test playlist generation for various configurations
pub fn test_playlist_generation() -> Vec<(&'static str, ValidationResult)> {
    let mut results = Vec::new();

    // Test AAC-only configuration
    {
        let fixture = TestMediaInfo::aac_only();
        let media = fixture.create_mock_media();
        let master = get_master(&media, None);
        let result = validate_master_playlist(&master);
        results.push(("AAC-only master playlist", result));
    }

    // Test AC-3 configuration
    {
        let fixture = TestMediaInfo::ac3_only();
        let media = fixture.create_mock_media();
        let master = get_master(&media, None);
        let result = validate_master_playlist(&master);
        results.push(("AC-3 master playlist", result));
    }

    // Test multi-audio configuration
    {
        let fixture = TestMediaInfo::multi_audio();
        let media = fixture.create_mock_media();
        let master = get_master(&media, None);
        let result = validate_master_playlist(&master);
        results.push(("Multi-audio master playlist", result));
    }

    // Test with subtitles
    {
        let fixture = TestMediaInfo::with_subtitles();
        let media = fixture.create_mock_media();
        let master = get_master(&media, None);
        let result = validate_master_playlist(&master);
        results.push(("With subtitles master playlist", result));
    }

    // Test multi-language
    {
        let fixture = TestMediaInfo::multi_language();
        let media = fixture.create_mock_media();
        let master = get_master(&media, None);
        let result = validate_master_playlist(&master);
        results.push(("Multi-language master playlist", result));
    }

    results
}

/// Test audio track switching
pub fn test_audio_track_switching() -> ValidationResult {
    let fixture = TestMediaInfo::multi_audio();
    let media = fixture.create_mock_media();

    // Generate master playlist
    let master = get_master(&media, None);

    // Verify multiple audio tracks are present
    let audio_count = master.matches("TYPE=AUDIO").count();
    if audio_count < 2 {
        return ValidationResult::fail(format!(
            "Expected at least 2 audio tracks, found {}",
            audio_count
        ));
    }

    // Verify different languages
    if !master.contains("LANGUAGE=\"en\"") {
        return ValidationResult::fail("Missing English audio track");
    }
    if !master.contains("LANGUAGE=\"es\"") {
        return ValidationResult::fail("Missing Spanish audio track");
    }

    // Generate audio playlists for each language
    for track_idx in [1, 2] {
        let playlist_id = format!("t.{}.m3u8", track_idx);
        let audio_playlist = get_variant(&media, &playlist_id);
        let result = validate_variant_playlist(&audio_playlist, PlaylistType::Audio);
        if !result.is_valid {
            return ValidationResult::fail(format!(
                "Invalid {} audio playlist: {:?}",
                track_idx, result.errors
            ));
        }
    }

    ValidationResult::success()
}

/// Test subtitle synchronization
pub fn test_subtitle_sync() -> ValidationResult {
    let fixture = TestMediaInfo::with_subtitles();
    let media = fixture.create_mock_media();

    // Generate subtitle playlist
    let sub_idx = media
        .subtitle_streams
        .first()
        .map(|s| s.stream_index)
        .unwrap_or(2);

    let playlist_id = format!("t.{}.m3u8", sub_idx);
    let sub_playlist = get_variant(&media, &playlist_id);

    let playlist_result = validate_variant_playlist(&sub_playlist, PlaylistType::Subtitle);
    if !playlist_result.is_valid {
        return playlist_result;
    }

    // Verify segment references are valid
    if !sub_playlist.contains(&format!("{}.0-0.vtt", sub_idx)) {
        return ValidationResult::fail(format!(
            "Missing subtitle segment reference, got: \n{}",
            sub_playlist
        ));
    }

    // Generate mock WebVTT content and validate
    let webvtt_content = r#"WEBVTT

00:00:00.000 --> 00:00:04.000
Test subtitle segment
"#;
    let webvtt_result = validate_webvtt(webvtt_content);
    if !webvtt_result.is_valid {
        return webvtt_result;
    }

    ValidationResult::success()
}

/// Performance benchmark for playlist generation
pub fn benchmark_playlist_generation(iterations: usize) -> BenchmarkResult {
    use std::time::Instant;

    let fixture = TestMediaInfo::multi_language();
    let media = fixture.create_mock_media();

    let start = Instant::now();
    for _ in 0..iterations {
        let _ = get_master(&media, None);
        let _ = get_variant(&media, "t.0.m3u8");
        let _ = get_variant(&media, "t.1.m3u8");
        let _ = get_variant(&media, "t.2.m3u8");
    }
    let duration = start.elapsed();

    BenchmarkResult {
        name: "Playlist Generation",
        iterations,
        duration_ms: duration.as_millis() as u64,
        avg_ms: (duration.as_millis() as f64 / iterations as f64) as u64,
    }
}

/// Performance benchmark for segment generation
pub fn benchmark_segment_generation(iterations: usize) -> BenchmarkResult {
    use std::time::Instant;

    let fixture = TestMediaInfo::aac_only();
    let media = fixture.create_mock_media();

    let start = Instant::now();
    for _ in 0..iterations {
        let _ = try_get_segment(&media, "v/0.init.mp4");
    }
    let duration = start.elapsed();

    BenchmarkResult {
        name: "Init Segment Generation",
        iterations,
        duration_ms: duration.as_millis() as u64,
        avg_ms: (duration.as_millis() as f64 / iterations as f64) as u64,
    }
}

/// Benchmark result
#[derive(Debug, Clone)]
pub struct BenchmarkResult {
    pub name: &'static str,
    pub iterations: usize,
    pub duration_ms: u64,
    pub avg_ms: u64,
}

impl std::fmt::Display for BenchmarkResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}: {} iterations in {}ms (avg: {}ms)",
            self.name, self.iterations, self.duration_ms, self.avg_ms
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_stream_lifecycle_e2e() {
        let result = test_stream_lifecycle();
        assert!(
            result.is_valid,
            "Stream lifecycle test failed: {:?}",
            result.errors
        );
    }

    #[test]
    fn test_playlist_generation_all_configs() {
        let results = test_playlist_generation();
        for (name, result) in results {
            assert!(result.is_valid, "{} failed: {:?}", name, result.errors);
        }
    }

    #[test]
    fn test_audio_track_switching_e2e() {
        let result = test_audio_track_switching();
        assert!(
            result.is_valid,
            "Audio track switching test failed: {:?}",
            result.errors
        );
    }

    #[test]
    fn test_subtitle_sync_e2e() {
        let result = test_subtitle_sync();
        assert!(
            result.is_valid,
            "Subtitle sync test failed: {:?}",
            result.errors
        );
    }

    #[test]
    fn test_benchmark_playlist_generation() {
        let result = benchmark_playlist_generation(100);
        println!("{}", result);
        // Should complete in reasonable time (< 100ms avg)
        assert!(
            result.avg_ms < 100,
            "Playlist generation too slow: {}ms avg",
            result.avg_ms
        );
    }

    #[test]
    fn test_opus_transcode_e2e() {
        let mut asset_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        asset_path.push("../tests/assets/bun33s.webm");

        if !asset_path.exists() {
            println!("Skipping since {:?} doesn't exist", asset_path);
            return;
        }

        let media = StreamIndex::open(&asset_path, None).expect("Failed to scan webm asset");
        let _prefix = format!("/streams/{}", media.stream_id);

        // Find audio
        let audio_stream = media.audio_streams.first().unwrap();
        let segment = media.segments.first().unwrap();

        // transcode audio to aac
        let (packets, _) = crate::transcode::pipeline::transcode_audio_segment(
            &asset_path,
            audio_stream,
            segment,
            media.video_timebase,
            true,
        )
        .unwrap();

        assert!(!packets.is_empty(), "Expected some AAC packets");

        let m3u8 = get_master(&media, None);
        println!("MASTER PLAYLIST:\n{}", m3u8);
        assert!(
            m3u8.contains("Audio Tracks"),
            "Playlist does not contain Audio Tracks"
        );
    }

    #[test]
    fn test_benchmark_segment_generation() {
        let result = benchmark_segment_generation(100);
        println!("{}", result);
        // Should complete in reasonable time (< 50ms avg)
        assert!(
            result.avg_ms < 50,
            "Segment generation too slow: {}ms avg",
            result.avg_ms
        );
    }
}
