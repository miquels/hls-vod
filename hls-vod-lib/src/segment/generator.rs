//! Segment generator - uses FFmpeg CLI for reliable segment generation

use bytes::Bytes;
use std::path::Path;

use ffmpeg_next::{self as ffmpeg, Rescale};

use crate::error::{HlsError, Result};
use crate::media::{SegmentInfo, StreamIndex};
use crate::segment::muxer::find_media_segment_offset;
use crate::segment::muxer::mux_aac_packets_to_fmp4;
use crate::segment::muxer::Fmp4Muxer;
use crate::subtitle::decoder::is_bitmap_subtitle_codec;
use crate::subtitle::extractor::SubtitleExtractor;
use crate::subtitle::webvtt::{WebVttConfig, WebVttWriter};
use crate::transcode::encoder::{get_recommended_bitrate, AacEncoder};
use crate::transcode::resampler::HLS_SAMPLE_RATE;

/// Builder for configuring and generating an initialization segment (`init.mp4`).
pub(crate) struct InitSegmentBuilder<'a> {
    index: &'a StreamIndex,
    video_idx: Option<usize>,
    audio_idx: Option<usize>,
    transcode_audio_to_aac: bool,
}

impl<'a> InitSegmentBuilder<'a> {
    /// Create a new builder targeting the provided stream index.
    pub fn new(index: &'a StreamIndex) -> Self {
        Self {
            index,
            video_idx: None,
            audio_idx: None,
            transcode_audio_to_aac: false,
        }
    }

    /// Add a generic rule to include all streams.
    pub fn with_all_streams(self) -> Self {
        // Technically this acts as a catch-all in `build()` if both are None,
        // but for safety we specify explicit inclusion when building if needed.
        self
    }

    /// Add a video track by its internal FFmpeg stream index.
    pub fn with_video_track(mut self, idx: usize) -> Self {
        self.video_idx = Some(idx);
        self
    }

    /// Add an audio track by its internal FFmpeg stream index.
    pub fn with_audio_track(mut self, idx: usize) -> Self {
        self.audio_idx = Some(idx);
        self
    }

    /// Specify whether the audio track should be treated as transcode-to-AAC.
    /// If true, the builder will use AAC codec parameters rather than source parameters.
    pub fn transcode_audio_to_aac(mut self, transcode: bool) -> Self {
        self.transcode_audio_to_aac = transcode;
        self
    }

    /// Construct the initialization segment bytes.
    pub fn build(self) -> Result<Bytes> {
        let mut input = self.index.get_context()?;
        let mut muxer = Fmp4Muxer::new()?;

        let mut has_video = false;
        let mut has_audio = false;

        let mut video_params = None;
        let mut audio_params = None;

        let include_all = self.video_idx.is_none() && self.audio_idx.is_none();

        // Pass 1: Collect stream parameters
        for stream in input.streams() {
            let params = stream.parameters();
            let codec_id = params.id();
            let idx = stream.index();

            if crate::ffmpeg_utils::utils::is_video_codec(codec_id) {
                if include_all || self.video_idx == Some(idx) {
                    video_params = Some((idx, params.clone()));
                }
            } else if crate::ffmpeg_utils::utils::is_audio_codec(codec_id) {
                if include_all || self.audio_idx == Some(idx) {
                    audio_params = Some((idx, params.clone()));
                }
            }
        }

        // Pass 2: Add streams to the muxer
        if let Some((idx, vp)) = &video_params {
            muxer.add_video_stream(vp, *idx)?;
            has_video = true;
        }

        // If including all, we need to iterate again just for adding them all,
        // because `video_params` and `audio_params` above only capture the *last* one if include_all is true.
        // Let's refine the logic to support both modes elegantly:
        if include_all {
            has_video = false;
            has_audio = false;
            for stream in input.streams() {
                let params = stream.parameters();
                let codec_id = params.id();
                let idx = stream.index();

                if crate::ffmpeg_utils::utils::is_video_codec(codec_id) {
                    muxer.add_video_stream(&params, idx)?;
                    has_video = true;
                } else if crate::ffmpeg_utils::utils::is_audio_codec(codec_id) {
                    muxer.add_audio_stream(&params, idx)?;
                    has_audio = true;
                }
            }
        } else {
            if let Some((idx, ap)) = &audio_params {
                if self.transcode_audio_to_aac {
                    // Use AAC encoder parameters instead of source
                    let audio_info = self.index.get_audio_stream(*idx);
                    let bitrate =
                        get_recommended_bitrate(audio_info.map(|a| a.channels).unwrap_or(2));
                    let encoder = AacEncoder::open(HLS_SAMPLE_RATE, 2, bitrate)?;
                    muxer.add_audio_stream(&encoder.codec_parameters(), *idx)?;
                } else {
                    muxer.add_audio_stream(ap, *idx)?;
                }
                has_audio = true;
            }

            if self.video_idx.is_some() && !has_video {
                return Err(HlsError::StreamNotFound(
                    "Video stream not found".to_string(),
                ));
            }
            if self.audio_idx.is_some() && !has_audio {
                return Err(HlsError::StreamNotFound(
                    "Audio stream not found".to_string(),
                ));
            }
        }

        // Pass 3: Construct the MP4 bytes
        // For codecs like AC-3 that don't have extradata, we must feed first packets to the muxer to generate `moov`.
        // We skip this if transcoding to AAC because we already fed the AAC codec parameters explicitly.
        let mut data = if self.transcode_audio_to_aac {
            muxer.write_header(false)?
        } else {
            let mut first_video = None;
            let mut first_audio = None;

            for (s, mut pkt) in input.packets() {
                let s_idx = s.index();

                let is_target_video = (include_all
                    && crate::ffmpeg_utils::utils::is_video_codec(s.parameters().id()))
                    || self.video_idx == Some(s_idx);
                let is_target_audio = (include_all
                    && crate::ffmpeg_utils::utils::is_audio_codec(s.parameters().id()))
                    || self.audio_idx == Some(s_idx);

                if is_target_video && first_video.is_none() {
                    if let Some(output_tb) = muxer.get_output_timebase(s_idx) {
                        pkt.rescale_ts(s.time_base(), output_tb);
                    }
                    pkt.set_pts(Some(0));
                    pkt.set_dts(Some(0));
                    first_video = Some(pkt);
                } else if is_target_audio && first_audio.is_none() {
                    if let Some(output_tb) = muxer.get_output_timebase(s_idx) {
                        pkt.rescale_ts(s.time_base(), output_tb);
                    }
                    pkt.set_pts(Some(0));
                    pkt.set_dts(Some(0));
                    first_audio = Some(pkt);
                }

                if (has_video == first_video.is_some()) && (has_audio == first_audio.is_some()) {
                    break;
                }
            }

            let mut packets = Vec::new();
            if let Some(vp) = first_video {
                packets.push(vp);
            }
            if let Some(ap) = first_audio {
                packets.push(ap);
            }

            if !packets.is_empty() {
                let refs: Vec<&mut ffmpeg::Packet> = packets.iter_mut().collect();
                muxer
                    .generate_init_segment_with_packets(refs)
                    .map_err(|e| {
                        HlsError::Muxing(format!(
                            "Failed to generate init segment via packets: {}",
                            e
                        ))
                    })?
            } else {
                muxer.write_header(false)?
            }
        };

        // Pass 4: Fix TREX durations
        let mut default_duration = 1024; // Safe fallback for audio/mixed
        if has_video {
            default_duration = 3000; // 30fps fallback
            if let Some(video_info) = self.index.video_streams.first() {
                let fps = video_info.framerate;
                if fps.numerator() > 0 {
                    default_duration =
                        (90000 * fps.denominator() as u64 / fps.numerator() as u64) as u32;
                }
            }
        }

        crate::segment::isobmff::fix_trex_durations(&mut data, default_duration);
        Ok(Bytes::from(data))
    }
}

