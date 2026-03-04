//! HLS parameters, derived from the URL.

use std::fmt;
use std::str::FromStr;

/// HlsParams contains a video playlist or segment decoded from a URL.
#[derive(Debug, Clone)]
pub struct HlsParams {
    /// Enum of subtype.
    pub url_type: UrlType,
    /// Optional session id. Is only None for the MainPlaylist.
    pub session_id: Option<String>,
    /// URL of the base video file.
    pub video_url: String,
}

/// Different types of encoded URLs.
#[derive(Debug, Clone)]
pub enum UrlType {
    MainPlaylist,
    Playlist(Playlist),
    VideoSegment(VideoSegment),
    AudioSegment(AudioSegment),
    VttSegment(VttSegment),
}

// helper.
fn basename(s: &str) -> &'_ str {
    s.split("/").last().unwrap()
}

// helper.
macro_rules! regex {
    ($re:literal $(,)?) => {{
        static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
        RE.get_or_init(|| regex::Regex::new($re).unwrap())
    }};
}

// helper.
fn usize_from_str(s: &str) -> usize {
    usize::from_str(s).expect("a number")
}

impl fmt::Display for HlsParams {
    /// Generate the encoded url, relative to the playlist it's in.
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match &self.url_type {
            UrlType::MainPlaylist => write!(f, "{}.as.m3u8", basename(&self.video_url)),
            UrlType::Playlist(s) => {
                // A playlist is included in from the main playlist, and at the same relative
                // position in the URL as the video file / the video.as.m3u8. So, we need
                // to prepend the videos' name, and the session id.
                write!(f, "{}/", basename(&self.video_url))?;
                if let Some(session_id) = &self.session_id {
                    write!(f, "{}/", session_id)?;
                }
                s.fmt(f)
            }
            UrlType::VideoSegment(s) => s.fmt(f),
            UrlType::AudioSegment(s) => s.fmt(f),
            UrlType::VttSegment(s) => s.fmt(f),
        }
    }
}

impl HlsParams {
    /// Parse a HLS URL.
    pub fn parse(url: &str) -> Option<HlsParams> {
        // Check for video.mp4.as.m3u8.
        if let Some(caps) = regex!(r"^(.+\.(?:mp4|mkv|webm))\.as\.m3u8$").captures(url) {
            return Some(HlsParams {
                url_type: UrlType::MainPlaylist,
                session_id: None,
                video_url: caps[1].to_string(),
            });
        }

        // Then something with a session id.
        let caps = regex!(r"^(.+\.(?:mp4|mkv|webm))/([^/]+)/(.+)$").captures(url)?;
        let video_url = caps[1].to_string();
        let session_id = Some(caps[2].to_string());
        let rest = &caps[3];

        // Playlists.
        // t.<track_id>.m3u8
        // t.<track_id>+<audio_track_id>.m3u8
        // t.<track_id>+<audio_track_id>-<codec>.m3u8
        if let Some(caps) = regex!(r"^t.(\d+)(?:\+(\d+))?(?:-(.+))?.(m3u8)").captures(rest) {
            return Some(HlsParams {
                url_type: UrlType::Playlist(Playlist {
                    track_id: usize_from_str(&caps[1]),
                    audio_track_id: caps.get(2).map(|m| usize_from_str(m.as_str())),
                    audio_transcode_to: caps.get(3).map(|m| m.as_str().to_string()),
                }),
                session_id,
                video_url,
            });
        }

        // Audio URL.
        //
        // a/<track_id>.init.mp4
        // a/<track_id>-<transcode_to>.init.mp4
        //
        // a/<track_id>.<segment_id>.m4s
        // a/<track_id>-<transcode_to>.<segment_id>.m4s
        if let Some(caps) =
            regex!(r"^a/(\d+)(?:-([a-z]+))?(?:\.(\d+))?\.(m4s|init.mp4)$").captures(rest)
        {
            if (&caps[4] == "init.mp4" && caps.get(3).is_some())
                || (&caps[4] == "m4s" && caps.get(3).is_none())
            {
                return None;
            }
            return Some(HlsParams {
                url_type: UrlType::AudioSegment(AudioSegment {
                    track_id: usize_from_str(&caps[1]),
                    transcode_to: caps.get(2).map(|m| m.as_str().to_string()),
                    segment_id: caps.get(3).map(|m| usize_from_str(m.as_str())),
                }),
                session_id,
                video_url,
            });
        }

        // Video URL.
        //
        // v/<track_id>.init.mp4
        // v/<track_id>+<audio_track_id>.init.mp4
        // v/<track_id>+<audio_track_id>-<audio_transcode_to>.init.mp4
        //
        // v/<track_id>.<segment_id>.m4s
        // v/<track_id>+<audio_track_id>.<segment_id>.m4s
        // v/<track_id>+<audio_track_id>-<audio_transcode_to>.<segment_id>.m4s
        if let Some(caps) =
            regex!(r"^v/(\d+)(?:\+(\d+)(?:-([a-z]+))?)?(?:\.(\d+))?\.(m4s|init.mp4)").captures(rest)
        {
            if (&caps[5] == "init.mp4" && caps.get(4).is_some())
                || (&caps[5] == "m4s" && caps.get(4).is_none())
            {
                return None;
            }
            return Some(HlsParams {
                url_type: UrlType::VideoSegment(VideoSegment {
                    track_id: usize_from_str(&caps[1]),
                    audio_track_id: caps.get(2).map(|m| usize_from_str(m.as_str())),
                    audio_transcode_to: caps
                        .get(2)
                        .and_then(|_| caps.get(3).map(|m| m.as_str().to_string())),
                    segment_id: caps.get(4).map(|m| usize_from_str(m.as_str())),
                }),
                session_id,
                video_url,
            });
        }

        // Subtitle URL.
        // s/<track_id>.<start_cue>.<end_cue>.vtt
        if let Some(caps) = regex!(r"^s/(\d+)\.(\d+)-(\d+)\.vtt$").captures(rest) {
            return Some(HlsParams {
                url_type: UrlType::VttSegment(VttSegment {
                    track_id: usize_from_str(&caps[1]),
                    start_cue: usize_from_str(&caps[2]),
                    end_cue: usize_from_str(&caps[3]),
                }),
                session_id,
                video_url,
            });
        }

        None
    }

