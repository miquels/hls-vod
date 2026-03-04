//! Playlist and segment generation.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use crate::media::StreamIndex;
use crate::params::{HlsParams, UrlType};

/// Playlist or segment generation.
///
/// This enum has two variants:
///
/// - MainPlaylist
///   This is the main or master playlist. On this variant you can
///   manipulate the tracks being included in the playlist, filter
///   on codecs, and so on, before generating the main playlist.
///
/// - PlaylistOrSegment
///   Derived from the main playlist. Cannot be changed.
///
/// You would use this like:
///
/// ```ignore
/// # use hls_vod_lib::{HlsVideo, HlsParams};
/// # let (video_path, hls_params) = unimplemented!();
/// let mut video = HlsVideo::open(video_path, hls_params)?;
/// if let HlsVideo::MainPlaylist(p) = &mut video {
///     p.filter_codecs(&["aac"]);
/// }
/// # Ok::<Vec<u8>, Box<dyn std::error::Error>>(video.generate()?)
/// ```
///
pub enum HlsVideo {
    MainPlaylist(MainPlaylist),
    PlaylistOrSegment(PlaylistOrSegment),
}

impl HlsVideo {
    /// Create a HlsVideo from a video file and a url.
    pub fn open(video: &Path, hls_params: HlsParams) -> crate::error::Result<HlsVideo> {
        let index = StreamIndex::open(video, hls_params.session_id.clone())?;
        Ok(match &hls_params.url_type {
            UrlType::MainPlaylist => HlsVideo::MainPlaylist(MainPlaylist::new(hls_params, index)),
            _ => HlsVideo::PlaylistOrSegment(PlaylistOrSegment { hls_params, index }),
        })
    }

    /// Generate playlist or segment.
    pub fn generate(self) -> crate::error::Result<Vec<u8>> {
        match self {
            HlsVideo::MainPlaylist(p) => p.generate(),
            HlsVideo::PlaylistOrSegment(p) => p.generate(),
        }
    }

    pub fn mime_type(&self) -> &'static str {
        match self {
            HlsVideo::MainPlaylist(p) => p.hls_params.mime_type(),
            HlsVideo::PlaylistOrSegment(s) => s.hls_params.mime_type(),
        }
    }
    pub fn cache_control(&self) -> &'static str {
        match self {
            HlsVideo::MainPlaylist(p) => p.hls_params.cache_control(),
            HlsVideo::PlaylistOrSegment(s) => s.hls_params.cache_control(),
        }
    }
}

/// HlsVideo main playlist variant.
///
/// Here you can enable/disable tracks, filter on codecs, set audio/video
/// interleaving just before generating the main playlist.
pub struct MainPlaylist {
    pub hls_params: HlsParams,
    pub index: Arc<StreamIndex>,
    pub tracks: HashSet<usize>,
    pub codecs: Vec<String>,
    pub transcode: HashMap<usize, String>,
    pub interleave: bool,
}

/// HlsVideo audio/video/subtitle playlist or segment variant.
///
/// This just generates the playlist or segment from the URL.
pub struct PlaylistOrSegment {
    hls_params: HlsParams,
    index: Arc<StreamIndex>,
}

impl PlaylistOrSegment {
    /// Construct directly from an already-opened stream index.
    /// Used in tests where we have an in-memory fixture without a real file path.
    #[cfg(test)]
    pub fn from_index(hls_params: HlsParams, index: Arc<StreamIndex>) -> Self {
        Self { hls_params, index }
    }
}

impl MainPlaylist {
    fn new(hls_params: HlsParams, index: Arc<StreamIndex>) -> MainPlaylist {
        let mut tracks = HashSet::default();

        // enable all tracks.
        for a in &index.audio_streams {
            tracks.insert(a.stream_index);
        }
        for v in &index.video_streams {
            tracks.insert(v.stream_index);
        }
        for s in &index.subtitle_streams {
            tracks.insert(s.stream_index);
        }

        MainPlaylist {
            hls_params,
            index: index,
            tracks,
            codecs: Vec::new(),
            transcode: HashMap::default(),
            interleave: false,
        }
    }

    /// Generate the main playlist.
    // TODO: returns Bytes instead of Vec<u8>
    pub fn generate(&self) -> crate::error::Result<Vec<u8>> {
        match &self.hls_params.url_type {
            UrlType::MainPlaylist => {
                let playlist = crate::playlist::generate_master_playlist(
                    &self.index,
                    &self.hls_params.video_url,
                    Some(&self.index.stream_id),
                    &self.codecs,
                    &self.tracks,
                    &self.transcode,
                    self.interleave,
                );
                Ok(playlist.into_bytes())
            }
            _ => panic!("impossible condition"),
        }
    }

