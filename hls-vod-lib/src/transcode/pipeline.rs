#![allow(dead_code)]

//! Audio transcoding pipeline
//!
//! Combines `AudioDecoder` → `AudioResampler` → `AacEncoder` to convert
//! non-AAC audio streams (AC-3, Opus, MP3, FLAC, …) into AAC-LC packets
//! ready for fMP4 muxing.

use ffmpeg_next as ffmpeg;

use crate::error::{HlsError, Result};
use crate::media::{AudioStreamInfo, SegmentInfo};

use super::decoder::AudioDecoder;
use super::encoder::{get_recommended_bitrate, AacEncoder};
use super::resampler::AudioResampler;

pub use super::resampler::HLS_SAMPLE_RATE;

/// Check if an audio stream needs transcoding for HLS compatibility.
///
/// AAC streams can be muxed directly; everything else must be decoded and
/// re-encoded to AAC.
pub fn needs_transcoding(audio_stream: &AudioStreamInfo) -> bool {
    !matches!(
        audio_stream.codec_id,
        ffmpeg_next::codec::Id::AAC
            | ffmpeg_next::codec::Id::AC3
            | ffmpeg_next::codec::Id::EAC3
            | ffmpeg_next::codec::Id::MP3
            | ffmpeg_next::codec::Id::OPUS
    )
}

