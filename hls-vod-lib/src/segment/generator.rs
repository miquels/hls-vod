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
use crate::transcode::pipeline::transcode_audio_segment;
use crate::transcode::resampler::HLS_SAMPLE_RATE;

/// Generate an initialization segment (init.mp4)
#[allow(dead_code)] // we'll need this when we support multiplexed tracks
pub(crate) fn generate_init_segment(index: &StreamIndex) -> Result<Bytes> {
    let input = index.get_context()?;

    let mut muxer = Fmp4Muxer::new()?;

    // Copy all video and audio streams
    // Note: We need to iterate by index to map correctly
    for stream in input.streams() {
        let params = stream.parameters();
        let codec_id = params.id();
        let index = stream.index();

        if crate::ffmpeg_utils::utils::is_video_codec(codec_id) {
            muxer.add_video_stream(&params, index)?;
        } else if crate::ffmpeg_utils::utils::is_audio_codec(codec_id) {
            muxer.add_audio_stream(&params, index)?;
        }
    }

    let mut data = muxer.write_header(false)?;

    // Fix default_sample_duration in trex boxes
    // For mixed content, 1024 is a reasonable safe default, though not perfect for video
    // But this function is primarily used for specific stream types now
    fix_trex_durations(&mut data, 1024);

    Ok(Bytes::from(data))
}

/// Generate a video-only initialization segment
pub(crate) fn generate_video_init_segment(index: &StreamIndex) -> Result<Bytes> {
    if index.video_streams.is_empty() {
        return Err(HlsError::NoVideoStream);
    }

    let input = index.get_context()?;

    let mut muxer = Fmp4Muxer::new()?;

    for stream in input.streams() {
        let params = stream.parameters();
        let codec_id = params.id();
        let index = stream.index();

        if crate::ffmpeg_utils::utils::is_video_codec(codec_id) {
            muxer.add_video_stream(&params, index)?;
        }
    }

    let mut data = muxer.write_header(false)?;

    // Calculate default duration (ticks) for video
    // Assume 90kHz timescale for video
    let mut default_duration = 3000; // Default fallback (30fps)

    if let Some(video_info) = index.video_streams.first() {
        let fps = video_info.framerate;
        if fps.numerator() > 0 {
            // Duration = 90000 / fps
            // fps = num / den
            // Duration = 90000 * den / num
            default_duration = (90000 * fps.denominator() as u64 / fps.numerator() as u64) as u32;
        }
    }

    fix_trex_durations(&mut data, default_duration);

    Ok(Bytes::from(data))
}

/// Generate an audio-only initialization segment for a specific track
///
/// For tracks that need transcoding (non-AAC), the init segment is produced
/// from the AAC encoder's codec parameters, not the source stream.
pub(crate) fn generate_audio_init_segment(
    index: &StreamIndex,
    track_index: usize,
    requested_transcode: Option<&str>,
) -> Result<Bytes> {
    // Check if this audio track needs transcoding
    // TODO: add support for more codecs.
    let audio_info = index.get_audio_stream(track_index)?;
    let transcode_to_aac = requested_transcode == Some("aac")
        || audio_info.transcode_to == Some(ffmpeg::codec::Id::AAC);

    if transcode_to_aac {
        // Build an init segment with AAC codec parameters
        let bitrate = get_recommended_bitrate(audio_info.channels);
        let encoder = AacEncoder::open(HLS_SAMPLE_RATE, 2, bitrate)?;

        let mut muxer = Fmp4Muxer::new()?;
        muxer.add_audio_stream(&encoder.codec_parameters(), track_index)?;

        let mut data = muxer.write_header(false)?;
        fix_trex_durations(&mut data, 1024);
        return Ok(Bytes::from(data));
    }

    // Source is already Native (AAC, AC3, Opus) — copy parameters directly
    let mut input = index.get_context()?;

    let mut muxer = Fmp4Muxer::new()?;

    let stream = input
        .stream(track_index)
        .ok_or(HlsError::StreamNotFound(format!("Stream {}", track_index)))?;

    if !crate::ffmpeg_utils::utils::is_audio_codec(stream.parameters().id()) {
        return Err(HlsError::Muxing(format!(
            "Stream {} is not an audio stream",
            track_index
        )));
    }

    muxer.add_audio_stream(&stream.parameters(), track_index)?;

    // Find the first packet for this stream to provide bitstream info
    // essential for writing the moov atom (especially for AC-3)
    let mut first_packet = None;
    for (s, mut pkt) in input.packets() {
        if s.index() == track_index {
            let input_tb = s.time_base();
            if let Some(output_tb) = muxer.get_output_timebase(track_index) {
                pkt.rescale_ts(input_tb, output_tb);
            }
            // Force PTS/DTS to 0 so FFmpeg doesn't generate an elst with a media_time delay
            pkt.set_pts(Some(0));
            pkt.set_dts(Some(0));
            first_packet = Some(pkt);
            break;
        }
    }

    let mut data = if let Some(mut pkt) = first_packet {
        muxer
            .generate_init_segment_with_packet(&mut pkt)
            .map_err(|e| {
                HlsError::Muxing(format!("Failed to generate init segment via packet: {}", e))
            })?
    } else {
        // Fallback for empty streams
        muxer.write_header(false)?
    };

    fix_trex_durations(&mut data, 1024);

    Ok(Bytes::from(data))
}