    /// Encode the HlsParams to a string.
    pub fn encode_url(&self) -> String {
        self.to_string()
    }

    /// Return the MIME type.
    pub(crate) fn mime_type(&self) -> &'static str {
        match &self.url_type {
            UrlType::MainPlaylist | UrlType::Playlist(_) => "application/vnd.apple.mpegurl",
            UrlType::VideoSegment(v) => {
                if v.segment_id.is_none() {
                    "video/mp4"
                } else {
                    "video/iso.segment"
                }
            }
            UrlType::AudioSegment(a) => {
                if a.segment_id.is_none() {
                    "video/mp4"
                } else {
                    "audio/mp4"
                }
            }
            UrlType::VttSegment(_) => "text/vtt",
        }
    }

    /// Return cache-control header hint.
    pub(crate) fn cache_control(&self) -> &'static str {
        match &self.url_type {
            UrlType::MainPlaylist | UrlType::Playlist(_) => "no-cache",
            _ => "max-age=3600",
        }
    }

    /// Create a new `HlsParams` for the next segment (segment_id + offset).
    ///
    /// Returns `None` for init segments, playlists, subtitles, or if no segment_id.
    pub fn with_segment_offset(&self, offset: usize) -> Option<HlsParams> {
        let new_url_type = match &self.url_type {
            UrlType::VideoSegment(v) => v.segment_id.map(|id| {
                UrlType::VideoSegment(VideoSegment {
                    track_id: v.track_id,
                    audio_track_id: v.audio_track_id,
                    audio_transcode_to: v.audio_transcode_to.clone(),
                    segment_id: Some(id + offset),
                })
            }),
            UrlType::AudioSegment(a) => a.segment_id.map(|id| {
                UrlType::AudioSegment(AudioSegment {
                    track_id: a.track_id,
                    transcode_to: a.transcode_to.clone(),
                    segment_id: Some(id + offset),
                })
            }),
            _ => None,
        }?;

        Some(HlsParams {
            url_type: new_url_type,
            session_id: self.session_id.clone(),
            video_url: self.video_url.clone(),
        })
    }
}

/// A video segment.
#[derive(Debug, Clone)]
pub struct VideoSegment {
    /// Track id.
    pub track_id: usize,
    /// Extra track id to be interleaved with. Optional. Always audio.
    pub audio_track_id: Option<usize>,
    /// Transcode
    pub audio_transcode_to: Option<String>,
    /// Segment id. If None, this is the init segment.
    pub segment_id: Option<usize>,
}

impl fmt::Display for VideoSegment {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "v/{}", self.track_id)?;
        if let Some(audio_track_id) = self.audio_track_id {
            write!(f, "+{}", audio_track_id)?;
            if let Some(audio_transcode_to) = &self.audio_transcode_to {
                write!(f, "-{}", audio_transcode_to)?;
            }
        }
        if let Some(segment_id) = self.segment_id {
            write!(f, ".{}.m4s", segment_id)?;
        } else {
            write!(f, ".init.mp4")?;
        }
        Ok(())
    }
}

/// An audio segment.
#[derive(Debug, Clone)]
pub struct AudioSegment {
    /// Track id.
    pub track_id: usize,
    /// Transcode to other codec.
    pub transcode_to: Option<String>,
    /// Segment id. If None, this is the init segment.
    pub segment_id: Option<usize>,
}

impl fmt::Display for AudioSegment {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "a/{}", self.track_id)?;
        if let Some(transcode_to) = &self.transcode_to {
            write!(f, "-{}", transcode_to)?;
        }
        if let Some(segment_id) = self.segment_id {
            write!(f, ".{}.m4s", segment_id)?;
        } else {
            write!(f, ".init.mp4")?;
        }
        Ok(())
    }
}

/// A vtt (subtitle) segment.
#[derive(Debug, Clone)]
pub struct VttSegment {
    /// Track id.
    pub track_id: usize,
    ///
    pub start_cue: usize,
    ///
    pub end_cue: usize,
}

/// A vtt segment (subtitles).
impl fmt::Display for VttSegment {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "s/{}.{}-{}.vtt",
            self.track_id, self.start_cue, self.end_cue
        )
    }
}

/// An audio / video / subtitle playlist.
#[derive(Debug, Clone)]
pub struct Playlist {
    /// Track id.
    pub track_id: usize,
    /// AUdio track to be interleaved with main track.
    pub audio_track_id: Option<usize>,
    /// Transcode audio.
    pub audio_transcode_to: Option<String>,
}

impl fmt::Display for Playlist {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "t.{}", self.track_id)?;
        if let Some(audio_track_id) = self.audio_track_id {
            write!(f, "+{}", audio_track_id)?;
            if let Some(audio_transcode_to) = &self.audio_transcode_to {
                write!(f, "-{}", audio_transcode_to)?;
            }
        }
        write!(f, ".m3u8")
    }
}