/// Transcode audio packets from a source segment into AAC packets.
///
/// Opens the source file, seeks to the segment boundary, decodes and resamples
/// each compressed audio packet, then encodes the PCM frames to AAC.
///
/// Returns a `Vec` of AAC packets ready to be written into an `Fmp4Muxer`.
/// Packet timestamps are expressed in the AAC encoder's output timebase
/// (1 / sample_rate).
pub fn transcode_audio_segment(
    source_path: &std::path::Path,
    audio_info: &AudioStreamInfo,
    segment: &SegmentInfo,
    video_timebase: ffmpeg::Rational,
    shift_to_zero: bool,
) -> Result<(Vec<ffmpeg::codec::packet::Packet>, ffmpeg::Rational)> {
    let stream_index = audio_info.stream_index;
    let bitrate = get_recommended_bitrate(audio_info.channels);

    tracing::debug!(
        seq = segment.sequence,
        stream_index,
        codec = ?audio_info.codec_id,
        start_pts = segment.start_pts,
        end_pts = segment.end_pts,
        "transcode_audio_segment: starting"
    );

    // ── 1. Open source file ────────────────────────────────────────────────
    let mut input = ffmpeg::format::input(source_path).map_err(|e| {
        HlsError::Ffmpeg(crate::error::FfmpegError::OpenInput(format!(
            "transcode_audio: failed to open {:?}: {}",
            source_path, e
        )))
    })?;

    // ── 2. Open decoder for the audio stream ──────────────────────────────
    let stream = input.stream(stream_index).ok_or_else(|| {
        HlsError::StreamNotFound(format!("audio stream {} not found", stream_index))
    })?;

    let mut decoder = AudioDecoder::open(&stream)?;
    tracing::debug!(stream_index, "transcode_audio_segment: decoder opened");

    // ── 3. Seek to segment start (with overlap to prime AAC encoder) ───────
    let pre_roll_delay_seconds = 0.5;

    // We aim to seek 0.5 seconds before the actual start PTS to collect primer frames
    let target_start_ts_video = segment.start_pts;
    let target_start_sec = target_start_ts_video as f64 * video_timebase.numerator() as f64
        / video_timebase.denominator() as f64;
    let seek_sec = (target_start_sec - pre_roll_delay_seconds).max(0.0);

    let seek_ts = (seek_sec * 1_000_000.0) as i64;

    input.seek(seek_ts, ..seek_ts).map_err(|e| {
        HlsError::Ffmpeg(crate::error::FfmpegError::ReadFrame(format!(
            "transcode_audio seek error: {}",
            e
        )))
    })?;
    tracing::debug!(seek_ts, "transcode_audio_segment: seeked");

    // ── 4. Decode compressed packets into PCM frames ───────────────────────
    let end_pts_90k = crate::ffmpeg_utils::utils::rescale_ts(
        segment.end_pts,
        video_timebase,
        ffmpeg::Rational(1, 90000),
    );

    let mut pcm_frames: Vec<ffmpeg::util::frame::Audio> = Vec::new();
    let mut resampler: Option<AudioResampler> = None;
    let mut first_frame_pts_48k: Option<i64> = None;

    for (stream, packet) in input.packets() {
        if stream.index() != stream_index {
            continue;
        }

        // Stop when we've passed the segment end
        let pkt_pts = packet.pts().or(packet.dts()).unwrap_or(0);
        let pkt_90k = crate::ffmpeg_utils::utils::rescale_ts(
            pkt_pts,
            stream.time_base(),
            ffmpeg::Rational(1, 90000),
        );
        if pkt_90k >= end_pts_90k {
            break;
        }

        decoder.send_packet(&packet)?;

        while let Some(frame) = decoder.receive_frame()? {
            // Lazily create the resampler from the first decoded frame
            let rsmp = match resampler {
                Some(ref mut r) => r,
                None => {
                    tracing::debug!(
                        sample_rate = frame.rate(),
                        channels = frame.channels(),
                        format = ?frame.format(),
                        "transcode_audio_segment: creating resampler from first frame"
                    );
                    resampler = Some(AudioResampler::new(&frame, HLS_SAMPLE_RATE)?);
                    resampler.as_mut().unwrap()
                }
            };

            // Capture the global timeline offset from the very first frame
            if first_frame_pts_48k.is_none() {
                let fr_pts = frame.pts().unwrap_or(0);
                first_frame_pts_48k = Some(crate::ffmpeg_utils::utils::rescale_ts(
                    fr_pts,
                    stream.time_base(),
                    ffmpeg::Rational(1, HLS_SAMPLE_RATE as i32),
                ));
            }

            let resampled = rsmp.convert(&frame)?;
            pcm_frames.extend(resampled);
        }
    }

    tracing::debug!(
        pcm_frames = pcm_frames.len(),
        "transcode_audio_segment: decode loop complete"
    );

    // Flush decoder
    decoder.send_eof()?;
    while let Some(frame) = decoder.receive_frame()? {
        if let Some(rsmp) = resampler.as_mut() {
            let resampled = rsmp.convert(&frame)?;
            pcm_frames.extend(resampled);
        }
    }

    // Flush resampler
    if let Some(rsmp) = resampler.as_mut() {
        let remaining = rsmp.flush()?;
        pcm_frames.extend(remaining);
    }

    tracing::debug!(
        pcm_frames = pcm_frames.len(),
        "transcode_audio_segment: after flush"
    );

    if pcm_frames.is_empty() {
        tracing::warn!(
            seq = segment.sequence,
            stream_index,
            seek_ts,
            end_pts_90k,
            "transcode_audio_segment: 0 PCM frames decoded - returning empty packet list"
        );
        return Ok((vec![], ffmpeg::Rational::new(1, HLS_SAMPLE_RATE as i32)));
    }

    // ── 5. Align grid and Encode PCM frames → AAC packets ─────────────────
    // The AAC encoder requires exactly 1024 samples per non-last frame.
    const AAC_FRAME_SIZE: usize = 1024;

    let base_pts_48k = first_frame_pts_48k.unwrap_or(0);
    // Determine the sample offset from the absolute grid boundary
    let grid_offset =
        (base_pts_48k % AAC_FRAME_SIZE as i64).rem_euclid(AAC_FRAME_SIZE as i64) as usize;

    // We want our chunks to mathematically align with the `start_frame * 1024` grid.
    // So we calculate how many samples to discard from the START of our resampled buffer
    // so that the first sample corresponds to an exact multiple of 1024.
    let discard_samples = if grid_offset == 0 {
        0
    } else {
        AAC_FRAME_SIZE - grid_offset
    };

    // Calculate the absolute PTS of the FIRST sample after discarding
    let mut aligned_pts_48k = base_pts_48k + discard_samples as i64;

    let channels: u16 = pcm_frames.first().map(|f| f.channels()).unwrap_or(2);
    let pcm_frames = rechunk_pcm_frames(pcm_frames, AAC_FRAME_SIZE, discard_samples);

    let mut encoder = AacEncoder::open(HLS_SAMPLE_RATE, channels, bitrate)?;
    let output_timebase = encoder.output_timebase();

    // The boundary of the requested segment
    let segment_start_sec = segment.start_pts as f64 * video_timebase.numerator() as f64
        / video_timebase.denominator() as f64;
    let segment_start_48k = (segment_start_sec * HLS_SAMPLE_RATE as f64) as i64;

    // Snap the segment start precisely to the mathematical 1024-sample boundary
    // to guarantee no gap/overlap drift across segments.
    let target_grid_start_48k = (segment_start_48k / AAC_FRAME_SIZE as i64) * AAC_FRAME_SIZE as i64;

    let mut aac_packets: Vec<ffmpeg::codec::packet::Packet> = Vec::new();

    for mut frame in pcm_frames {
        let frame_samples = frame.samples();
        frame.set_pts(Some(aligned_pts_48k));
        encoder.send_frame(&frame)?;

        while let Some(mut pkt) = encoder.receive_packet()? {
            let pkt_pts = pkt.pts().unwrap_or(0);
            // Drop packets that are entirely before our target segment boundary (pre-roll/primer)
            if pkt_pts >= target_grid_start_48k {
                if shift_to_zero {
                    let relative_pts = pkt_pts - target_grid_start_48k;
                    pkt.set_pts(Some(relative_pts));
                    pkt.set_dts(Some(relative_pts));
                }
                aac_packets.push(pkt);
            }
        }
        aligned_pts_48k += frame_samples as i64;
    }

    // Flush encoder
    let tail = encoder.flush()?;
    for mut pkt in tail {
        let pkt_pts = pkt.pts().unwrap_or(0);
        if pkt_pts >= target_grid_start_48k {
            if shift_to_zero {
                let relative_pts = pkt_pts - target_grid_start_48k;
                pkt.set_pts(Some(relative_pts));
                pkt.set_dts(Some(relative_pts));
            }
            aac_packets.push(pkt);
        }
    }

    tracing::debug!(
        aac_packets = aac_packets.len(),
        "transcode_audio_segment: done"
    );

    Ok((aac_packets, output_timebase))
}

