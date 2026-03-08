//! Segment generator - uses FFmpeg CLI for reliable segment generation

use bytes::Bytes;
use std::path::Path;

use ffmpeg_next::{self as ffmpeg, Rescale};

use crate::error::{HlsError, Result};
use crate::media::{SegmentInfo, StreamIndex};
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

        let include_all = self.video_idx.is_none() && self.audio_idx.is_none();

        // Pass 1: Register streams with the muxer
        for stream in input.streams() {
            let params = stream.parameters();
            let codec_id = params.id();
            let idx = stream.index();

            let is_target_video =
                (include_all && crate::ffmpeg_utils::utils::is_video_codec(codec_id))
                    || self.video_idx == Some(idx);
            let is_target_audio =
                (include_all && crate::ffmpeg_utils::utils::is_audio_codec(codec_id))
                    || self.audio_idx == Some(idx);

            if is_target_video {
                muxer.add_video_stream(&params, idx)?;
                has_video = true;
            } else if is_target_audio {
                if self.transcode_audio_to_aac {
                    let audio_info = self.index.get_audio_stream(idx);
                    let bitrate =
                        get_recommended_bitrate(audio_info.map(|a| a.channels).unwrap_or(2));
                    let encoder = AacEncoder::open(HLS_SAMPLE_RATE, 2, bitrate)?;
                    muxer.add_audio_stream(&encoder.codec_parameters(), idx)?;
                } else {
                    muxer.add_audio_stream(&params, idx)?;
                }
                has_audio = true;
            }
        }

        if self.video_idx.is_some() && !has_video {
            return Err(HlsError::StreamNotFound("Video stream not found".into()));
        }
        if self.audio_idx.is_some() && !has_audio {
            return Err(HlsError::StreamNotFound("Audio stream not found".into()));
        }

        // Pass 2: Construct the MP4 bytes.
        // For codecs like AC-3 that don't have extradata, we must feed first packets to the muxer.
        // We skip this if transcoding to AAC because we already fed the AAC codec parameters explicitly.
        let mut data = if self.transcode_audio_to_aac {
            muxer.write_header(false)?
        } else {
            let mut packets = self.peek_first_packets(&mut input, &muxer, include_all)?;
            if !packets.is_empty() {
                let refs: Vec<&mut ffmpeg::Packet> = packets.iter_mut().collect();
                muxer
                    .generate_init_segment_with_packets(refs, false)
                    .map_err(|e| HlsError::Muxing(format!("Init segment packet error: {}", e)))?
            } else {
                muxer.write_header(false)?
            }
        };

        // Pass 3: Fix TREX durations
        self.apply_trex_fixes(&mut data, has_video, has_audio);

        Ok(Bytes::from(data))
    }

    /// Peek at the first packets of the targeted streams to help FFmpeg generate the `moov` box.
    fn peek_first_packets(
        &self,
        input: &mut ffmpeg::format::context::Input,
        muxer: &Fmp4Muxer,
        include_all: bool,
    ) -> Result<Vec<ffmpeg::Packet>> {
        let mut first_video = None;
        let mut first_audio = None;

        for (s, mut pkt) in input.packets() {
            let s_idx = s.index();
            let codec_id = s.parameters().id();

            let is_target_v = (include_all && crate::ffmpeg_utils::utils::is_video_codec(codec_id))
                || self.video_idx == Some(s_idx);
            let is_target_a = (include_all && crate::ffmpeg_utils::utils::is_audio_codec(codec_id))
                || self.audio_idx == Some(s_idx);

            if is_target_v && first_video.is_none() {
                if let Some(output_tb) = muxer.get_output_timebase(s_idx) {
                    pkt.rescale_ts(s.time_base(), output_tb);
                }
                pkt.set_pts(Some(0));
                pkt.set_dts(Some(0));
                first_video = Some(pkt);
            } else if is_target_a && first_audio.is_none() {
                if let Some(output_tb) = muxer.get_output_timebase(s_idx) {
                    pkt.rescale_ts(s.time_base(), output_tb);
                }
                pkt.set_pts(Some(0));
                pkt.set_dts(Some(0));
                first_audio = Some(pkt);
            }

            if (self.video_idx.is_none() || first_video.is_some())
                && (self.audio_idx.is_none() || first_audio.is_some())
            {
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
        Ok(packets)
    }

    fn apply_trex_fixes(&self, data: &mut Vec<u8>, has_video: bool, has_audio: bool) {
        let video_frame_dur = if has_video {
            self.index
                .video_streams
                .first()
                .map(|v| {
                    let fps = v.framerate;
                    if fps.numerator() > 0 {
                        (90000 * fps.denominator() as u64 / fps.numerator() as u64) as u32
                    } else {
                        3000 // 30fps fallback
                    }
                })
                .unwrap_or(3000)
        } else {
            0
        };

        if has_video && has_audio {
            crate::segment::isobmff::fix_trex_durations_per_track(data, 1, video_frame_dur, 2, 1024);
        } else {
            let default_duration = if has_video { video_frame_dur } else { 1024 };
            crate::segment::isobmff::fix_trex_durations(data, default_duration);
        }
    }
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

/// Generate an interleaved audio+video media segment (`.m4s`).
///
/// Resolves whether the audio track needs transcoding to AAC and delegates to
/// the common FFmpeg muxing path.
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

/// Generate a video-only media segment (`.m4s`) for the given sequence number.
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
        generate_media_segment_ffmpeg(
            segment,
            "audio",
            None,
            Some(track_index),
            index,
            transcode_to_aac,
        )
    } else {
        generate_media_segment_ffmpeg(segment, "audio", None, Some(track_index), index, false)
    }
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

/// A demuxed packet held in memory while the full segment is being collected.
///
/// Carries the stream metadata needed for timestamp rescaling alongside the
/// packet itself, so callers don't need to keep a reference to the source stream.
pub(crate) struct BufferedPacket {
    /// Source stream index this packet belongs to.
    pub stream_id: usize,
    /// The raw compressed packet.
    pub packet: ffmpeg::Packet,
    /// Time base of the source stream (used for timestamp rescaling).
    pub timebase: ffmpeg::Rational,
    /// `true` for video packets, `false` for audio.
    pub is_video_stream: bool,
}

/// Read and buffer all packets belonging to one segment from `input`.
///
/// Iterates the demuxer until both video (stopped at the next keyframe boundary)
/// and audio (stopped at `segment.end_pts`) are fully consumed.  Returns packets
/// in demux order, each tagged with their stream metadata for later rescaling.
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

    let mut video_done = !is_interleaved && segment_type == "audio";
    let mut audio_done = !is_interleaved && segment_type == "video";

    for (stream, packet) in input.packets() {
        let stream_id = stream.index();
        let is_video_stream = crate::ffmpeg_utils::utils::is_video_codec(stream.parameters().id());

        if is_interleaved
            && !stream_indices.contains(&stream_id)
            && audio_track_index != Some(stream_id)
        {
            continue;
        }
        if !is_interleaved && stream_id != stream_indices[0] {
            continue;
        }

        let pts_90k = crate::ffmpeg_utils::utils::rescale_ts(
            packet.pts().or(packet.dts()).unwrap_or(0),
            stream.time_base(),
            ffmpeg::Rational(1, 90000),
        );

        if is_video_stream {
            if packet.is_key() && pts_90k >= end_pts_90k {
                video_done = true;
            }
            if video_done {
                if audio_done {
                    break;
                }
                continue;
            }
        } else {
            if pts_90k >= end_pts_90k {
                if is_interleaved || packet_count > 0 {
                    audio_done = true;
                }
            }

            if audio_done {
                if video_done {
                    break;
                }
                continue;
            }
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

/// Transcode buffered audio packets to AAC if requested, otherwise no-op.
///
/// When `transcode_audio_to_aac` is true, extracts the raw audio packets from
/// `buffered_packets`, runs them through the decode → resample → encode pipeline,
/// and returns the resulting AAC packets along with their output timebase.
/// When false, returns empty vecs immediately.
fn transcode_audio_if_needed(
    input: &mut ffmpeg::format::context::Input,
    index: &StreamIndex,
    audio_track_index: Option<usize>,
    transcode_audio_to_aac: bool,
    buffered_packets: &[BufferedPacket],
    segment: &SegmentInfo,
    video_timebase: ffmpeg::Rational,
    audio_preroll: Vec<ffmpeg::Packet>,
) -> Result<(Vec<ffmpeg::Packet>, Option<ffmpeg::Rational>)> {
    let mut transcoded_audio_packets = Vec::new();
    let mut audio_output_tb = None;

    if transcode_audio_to_aac {
        if let Some(audio_idx) = audio_track_index {
            if let Some(s) = input.stream(audio_idx) {
                let decoder = crate::transcode::decoder::AudioDecoder::open(&s)?;
                let audio_info = index.get_audio_stream(audio_idx)?;
                let audio_tb = s.time_base();

                // Collect audio packets from the main buffered set
                let main_audio_packets: Vec<_> = buffered_packets
                    .iter()
                    .filter(|p| p.stream_id == audio_idx)
                    .map(|p| p.packet.clone())
                    .collect();

                // Merge pre-roll with main packets, deduplicating by DTS.
                // The pre-roll seek may return packets that also appear after the
                // main byte-aligned seek (interleaving overlap), so we skip any
                // main packet whose DTS already appears in the pre-roll.
                let preroll_dts: std::collections::HashSet<i64> = audio_preroll
                    .iter()
                    .map(|p| p.dts().or(p.pts()).unwrap_or(i64::MIN))
                    .collect();
                let mut all_audio_packets = audio_preroll;
                for pkt in main_audio_packets {
                    let dts = pkt.dts().or(pkt.pts()).unwrap_or(i64::MIN);
                    if !preroll_dts.contains(&dts) {
                        all_audio_packets.push(pkt);
                    }
                }
                all_audio_packets.sort_by_key(|p| p.dts().or(p.pts()).unwrap_or(0));

                let (aac_packets, output_tb) = crate::transcode::pipeline::transcode_audio_segment(
                    decoder,
                    all_audio_packets,
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

/// Write buffered packets into `muxer`, interleaving transcoded audio as needed.
///
/// Filters out packets that precede the segment's nominal start time, rescales
/// timestamps to the muxer's output timebase, and interleaves transcoded AAC
/// packets with the video stream in decode order.  Returns the muxer (ready for
/// `finalize_segment`) plus the DTS of the first video packet, the first audio
/// packet, and the first packet of either kind — used to set TFDT values.
fn mux_media_segment(
    _segment_type: &str,
    is_interleaved: bool,
    transcode_audio_to_aac: bool,
    video_timebase: ffmpeg::Rational,
    segment: &SegmentInfo,
    mut muxer: Fmp4Muxer,
    buffered_packets: Vec<BufferedPacket>,
    audio_track_index: Option<usize>,
    transcoded_audio_packets: Vec<ffmpeg::Packet>,
    audio_output_tb: Option<ffmpeg::Rational>,
) -> Result<(Fmp4Muxer, Option<i64>, Option<i64>, Option<i64>)> {
    let start_pts_90k = crate::ffmpeg_utils::utils::rescale_ts(
        segment.start_pts,
        video_timebase,
        ffmpeg::Rational(1, 90000),
    );

    // Use the segment's nominal start DTS (in 90kHz ticks) as the audio filter
    // threshold. segment.start_pts stores the IDR's DTS (from AVIndexEntry), which
    // is slightly before the IDR's display PTS. Using DTS as the cutoff lets the
    // audio that sits between IDR_DTS and IDR_PTS pass into this segment, which
    // preserves continuity with the previous segment's audio (which ended at ~IDR_DTS).
    // Using IDR_PTS instead would drop ~one-CTO worth of audio (~83ms for 24fps
    // B-frame content), creating an audible gap at every segment boundary.
    let audio_start_pts_90k: i64 = start_pts_90k;
    let mut first_packet_dts: Option<i64> = None;
    let mut first_video_dts: Option<i64> = None;
    let mut first_audio_dts: Option<i64> = None;
    let mut video_dts_corrected = false;

    // Helper to manage stateful interleaving of transcoded AAC packets.
    struct AacInterleaver {
        packets: Vec<ffmpeg::Packet>,
        idx: usize,
        tb: Option<ffmpeg::Rational>,
        track_idx: Option<usize>,
    }

    impl AacInterleaver {
        fn write_upto(
            &mut self,
            muxer: &mut Fmp4Muxer,
            target_dts_90k: i64,
            first_packet_dts: &mut Option<i64>,
            first_audio_dts: &mut Option<i64>,
        ) -> Result<()> {
            if self.packets.is_empty() || self.idx >= self.packets.len() {
                return Ok(());
            }
            let tb = self.tb.unwrap();
            let audio_idx = self.track_idx.unwrap();

            while self.idx < self.packets.len() {
                let pkt = &mut self.packets[self.idx];
                let pkt_dts = pkt.dts().or(pkt.pts()).unwrap_or(0);
                let pkt_dts_90k =
                    crate::ffmpeg_utils::utils::rescale_ts(pkt_dts, tb, ffmpeg::Rational(1, 90000));

                if pkt_dts_90k <= target_dts_90k {
                    if first_packet_dts.is_none() {
                        *first_packet_dts = Some(pkt_dts);
                    }
                    if first_audio_dts.is_none() {
                        *first_audio_dts = Some(pkt_dts);
                    }

                    pkt.set_stream(audio_idx);
                    // Ensure AAC frame duration is explicitly 1024 so the mp4 muxer
                    // uses it for the last sample's trun entry.
                    pkt.set_duration(1024);
                    muxer.write_packet(pkt)?;
                    self.idx += 1;
                } else {
                    break;
                }
            }
            Ok(())
        }
    }

    let mut interleaver = AacInterleaver {
        packets: transcoded_audio_packets,
        idx: 0,
        tb: audio_output_tb,
        track_idx: audio_track_index,
    };

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

        if transcode_audio_to_aac && audio_track_index == Some(stream_id) {
            continue;
        }

        let stream_threshold = if is_interleaved && !is_video_stream {
            audio_start_pts_90k
        } else {
            start_pts_90k
        };
        if pts_90k < stream_threshold {
            continue;
        }

        // Fix FFmpeg post-seek DTS=PTS bug: after certain seeks, FFmpeg's MOV
        // demuxer sets DTS=PTS for the first video packet. Correct DTS to
        // segment.start_pts (the IDR's actual DTS from the container index)
        // so that both TFDT and CTTS are computed correctly by the muxer.
        if is_video_stream && !video_dts_corrected {
            packet.set_dts(Some(segment.start_pts));
            video_dts_corrected = true;
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

        if pts_90k < stream_threshold {
            continue;
        }

        if transcode_audio_to_aac {
            interleaver.write_upto(
                &mut muxer,
                dts_90k,
                &mut first_packet_dts,
                &mut first_audio_dts,
            )?;
        }

        muxer.write_packet(&mut packet)?;
    }

    if transcode_audio_to_aac {
        interleaver.write_upto(
            &mut muxer,
            i64::MAX,
            &mut first_packet_dts,
            &mut first_audio_dts,
        )?;
    }

    Ok((muxer, first_video_dts, first_audio_dts, first_packet_dts))
}

/// Flush `muxer`, strip the init segment prefix, correct TFDT values in every
/// `moof` fragment, prepend a `styp` box, and return the final `.m4s` bytes.
///
/// For interleaved segments the video and audio TFDTs are patched independently
/// via their track IDs.  For single-track segments a single delta is applied.
/// The `first_*_dts` values returned by `mux_media_segment` are used as the
/// base for the delta so that the TFDT matches the actual first decoded frame.
fn finalize_segment(
    segment_type: &str,
    is_interleaved: bool,
    transcode_audio_to_aac: bool,
    video_timebase: ffmpeg::Rational,
    segment: &SegmentInfo,
    index: &StreamIndex,
    audio_track_index: Option<usize>,
    mut muxer: Fmp4Muxer,
    first_video_dts: Option<i64>,
    first_audio_dts: Option<i64>,
    first_packet_dts: Option<i64>,
) -> Result<Bytes> {
    let full_data = muxer.finalize()?;

    let media_offset =
        crate::segment::muxer::find_media_segment_offset(&full_data).ok_or_else(|| {
            HlsError::Muxing("No media segment data found (moof/styp missing)".to_string())
        })?;
    let mut media_data = full_data[media_offset..].to_vec();

    let (audio_tb, encoder_delay): (ffmpeg::Rational, i64) = if let Some(target) = audio_track_index {
        if let Ok(info) = index.get_audio_stream(target) {
            let delay = if transcode_audio_to_aac {
                1024 // AAC encoder delay
            } else {
                info.encoder_delay
            };
            (ffmpeg::Rational::new(1, info.sample_rate as i32), delay)
        } else {
            (ffmpeg::Rational::new(1, 48000), 0)
        }
    } else {
        (ffmpeg::Rational::new(1, 48000), 0)
    };

    let video_target_tfdt = crate::ffmpeg_utils::utils::rescale_ts(
        segment.start_pts,
        video_timebase,
        ffmpeg::Rational(1, 90000),
    )
    .max(0) as u64;

    let audio_target_tfdt = crate::ffmpeg_utils::utils::rescale_ts(
        segment.start_pts,
        video_timebase,
        audio_tb,
    )
    .max(0) as i64
        - encoder_delay;
    let audio_target_tfdt = audio_target_tfdt.max(0) as u64;

    let start_frag_seq = segment.sequence as u32 + 1;

    if is_interleaved {
        let v_track: u32 = 1;
        let a_track: u32 = 2;

        let audio_tfdt_for_patch = if let Some(dts) = first_audio_dts {
            // first_audio_dts is the DTS of the first packet we wrote (the priming packet).
            // By setting tfdt to (dts - delay), the player's decoder (which
            // also has a delay) will output the sample at dts.
            (dts as i64 - encoder_delay).max(0) as u64
        } else {
            audio_target_tfdt
        };

        let video_tfdt_for_patch = if let Some(dts) = first_video_dts {
            dts.max(0) as u64
        } else {
            video_target_tfdt
        };

        crate::segment::isobmff::patch_tfdts_per_track(
            &mut media_data,
            start_frag_seq,
            v_track,
            a_track,
            video_tfdt_for_patch,
            audio_tfdt_for_patch,
        );
    } else {
        let single_track_tfdt = if segment_type == "video" {
            if let Some(dts) = first_packet_dts {
                dts.max(0) as u64
            } else {
                video_target_tfdt
            }
        } else {
            if let Some(dts) = first_packet_dts {
                // For audio only, first_packet_dts is in audio_tb.
                // We must shift by encoder_delay to align with presentation.
                (dts as i64 - encoder_delay).max(0) as u64
            } else {
                let a_idx = audio_track_index.unwrap_or(0);
                if let Ok(audio_info) = index.get_audio_stream(a_idx) {
                    let audio_tb = ffmpeg::Rational::new(1, audio_info.sample_rate as i32);
                    crate::ffmpeg_utils::utils::rescale_ts(
                        segment.start_pts,
                        video_timebase,
                        audio_tb,
                    )
                    .max(0) as u64
                } else {
                    0
                }
            }
        };
        crate::segment::isobmff::patch_tfdts(&mut media_data, single_track_tfdt, start_frag_seq);
    }

    let styp_box: [u8; 24] = [
        0x00, 0x00, 0x00, 24, b's', b't', b'y', b'p', b'i', b's', b'o', b'8', 0x00, 0x00, 0x02,
        0x00, b'i', b's', b'o', b'8', b'c', b'm', b'f', b'c',
    ];
    media_data.splice(0..0, styp_box);

    Ok(Bytes::from(media_data))
}

/// Core FFmpeg-based segment generator shared by all media segment types.
///
/// Seeks the demuxer to the target IDR (with a 500 ms slack to work around the
/// mov demuxer's PTS-based seek comparison for B-frame sources), registers the
/// requested streams with the muxer, buffers packets until the segment boundary,
/// optionally transcodes audio to AAC, muxes everything, and delegates final
/// TFDT patching and `styp` insertion to `finalize_segment`.
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
    let seek_ts = (target_start_sec * 1_000_000.0) as i64;

    let mut input = index.get_context()?;
    // avformat_seek_file (mov demuxer) compares the target `ts` against PTS, not
    // DTS. For B-frame video the target IDR has PTS > DTS by one CTO (~83ms for
    // typical 24-30fps content with a 2-frame reorder window). seek_ts is derived
    // from segment.start_pts which stores the IDR's DTS (AVIndexEntry.timestamp).
    // With ts = seek_ts = DTS and backward seek semantics, the mov demuxer finds
    // the last keyframe whose PTS < ts, which excludes the target IDR (PTS > DTS)
    // and lands on the PREVIOUS keyframe instead. Adding 500ms to ts ensures
    // PTS(target IDR) <= ts while still being well below the next segment's IDR.
    let seek_ts_with_slack = seek_ts + 500_000; // +500ms to clear B-frame CTO

    // For transcoded audio in interleaved segments, collect a pre-roll window
    // of audio packets before the main seek position.  In MP4 files the
    // demuxer's timestamp-based seek lands at the video IDR's byte offset;
    // audio packets interleaved just before that offset are skipped, creating
    // an 85 ms+ gap at certain segment boundaries.  By collecting those packets
    // in a separate backward seek (before the main seek repositions the
    // demuxer) and prepending them to the transcoder's input, the
    // target_grid_start_48k filter still discards out-of-range output — so
    // this pre-roll has no effect on segment boundaries, only on coverage.
    let audio_preroll_packets: Vec<ffmpeg::Packet> = if transcode_audio_to_aac {
        if let Some(audio_idx) = audio_track_index {
            let preroll_seek_us = (seek_ts - 1_000_000).max(0);
            let mut preroll = Vec::new();
            let _ = input.seek(preroll_seek_us, ..seek_ts_with_slack);
            for (stream, packet) in input.packets() {
                if stream.index() != audio_idx {
                    continue;
                }
                let pkt_pts = packet.pts().or(packet.dts()).unwrap_or(0);
                let pkt_us = crate::ffmpeg_utils::utils::rescale_ts(
                    pkt_pts,
                    stream.time_base(),
                    ffmpeg::Rational(1, 1_000_000),
                );
                // Stop once we enter the window that buffer_media_packets will cover
                if pkt_us >= seek_ts_with_slack {
                    break;
                }
                preroll.push(packet);
            }
            preroll
        } else {
            vec![]
        }
    } else {
        vec![]
    };

    input
        .seek(seek_ts_with_slack, ..(seek_ts + 2_000_000))
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
                    if transcode_audio_to_aac {
                        let audio_info = index.get_audio_stream(idx)?;
                        let bitrate = get_recommended_bitrate(audio_info.channels);
                        let encoder = AacEncoder::open(HLS_SAMPLE_RATE, 2, bitrate)?;
                        muxer.add_audio_stream(&encoder.codec_parameters(), idx)?;
                    } else {
                        muxer.add_audio_stream(&params, idx)?;
                    }
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

    // delay_moov is required when:
    //   1. Pure audio segments: no video keyframes to drive fragmentation.
    //   2. Non-transcoded interleaved segments with a non-AAC audio codec
    //      (AC-3, E-AC-3, MP3, Opus, FLAC, TrueHD, …): the mov muxer can't
    //      write the moov without seeing actual packets because those codecs
    //      either need bitstream-derived extradata or have variable frame sizes.
    //
    // We now also enable it always for transcoded audio to ensure the muxer
    // correctly handles the non-zero (often negative) start timestamps and
    // writes the necessary elst (edit list) for AAC priming.
    //
    // Since we enabled CTTS v1 (negative_cts_offsets) in muxer.rs, delay_moov
    // no longer causes the CTTS/tfdt corruption for B-frame video.
    let needs_delay_moov = segment_type == "audio" || segment_type == "av";
    muxer.write_header(needs_delay_moov)?;

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
        audio_preroll_packets,
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
        _v_dts,
        _a_dts,
        _p_dts,
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
            stream_index: 1,
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

    #[test]
    fn test_generate_audio_segment_transcode() {
        let _ = ffmpeg::init();
        let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
        let source_path = std::path::PathBuf::from(manifest_dir)
            .join("tests")
            .join("assets")
            .join("video.mp4");

        if !source_path.exists() {
            return;
        }

        let mut index = StreamIndex::new(source_path.clone());
        index.audio_streams.push(crate::media::AudioStreamInfo {
            stream_index: 1,
            codec_id: ffmpeg::codec::Id::AC3, // Mock as AC3 to trigger transcode logic
            sample_rate: 48000,
            channels: 2,
            bitrate: 128000,
            language: Some("en".to_string()),
            transcode_to: Some(ffmpeg::codec::Id::AAC),
            encoder_delay: 0,
        });

        let segment = crate::media::SegmentInfo {
            sequence: 0,
            start_pts: 0,
            end_pts: 360000,
            duration_secs: 4.0,
            is_keyframe: true,
            video_byte_offset: 0,
        };
        index.segments.push(segment);

        let result = generate_audio_segment(&index, 1, 0, &source_path, Some("aac"));

        match result {
            Ok(bytes) => {
                assert!(!bytes.is_empty());
                assert!(bytes.windows(4).any(|w| w == b"styp"));
                assert!(bytes.windows(4).any(|w| w == b"moof"));
                assert!(bytes.windows(4).any(|w| w == b"mdat"));
            }
            Err(e) => panic!("Failed to transcode audio segment: {:?}", e),
        }
    }
}