    /// Enable audio/video interleaving.
    ///
    /// This will cause audio and video to be interleaved in one
    /// track, but only if the playlist has _one_ audio track and _one_ video track.
    pub fn interleave(&mut self) {
        self.interleave = true;
    }

    /// Only leave tracks enabled that match the codecs.
    ///
    /// For now, we only look at audio and subtitles.
    pub fn filter_codecs(&mut self, codecs: &[impl AsRef<str>]) {
        self.codecs = codecs.iter().map(|c| c.as_ref().into()).collect();
    }

    /// Enable only the specified tracks.
    pub fn enable_tracks(&mut self, tracks: &[usize]) {
        self.tracks = tracks.iter().cloned().collect();
    }
}

impl PlaylistOrSegment {
    /// Generate the playlist or segment.
    // TODO: returns Bytes instead of Vec<u8>
    pub fn generate(&self) -> crate::error::Result<Vec<u8>> {
        let segment_key = self.hls_params.to_string();

        // Fast path: check cache without locking.
        if let Some(c) = crate::cache::segment_cache() {
            if let Some(b) = c.get(&self.index.stream_id, &segment_key) {
                // Continue the look-ahead chain even on cache hits,
                // otherwise the chain breaks after `lookahead` segments.
                self.spawn_lookahead();
                return Ok(b.to_vec());
            }
        }

        // For media segments, use double-checked locking to avoid
        // duplicate generation (e.g. from look-ahead + player request).
        let is_media_segment = self.is_media_segment();
        if is_media_segment {
            if let Some(c) = crate::cache::segment_cache() {
                let lock = c.acquire_generation_lock(&self.index.stream_id, &segment_key);
                let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());

                // Re-check cache — another thread may have completed while we waited.
                if let Some(b) = c.get(&self.index.stream_id, &segment_key) {
                    c.cleanup_generation_lock(&self.index.stream_id, &segment_key);
                    return Ok(b.to_vec());
                }
            }
        }

        // Generate the actual content.
        let (data, cache_it) = self.do_generate()?;

        // Insert into cache.
        if cache_it {
            if let Some(c) = crate::cache::segment_cache() {
                c.insert(
                    &self.index.stream_id,
                    &segment_key,
                    bytes::Bytes::from(data.clone()),
                );
                c.cleanup_generation_lock(&self.index.stream_id, &segment_key);
            }
        }

        // Spawn look-ahead background generation.
        if is_media_segment {
            self.spawn_lookahead();
        }