/// Generate an initialization segment (init.mp4)
#[allow(dead_code)] // we'll need this when we support multiplexed tracks
pub(crate) fn generate_init_segment(index: &StreamIndex) -> Result<Bytes> {
    InitSegmentBuilder::new(index).with_all_streams().build()
}

/// Generate a video-only initialization segment
pub(crate) fn generate_video_init_segment(index: &StreamIndex) -> Result<Bytes> {
    if index.video_streams.is_empty() {
        return Err(HlsError::NoVideoStream);
    }
    // We assume the first video stream for the legacy helper, or all video streams if passing None.
    // The previous implementation added ALL video streams.
    // InitSegmentBuilder::with_all_streams combined with only video code is harder,
    // but looking at original, it iterates streams and includes is_video.
    // Let's manually recreate or just use `with_all_streams` for video?
    // Actually the previous `generate_video_init_segment` only added video streams. Let's make a video-only fallback, or just grab the first index.
    // The previous code explicitly checked for video_streams.is_empty() and added all video codecs.
    let mut builder = InitSegmentBuilder::new(index);
    if let Some(vi) = index.video_streams.first() {
        builder = builder.with_video_track(vi.stream_index);
    }
    builder.build()
}

/// Generate an audio-only initialization segment for a specific track
pub(crate) fn generate_audio_init_segment(
    index: &StreamIndex,
    track_index: usize,
    requested_transcode: Option<&str>,
) -> Result<Bytes> {
    let audio_info = index.get_audio_stream(track_index)?;
    let transcode_to_aac = requested_transcode == Some("aac")
        || audio_info.transcode_to == Some(ffmpeg::codec::Id::AAC);

    InitSegmentBuilder::new(index)
        .with_audio_track(track_index)
        .transcode_audio_to_aac(transcode_to_aac)
        .build()
}

/// Generate an interleaved audio-video initialization segment
pub(crate) fn generate_interleaved_init_segment(
    index: &StreamIndex,
    video_idx: usize,
    audio_idx: usize,
    requested_audio_transcode: Option<&str>,
) -> Result<Bytes> {
    let audio_info = index.get_audio_stream(audio_idx).ok();
    let transcode_to_aac = requested_audio_transcode == Some("aac")
        || audio_info
            .map(|t| t.transcode_to == Some(ffmpeg::codec::Id::AAC))
            .unwrap_or(false);

    InitSegmentBuilder::new(index)
        .with_video_track(video_idx)
        .with_audio_track(audio_idx)
        .transcode_audio_to_aac(transcode_to_aac)
        .build()
}

pub(crate) fn generate_interleaved_segment(
    index: &StreamIndex,
    video_idx: usize,
    audio_idx: usize,
    segment: &SegmentInfo,
    _source_path: &Path,
    requested_audio_transcode: Option<&str>,
) -> Result<Bytes> {
    if index.video_streams.is_empty() || index.audio_streams.is_empty() {
        return Err(HlsError::StreamNotFound(
            "Interleaved segment requires both video and audio streams".to_string(),
        ));
    }

    let transcode_to_aac = requested_audio_transcode == Some("aac")
        || index
            .get_audio_stream(audio_idx)
            .map(|t| t.transcode_to == Some(ffmpeg::codec::Id::AAC))
            .unwrap_or(false);

    generate_media_segment_ffmpeg(
        segment,
        "av",
        Some(video_idx),
        Some(audio_idx),
        index,
        transcode_to_aac,
    )
}

pub(crate) fn generate_video_segment(
    index: &StreamIndex,
    track_index: usize,
    sequence: usize,
    _source_path: &Path,
) -> Result<Bytes> {
    let segment = index.get_segment("video", sequence)?;
    generate_media_segment_ffmpeg(segment, "video", Some(track_index), None, index, false)
}

/// Generate an audio segment
///
/// Dispatches to the transcoding pipeline for non-AAC streams; falls back to
/// direct packet copy for AAC streams.
pub(crate) fn generate_audio_segment(
    index: &StreamIndex,
    track_index: usize,
    sequence: usize,
    _source_path: &Path,
    requested_transcode: Option<&str>,
) -> Result<Bytes> {
    let segment = index.get_segment("audio", sequence)?;

    // Check if this track needs transcoding
    // TODO: support more codecs than aac.
    let audio_info = index.get_audio_stream(track_index)?;
    let transcode_to_aac = requested_transcode == Some("aac")
        || audio_info.transcode_to == Some(ffmpeg::codec::Id::AAC);

    if transcode_to_aac {
        generate_transcoded_audio_segment(index, audio_info, segment)
    } else {
        generate_media_segment_ffmpeg(segment, "audio", None, Some(track_index), index, false)
    }
}

