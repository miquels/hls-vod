use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// Helper to deserialize strings or numbers gracefully into Option<T>
pub fn string_or_number<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::de::Deserializer<'de>,
    T: std::str::FromStr + serde::Deserialize<'de>,
    <T as std::str::FromStr>::Err: std::fmt::Display,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StringOrNumber<T> {
        String(String),
        Number(T),
    }

    match Option::<StringOrNumber<T>>::deserialize(deserializer)? {
        Some(StringOrNumber::String(s)) => {
            if s.is_empty() {
                Ok(None)
            } else {
                s.parse::<T>().map(Some).map_err(serde::de::Error::custom)
            }
        }
        Some(StringOrNumber::Number(n)) => Ok(Some(n)),
        None => Ok(None),
    }
}

// Helper to serialize number to string.
fn number_to_string<S>(number: &Option<i32>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    if let Some(number) = number {
        serializer.serialize_str(&format!("{}", number))
    } else {
        serializer.serialize_str("0")
    }
}

//
// First, the PlaybackInfoRequest sent to the PlaybackInfo endpoint.
//
#[derive(Debug, Serialize, Deserialize, Default)]
#[serde(rename_all = "PascalCase")]
pub struct PlaybackInfoRequest {
    /// The specific device ID making the request
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// The ID of the user requesting playback
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    /// Starting position in Ticks (1 second = 10,000,000 ticks)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_time_ticks: Option<i64>,
    /// The index of the audio stream to play
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "string_or_number",
        serialize_with = "number_to_string"
    )]
    pub audio_stream_index: Option<i32>,
    /// The index of the subtitle stream to play
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "string_or_number",
        serialize_with = "number_to_string"
    )]
    pub subtitle_stream_index: Option<i32>,
    /// The preferred media source ID
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_source_id: Option<String>,
    /// Max bitrate the client can handle
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_streaming_bitrate: Option<i64>,
    /// Whether to enable direct play
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enable_direct_play: Option<bool>,
    /// Whether to enable transcoding
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enable_transcoding: Option<bool>,
    /// Is this playback or just an info request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_playback: Option<bool>,
    /// Not sure what this is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_open_live_stream: Option<bool>,
    /// Always burn in subtitle when transcoding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub always_burn_in_subtitle_when_transcoding: Option<bool>,
    /// The hardware/software capabilities of the client
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_profile: Option<DeviceProfile>,
}

#[derive(Debug, Serialize, Deserialize, Default)]
#[serde(rename_all = "PascalCase")]
pub struct DeviceProfile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_streaming_bitrate: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_static_bitrate: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub music_streaming_transcoding_bitrate: Option<i64>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub direct_play_profiles: Vec<DirectPlayProfile>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub transcoding_profiles: Vec<TranscodingProfile>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub codec_profiles: Vec<CodecProfile>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub subtitle_profiles: Vec<SubtitleProfile>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub response_profiles: Vec<ResponseProfile>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct DirectPlayProfile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_codec: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_codec: Option<String>,
    #[serde(rename = "Type")]
    pub profile_type: String, // e.g., "Video"
}

#[derive(Debug, Serialize, Deserialize, Default)]
#[serde(rename_all = "PascalCase")]
pub struct TranscodingProfile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    #[serde(rename = "Type")]
    pub profile_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_codec: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_codec: Option<String>,
    pub context: String,  // e.g., "Streaming"
    pub protocol: String, // e.g., "hls"
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "string_or_number",
        serialize_with = "number_to_string"
    )]
    pub max_audio_channels: Option<i32>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "string_or_number",
        serialize_with = "number_to_string"
    )]
    pub min_segments: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub break_on_non_key_frames: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize, Default)]
#[serde(rename_all = "PascalCase")]
pub struct CodecProfile {
    #[serde(rename = "Type")]
    pub profile_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codec: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub conditions: Vec<CodecCondition>,
}

#[derive(Debug, Serialize, Deserialize, Default)]
#[serde(rename_all = "PascalCase")]
pub struct CodecCondition {
    pub condition: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub property: Option<String>, // not sure if required or optional.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>, // not sure if required or optional.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_required: Option<bool>, // not sure if required or optional.
}

#[derive(Debug, Serialize, Deserialize, Default)]
#[serde(rename_all = "PascalCase")]
pub struct SubtitleProfile {
    pub format: String, // e.g., "srt", "vtt"
    pub method: String, // e.g., "External", "Hls", "Embed"
}

#[derive(Debug, Serialize, Deserialize, Default)]
#[serde(rename_all = "PascalCase")]
pub struct ResponseProfile {
    #[serde(rename = "Type")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
}

//
// Then the response sent by the jellyfin server.
//
#[derive(Debug, Serialize, Deserialize, Default)]
#[serde(rename_all = "PascalCase")]
pub struct PlaybackInfoResponse {
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub media_sources: Vec<MediaSource>,
    pub play_session_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "PascalCase")]
