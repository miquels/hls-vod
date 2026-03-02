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
        // See if it's in the cache.
        let segment_key = self.hls_params.to_string();
        if let Some(c) = crate::cache::segment_cache() {
            if let Some(b) = c.get(&self.index.stream_id, &segment_key) {
                return Ok(b.to_vec());
            }
        }
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

        if cache_it {
            if let Some(c) = crate::cache::segment_cache() {
                c.insert(
                    &self.index.stream_id,
                    &segment_key,
                    bytes::Bytes::from(data.clone()),
                );
            }
        }

        Ok(data)
    }
}