/// Generate an audio segment by transcoding (decode → resample → AAC encode → fMP4 mux)
fn generate_transcoded_audio_segment(
    index: &StreamIndex,
    audio_info: &crate::media::AudioStreamInfo,
    segment: &SegmentInfo,
) -> Result<Bytes> {
    let video_timebase = index.video_timebase;

    let target_start_sec = segment.start_pts as f64 * video_timebase.numerator() as f64
        / video_timebase.denominator() as f64;
    let seek_sec = (target_start_sec - 0.5).max(0.0);
    let seek_ts = (seek_sec * 1_000_000.0) as i64;

    let mut input = index.get_context()?;

    input
        .seek(seek_ts, ..seek_ts)
        .map_err(|e| HlsError::Ffmpeg(crate::error::FfmpegError::ReadFrame(e.to_string())))?;

    let mut buffered_packets = Vec::new();
    let stream_index = audio_info.stream_index;

    let audio_stream = input.stream(stream_index).ok_or_else(|| {
        HlsError::StreamNotFound(format!("audio stream {} not found", stream_index))
    })?;
    let audio_timebase = audio_stream.time_base();
    let audio_decoder = crate::transcode::decoder::AudioDecoder::open(&audio_stream)?;

    let end_pts_90k = crate::ffmpeg_utils::utils::rescale_ts(
        segment.end_pts,
        video_timebase,
        ffmpeg::Rational(1, 90000),
    );

    for (stream, packet) in input.packets() {
        if stream.index() != stream_index {
            continue;
        }

        let pkt_pts = packet.pts().or(packet.dts()).unwrap_or(0);
        let pkt_90k = crate::ffmpeg_utils::utils::rescale_ts(
            pkt_pts,
            stream.time_base(),
            ffmpeg::Rational(1, 90000),
        );
        if pkt_90k >= end_pts_90k {
            break;
        }

        buffered_packets.push(packet);
    }

    std::mem::drop(input);

    let (aac_packets, output_timebase) = crate::transcode::pipeline::transcode_audio_segment(
        audio_decoder,
        buffered_packets,
        audio_timebase,
        audio_info,
        segment,
        video_timebase,
        true,
    )?;

    if aac_packets.is_empty() {
        tracing::warn!(
            seq = segment.sequence,
            codec = ?audio_info.codec_id,
        "Audio transcoding produced 0 packets - returning empty segment"
        );
        // Return a minimal empty fMP4 rather than a 500 so the player skips
        // this segment gracefully instead of retrying forever.
        return Err(HlsError::Muxing(format!(
            "Audio transcoding produced no packets for segment {} (codec={:?})",
            segment.sequence, audio_info.codec_id
        )));
    }

    // Mux the AAC packets into an fMP4 segment using frag_every_frame so each
    // write_packet call emits a moof+mdat immediately (audio-only has no video
    // keyframes to trigger fragmentation, so frag_keyframe doesn't work here).
    let bitrate = get_recommended_bitrate(audio_info.channels);
    let encoder = AacEncoder::open(HLS_SAMPLE_RATE, 2, bitrate)?;

    let full_data = mux_aac_packets_to_fmp4(&encoder.codec_parameters(), aac_packets)?;

    // Strip the init segment, return only the media segment
    let media_offset = find_media_segment_offset(&full_data).ok_or_else(|| {
        HlsError::Muxing("Transcoded audio: no media segment found (moof/styp missing)".to_string())
    })?;

    let mut media_data = full_data[media_offset..].to_vec();

    // Patch tfdt with the correct segment start time
    // We strictly align target_time to the 1024-sample grid (the AAC frame size)
    // so that consecutive fragments do not accumulate decimal rounding errors or drift.
    let target_time = {
        let exact_target = crate::ffmpeg_utils::utils::rescale_ts(
            segment.start_pts,
            video_timebase,
            output_timebase,
        )
        .max(0) as u64;

        let grid_size = 1024;
        (exact_target / grid_size) * grid_size
    };

    // Use a large multiplier for fragment sequence numbers to ensure they are
    // Each segment gets its own ID. We use segment.sequence + 1 to be contiguous (1-based).
    let start_frag_seq = segment.sequence as u32 + 1;

    crate::segment::isobmff::patch_tfdts(&mut media_data, target_time, start_frag_seq);

    Ok(Bytes::from(media_data))
}