pub struct MediaSource {
    /// The transport protocol used to access the file (e.g., "File", "Http", "Rtmp").
    pub protocol: String,
    /// A unique identifier for this specific media source.
    pub id: String,
    /// The physical or virtual path to the file on the server.
    pub path: String,
    /// Path to the specific encoder binary if using external tools.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoder_path: Option<String>,
    /// Protocol used by the encoder (usually "Http" for streaming).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoder_protocol: Option<String>,
    /// The type of source (usually "Default" or "Grouping").
    #[serde(rename = "Type")]
    pub r#type: String,
    /// The file container format (e.g., "mkv", "mp4", "m4s").
    pub container: String,
    /// Total file size in bytes.
    #[serde(default)]
    pub size: i64,
    /// Human-readable name for the source.
    pub name: String,
    /// Whether the file is located on a remote network/cloud.
    #[serde(default)]
    pub is_remote: bool,
    /// Total duration of the media in Ticks (1 sec = 10,000,000 ticks).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_time_ticks: Option<i64>,
    /// Tells the client if the server is capable of transcoding this file.
    #[serde(default)]
    pub supports_transcoding: bool,
    /// Whether the server can "remux" (change container only) without re-encoding.
    #[serde(default)]
    pub supports_direct_stream: bool,
    /// Whether the client can play the raw file via HTTP without server help.
    #[serde(default)]
    pub supports_direct_play: bool,
    /// Used for live streams that do not have a defined end.
    #[serde(default)]
    pub is_infinite_stream: bool,
    /// Whether the media requires a "LiveStream" open request before playback.
    #[serde(default)]
    pub requires_opening: bool,
    /// Token used to maintain an open session for protected/live streams.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_token: Option<String>,
    /// Whether the server needs a "Close" signal when the user stops watching.
    #[serde(default)]
    pub requires_closing: bool,
    /// ID associated with a persistent live stream session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live_stream_id: Option<String>,
    /// Suggested buffer size for the client in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub buffer_ms: Option<i32>,
    /// Whether the player should loop back to the start automatically.
    #[serde(default)]
    pub requires_looping: bool,
    /// Whether the stream can be passed to an external player (like VLC/MPV).
    #[serde(default)]
    pub supports_external_stream: bool,
    /// The list of video, audio, and subtitle tracks found in the file.
    pub media_streams: Vec<MediaStream>,
    /// List of compatible containers for this source.
    pub formats: Vec<String>,
    /// Total combined bitrate (video + audio) in bits per second.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bitrate: Option<i32>,
    /// Last modified or creation timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
    /// Custom headers required by the client to fetch segments (e.g., Cookies/Auth).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub required_http_headers: Option<HashMap<String, String>>,
    /// The critical HLS/DASH URL used for transcoding sessions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcoding_url: Option<String>,
    /// The sub-protocol for transcoding (usually "hls").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcoding_sub_protocol: Option<String>,
    /// The container used for transcode segments (e.g., "ts" or "mp4").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcoding_container: Option<String>,
    /// How many ms the client should analyze the stream before playing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub analyze_duration_ms: Option<i32>,
    /// The index of the audio track the server recommends playing by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_audio_stream_index: Option<i32>,
    /// The index of the subtitle track the server recommends playing by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_subtitle_stream_index: Option<i32>,
    /// Categorization of video (e.g., "Video", "Map", "Thumbnail").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_type: Option<String>,
    /// Unique hash to help with client-side caching.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
    /// URL for direct stream access (if different from Path).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub direct_stream_url: Option<String>,
    /// List of extra files (like fonts or posters) associated with the source.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_attachments: Option<Vec<String>>,
    /// Whether the server should force reading at the original FPS (common for Live).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_at_native_framerate: Option<bool>,
    /// Whether the media is pre-segmented (DASH/HLS) on the server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub has_segments: Option<bool>,
    /// Tells the client to ignore DTS timestamps (fixes some sync issues).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ignore_dts: Option<bool>,
    /// Tells the client to ignore the file index and scan sequentially.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ignore_index: Option<bool>,
    /// Tells FFmpeg to generate PTS timestamps on the fly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gen_pts_input: Option<bool>,
    /// Whether the server has already "probed" the file for metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_probing: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct MediaStream {
    /// The codec name (e.g., "h264", "aac", "subrip").
    pub codec: String,
    /// 3-letter ISO language code (e.g., "eng", "fra").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// The internal time base for the stream (e.g., "1/90000").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_base: Option<String>,
    /// The title attribute from the stream metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The user-friendly title shown in the client UI (e.g., "English (AAC 5.1)").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_title: Option<String>,
    /// The localized name of the language (e.g., "Spanish").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_language: Option<String>,
    /// Whether the video is interlaced (as opposed to progressive).
    #[serde(default)]
    pub is_interlaced: bool,
    /// The audio channel layout (e.g., "5.1", "stereo", "7.1").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_layout: Option<String>,
    /// Bitrate of this specific track in bits per second.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bit_rate: Option<i32>,
    /// Color depth of the video (e.g., 8, 10 for HDR).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bit_depth: Option<i32>,
    /// Number of reference frames (relevant for H.264 profiles).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ref_frames: Option<i32>,
    /// Internal packet length (mostly for specialized transport).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub packet_length: Option<i32>,
    /// Number of audio channels (e.g., 2, 6).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channels: Option<i32>,
    /// Audio sample rate in Hz (e.g., 44100, 48000).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample_rate: Option<i32>,
    /// Whether this is the default track for its type.
    pub is_default: bool,
    /// Whether this is a "forced" subtitle track.
    pub is_forced: bool,
    /// Video height in pixels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<i32>,
    /// Video width in pixels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<i32>,
    /// The average FPS of the video.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub average_frame_rate: Option<f32>,
    /// The actual/variable frame rate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub real_frame_rate: Option<f32>,
    /// The codec profile (e.g., "High", "Main 10").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// Type of stream: "Video", "Audio", "Subtitle", or "Data".
    #[serde(rename = "Type")]
    pub stream_type: String,
    /// Display aspect ratio (e.g., "16:9").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aspect_ratio: Option<String>,
    /// The absolute global index of this stream in the file (FFmpeg index).
    #[serde(default)]
    pub index: i32,
    /// Internal priority score for track selection logic.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<i32>,
    /// Whether this track is an external file (e.g., .srt) or embedded.
    #[serde(default)]
    pub is_external: bool,
    /// How the track is delivered: "External", "Hls", "Embed".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery_method: Option<String>,
    /// URL to fetch the subtitle or audio file if it is external.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery_url: Option<String>,
    /// Whether the delivery URL points to a different server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_external_url: Option<bool>,
    /// True if the subtitle is text-based (SRT/ASS) vs image-based (PGS).
    #[serde(default)]
    pub is_text_subtitle_stream: bool,
    /// Whether this stream supports being served via an external URL.
    #[serde(default)]
    pub supports_external_stream: bool,
    /// Path to the external track file on disk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Video pixel format (e.g., "yuv420p", "yuv420p10le").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pixel_format: Option<String>,
    /// Codec level (e.g., 4.1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<f32>,
    /// The internal codec tag (e.g., "avc1", "hvc1").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codec_tag: Option<String>,
    /// Whether the pixels are non-square (common in DVD rips).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_anamorphic: Option<bool>,
    /// The color range (e.g., "SDR", "HDR10", "HLG", "DOVI").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_range: Option<String>,
    /// Specific color space metadata (e.g., "BT709", "BT2020").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_range_type: Option<String>,
    /// Information for Dolby Atmos or DTS:X spatial audio.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_spatial_format: Option<String>,
    /// Localized string for the "Default" label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub localized_default: Option<String>,
    /// Localized string for the "External" label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub localized_external: Option<String>,
    /// True if the video codec is H.264/AVC.
    #[serde(rename = "IsAVC")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_avc: Option<bool>,
    /// Flag for SDH (Subtitles for the Deaf and Hard of Hearing).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_hearing_impaired: Option<bool>,
    /// If the HLS stream interleaves audio and video.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_interleaved: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "PascalCase")]
pub struct HlsTranscodingParameters {
    // --- Identity & Session ---
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_source_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub play_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// Unique tag for caching/identification of the specific media version
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,