/// Rechunk a list of FLTP audio frames so every frame except the last has
/// exactly `chunk_size` samples. Required because the AAC encoder demands
/// 1024 samples/frame while Opus decodes 960 samples/frame.
///
/// Handles FLTP (planar float32) only — the standard intermediate format
/// produced by our AudioResampler.
fn rechunk_pcm_frames(
    frames: Vec<ffmpeg::util::frame::Audio>,
    chunk_size: usize,
    skip_samples: usize,
) -> Vec<ffmpeg::util::frame::Audio> {
    use ffmpeg_next::util::channel_layout::ChannelLayout;

    if frames.is_empty() {
        return vec![];
    }

    let channels = frames[0].channels() as usize;
    let rate = frames[0].rate();
    let format = frames[0].format();
    let layout = {
        let l = frames[0].channel_layout();
        if l.bits() == 0 {
            if channels == 1 {
                ChannelLayout::MONO
            } else {
                ChannelLayout::STEREO
            }
        } else {
            l
        }
    };

    // Flatten every channel into its own Vec<f32>
    let mut bufs: Vec<Vec<f32>> = vec![Vec::new(); channels];
    for frame in &frames {
        let n = frame.samples();
        for ch in 0..channels {
            let data = crate::ffmpeg_utils::helpers::audio_plane_data(frame, ch);
            let floats = crate::ffmpeg_utils::helpers::fltp_plane_as_f32(data, n)
                .unwrap_or_else(|| panic!("FLTP plane: bad alignment or length. format={:?}, channels={}, ch={}, n={}, data.len()={}, ptr_align={}", format, channels, ch, n, data.len(), data.as_ptr() as usize % 4));
            bufs[ch].extend_from_slice(floats);
        }
    }

    let total = bufs[0].len();
    let mut result = Vec::new();
    let mut offset = skip_samples;

    while offset < total {
        let n = chunk_size.min(total - offset);
        let mut out = ffmpeg::util::frame::Audio::new(format, n, layout);
        out.set_rate(rate);
        for ch in 0..channels {
            let plane = crate::ffmpeg_utils::helpers::audio_plane_data_mut(&mut out, ch);
            let floats_out = crate::ffmpeg_utils::helpers::fltp_plane_as_f32_mut(plane, n)
                .expect("FLTP plane: bad alignment or length");
            floats_out.copy_from_slice(&bufs[ch][offset..offset + n]);
        }
        result.push(out);
        offset += n;
    }

    result
}