/// Generate an interleaved audio-video initialization segment
pub(crate) fn generate_interleaved_init_segment(
    index: &StreamIndex,
    video_idx: usize,
    audio_idx: usize,
    requested_audio_transcode: Option<&str>,
) -> Result<Bytes> {
    if index.video_streams.is_empty() || index.audio_streams.is_empty() {
        return Err(HlsError::StreamNotFound(
            "Interleaved segment requires both video and audio streams".to_string(),
        ));
    }

    // Only transcode if explicitly requested.
    let transcode_to_aac = requested_audio_transcode == Some("aac")
        || index
            .get_audio_stream(audio_idx)
            .map(|t| t.transcode_to == Some(ffmpeg::codec::Id::AAC))
            .unwrap_or(false);

    let mut input = index.get_context()?;
    let mut muxer = Fmp4Muxer::new()?;

    let mut has_video = false;
    let mut has_audio = false;

    // Collect parameters first to avoid mutably borrowing input while iterating streams
    let mut video_params = None;
    let mut audio_params = None;

    for stream in input.streams() {
        let params = stream.parameters();
        let codec_id = params.id();
        let idx = stream.index();

        if crate::ffmpeg_utils::utils::is_video_codec(codec_id) {
            if idx == video_idx {
                video_params = Some(params.clone());
            }
        } else if crate::ffmpeg_utils::utils::is_audio_codec(codec_id) && idx == audio_idx {
            audio_params = Some(params.clone());
        }
    }

    if let Some(vp) = &video_params {
        muxer.add_video_stream(vp, video_idx)?;
        has_video = true;
    }

    if let Some(ap) = &audio_params {
        if transcode_to_aac {
            // Use AAC encoder parameters instead of source
            let audio_info = index.get_audio_stream(audio_idx);
            let bitrate = get_recommended_bitrate(audio_info.map(|a| a.channels).unwrap_or(2));
            let encoder = AacEncoder::open(HLS_SAMPLE_RATE, 2, bitrate)?;
            muxer.add_audio_stream(&encoder.codec_parameters(), audio_idx)?;
        } else {
            muxer.add_audio_stream(ap, audio_idx)?;
        }
        has_audio = true;
    }

    // Check if we found the requested streams
    if !has_video || !has_audio {
        return Err(HlsError::StreamNotFound(
            "Specified video or audio stream not found".to_string(),
        ));
    }

    let mut data = if transcode_to_aac {
        muxer.write_header(false)?
    } else {
        // Fetch the first packet for video and audio to feed them to the muxer
        let mut first_video = None;
        let mut first_audio = None;

        for (s, mut pkt) in input.packets() {
            if s.index() == video_idx && first_video.is_none() {
                if let Some(output_tb) = muxer.get_output_timebase(video_idx) {
                    let input_tb = s.time_base();
                    pkt.rescale_ts(input_tb, output_tb);
                }
                pkt.set_pts(Some(0));
                pkt.set_dts(Some(0));
                first_video = Some(pkt);
            } else if s.index() == audio_idx && first_audio.is_none() {
                if let Some(output_tb) = muxer.get_output_timebase(audio_idx) {
                    let input_tb = s.time_base();
                    pkt.rescale_ts(input_tb, output_tb);
                }
                pkt.set_pts(Some(0));
                pkt.set_dts(Some(0));
                first_audio = Some(pkt);
            }
            if first_video.is_some() && first_audio.is_some() {
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
                        "Failed to generate interleaved init segment with packets: {}",
                        e
                    ))
                })?
        } else {
            muxer.write_header(false)?
        }
    };

    // Calculate default duration for video trex
    let mut default_duration = 3000; // Default fallback (30fps)
    if let Some(video_info) = index.video_streams.first() {
        let fps = video_info.framerate;
        if fps.numerator() > 0 {
            default_duration = (90000 * fps.denominator() as u64 / fps.numerator() as u64) as u32;
        }
    }

    fix_trex_durations(&mut data, default_duration);

    Ok(Bytes::from(data))
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