/// Generate a subtitle segment (WebVTT).
///
/// Uses the per-sample byte-offset index built at scan time to seek directly
/// to each subtitle sample in the file.  No full-file scan, no iteration over
/// video/audio packets — only the subtitle samples that fall within the
/// requested time range are read.
pub(crate) fn generate_subtitle_segment(
    index: &StreamIndex,
    track_index: usize,
    start_sequence: usize,
    end_sequence: usize,
    _source_path: &Path,
) -> Result<Bytes> {
    let start_segment = index.get_segment("subtitle", start_sequence)?;
    let end_segment = index.get_segment("subtitle", end_sequence)?;

    let sub_info = index.get_subtitle_stream(track_index)?;

    if is_bitmap_subtitle_codec(sub_info.codec_id) {
        return Err(HlsError::Muxing(format!(
            "Subtitle stream {} uses a bitmap codec ({:?}) which cannot be converted to WebVTT",
            track_index, sub_info.codec_id
        )));
    }

    let video_tb = index.video_timebase;
    let stream_timebase = sub_info.timebase;
    let sub_start_time = sub_info.start_time;

    // Compute segment boundaries in the subtitle stream's timebase (playtime-relative)
    // video_st is 0 for the purpose of playtime since start_pts already accounts for it.
    let seg_start_playtime = start_segment.start_pts;
    let seg_end_playtime = end_segment.end_pts;

    let start_ts_playtime =
        crate::ffmpeg_utils::utils::rescale_ts(seg_start_playtime, video_tb, stream_timebase);
    let end_ts_playtime =
        crate::ffmpeg_utils::utils::rescale_ts(seg_end_playtime, video_tb, stream_timebase);

    // Segment bounds in milliseconds for clamping
    let seg_start_ms = crate::ffmpeg_utils::utils::rescale_ts(
        start_segment.start_pts,
        video_tb,
        ffmpeg::Rational::new(1, 1000),
    );
    let seg_end_ms = crate::ffmpeg_utils::utils::rescale_ts(
        end_segment.end_pts,
        video_tb,
        ffmpeg::Rational::new(1, 1000),
    );

    // Binary-search the sample index for the first entry that could overlap
    // the segment: find the first entry whose pts + (some duration) >= start.
    // Since we don't store duration in the index, we use pts >= start - 10s
    // as a conservative lower bound (subtitle cues are rarely > 10s long).
    let search_start_ts = start_ts_playtime + sub_start_time
        - crate::ffmpeg_utils::utils::rescale_ts(10, ffmpeg::Rational::new(1, 1), stream_timebase);

    let first_idx = sub_info
        .sample_index
        .partition_point(|s| s.pts < search_start_ts);

    // Collect the subset of samples that fall within [start_ts_playtime, end_ts_playtime)
    // expressed in the subtitle stream's absolute PTS space.
    let abs_start = start_ts_playtime + sub_start_time;
    let abs_end = end_ts_playtime + sub_start_time;

    let matching: Vec<_> = sub_info.sample_index[first_idx..]
        .iter()
        .take_while(|s| s.pts < abs_end)
        .collect();

    if matching.is_empty() {
        // No subtitle cues in this segment — return an empty WebVTT
        let config = WebVttConfig {
            include_header_comment: false,
        };
        let mut writer = WebVttWriter::with_config(config);
        return Ok(writer.write(&[]));
    }

    // Open the file and seek once to the start of the subtitle window.
    // AVSEEK_FLAG_BYTE is not used: avformat_find_stream_info reads ~13MB on open,
    // after which backward byte-seeks are ignored by the MP4 demuxer.
    // Instead we do a single timestamp seek and iterate only subtitle packets,
    // stopping as soon as we pass abs_end.  The sample_index tells us the exact
    // PTS range so we never scan the whole file.
    let mut input = index.get_context()?;

    // Seek to just before the first matching sample using AV_TIME_BASE (µs).
    let first_sample_pts = matching.first().map(|s| s.pts).unwrap_or(abs_start);
    // Convert from subtitle stream timebase to AV_TIME_BASE (µs)
    let seek_us = crate::ffmpeg_utils::utils::rescale_ts(
        first_sample_pts,
        stream_timebase,
        ffmpeg::Rational::new(1, 1_000_000),
    );
    let _ = input.seek(seek_us, ..seek_us); // non-fatal; worst case we read a few extra packets

    let extractor = SubtitleExtractor::new(sub_info.codec_id, stream_timebase);
    let mut cues = Vec::new();

    // video_st_in_sub_tb: used to align subtitle PTS to the video timeline
    let video_st = {
        let st = index
            .video_streams
            .first()
            .and_then(|v| input.stream(v.stream_index))
            .map(|s| s.start_time())
            .unwrap_or(0);
        if st == std::i64::MIN {
            0
        } else {
            st
        }
    };
    let video_st_in_sub_tb =
        crate::ffmpeg_utils::utils::rescale_ts(video_st, video_tb, stream_timebase);

    // Build a set of the expected PTS values so we can stop early once all are seen.
    let mut remaining: std::collections::HashSet<i64> = matching.iter().map(|s| s.pts).collect();

    for (stream, mut packet) in input.packets() {
        if stream.index() != track_index {
            continue;
        }
        let pts = packet.pts().unwrap_or(0);
        if pts >= abs_end {
            break;
        }
        if pts < abs_start {
            continue;
        }

        remaining.remove(&pts);

        let sub_playtime = pts.saturating_sub(sub_start_time);
        let aligned_pts = sub_playtime + video_st_in_sub_tb;
        packet.set_pts(Some(aligned_pts));

        match extractor.extract_cues(&packet) {
            Ok(c) => cues.extend(c),
            Err(e) => tracing::debug!(
                track_index,
                start_sequence,
                end_sequence,
                "subtitle cue extraction error (skipping): {}",
                e
            ),
        }

        if remaining.is_empty() {
            break;
        }
    }

    // Clamp cue timestamps to segment bounds
    for cue in &mut cues {
        if cue.start_ms < seg_start_ms {
            cue.start_ms = seg_start_ms;
        }
        if cue.end_ms > seg_end_ms {
            cue.end_ms = seg_end_ms;
        }
    }
    cues.retain(|cue| cue.start_ms < cue.end_ms);

    let config = WebVttConfig {
        include_header_comment: false,
    };
    let mut writer = WebVttWriter::with_config(config);
    let bytes = writer.write(&cues);

    tracing::debug!(
        track_index,
        start_sequence,
        end_sequence,
        cues = cues.len(),
        "generate_subtitle_segment: done"
    );

    Ok(bytes)
}

pub(crate) struct BufferedPacket {
    pub stream_id: usize,
    pub packet: ffmpeg::Packet,
    pub timebase: ffmpeg::Rational,
    pub is_video_stream: bool,
}

fn buffer_media_packets(
    input: &mut ffmpeg::format::context::Input,
    segment: &SegmentInfo,
    segment_type: &str,
    video_timebase: ffmpeg::Rational,
    stream_indices: &[usize],
    audio_track_index: Option<usize>,
) -> Vec<BufferedPacket> {
    let mut buffered_packets = Vec::new();
    let is_interleaved = segment_type == "av";

    let end_pts_90k = crate::ffmpeg_utils::utils::rescale_ts(
        segment.end_pts,
        video_timebase,
        ffmpeg::Rational(1, 90000),
    );

    let mut packet_count = 0;

    for (stream, packet) in input.packets() {
        let stream_id = stream.index();
        let is_video_stream = crate::ffmpeg_utils::utils::is_video_codec(stream.parameters().id());
        let pts_90k = crate::ffmpeg_utils::utils::rescale_ts(
            packet.pts().or(packet.dts()).unwrap_or(0),
            stream.time_base(),
            ffmpeg::Rational(1, 90000),
        );
        let dts_90k = crate::ffmpeg_utils::utils::rescale_ts(
            packet.dts().or(packet.pts()).unwrap_or(0),
            stream.time_base(),
            ffmpeg::Rational(1, 90000),
        );

        if segment_type == "video" || (is_interleaved && is_video_stream) {
            if packet.is_key() && dts_90k >= end_pts_90k {
                break;
            }
        } else {
            if !is_interleaved {
                if pts_90k >= end_pts_90k {
                    if packet_count > 0 {
                        break;
                    } else {
                        continue;
                    }
                }
            }
        }

        if is_interleaved
            && !stream_indices.contains(&stream_id)
            && audio_track_index != Some(stream_id)
        {
            continue;
        }
        if !is_interleaved && stream_id != stream_indices[0] {
            continue;
        }

        buffered_packets.push(BufferedPacket {
            stream_id,
            packet,
            timebase: stream.time_base().clone(),
            is_video_stream,
        });
        packet_count += 1;
    }

    buffered_packets
}