/// Transcoding requirements (kept for compatibility and tests)
#[derive(Debug, Clone)]
pub struct TranscodeRequirements {
    pub needs_transcoding: bool,
    pub source_codec: ffmpeg::codec::Id,
    pub source_sample_rate: u32,
    pub source_channels: u16,
    pub target_sample_rate: u32,
    pub target_channels: u16,
    pub target_bitrate: u64,
}

/// Get transcoding requirements for an audio stream.
pub fn get_transcode_requirements(audio_stream: &AudioStreamInfo) -> TranscodeRequirements {
    TranscodeRequirements {
        needs_transcoding: needs_transcoding(audio_stream),
        source_codec: audio_stream.codec_id,
        source_sample_rate: audio_stream.sample_rate,
        source_channels: audio_stream.channels,
        target_sample_rate: HLS_SAMPLE_RATE,
        target_channels: 2,
        target_bitrate: get_recommended_bitrate(audio_stream.channels),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::AudioStreamInfo;

    fn make_audio_stream(codec: ffmpeg::codec::Id) -> AudioStreamInfo {
        AudioStreamInfo {
            stream_index: 0,
            codec_id: codec,
            sample_rate: 48000,
            channels: 2,
            bitrate: 128000,
            language: Some("en".to_string()),
            transcode_to: None,
            encoder_delay: 0,
        }
    }

    #[test]
    fn test_needs_transcoding_aac() {
        assert!(!needs_transcoding(&make_audio_stream(
            ffmpeg::codec::Id::AAC
        )));
    }

    #[test]
    fn test_needs_transcoding_ac3() {
        assert!(!needs_transcoding(&make_audio_stream(
            ffmpeg::codec::Id::AC3
        )));
    }

    #[test]
    fn test_needs_transcoding_opus() {
        assert!(!needs_transcoding(&make_audio_stream(
            ffmpeg::codec::Id::OPUS
        )));
    }

    #[test]
    fn test_get_transcode_requirements() {
        let stream = AudioStreamInfo {
            stream_index: 0,
            codec_id: ffmpeg::codec::Id::VORBIS,
            sample_rate: 48000,
            channels: 6,
            bitrate: 384000,
            language: Some("en".to_string()),
            transcode_to: None,
            encoder_delay: 0,
        };
        let reqs = get_transcode_requirements(&stream);
        assert!(reqs.needs_transcoding);
        assert_eq!(reqs.source_codec, ffmpeg::codec::Id::VORBIS);
        assert_eq!(reqs.target_sample_rate, 48000);
        assert_eq!(reqs.target_channels, 2);
    }

    #[test]
    fn test_transcoder_config_default() {
        assert_eq!(HLS_SAMPLE_RATE, 48000);
    }
}