/// Fix default_sample_duration in trex boxes
/// FFmpeg with stream copy sets duration to 1, but players need reasonable values
fn fix_trex_durations(data: &mut Vec<u8>, duration: u32) {
    crate::segment::isobmff::walk_boxes_mut(data, &[b"moov", b"mvex"], &mut |btype, payload| {
        if btype == b"trex" && payload.len() >= 16 {
            // Set default_sample_duration (offset 16 from start of payload, which is pos + 24 from start of box)
            // Wait, in the original code, it was offset 20 from pos. payload starts at pos + 8. So it's offset 12 in payload.
            payload[12..16].copy_from_slice(&duration.to_be_bytes());
        }
    });
}

/// Patch tfdt.baseMediaDecodeTime and mfhd.FragmentSequenceNumber in media segment data.
///
/// Sets all tfdt boxes so the first one matches `target_time` and subsequent
/// ones are adjusted by the same delta (preserving relative offsets for
/// multi-fragment segments). Also patches mfhd sequence numbers starting from
/// `start_frag_seq`.
fn patch_tfdts(media_data: &mut Vec<u8>, target_time: u64, start_frag_seq: u32) {
    let mut tfdt_delta: Option<i64> = None;
    let mut frag_count = 0;

    crate::segment::isobmff::walk_boxes_mut(
        media_data,
        &[b"moof", b"traf"],
        &mut |btype, payload| {
            if btype == b"moof" {
                // moof is a container, nothing to mutate directly here.
            } else if btype == b"mfhd" {
                let current_frag_seq = start_frag_seq.wrapping_add(frag_count);
                frag_count += 1;
                if payload.len() >= 8 {
                    payload[4..8].copy_from_slice(&current_frag_seq.to_be_bytes());
                }
            } else if btype == b"tfdt" {
                if payload.is_empty() {
                    return;
                }
                let version = payload[0];
                let (current_tfdt, value_offset) = if version == 1 && payload.len() >= 12 {
                    (u64::from_be_bytes(payload[4..12].try_into().unwrap()), 4)
                } else if payload.len() >= 8 {
                    (
                        u32::from_be_bytes(payload[4..8].try_into().unwrap()) as u64,
                        4,
                    )
                } else {
                    (0, 0)
                };

                if value_offset > 0 {
                    if tfdt_delta.is_none() {
                        tfdt_delta = Some(target_time as i64 - current_tfdt as i64);
                    }
                    let new_tfdt = (current_tfdt as i64 + tfdt_delta.unwrap()) as u64;
                    if version == 1 {
                        payload[4..12].copy_from_slice(&new_tfdt.to_be_bytes());
                    } else {
                        payload[4..8].copy_from_slice(&(new_tfdt as u32).to_be_bytes());
                    }
                }
            }
        },
    );
}