        Ok(data)
    }

    /// Whether this request is for a media segment (not init segment or playlist).
    fn is_media_segment(&self) -> bool {
        matches!(
            &self.hls_params.url_type,
            crate::params::UrlType::VideoSegment(v) if v.segment_id.is_some()
        ) || matches!(
            &self.hls_params.url_type,
            crate::params::UrlType::AudioSegment(a) if a.segment_id.is_some()
        ) || matches!(
            &self.hls_params.url_type,
            crate::params::UrlType::VttSegment(_)
        )
    }

    /// Perform the actual generation (separated from caching/dedup logic).
    fn do_generate(&self) -> crate::error::Result<(Vec<u8>, bool)> {
        let mut cache_it = false;

        let data = match &self.hls_params.url_type {
            UrlType::MainPlaylist => panic!("impossible condition"),
            UrlType::Playlist(p) => {
                let playlist = if let Some(audio_idx) = p.audio_track_id {
                    // Audio / Video interleaved playlist
                    crate::playlist::variant::generate_interleaved_playlist(
                        &self.index,
                        p.track_id,
                        audio_idx,
                        p.audio_transcode_to.as_deref(),
                    )
                } else if self
                    .index
                    .audio_streams
                    .iter()
                    .any(|a| a.stream_index == p.track_id)
                {
                    // Audio only playlist
                    crate::playlist::variant::generate_audio_playlist(
                        &self.index,
                        p.track_id,
                        p.audio_transcode_to.as_deref(),
                    )
                } else if self
                    .index
                    // Subtitle only playlist
                    .subtitle_streams
                    .iter()
                    .any(|s| s.stream_index == p.track_id)
                {
                    crate::playlist::variant::generate_subtitle_playlist(&self.index, p.track_id)
                } else {
                    // Main video playlist.
                    crate::playlist::variant::generate_video_playlist(&self.index)
                };
                Ok(playlist.into_bytes())
            }
            UrlType::VideoSegment(v) => {
                if let Some(audio_idx) = v.audio_track_id {
                    if let Some(seq) = v.segment_id {
                        let segment = self.index.get_segment("video", seq)?;
                        let buf = crate::segment::generator::generate_interleaved_segment(
                            &self.index,
                            v.track_id,
                            audio_idx,
                            segment,
                            &self.index.source_path,
                            v.audio_transcode_to.as_deref(),
                        )
                        .map(|b| b.to_vec())?;
                        cache_it = true;
                        Ok(buf)
                    } else {
                        crate::segment::generator::generate_interleaved_init_segment(
                            &self.index,
                            v.track_id,
                            audio_idx,
                            v.audio_transcode_to.as_deref(),
                        )
                        .map(|b| b.to_vec())
                    }
                } else if let Some(seq) = v.segment_id {
                    let buf = crate::segment::generator::generate_video_segment(
                        &self.index,
                        v.track_id,
                        seq,
                        &self.index.source_path,
                    )
                    .map(|b| b.to_vec())?;
                    cache_it = true;
                    Ok(buf)
                } else {
                    crate::segment::generator::generate_video_init_segment(&self.index)
                        .map(|b| b.to_vec())
                }
            }
            UrlType::AudioSegment(a) => {
                if let Some(seq) = a.segment_id {
                    let buf = crate::segment::generator::generate_audio_segment(
                        &self.index,
                        a.track_id,
                        seq,
                        &self.index.source_path,
                        a.transcode_to.as_deref(),
                    )
                    .map(|b| b.to_vec())?;
                    cache_it = true;
                    Ok(buf)
                } else {
                    crate::segment::generator::generate_audio_init_segment(
                        &self.index,
                        a.track_id,
                        a.transcode_to.as_deref(),
                    )
                    .map(|b| b.to_vec())
                }
            }
            UrlType::VttSegment(s) => {
                let buf = crate::segment::generator::generate_subtitle_segment(
                    &self.index,
                    s.track_id,
                    s.start_cue,
                    s.end_cue,
                    &self.index.source_path,
                )
                .map(|b| b.to_vec())?;
                cache_it = true;
                Ok(buf)
            }
        }?;

        Ok((data, cache_it))
    }

    /// Spawn a single background thread to pre-generate upcoming segments sequentially.
    ///
    /// Generating sequentially (not in parallel) avoids contention on the shared
    /// FFmpeg input context mutex and disk I/O, which would otherwise double the
    /// generation time per segment.
    fn spawn_lookahead(&self) {
        let lookahead = crate::cache::segment_cache()
            .map(|c| c.lookahead())
            .unwrap_or(0);

        if lookahead == 0 {
            return;
        }

        let total_segments = self.index.segment_count();

        // Collect the params for segments that actually need generating.
        let mut work: Vec<crate::params::HlsParams> = Vec::new();
        for offset in 1..=lookahead {
            let Some(next_params) = self.hls_params.with_segment_offset(offset) else {
                break;
            };

            // Check if the next segment_id is within bounds.
            let next_seg_id = match &next_params.url_type {
                UrlType::VideoSegment(v) => v.segment_id,
                UrlType::AudioSegment(a) => a.segment_id,
                _ => None,
            };
            if let Some(id) = next_seg_id {
                if id >= total_segments {
                    break; // past the end of the media
                }
            }

            let next_key = next_params.to_string();
            let stream_id = &self.index.stream_id;

            // Skip if already cached.
            if let Some(c) = crate::cache::segment_cache() {
                if c.get(stream_id, &next_key).is_some() {
                    continue;
                }
            }

            work.push(next_params);
        }

        if work.is_empty() {
            return;
        }

        // Spawn ONE thread that generates all look-ahead segments sequentially.
        let index = self.index.clone();
        std::thread::spawn(move || {
            for next_params in work {
                let segment_key = next_params.to_string();
                let stream_id = index.stream_id.clone();

                tracing::debug!(
                    segment_key = %segment_key,
                    "look-ahead: starting pre-generation"
                );

                // Double-checked locking for dedup.
                if let Some(c) = crate::cache::segment_cache() {
                    if c.get(&stream_id, &segment_key).is_some() {
                        continue; // already cached (player or earlier lookahead)
                    }
                    let lock = c.acquire_generation_lock(&stream_id, &segment_key);
                    let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());
                    if c.get(&stream_id, &segment_key).is_some() {
                        c.cleanup_generation_lock(&stream_id, &segment_key);
                        continue; // completed by another thread
                    }
                }

                let ps = PlaylistOrSegment {
                    hls_params: next_params,
                    index: index.clone(),
                };

                match ps.do_generate() {
                    Ok((data, _)) => {
                        if let Some(c) = crate::cache::segment_cache() {
                            c.insert(&stream_id, &segment_key, bytes::Bytes::from(data));
                            c.cleanup_generation_lock(&stream_id, &segment_key);
                        }
                        tracing::debug!(
                            segment_key = %segment_key,
                            "look-ahead: completed pre-generation"
                        );
                    }
                    Err(e) => {
                        if let Some(c) = crate::cache::segment_cache() {
                            c.cleanup_generation_lock(&stream_id, &segment_key);
                        }
                        tracing::warn!(
                            segment_key = %segment_key,
                            error = %e,
                            "look-ahead: pre-generation failed"
                        );
                    }
                }
            }
        });
    }
}
