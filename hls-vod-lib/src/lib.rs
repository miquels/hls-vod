//! # HLS VOD Library
//!
//! `hls-vod-lib` is a library for generating HTTP Live Streaming (HLS) playlists and segments
//! on-the-fly from local video files. It leverages FFmpeg (via `ffmpeg-next`) to demux,
//! optionally transcode, and mux media content into fragmented MP4 (fMP4) or WebVTT segments
//! suitable for HLS playback.
//!
//! ## Core Features
//!
//! - **On-the-fly Packaging:** Muxes existing compatible video (e.g., H.264) directly into fMP4 without transcoding.
//! - **Audio Transcoding:** Can transcode audio on-the-fly.
//! - **Multiple Tracks:** Supports multiple audio and subtitle tracks, accurately multiplexing them into HLS variant playlists.
//! - **Subtitle Support:** Extracts and serves embedded subtitles (tx3g, srt, vtt) as WebVTT segments.
//!
//! ## Usage
//!
//! ```ignore
//! fn main() {
//!     hls_vod_lib::ffmpeg_init();
//!     hls_vod_lib::ffmpeg_log_filter();
//!
//!     start_http_server();
//! }
//!
//! fn handle_request(url_path: &str) -> Result<Vec<u8>> {
//!     // Parse the URL path.
//!     let hls_params = hls_vod_lib::HlsParams::parse(&url_path)?;
//!
//!     // Calculate path to video file.
//!     let media_path = std::path::PathBuf::from(&format!("/{}", hls_params.video_url));
//!
//!     // Open video.
//!     let mut hls_video = HlsVideo::open(&media_path, hls_params)?;
//!
//!     // Filter codecs, enable/disable tracks, etc.
//!     if let HlsVideo::MainPlaylist(p) = &mut hls_video {
//!         p.filter_codecs(&["aac"]);
//!     }
//!
//!     // Generate playlist or segments.
//!     hls_video.generate()
//! }
//! ```
//!
//! If you are using an async server such as Axum, you should wrap `HlsVideo::open`
//! and `hls_video.generate()` in calls to `tokio::task::spawn_blocking()`.
//!
pub(crate) mod error;
pub(crate) mod ffmpeg_utils;
pub(crate) mod index;
pub(crate) mod playlist;
pub(crate) mod segment;
pub(crate) mod subtitle;
pub(crate) mod transcode;

pub mod cache;
pub mod hlsvideo;
pub mod lookahead;
pub mod media;
pub mod params;

#[cfg(test)]
pub(crate) mod tests;

pub use error::{FfmpegError, HlsError, Result};
pub use ffmpeg_utils::version_info as ffmpeg_version_info;
pub use ffmpeg_utils::{init as ffmpeg_init, install_log_filter as ffmpeg_log_filter};
pub use hlsvideo::HlsVideo;
pub use params::HlsParams;