/// Patch `mfhd` sequence numbers AND each track's `tfdt` independently.
///
/// For interleaved (multi-track) segments, `delay_moov=true` causes FFmpeg to
/// shift all timestamps to start near 0.  We must restore the correct target
/// decode-time for each track separately using its `trak_id` from `tfhd`.
///
/// `video_track_id` / `audio_track_id` are the 1-based mp4 track IDs emitted
/// by the muxer (not the source stream indices).
fn patch_tfdts_per_track(
    media_data: &mut Vec<u8>,
    start_frag_seq: u32,
    video_track_id: u32,
    audio_track_id: u32,
    video_target_tfdt: u64,
    audio_target_tfdt: u64,
) {
    let mut frag_count = 0u32;
    // Track the last track_id we read from tfhd
    let mut current_track_id: u32 = 0;

    let mut video_tfdt_delta: Option<i64> = None;
    let mut audio_tfdt_delta: Option<i64> = None;

    crate::segment::isobmff::walk_boxes_mut(
        media_data,
        &[b"moof", b"traf"],
        &mut |btype, payload| {
            if btype == b"mfhd" && payload.len() >= 8 {
                let seq = start_frag_seq.wrapping_add(frag_count);
                frag_count += 1;
                payload[4..8].copy_from_slice(&seq.to_be_bytes());
            } else if btype == b"tfhd" && payload.len() >= 8 {
                // tfhd layout: version(1) + flags(3) + track_id(4)
                current_track_id = u32::from_be_bytes(payload[4..8].try_into().unwrap_or([0; 4]));
            } else if btype == b"tfdt" && !payload.is_empty() {
                let version = payload[0];
                let (current_tfdt, value_offset) = if version == 1 && payload.len() >= 12 {
                    (u64::from_be_bytes(payload[4..12].try_into().unwrap()), 4)
                } else if payload.len() >= 8 {
                    (
                        u32::from_be_bytes(payload[4..8].try_into().unwrap()) as u64,
                        4,
                    )
                } else {
                    (0, 0)
                };

                if value_offset > 0 {
                    if current_track_id == video_track_id {
                        if video_tfdt_delta.is_none() {
                            video_tfdt_delta = Some(video_target_tfdt as i64 - current_tfdt as i64);
                        }
                        let new_tfdt = (current_tfdt as i64 + video_tfdt_delta.unwrap()) as u64;
                        if version == 1 {
                            payload[4..12].copy_from_slice(&new_tfdt.to_be_bytes());
                        } else {
                            payload[4..8].copy_from_slice(&(new_tfdt as u32).to_be_bytes());
                        }
                    } else if current_track_id == audio_track_id {
                        if audio_tfdt_delta.is_none() {
                            audio_tfdt_delta = Some(audio_target_tfdt as i64 - current_tfdt as i64);
                        }
                        let new_tfdt = (current_tfdt as i64 + audio_tfdt_delta.unwrap()) as u64;
                        if version == 1 {
                            payload[4..12].copy_from_slice(&new_tfdt.to_be_bytes());
                        } else {
                            payload[4..8].copy_from_slice(&(new_tfdt as u32).to_be_bytes());
                        }
                    }
                }
            }
        },
    );
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
    // Use the video timebase stored at index time — no need to re-open the file.
    let video_timebase = index.video_timebase;

    // Run the full transcode pipeline
    let (aac_packets, output_timebase) = transcode_audio_segment(
        &index.source_path,
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

    patch_tfdts(&mut media_data, target_time, start_frag_seq);

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

/// Parse the minimum display PTS across all samples in a muxed fMP4 segment.
/// Scans every moof/traf/tfdt/trun box and returns the smallest (tfdt + sample_CT) value.
/// This is the earliest frame display time, used to align audio tfdt.
pub fn read_first_display_pts(data: &[u8]) -> Option<i64> {
    let mut first_display_pts: Option<i64> = None;
    let mut current_tfdt: Option<i64> = None;

    crate::segment::isobmff::walk_boxes(
        data,
        &[b"moof", b"traf"],
        &mut |btype, payload| match btype {
            b"tfdt" => {
                if payload.is_empty() {
                    return;
                }
                let version = payload[0];
                current_tfdt = Some(if version == 1 && payload.len() >= 12 {
                    i64::from_be_bytes(payload[4..12].try_into().unwrap())
                } else if payload.len() >= 8 {
                    u32::from_be_bytes(payload[4..8].try_into().unwrap()) as i64
                } else {
                    0
                });
            }
            b"trun" => {
                if let Some(tfdt) = current_tfdt {
                    if payload.len() >= 12 {
                        let trun_flags =
                            u32::from_be_bytes([0, payload[1], payload[2], payload[3]]);
                        let sample_count = u32::from_be_bytes(payload[4..8].try_into().unwrap());

                        let has_duration = trun_flags & 0x0100 != 0;
                        let has_ct_offset = trun_flags & 0x0800 != 0;
                        let mut entry_offset = 8;
                        if trun_flags & 0x0001 != 0 {
                            entry_offset += 4;
                        }
                        if trun_flags & 0x0004 != 0 {
                            entry_offset += 4;
                        }

                        let mut per_sample_size = 0usize;
                        let mut ct_field_offset = 0usize;

                        if has_duration {
                            per_sample_size += 4;
                        }
                        if trun_flags & 0x0200 != 0 {
                            per_sample_size += 4;
                        }
                        if trun_flags & 0x0400 != 0 {
                            per_sample_size += 4;
                        }
                        if has_ct_offset {
                            ct_field_offset = per_sample_size;
                            per_sample_size += 4;
                        }

                        let mut running_dts = tfdt;
                        let mut off = entry_offset;
                        for _ in 0..sample_count {
                            if off + per_sample_size > payload.len() {
                                break;
                            }

                            let mut sample_pts = running_dts;
                            if has_ct_offset {
                                let ct_bytes =
                                    &payload[off + ct_field_offset..off + ct_field_offset + 4];
                                let ct = i32::from_be_bytes(ct_bytes.try_into().unwrap());
                                sample_pts += ct as i64;
                            }

                            first_display_pts = Some(match first_display_pts {
                                Some(cur) => cur.min(sample_pts),
                                None => sample_pts,
                            });

                            if has_duration {
                                let dur_bytes = &payload[off..off + 4];
                                let dur = u32::from_be_bytes(dur_bytes.try_into().unwrap());
                                running_dts += dur as i64;
                            }

                            off += per_sample_size;
                        }
                    }
                }
            }
            _ => {}
        },
    );

    first_display_pts
}

fn generate_media_segment_ffmpeg(
    segment: &SegmentInfo,
    segment_type: &str,
    video_track_index: Option<usize>,
    audio_track_index: Option<usize>,
    index: &StreamIndex,
    transcode_audio_to_aac: bool,
) -> Result<Bytes> {
    // For interleaved segments, we need to mux both audio and video
    let is_interleaved = segment_type == "av";

    let mut input = index.get_context()?;

    let mut muxer = Fmp4Muxer::new()?;
    // We create a new muxer for each segment, which writes an init segment (header).
    // We will strip the init segment and returns only the fragments.
    // However, Fmp4Muxer writes header upon write_header call.

    // Find relevant stream(s)
    let mut stream_indices = Vec::new();
    for stream in input.streams() {
        let params = stream.parameters();
        let codec_id = params.id();
        let idx = stream.index();

        if is_interleaved {
            // Add both video and audio streams
            if let Some(video_idx) = video_track_index {
                if idx == video_idx && crate::ffmpeg_utils::utils::is_video_codec(codec_id) {
                    muxer.add_video_stream(&params, idx)?;
                    stream_indices.push(idx);
                }
            }
            // Patch it
            let _start_pts_rescaled = segment.start_pts;
            if let Some(audio_idx) = audio_track_index {
                if idx == audio_idx && crate::ffmpeg_utils::utils::is_audio_codec(codec_id) {
                    let audio_info = index.get_audio_stream(audio_idx)?;
                    // TODO: support more codecs than AAC
                    if transcode_audio_to_aac {
                        // Use AAC encoder parameters for transcoded audio
                        let bitrate = get_recommended_bitrate(audio_info.channels);
                        // FIXME: should '2' here be 'audio_info.channels' ?
                        let encoder = AacEncoder::open(HLS_SAMPLE_RATE, 2, bitrate)?;
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

    // Write header WITHOUT delay_moov for video segments.
    // delay_moov causes FFmpeg to emit a pre-roll moof[0] containing B-frames with
    // large composition-time offsets. Those frames display within the segment but
    // their presence makes the first *displayable* PTS of the segment appear hundreds
    // of milliseconds after the last displayable PTS of the previous segment —
    // producing a visible freeze/jump at every segment boundary.
    // Without delay_moov, the first moof starts at the keyframe DTS directly,
    // giving smooth PTS continuity across segment boundaries.
    // For interleaved (av) mode, delay_moov=true is needed for the muxer to correctly
    // interleave audio and video packets; the seek issues are handled separately.
    // For audio-only mode, delay_moov=true is needed because codecs like AC-3
    // lack extradata in the source and FFmpeg requires delay_moov to write the moov atom.
    let _init_bytes = muxer.write_header(segment_type == "av" || segment_type == "audio")?;

    // Encoder delay: the number of samples (in output timebase) that the codec
    // prepends as pre-roll before the first presented sample.  FFmpeg signals this
    // by giving the first packet a *negative* DTS (e.g. -1024 @ 48 kHz for AAC).
    // The init segment's edit list (edts/elst) tells the player to subtract this
    // value from every tfdt to get the presentation time:
    //   presentation = (tfdt - encoder_delay) / timescale
    // so we must set: tfdt = video_presentation * timescale + encoder_delay
    //
    // We read this from StreamIndex where it was captured at scan time by reading
    // the first packet of the stream — the universal FFmpeg approach that works
    // for any container (MP4, MKV, etc.) and any codec (AAC, Opus, Vorbis, etc.).
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
    tracing::debug!(
        "[sync] {} seg={} encoder_delay={}",
        segment_type,
        segment.sequence,
        encoder_delay
    );

    // Use the video timebase stored at index time — no need to re-read it from the file.
    let video_timebase = index.video_timebase;

    // Seek to segment start using timestamp-based seek (AV_TIME_BASE / microseconds).
    // AVSEEK_FLAG_BYTE is not used here: avformat_find_stream_info already consumed
    // ~13MB of the file, so backward byte-seeks are silently ignored by the MP4
    // demuxer, resulting in 0 packets read.  Timestamp seek works correctly.
    let seek_ts = if video_timebase.denominator() != 0 {
        segment.start_pts * 1_000_000 * video_timebase.numerator() as i64
            / video_timebase.denominator() as i64
    } else {
        segment.start_pts * 1_000_000 / 90_000
    };
    tracing::debug!(
        "[sync] {} seg={} start_pts={} video_tb={}/{} seek_ts_us={}",
        segment_type,
        segment.sequence,
        segment.start_pts,
        video_timebase.numerator(),
        video_timebase.denominator(),
        seek_ts
    );

    input
        .seek(seek_ts, ..seek_ts)
        .map_err(|e| HlsError::Ffmpeg(crate::error::FfmpegError::ReadFrame(e.to_string())))?;

    // ── Pre-Transcode Audio if necessary ──
    let mut transcoded_audio_packets = Vec::new();
    let mut audio_output_tb = None;
    let mut audio_packet_idx = 0;

    if is_interleaved && transcode_audio_to_aac {
        if let Some(audio_idx) = audio_track_index {
            let audio_info = index.get_audio_stream(audio_idx)?;
            let (aac_packets, output_tb) = crate::transcode::pipeline::transcode_audio_segment(
                &index.source_path,
                audio_info,
                segment,
                video_timebase,
                false, // DO NOT shift to zero, we need absolute timeline for Fmp4Muxer
            )?;
            transcoded_audio_packets = aac_packets;
            audio_output_tb = Some(output_tb);
            tracing::debug!(
                "Pre-transcoded {} AAC packets for interleaved segment {}",
                transcoded_audio_packets.len(),
                segment.sequence
            );
        }
    }

    let mut _packet_count = 0;
    // Track the rescaled DTS of the first packet we write...
    let mut first_packet_dts: Option<i64> = None;
    let mut first_video_dts: Option<i64> = None;
    let mut first_audio_dts: Option<i64> = None;

    // Helper closure to inject audio packets in interleaved mode.
    // It captures `audio_packet_idx` and writes any transcoded packets that have DTS <= the target_dts.
    let mut write_transcoded_audio_upto = |target_dts_90k: i64,
                                           muxer: &mut Fmp4Muxer,
                                           first_dts_ref: &mut Option<i64>,
                                           overall_first_dts_ref: &mut Option<i64>|
     -> Result<()> {
        if !transcoded_audio_packets.is_empty() && audio_packet_idx < transcoded_audio_packets.len()
        {
            let tb = audio_output_tb.unwrap();
            let audio_idx = audio_track_index.unwrap();

            while audio_packet_idx < transcoded_audio_packets.len() {
                let pkt = &mut transcoded_audio_packets[audio_packet_idx];
                let pkt_dts = pkt.dts().or(pkt.pts()).unwrap_or(0);
                let pkt_dts_90k =
                    crate::ffmpeg_utils::utils::rescale_ts(pkt_dts, tb, ffmpeg::Rational(1, 90000));

                if pkt_dts_90k <= target_dts_90k {
                    // Update bounds
                    if overall_first_dts_ref.is_none() {
                        *overall_first_dts_ref = Some(pkt_dts);
                    }
                    if first_dts_ref.is_none() {
                        *first_dts_ref = Some(pkt_dts);
                    }

                    pkt.set_stream(audio_idx);
                    muxer.write_packet(pkt)?;
                    audio_packet_idx += 1;
                } else {
                    break;
                }
            }
        }
        Ok(())
    };

    for (stream, mut packet) in input.packets() {
        let stream_id = stream.index();

        // For interleaved mode, accept both video and audio streams.
        // If we are transcoding audio, we ONLY want to process video packets from the demuxer here,
        // as the audio packets are manually pulled from `transcoded_audio_packets`.
        if is_interleaved {
            if !stream_indices.contains(&stream_id) {
                continue;
            }
            if transcode_audio_to_aac && audio_track_index == Some(stream_id) {
                continue;
            }
        } else {
            let stream_idx = stream_indices[0];
            if stream_id != stream_idx {
                continue;
            }
        }

        let pts = packet.pts().or(packet.dts()).unwrap_or(0);
        let dts = packet.dts().or(packet.pts()).unwrap_or(0);
        let timebase = stream.time_base();

        // Convert current packet timestamps to 90kHz for comparison
        let pts_90k =
            crate::ffmpeg_utils::utils::rescale_ts(pts, timebase, ffmpeg::Rational(1, 90000));
        let dts_90k =
            crate::ffmpeg_utils::utils::rescale_ts(dts, timebase, ffmpeg::Rational(1, 90000));
        // Convert segment boundaries (which are in video_timebase) to 90kHz
        let start_pts_90k = crate::ffmpeg_utils::utils::rescale_ts(
            segment.start_pts,
            video_timebase,
            ffmpeg::Rational(1, 90000),
        );
        let end_pts_90k = crate::ffmpeg_utils::utils::rescale_ts(
            segment.end_pts,
            video_timebase,
            ffmpeg::Rational(1, 90000),
        );

        // For interleaved mode, use video stream for segment boundary detection
        let is_video_stream = crate::ffmpeg_utils::utils::is_video_codec(stream.parameters().id());

        // Video-specific segment boundary logic.
        //
        // Segment boundaries are defined by keyframe DTS values. For video:
        //
        // Stop condition: stop when we see the next segment's keyframe (is_key AND dts >= end_pts).
        // Non-keyframe B-frames with dts >= end_pts still display within this segment
        // (their PTS < end_pts + ct_offset) and must be written.
        //
        // Start filter: exclude pre-roll B-frames with DTS < start_pts. These frames
        // decode before this segment's keyframe and are already in the previous segment.
        // Using DTS (not PTS) is correct: pre-roll B-frames always have DTS < keyframe DTS.
        //
        // Audio uses PTS-based filtering (below) since it has no B-frames.
        // For interleaved mode, video controls boundaries, audio follows.
        if segment_type == "video" || (is_interleaved && is_video_stream) {
            let is_keyframe = packet.is_key();
            if is_keyframe && dts_90k >= end_pts_90k {
                tracing::debug!(
                    "Reached segment end at keyframe (dts_90k={}, end_pts_90k={}), stopping",
                    dts_90k,
                    end_pts_90k
                );
                break;
            }
            if dts_90k < start_pts_90k {
                continue;
            }
        } else {
            // Audio: simple PTS-based range filter
            // For interleaved mode, audio packets within video boundaries are included
            if !is_interleaved {
                if pts_90k >= end_pts_90k {
                    if _packet_count > 0 {
                        break;
                    } else {
                        continue;
                    }
                }
                if pts_90k < start_pts_90k {
                    continue;
                }
            } else {
                // In interleaved mode, audio follows video boundaries
                if pts_90k >= end_pts_90k {
                    // Allow some slack for audio to ensure we have audio coverage
                    // Audio packets slightly beyond video end may be needed
                    break;
                }
                if pts_90k < start_pts_90k {
                    continue;
                }
            }
        }

        // Rescale packet timestamps to output stream timebase
        if let Some(out_tb) = muxer.get_output_timebase(stream.index()) {
            let in_tb = stream.time_base();
            if let Some(pts) = packet.pts() {
                let out_pts = pts.rescale(in_tb, out_tb);
                packet.set_pts(Some(out_pts));
                if let Some(dts) = packet.dts() {
                    let out_dts = dts.rescale(in_tb, out_tb);
                    packet.set_dts(Some(out_dts));
                    // Capture first-packet DTS for patching later.
                    if first_packet_dts.is_none() {
                        tracing::debug!(
                            "[sync] {} seg={} first_pkt: in_pts={} out_pts={} in_dts={:?} out_dts={} in_tb={}/{} out_tb={}/{}",
                            segment_type, segment.sequence,
                            pts, out_pts,
                            packet.dts(), out_dts,
                            stream.time_base().numerator(), stream.time_base().denominator(),
                            out_tb.numerator(), out_tb.denominator()
                        );
                        first_packet_dts = Some(out_dts);
                    }
                    // For interleaved, also capture first video/audio DTS separately.
                    if is_interleaved {
                        if crate::ffmpeg_utils::utils::is_video_codec(stream.parameters().id()) {
                            if first_video_dts.is_none() {
                                first_video_dts = Some(out_dts);
                            }
                        } else if crate::ffmpeg_utils::utils::is_audio_codec(
                            stream.parameters().id(),
                        ) {
                            if first_audio_dts.is_none() {
                                first_audio_dts = Some(out_dts);
                            }
                        }
                    }
                } else if first_packet_dts.is_none() {
                    // Fallback: use rescaled PTS if DTS is absent
                    first_packet_dts = Some(out_pts);
                }

                // Always set the packet duration explicitly (rescaled to output tb).
                // FFmpeg normally computes trun sample duration as next_dts - current_dts.
                // When the packet loop stops before writing the next packet (at segment
                // boundary), the last sample gets a wrong duration — typically a tiny
                // residual value (e.g. 672 ticks instead of 3780). This causes a DTS
                // discontinuity at the segment boundary and a visible glitch.
                // Using the demuxer's own duration value (which is always correct) for
                // every packet prevents this.
                let in_dur = packet.duration();
                if in_dur > 0 {
                    let out_dur = in_dur.rescale(in_tb, out_tb);
                    packet.set_duration(out_dur);
                }

                // Keep the trace log for detailed debugging if needed
                tracing::debug!(
                    "Pkt: InTB={:?}, OutTB={:?}, InPts={:?}, OutPts={:?}, InDts={:?}, OutDts={:?}, Dur={:?}, SegStart={:?}",
                    in_tb,
                    out_tb,
                    pts,
                    out_pts,
                    packet.dts(),
                    packet.dts().map(|d| d.rescale(in_tb, out_tb)),
                    packet.duration(),
                    segment.start_pts
                );
            }
            _packet_count += 1;
        }

        // --- Interleave transcoded audio packets ---
        if transcode_audio_to_aac {
            write_transcoded_audio_upto(
                dts_90k,
                &mut muxer,
                &mut first_audio_dts,
                &mut first_packet_dts,
            )?;
        }

        muxer.write_packet(&mut packet)?;
    }

    // Flush any remaining transcoded audio packets that weren't captured by the video packets DTS loop.
    if transcode_audio_to_aac {
        write_transcoded_audio_upto(
            i64::MAX,
            &mut muxer,
            &mut first_audio_dts,
            &mut first_packet_dts,
        )?;
    }

    // Finalize
    tracing::debug!(
        "[trace] first_video={:?} first_audio={:?}",
        first_video_dts,
        first_audio_dts
    );

    let full_data = muxer.finalize()?;

    // Use robust offset detection to find start of media segment
    let media_offset = find_media_segment_offset(&full_data).ok_or_else(|| {
        HlsError::Muxing("No media segment data found (moof/styp missing)".to_string())
    })?;

    // We want only the media segment (moof + mdat)
    let mut media_data = full_data[media_offset..].to_vec();

    // Start fragment sequence for this segment.
    let start_frag_seq = segment.sequence as u32 + 1;

    // Normalize timeline to 0-based by anchoring to segment.start_pts (which is already 0-based in index).
    // This ensures EXTINF durations match the tfdt timeline perfectly across all track types.
    let video_tb = index.video_timebase;
    let out_tb_90k = ffmpeg::Rational::new(1, 90000);

    let video_target_tfdt =
        crate::ffmpeg_utils::utils::rescale_ts(segment.start_pts, video_tb, out_tb_90k) as u64;

    if is_interleaved {
        let v_idx = video_track_index.unwrap_or(stream_indices[0]);
        let a_idx = audio_track_index.unwrap_or(stream_indices.last().copied().unwrap_or(0));
        let v_track_id = muxer.get_output_track_id(v_idx).unwrap_or(1);
        let a_track_id = muxer.get_output_track_id(a_idx).unwrap_or(2);

        let audio_info = index.get_audio_stream(a_idx)?;
        let audio_tb = ffmpeg::Rational::new(1, audio_info.sample_rate as i32);

        let audio_target_tfdt = match index.get_segment_first_pts(segment.sequence) {
            Some(pts_90k) => {
                // Video already generated and cached the first display PTS.
                // Sync audio to it exactly.
                crate::ffmpeg_utils::utils::rescale_ts(pts_90k, out_tb_90k, audio_tb) as u64
            }
            None => {
                // Fallback: use segment.start_pts
                crate::ffmpeg_utils::utils::rescale_ts(segment.start_pts, video_tb, audio_tb) as u64
            }
        };

        tracing::info!(
            "[sync] interleaved seq={} v_tfdt={} a_tfdt={}",
            segment.sequence,
            video_target_tfdt,
            audio_target_tfdt
        );

        patch_tfdts_per_track(
            &mut media_data,
            start_frag_seq,
            v_track_id,
            a_track_id,
            video_target_tfdt,
            audio_target_tfdt,
        );
    } else if segment_type == "video" {
        patch_tfdts(&mut media_data, video_target_tfdt, start_frag_seq);

        // Success: cache the first display PTS for the audio track (if non-interleaved)
        if let Some(pts_90k) = read_first_display_pts(&media_data) {
            index.set_segment_first_pts(segment.sequence, pts_90k);
        }
    } else if segment_type == "audio" {
        let a_idx = audio_track_index.unwrap_or(stream_indices[0]);
        let audio_info = index.get_audio_stream(a_idx)?;
        let audio_tb = ffmpeg::Rational::new(1, audio_info.sample_rate as i32);

        let audio_target_tfdt = match index.get_segment_first_pts(segment.sequence) {
            Some(pts_90k) => {
                crate::ffmpeg_utils::utils::rescale_ts(pts_90k, out_tb_90k, audio_tb) as u64
            }
            None => {
                crate::ffmpeg_utils::utils::rescale_ts(segment.start_pts, video_tb, audio_tb) as u64
            }
        };

        tracing::info!(
            "[sync] audio seq={} tfdt={}",
            segment.sequence,
            audio_target_tfdt
        );

        patch_tfdts(&mut media_data, audio_target_tfdt, start_frag_seq);
    }

    // Prepend 'styp' box (Required for HLS fMP4)
    // Structure: Size (4), Type (4), Major Brand (4), Minor Version (4), Compatible Brands (4...)
    // Uses "iso8" as major brand, "cmfc" (CMAF) and "iso8" as compatible.
    let styp_box = vec![
        0x00, 0x00, 0x00, 24, // Size (24 bytes)
        b's', b't', b'y', b'p', // Type: styp
        b'i', b's', b'o', b'8', // Major Brand: iso8
        0x00, 0x00, 0x02, 0x00, // Minor Version: 512
        b'i', b's', b'o', b'8', // Compatible: iso8
        b'c', b'm', b'f', b'c', // Compatible: cmfc
    ];

    // Efficiently prepend
    media_data.splice(0..0, styp_box);

    Ok(Bytes::from(media_data))
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