fn transcode_audio_if_needed(
    input: &mut ffmpeg::format::context::Input,
    index: &StreamIndex,
    audio_track_index: Option<usize>,
    transcode_audio_to_aac: bool,
    buffered_packets: &[BufferedPacket],
    segment: &SegmentInfo,
    video_timebase: ffmpeg::Rational,
) -> Result<(Vec<ffmpeg::Packet>, Option<ffmpeg::Rational>)> {
    let mut transcoded_audio_packets = Vec::new();
    let mut audio_output_tb = None;

    if transcode_audio_to_aac {
        if let Some(audio_idx) = audio_track_index {
            if let Some(s) = input.stream(audio_idx) {
                let decoder = crate::transcode::decoder::AudioDecoder::open(&s)?;
                let audio_info = index.get_audio_stream(audio_idx)?;
                let raw_audio_packets: Vec<_> = buffered_packets
                    .iter()
                    .filter(|p| p.stream_id == audio_idx)
                    .map(|p| p.packet.clone())
                    .collect();

                let mut audio_tb = ffmpeg::Rational(1, 90000);
                if let Some(p) = buffered_packets.iter().find(|p| p.stream_id == audio_idx) {
                    audio_tb = p.timebase;
                }

                let (aac_packets, output_tb) = crate::transcode::pipeline::transcode_audio_segment(
                    decoder,
                    raw_audio_packets,
                    audio_tb,
                    audio_info,
                    segment,
                    video_timebase,
                    false,
                )?;
                transcoded_audio_packets = aac_packets;
                audio_output_tb = Some(output_tb);
            }
        }
    }

    Ok((transcoded_audio_packets, audio_output_tb))
}

fn mux_media_segment(
    segment_type: &str,
    is_interleaved: bool,
    transcode_audio_to_aac: bool,
    video_timebase: ffmpeg::Rational,
    segment: &SegmentInfo,
    mut muxer: Fmp4Muxer,
    buffered_packets: Vec<BufferedPacket>,
    audio_track_index: Option<usize>,
    mut transcoded_audio_packets: Vec<ffmpeg::Packet>,
    audio_output_tb: Option<ffmpeg::Rational>,
) -> Result<(Fmp4Muxer, Option<i64>, Option<i64>, Option<i64>)> {
    let start_pts_90k = crate::ffmpeg_utils::utils::rescale_ts(
        segment.start_pts,
        video_timebase,
        ffmpeg::Rational(1, 90000),
    );

    let mut first_packet_dts: Option<i64> = None;
    let mut first_video_dts: Option<i64> = None;
    let mut first_audio_dts: Option<i64> = None;

    let mut audio_packet_idx = 0;

    // We can't capture mut muxer in a closure easily if we return it later,
    // so we just define a helper nested or inline macro.
    macro_rules! write_transcoded_audio_upto {
        ($target_dts_90k:expr) => {
            if !transcoded_audio_packets.is_empty()
                && audio_packet_idx < transcoded_audio_packets.len()
            {
                let tb = audio_output_tb.unwrap();
                let audio_idx = audio_track_index.unwrap();

                while audio_packet_idx < transcoded_audio_packets.len() {
                    let pkt = &mut transcoded_audio_packets[audio_packet_idx];
                    let pkt_dts = pkt.dts().or(pkt.pts()).unwrap_or(0);
                    let pkt_dts_90k = crate::ffmpeg_utils::utils::rescale_ts(
                        pkt_dts,
                        tb,
                        ffmpeg::Rational(1, 90000),
                    );

                    if pkt_dts_90k <= $target_dts_90k {
                        if first_packet_dts.is_none() {
                            first_packet_dts = Some(pkt_dts);
                        }
                        if first_audio_dts.is_none() {
                            first_audio_dts = Some(pkt_dts);
                        }

                        pkt.set_stream(audio_idx);
                        muxer.write_packet(pkt)?;
                        audio_packet_idx += 1;
                    } else {
                        break;
                    }
                }
            }
        };
    }

    for BufferedPacket {
        stream_id,
        mut packet,
        timebase,
        is_video_stream,
    } in buffered_packets
    {
        let pts_90k = crate::ffmpeg_utils::utils::rescale_ts(
            packet.pts().or(packet.dts()).unwrap_or(0),
            timebase,
            ffmpeg::Rational(1, 90000),
        );
        let dts_90k = crate::ffmpeg_utils::utils::rescale_ts(
            packet.dts().or(packet.pts()).unwrap_or(0),
            timebase,
            ffmpeg::Rational(1, 90000),
        );

        if is_interleaved && transcode_audio_to_aac && audio_track_index == Some(stream_id) {
            continue;
        }

        if segment_type == "video" || (is_interleaved && is_video_stream) {
            if dts_90k < start_pts_90k {
                continue;
            }
        } else {
            if pts_90k < start_pts_90k {
                continue;
            }
        }

        if let Some(out_tb) = muxer.get_output_timebase(stream_id) {
            let in_tb = timebase;
            if let Some(pts) = packet.pts() {
                let out_pts = pts.rescale(in_tb, out_tb);
                packet.set_pts(Some(out_pts));
                if let Some(dts) = packet.dts() {
                    let out_dts = dts.rescale(in_tb, out_tb);
                    packet.set_dts(Some(out_dts));
                    if first_packet_dts.is_none() {
                        first_packet_dts = Some(out_dts);
                    }
                    if is_interleaved {
                        if is_video_stream {
                            if first_video_dts.is_none() {
                                first_video_dts = Some(out_dts);
                            }
                        } else {
                            if first_audio_dts.is_none() {
                                first_audio_dts = Some(out_dts);
                            }
                        }
                    }
                } else if first_packet_dts.is_none() {
                    first_packet_dts = Some(out_pts);
                }

                let in_dur = packet.duration();
                if in_dur > 0 {
                    let out_dur = in_dur.rescale(in_tb, out_tb);
                    packet.set_duration(out_dur);
                }
            }
        }

        if transcode_audio_to_aac {
            write_transcoded_audio_upto!(dts_90k);
        }

        muxer.write_packet(&mut packet)?;
    }

    if transcode_audio_to_aac {
        write_transcoded_audio_upto!(i64::MAX);
    }

    Ok((muxer, first_video_dts, first_audio_dts, first_packet_dts))
}