    // --- Stream Selection & Logic ---
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_stream_index: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtitle_stream_index: Option<String>,
    /// Reasons for transcoding (e.g., "AudioCodecNotSupported")
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcode_reasons: Option<String>,

    // --- Codec Negotiations (Comma-separated lists) ---
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_codec: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_codec: Option<String>,

    // --- Quality & Bitrate ---
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_bitrate: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_bitrate: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_framerate: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_width: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_height: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcoding_max_audio_channels: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enable_audio_vbr_encoding: Option<String>,

    // --- HLS Segmenter Configuration ---
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub segment_container: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_segments: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub break_on_non_key_frames: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_time_ticks: Option<String>,

    // --- Codec-Specific Capabilities (The hyphenated keys) ---

    // H.264
    #[serde(rename = "h264-profile")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub h264_profile: Option<String>,
    #[serde(rename = "h264-level")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub h264_level: Option<String>,
    #[serde(rename = "h264-videobitdepth")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub h264_bit_depth: Option<String>,
    #[serde(rename = "h264-rangetype")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub h264_range_type: Option<String>,
    #[serde(rename = "h264-deinterlace")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub h264_deinterlace: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub require_avc: Option<String>,

    // HEVC
    #[serde(rename = "hevc-profile")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hevc_profile: Option<String>,
    #[serde(rename = "hevc-level")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hevc_level: Option<String>,
    #[serde(rename = "hevc-rangetype")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hevc_range_type: Option<String>,
    #[serde(rename = "hevc-deinterlace")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hevc_deinterlace: Option<String>,

    // AV1
    #[serde(rename = "av1-profile")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub av1_profile: Option<String>,
    #[serde(rename = "av1-level")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub av1_level: Option<String>,
    #[serde(rename = "av1-rangetype")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub av1_range_type: Option<String>,

    // VP9
    #[serde(rename = "vp9-rangetype")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vp9_range_type: Option<String>,

    // --- Logic Flags ---
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_video_stream_copy: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_audio_stream_copy: Option<String>,
}