fn finalize_segment(
    segment_type: &str,
    is_interleaved: bool,
    transcode_audio_to_aac: bool,
    video_timebase: ffmpeg::Rational,
    segment: &SegmentInfo,
    index: &StreamIndex,
    audio_track_index: Option<usize>,
    mut muxer: Fmp4Muxer,
) -> Result<Bytes> {
    let full_data = muxer.finalize()?;

    let media_offset =
        crate::segment::muxer::find_media_segment_offset(&full_data).ok_or_else(|| {
            HlsError::Muxing("No media segment data found (moof/styp missing)".to_string())
        })?;
    let mut media_data = full_data[media_offset..].to_vec();

    let encoder_delay: i64 = if segment_type == "audio" {
        if let Some(target) = audio_track_index {
            index
                .audio_streams
                .iter()
                .find(|a| a.stream_index == target)
                .map(|a| a.encoder_delay)
                .unwrap_or(0)
        } else {
            index
                .audio_streams
                .first()
                .map(|a| a.encoder_delay)
                .unwrap_or(0)
        }
    } else {
        0
    };

    let video_target_tfdt = if segment_type == "audio" {
        0
    } else {
        crate::ffmpeg_utils::utils::rescale_ts(
            segment.start_pts,
            video_timebase,
            ffmpeg::Rational(1, 90000),
        )
        .max(0) as u64
            + encoder_delay as u64
    };

    let audio_target_tfdt = if segment_type == "audio" || is_interleaved {
        crate::ffmpeg_utils::utils::rescale_ts(
            segment.start_pts,
            video_timebase,
            ffmpeg::Rational(1, 90000),
        )
        .max(0) as u64
            + encoder_delay as u64
    } else {
        video_target_tfdt
    };

    let start_frag_seq = segment.sequence as u32 + 1;

    if is_interleaved {
        let v_track: u32 = 1;
        let a_track: u32 = 2;

        let audio_tfdt_for_patch = if transcode_audio_to_aac {
            let audio_tb =
                ffmpeg::Rational::new(1, crate::transcode::pipeline::HLS_SAMPLE_RATE as i32);
            crate::ffmpeg_utils::utils::rescale_ts(segment.start_pts, video_timebase, audio_tb)
                .max(0) as u64
        } else {
            let a_idx = audio_track_index.unwrap_or(0);
            if let Ok(audio_info) = index.get_audio_stream(a_idx) {
                let audio_tb = ffmpeg::Rational::new(1, audio_info.sample_rate as i32);
                crate::ffmpeg_utils::utils::rescale_ts(segment.start_pts, video_timebase, audio_tb)
                    .max(0) as u64
            } else {
                audio_target_tfdt
            }
        };

        crate::segment::isobmff::patch_tfdts_per_track(
            &mut media_data,
            start_frag_seq,
            v_track,
            a_track,
            video_target_tfdt,
            audio_tfdt_for_patch,
        );
    } else {
        crate::segment::isobmff::patch_tfdts(&mut media_data, video_target_tfdt, start_frag_seq);
    }

    let styp_box: [u8; 24] = [
        0x00, 0x00, 0x00, 24, b's', b't', b'y', b'p', b'i', b's', b'o', b'8', 0x00, 0x00, 0x02,
        0x00, b'i', b's', b'o', b'8', b'c', b'm', b'f', b'c',
    ];
    media_data.splice(0..0, styp_box);

    Ok(Bytes::from(media_data))
}

/// Parse the minimum display PTS across all samples in a muxed fMP4 segment.
/// Scans every moof/traf/tfdt/trun box and returns the smallest (tfdt + sample_CT) value.
/// This is the earliest frame display time, used to align audio tfdt.
fn generate_media_segment_ffmpeg(
    segment: &SegmentInfo,
    segment_type: &str,
    video_track_index: Option<usize>,
    audio_track_index: Option<usize>,
    index: &StreamIndex,
    transcode_audio_to_aac: bool,
) -> Result<Bytes> {
    let is_interleaved = segment_type == "av";
    let video_timebase = index.video_timebase;

    let target_start_sec = segment.start_pts as f64 * video_timebase.numerator() as f64
        / video_timebase.denominator() as f64;
    let seek_sec = if is_interleaved && transcode_audio_to_aac {
        (target_start_sec - 0.5).max(0.0)
    } else {
        target_start_sec
    };
    let seek_ts = (seek_sec * 1_000_000.0) as i64;

    let mut input = index.get_context()?;
    input
        .seek(seek_ts, ..seek_ts)
        .map_err(|e| HlsError::Ffmpeg(crate::error::FfmpegError::ReadFrame(e.to_string())))?;

    let mut muxer = Fmp4Muxer::new()?;
    let mut stream_indices = Vec::new();

    for stream in input.streams() {
        let params = stream.parameters();
        let codec_id = params.id();
        let idx = stream.index();

        if is_interleaved {
            if let Some(video_idx) = video_track_index {
                if idx == video_idx && crate::ffmpeg_utils::utils::is_video_codec(codec_id) {
                    muxer.add_video_stream(&params, idx)?;
                    stream_indices.push(idx);
                }
            }
            if let Some(audio_idx) = audio_track_index {
                if idx == audio_idx && crate::ffmpeg_utils::utils::is_audio_codec(codec_id) {
                    let audio_info = index.get_audio_stream(audio_idx)?;
                    if transcode_audio_to_aac {
                        let bitrate =
                            crate::transcode::encoder::get_recommended_bitrate(audio_info.channels);
                        let encoder = crate::transcode::encoder::AacEncoder::open(
                            crate::transcode::pipeline::HLS_SAMPLE_RATE,
                            2,
                            bitrate,
                        )?;
                        muxer.add_audio_stream(&encoder.codec_parameters(), idx)?;
                    } else {
                        muxer.add_audio_stream(&params, idx)?;
                    }
                    stream_indices.push(idx);
                }
            }
        } else {
            let is_video =
                segment_type == "video" && crate::ffmpeg_utils::utils::is_video_codec(codec_id);
            let is_audio =
                segment_type == "audio" && crate::ffmpeg_utils::utils::is_audio_codec(codec_id);

            if is_video || is_audio {
                if let Some(target) = video_track_index.or(audio_track_index) {
                    if idx != target {
                        continue;
                    }
                }
                if is_video {
                    muxer.add_video_stream(&params, idx)?;
                } else {
                    muxer.add_audio_stream(&params, idx)?;
                }
                stream_indices.push(idx);
                break;
            }
        }
    }

    if stream_indices.is_empty() {
        return Err(HlsError::StreamNotFound(format!(
            "No {} stream found",
            segment_type
        )));
    }

    muxer.write_header(segment_type == "av" || segment_type == "audio")?;

    let buffered_packets = buffer_media_packets(
        &mut input,
        segment,
        segment_type,
        video_timebase,
        &stream_indices,
        audio_track_index,
    );

    let (transcoded_audio_packets, audio_output_tb) = transcode_audio_if_needed(
        &mut input,
        index,
        audio_track_index,
        transcode_audio_to_aac,
        &buffered_packets,
        segment,
        video_timebase,
    )?;

    std::mem::drop(input);

    let (muxer, _v_dts, _a_dts, _p_dts) = mux_media_segment(
        segment_type,
        is_interleaved,
        transcode_audio_to_aac,
        video_timebase,
        segment,
        muxer,
        buffered_packets,
        audio_track_index,
        transcoded_audio_packets,
        audio_output_tb,
    )?;

    finalize_segment(
        segment_type,
        is_interleaved,
        transcode_audio_to_aac,
        video_timebase,
        segment,
        index,
        audio_track_index,
        muxer,
    )
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::StreamIndex;

    #[test]
    fn test_generate_video_segment_integration() {
        // Initialize FFmpeg
        let _ = ffmpeg::init();

        let mut path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        path.push("testvideos");
        path.push("bun33s.mp4");

        if !path.exists() {
            eprintln!("Test video not found at {:?}, skipping test", path);
            return;
        }

        // Mock StreamIndex
        let mut index = StreamIndex::new(path.clone());

        // Mock a segment (first 4 seconds)
        let segment = crate::media::SegmentInfo {
            sequence: 0,
            start_pts: 0,
            end_pts: 360000, // 4 seconds * 90000
            duration_secs: 4.0,
            is_keyframe: true,
            video_byte_offset: 0,
        };
        index.segments.push(segment);

        // Call generate_video_segment
        // Note: The third argument source_path in generate_video_segment is seemingly unused in the function body
        // (it uses index.source_path), but we pass it anyway.
        let result = generate_video_segment(&index, 0, 0, &path);

        match result {
            Ok(bytes) => {
                assert!(!bytes.is_empty(), "Generated segment should not be empty");
                println!("Generated video segment size: {}", bytes.len());

                // Check for 'styp', 'moof', 'mdat'
                assert!(bytes.windows(4).any(|w| w == b"styp"));
                assert!(bytes.windows(4).any(|w| w == b"moof"));
                assert!(bytes.windows(4).any(|w| w == b"mdat"));
            }
            Err(e) => panic!("Failed to generate video segment: {:?}", e),
        }
    }

    #[test]
    fn test_generate_video_segment_advancement() {
        let _ = ffmpeg::init();
        let mut path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        path.push("testvideos");
        path.push("bun33s.mp4");
        if !path.exists() {
            return;
        }

        let mut index = StreamIndex::new(path.clone());
        // Sequence 1 starts at 4.0s (360000 pts)
        let segment = crate::media::SegmentInfo {
            sequence: 1,
            start_pts: 360000,
            end_pts: 720000,
            duration_secs: 4.0,
            is_keyframe: true,
            video_byte_offset: 0,
        };
        index.segments.push(segment.clone());
        // Simplest way to have sequence 1 at index 1
        index.segments.push(segment);

        let result = generate_video_segment(&index, 0, 1, &path);

        match result {
            Ok(bytes) => {
                assert!(!bytes.is_empty());
                // We patched it, so let's check if we see the log or if we can parse it here
                // We'll rely on the eprintln! for manual verification in --nocapture
            }
            Err(e) => panic!("Failed to generate video segment: {:?}", e),
        }
    }

    #[test]
    fn test_generate_video_init_segment_trex() {
        use crate::media::VideoStreamInfo;

        // Initialize FFmpeg
        let _ = ffmpeg::init();

        let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
        // Use the generated test video
        let source_path = std::path::PathBuf::from(manifest_dir)
            .join("tests")
            .join("assets")
            .join("video.mp4");

        if !source_path.exists() {
            eprintln!("Test video not found at {:?}, skipping test", source_path);
            return;
        }

        // Mock StreamIndex with 25fps (duration should be 3600)
        let index = StreamIndex {
            stream_id: "test_stream".to_string(),
            source_path: source_path.clone(),
            duration_secs: 5.0,
            video_timebase: ffmpeg::Rational(1, 12800),
            video_streams: vec![VideoStreamInfo {
                stream_index: 0,
                width: 640,
                height: 360,
                framerate: ffmpeg::Rational(25, 1),
                codec_id: ffmpeg::codec::Id::H264,
                bitrate: 500000,
                language: None,
                profile: None,
                level: None,
            }],
            audio_streams: vec![],
            subtitle_streams: vec![],
            segments: vec![],
            indexed_at: std::time::SystemTime::now(),
            last_accessed: std::sync::atomic::AtomicU64::new(0),
            segment_first_pts: std::sync::Arc::new(Vec::new()),
            cached_context: None,
            cache_enabled: true,
            last_requested_segment: std::sync::atomic::AtomicI64::new(-1),
            lookahead_queue: std::sync::Mutex::new(std::collections::VecDeque::new()),
        };

        let init_segment =
            generate_video_init_segment(&index).expect("Failed to generate init segment");

        // Parse trex
        let mut pos = 0;
        let mut found_trex = false;
        while pos + 8 <= init_segment.len() {
            let size = u32::from_be_bytes(init_segment[pos..pos + 4].try_into().unwrap()) as usize;
            let type_bytes = &init_segment[pos + 4..pos + 8];

            if type_bytes == b"moov" {
                let mut p2 = pos + 8;
                let end2 = pos + size;
                while p2 + 8 <= end2 {
                    let s2 =
                        u32::from_be_bytes(init_segment[p2..p2 + 4].try_into().unwrap()) as usize;
                    let t2 = &init_segment[p2 + 4..p2 + 8];
                    if t2 == b"mvex" {
                        let mut p3 = p2 + 8;
                        let end3 = p2 + s2;
                        while p3 + 8 <= end3 {
                            let s3 =
                                u32::from_be_bytes(init_segment[p3..p3 + 4].try_into().unwrap())
                                    as usize;
                            let t3 = &init_segment[p3 + 4..p3 + 8];
                            if t3 == b"trex" {
                                let dur = u32::from_be_bytes(
                                    init_segment[p3 + 20..p3 + 24].try_into().unwrap(),
                                );
                                println!("Found trex default sample duration: {}", dur);
                                assert_eq!(dur, 3600, "Expected default sample duration 3600 for 25fps video (got {})", dur);
                                found_trex = true;
                            }
                            p3 += s3;
                        }
                    }
                    p2 += s2;
                }
            }
            pos += size;
        }
        assert!(found_trex, "trex box not found in init segment");
    }

    #[test]
    fn test_generate_audio_segment_integration() {
        // Initialize FFmpeg
        let _ = ffmpeg::init();

        let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
        let source_path = std::path::PathBuf::from(manifest_dir)
            .join("testvideos")
            .join("bun33s.mp4");

        if !source_path.exists() {
            eprintln!("Test video not found at {:?}, skipping test", source_path);
            return;
        }

        // Mock StreamIndex
        let mut index = StreamIndex::new(source_path.clone());

        // Add an audio stream info
        index.audio_streams.push(crate::media::AudioStreamInfo {
            stream_index: 1, // In bun33s.mp4, index 1 is audio
            codec_id: ffmpeg::codec::Id::AAC,
            sample_rate: 48000,
            channels: 2,
            bitrate: 128000,
            language: Some("en".to_string()),
            transcode_to: None,
            encoder_delay: 0,
        });

        // Mock a segment (first 4 seconds)
        let segment = crate::media::SegmentInfo {
            sequence: 0,
            start_pts: 0,
            end_pts: 360000, // 4 seconds * 90000
            duration_secs: 4.0,
            is_keyframe: true,
            video_byte_offset: 0,
        };
        index.segments.push(segment);

        // Call generate_audio_segment
        let result = generate_audio_segment(&index, 1, 0, &source_path, None);

        match result {
            Ok(bytes) => {
                println!("Generated audio segment: {} bytes", bytes.len());
                assert!(bytes.len() > 100);

                // Check for 'styp' and 'moof'
                assert!(bytes.windows(4).any(|w| w == b"styp"));
                assert!(bytes.windows(4).any(|w| w == b"moof"));
                assert!(bytes.windows(4).any(|w| w == b"mdat"));
            }
            Err(e) => panic!("Failed to generate audio segment: {:?}", e),
        }
    }

    #[test]
    fn test_generate_audio_init_timescale() {
        // Initialize FFmpeg
        let _ = ffmpeg::init();

        let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
        let source_path = std::path::PathBuf::from(manifest_dir)
            .join("testvideos")
            .join("bun33s.mp4");

        if !source_path.exists() {
            eprintln!("Test video not found at {:?}, skipping test", source_path);
            return;
        }

        // Mock StreamIndex
        let mut index = StreamIndex::new(source_path.clone());

        // Add an audio stream info with specific sample rate
        index.audio_streams.push(crate::media::AudioStreamInfo {
            stream_index: 1,
            codec_id: ffmpeg::codec::Id::AAC,
            sample_rate: 44100, // Match bun33s.mp4
            channels: 2,
            bitrate: 128000,
            language: Some("en".to_string()),
            transcode_to: None,
            encoder_delay: 0,
        });

        let init_segment = generate_audio_init_segment(&index, 1, None)
            .expect("Failed to generate audio init segment");

        // Find 'mdhd' box for the audio track and check timescale
        // mdhd version 0:
        // Size(4), Type(4), Version(1), Flags(3), Creation(4), Mod(4), Timescale(4), Duration(4)...
        // Total offset to Timescale: 4+4+1+3+4+4 = 20

        let mut pos = 0;
        let mut found_mdhd = false;
        while pos + 8 <= init_segment.len() {
            let size = u32::from_be_bytes(init_segment[pos..pos + 4].try_into().unwrap()) as usize;
            let type_bytes = &init_segment[pos + 4..pos + 8];

            if type_bytes == b"moov" || type_bytes == b"trak" || type_bytes == b"mdia" {
                // Recurse into container boxes manually for simplicity in this test
                pos += 8;
                continue;
            }

            if type_bytes == b"mdhd" {
                let timescale =
                    u32::from_be_bytes(init_segment[pos + 20..pos + 24].try_into().unwrap());
                println!("Found audio mdhd timescale: {}", timescale);
                assert_eq!(timescale, 44100);
                found_mdhd = true;
                break;
            }

            if size < 8 {
                break;
            }
            pos += size;
        }
        assert!(found_mdhd, "mdhd box not found in audio init segment");
    }
}
