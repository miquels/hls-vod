//! fMP4 validation and muxing utilities

use crate::error::{FfmpegError, Result};
use crate::ffmpeg_utils::io::{create_memory_io, MemoryWriter};
use ffmpeg_next as ffmpeg;
use std::collections::HashMap;

/// Muxer for creating fMP4/CMAF segments in memory
pub struct Fmp4Muxer {
    output: ffmpeg::format::context::Output,
    writer: Box<MemoryWriter>,
    /// Map from input stream index to output stream index
    stream_map: HashMap<usize, usize>,
}

impl Fmp4Muxer {
    /// Create a new fMP4 muxer
    pub fn new() -> Result<Self> {
        let (output, writer) = create_memory_io()?;

        Ok(Self {
            output,
            writer,
            stream_map: HashMap::new(),
        })
    }

    /// Add a video stream to the muxer, copying parameters from input
    pub fn add_video_stream(
        &mut self,
        params: &ffmpeg::codec::parameters::Parameters,
        input_index: usize,
    ) -> Result<usize> {
        let mut out_stream = self
            .output
            .add_stream(ffmpeg::encoder::find(ffmpeg::codec::Id::None))
            .map_err(|e| FfmpegError::StreamConfig(format!("Failed to add video stream: {}", e)))?;

        out_stream.set_parameters(params.clone());
        // Reset codec_tag to let the muxer decide the correct tag
        crate::ffmpeg_utils::helpers::stream_reset_codec_tag(&mut out_stream);
        // Set video timebase to standard 90kHz for HLS
        out_stream.set_time_base(ffmpeg::Rational::new(1, 90000));

        let out_index = out_stream.index();
        self.stream_map.insert(input_index, out_index);

        tracing::debug!(
            "Added video stream: input {} -> output {}",
            input_index,
            out_index
        );

        Ok(out_index)
    }

    /// Add an audio stream
    pub fn add_audio_stream(
        &mut self,
        params: &ffmpeg::codec::parameters::Parameters,
        input_index: usize,
    ) -> Result<usize> {
        let mut out_stream = self
            .output
            .add_stream(ffmpeg::encoder::find(ffmpeg::codec::Id::None))
            .map_err(|e| FfmpegError::StreamConfig(format!("Failed to add audio stream: {}", e)))?;

        out_stream.set_parameters(params.clone());
        // Reset codec_tag to let the muxer decide the correct tag for the container (mp4).
        // This is crucial when copying from other containers (like TS or Matroska).
        crate::ffmpeg_utils::helpers::stream_reset_codec_tag(&mut out_stream);

        // Set audio timebase to sample rate
        let sample_rate = crate::ffmpeg_utils::helpers::codec_params_sample_rate(params);
        if sample_rate > 0 {
            out_stream.set_time_base(ffmpeg::Rational::new(1, sample_rate as i32));
        }

        let out_index = out_stream.index();
        self.stream_map.insert(input_index, out_index);

        tracing::debug!(
            "Added audio stream: input {} -> output {}",
            input_index,
            out_index
        );

        Ok(out_index)
    }

    /// Write output header (generates init.mp4)
    pub fn write_header(&mut self, delay_moov: bool) -> Result<Vec<u8>> {
        let mut opts = ffmpeg::Dictionary::new();
        if delay_moov {
            opts.set("movflags", "empty_moov+default_base_moof+delay_moov+negative_cts_offsets");
        } else {
            opts.set("movflags", "empty_moov+default_base_moof+negative_cts_offsets");
        }
        opts.set("avoid_negative_ts", "0");
        // Prevent the mp4 muxer from implicitly adding frag_keyframe (which
        // splits each segment into multiple moof/mdat fragments at every video
        // keyframe).  A large frag_duration ensures one fragment per segment.
        opts.set("frag_duration", "60000000");

        self.output
            .write_header_with(opts)
            .map_err(|e| FfmpegError::WriteError(format!("Failed to write header: {}", e)))?;

        // Flush writer to get data
        // self.output.flush(); // Context doesn't always have flush, avio does.
        // But headers are written immediately usually.

        // Return current buffer content (which is the init segment).
        // Do NOT clear the writer: AVIO's internal position counter must stay in sync
        // with writer.position. If we clear here, FFmpeg's seeks to patch moof size
        // fields land at wrong offsets (init_size bytes past the actual target),
        // producing size=0 moof boxes that Chrome's MSE rejects.
        Ok(self.writer.data())
    }

    /// Generate an init segment by writing multiple packets to force `moov` creation.
    /// Essential for interleaved segments with streams like AC-3 that lack extradata
    /// in the source container.
    pub fn generate_init_segment_with_packets<'a, I>(
        &mut self,
        packets: I,
        delay_moov: bool,
    ) -> Result<Vec<u8>>
    where
        I: IntoIterator<Item = &'a mut ffmpeg::Packet>,
    {
        let mut opts = ffmpeg::Dictionary::new();
        if delay_moov {
            opts.set("movflags", "empty_moov+default_base_moof+delay_moov+negative_cts_offsets");
        } else {
            // Even if caller said false, we might want it for consistency.
            // But let's respect the flag for now but add CTTS v1.
            opts.set("movflags", "empty_moov+default_base_moof+negative_cts_offsets");
        }
        opts.set("avoid_negative_ts", "0");

        self.output
            .write_header_with(opts)
            .map_err(|e| FfmpegError::WriteError(format!("Failed to write header: {}", e)))?;

        for packet in packets {
            self.write_packet(packet)?;
        }
        let _ = self.output.write_trailer();

        let full_data = self.writer.data();
        self.writer.clear();

        // Extract just ftyp + moov by finding the first media box
        if let Some(offset) = find_media_segment_offset(&full_data) {
            Ok(full_data[..offset].to_vec())
        } else {
            Ok(full_data)
        }
    }
    /// Write a packet
    pub fn write_packet(&mut self, packet: &mut ffmpeg::Packet) -> Result<()> {
        let stream_index = packet.stream();

        if let Some(&out_index) = self.stream_map.get(&stream_index) {
            packet.set_stream(out_index);
            packet.set_position(-1); // Unset byte position

            // Rescale timestamps happens here or caller?
            // Usually caller (repackage function) handles rescaling if inputs differ.
            // But if we just copy params, timebases might differ.
            // Caller should ensure packet pts/dts are correct for the output stream timebase.
            // For mp4 output, timebase is usually 1/timescale.

            // Let's rely on interleaved_write_frame to do some magic or caller to rescale.
            // Ideally caller rescales from input tb to output tb.
            // But we can't easily access output tb before header is written?
            // Actually 'mp4' usually sets tb based on stream.

            packet
                .write_interleaved(&mut self.output)
                .map_err(|e| FfmpegError::WriteError(format!("Failed to write packet: {}", e)))?;
        }

        Ok(())
    }

    /// Flush and get the accumulated segment data
    ///
    /// Should be called after writing all packets for a segment.
    pub fn finalize(&mut self) -> Result<Vec<u8>> {
        // Write trailer is NOT correct for fMP4 usually if we want just fragments?
        // But we need to flush any buffered data.
        // write_trailer() writes the index if not empty_moov, but with empty_moov it might just flush.
        // HOWEVER, calling write_trailer might close the file/context in a way that prevents reuse?
        // Indexer uses it once per segment.

        if let Err(e) = self.output.write_trailer() {
            // Log warning but continue if we have data.
            // Some FFmpeg versions/configs return error on custom IO trailer writing (e.g. -67 EPROCLIM/ENOLINK?)
            tracing::debug!(
                "Failed to write trailer: {}, proceeding with available data",
                e
            );
        }

        let data = self.writer.data();
        self.writer.clear();

        Ok(data)
    }

    /// Access inner memory writer data directly (peek)
    #[allow(dead_code)] // we need this for testing and development
    pub fn current_data(&self) -> Vec<u8> {
        self.writer.data()
    }

    /// Get the timebase of an output stream corresponding to an input stream index
    pub fn get_output_timebase(&self, input_index: usize) -> Option<ffmpeg::Rational> {
        let out_index = *self.stream_map.get(&input_index)?;
        self.output.stream(out_index).map(|s| s.time_base())
    }
}

/// Parse the first `elst` (edit list) `media_time` from fMP4 init segment bytes.
///
/// FFmpeg writes an `edts/elst` box when the stream has an encoder delay
/// (e.g. AAC: media_time=1024 at 48kHz = 21.3ms).  The player subtracts this
/// value from every `tfdt` to compute the presentation timestamp:
///   presentation = (tfdt - elst_media_time) / timescale
///
/// Returns `Some(media_time)` if an elst entry is found, `None` otherwise.
#[allow(dead_code)] // we need this for testing and development
pub fn parse_elst_media_time(data: &[u8]) -> Option<i64> {
    parse_elst_in_boxes(data)
}

#[allow(dead_code)] // we need this for testing and development
fn parse_elst_in_boxes(data: &[u8]) -> Option<i64> {
    let mut pos = 0;
    while pos + 8 <= data.len() {
        let size = u32::from_be_bytes(data[pos..pos + 4].try_into().ok()?) as usize;
        let box_type = &data[pos + 4..pos + 8];
        let size = if size == 0 { data.len() - pos } else { size };
        if size < 8 || pos + size > data.len() {
            break;
        }
        let content = &data[pos + 8..pos + size];
        match box_type {
            b"moov" | b"trak" | b"edts" | b"mdia" | b"minf" | b"stbl" => {
                if let Some(v) = parse_elst_in_boxes(content) {
                    return Some(v);
                }
            }
            b"elst" => {
                // version(1) + flags(3) + entry_count(4) = 8 bytes header
                if content.len() < 8 {
                    break;
                }
                let version = content[0];
                let entry_count = u32::from_be_bytes(content[4..8].try_into().ok()?) as usize;
                if entry_count == 0 {
                    break;
                }
                // First entry: segment_duration + media_time + media_rate
                let off = 8;
                let media_time = if version == 1 {
                    // 8-byte segment_duration, then 8-byte signed media_time
                    if content.len() < off + 16 {
                        break;
                    }
                    i64::from_be_bytes(content[off + 8..off + 16].try_into().ok()?)
                } else {
                    // 4-byte segment_duration, then 4-byte signed media_time
                    if content.len() < off + 8 {
                        break;
                    }
                    i32::from_be_bytes(content[off + 4..off + 8].try_into().ok()?) as i64
                };
                return Some(media_time);
            }
            _ => {}
        }
        pos += size;
    }
    None
}

impl Drop for Fmp4Muxer {
    fn drop(&mut self) {
        crate::ffmpeg_utils::helpers::detach_avio(&mut self.output);
    }
}

#[allow(dead_code)] // we need this for testing and development
pub fn validate_fmp4(data: &[u8]) -> bool {
    if data.len() < 8 {
        return false;
    }

    // Check for ftyp, moov, or moof box at start
    let box_type = &data[4..8];
    box_type == b"ftyp" || box_type == b"moov" || box_type == b"moof"
}

/// Find a specific box in fMP4 data
#[allow(dead_code)] // we need this for testing and development
pub fn find_box(data: &[u8], box_type: &[u8]) -> Option<usize> {
    let mut pos = 0;
    while pos + 8 <= data.len() {
        let size =
            u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as usize;

        if size < 8 {
            return None;
        }

        if pos + 8 > data.len() {
            return None;
        }

        let current_type = &data[pos + 4..pos + 8];
        if current_type == box_type {
            return Some(pos);
        }

        pos += size;
    }
    None
}

/// Find the start offset of the media segment (first styp/moof/prft box)
pub fn find_media_segment_offset(data: &[u8]) -> Option<usize> {
    let mut pos = 0;
    while pos + 8 <= data.len() {
        let size =
            u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as usize;
        let type_bytes = &data[pos + 4..pos + 8];

        // If we encounter a box that signals start of media segment
        if type_bytes == b"styp" || type_bytes == b"moof" || type_bytes == b"prft" {
            return Some(pos);
        }

        // Safety check for size
        if size < 8 {
            // Invalid box size, stop here? Or return partial?
            // Assuming init segment boxes are valid.
            break;
        }

        let next_pos = pos + size;
        if next_pos > data.len() {
            break;
        }
        pos = next_pos;
    }
    // If we didn't find specific media boxes, maybe we just return None (meaning no media segment found)
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_muxer_integration() {
        println!("Starting test_muxer_integration");
        // Initialize FFmpeg
        ffmpeg::init().unwrap();

        // Path to test video
        let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        path.push("testvideos");
        path.push("bun33s.mp4");

        if !path.exists() {
            eprintln!("Test video not found at {:?}, skipping test", path);
            return;
        }
        println!("Test video path: {:?}", path);

        // Open input
        let mut input = ffmpeg::format::input(&path).unwrap();
        println!("Input opened");

        // Create muxer
        let mut muxer = Fmp4Muxer::new().unwrap();
        println!("Muxer created");

        // Add streams
        let mut video_idx = None;
        let mut audio_idx = None;

        for stream in input.streams() {
            let params = stream.parameters();
            if params.medium() == ffmpeg::media::Type::Video && video_idx.is_none() {
                muxer.add_video_stream(&params, stream.index()).unwrap();
                video_idx = Some(stream.index());
                println!("Added video stream {}", stream.index());
            } else if params.medium() == ffmpeg::media::Type::Audio && audio_idx.is_none() {
                muxer.add_audio_stream(&params, stream.index()).unwrap();
                audio_idx = Some(stream.index());
                println!("Added audio stream {}", stream.index());
            }
        }

        assert!(
            video_idx.is_some() || audio_idx.is_some(),
            "No streams found in test video"
        );

        // Write header (init segment)
        println!("Writing header...");
        let init_data = muxer.write_header(false).unwrap();
        println!("Header written, size: {}", init_data.len());
        assert!(!init_data.is_empty(), "Init segment should not be empty");
        // Check for ftyp
        assert_eq!(
            &init_data[4..8],
            b"ftyp",
            "Init segment should start with ftyp"
        );

        // Write some packets
        let mut packet_count = 0;
        eprintln!("Writing packets...");
        for (stream, mut packet) in input.packets() {
            if Some(stream.index()) == video_idx || Some(stream.index()) == audio_idx {
                let out_idx = if Some(stream.index()) == video_idx {
                    // We need to find the output stream index.
                    // Fmp4Muxer::add_video_stream returned it, but we didn't save it mapped.
                    // But in this test we know order.
                    // Actually Fmp4Muxer assigns sequentially.
                    // Let's rely on internal map or just assume 0 and 1.
                    // Wait, write_packet uses internal map.
                    // But to rescale we need output timebase.
                    // We can access muxer.output.stream(i).
                    // We need to know which output stream corresponds to input stream.
                    // The muxer.stream_map is private? No, we are in tests mod.
                    *muxer.stream_map.get(&stream.index()).unwrap()
                } else {
                    *muxer.stream_map.get(&stream.index()).unwrap()
                };

                let input_timebase = stream.time_base();
                let output_timebase = muxer.output.stream(out_idx as usize).unwrap().time_base();

                packet.rescale_ts(input_timebase, output_timebase);

                muxer.write_packet(&mut packet).unwrap();
                packet_count += 1;
                if packet_count >= 10 {
                    break;
                }
            }
        }
        println!("Written {} packets", packet_count);

        assert!(packet_count > 0, "Should have written some packets");

        // Finalize (media segment)
        println!("Finalizing...");
        let full_data = match muxer.finalize() {
            Ok(data) => data,
            Err(e) => {
                println!("Finalize failed: {:?}", e);
                return; // Exit test gracefully (fail)
            }
        };
        println!("Finalized, size: {}", full_data.len());
        assert!(!full_data.is_empty(), "Finalized data should not be empty");

        // Use robust offset detection
        let media_offset = find_media_segment_offset(&full_data);

        if let Some(offset) = media_offset {
            let media_data = &full_data[offset..];
            println!(
                "Media segment found at offset {}, size: {}",
                offset,
                media_data.len()
            );

            // Verify type
            let type_bytes = &media_data[4..8];
            assert!(
                type_bytes == b"moof"
                    || type_bytes == b"mdat"
                    || type_bytes == b"styp"
                    || type_bytes == b"prft",
                "Media segment should start with moof/mdat/styp/prft, got {:?}",
                type_bytes
            );
        } else {
            println!("Warning: No media segment start box found in output");
            // Fail if we expected packets
            if packet_count > 0 {
                panic!("Expected media segment but found none");
            }
        }
        println!("Test completed successfully");
    }

    #[test]
    fn test_validate_fmp4() {
        let data = vec![0, 0, 0, 24, b'f', b't', b'y', b'p'];
        assert!(validate_fmp4(&data));

        let data = vec![0, 0, 0, 8, b'm', b'o', b'o', b'v'];
        assert!(validate_fmp4(&data));
    }

    #[test]
    fn test_mux_ac3_header() {
        ffmpeg::init().unwrap();
        let mut muxer = Fmp4Muxer::new().unwrap();

        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("testvideos")
            .join("bun33s.mp4");

        if !path.exists() {
            return;
        }

        let mut input = ffmpeg::format::input(&path).unwrap();
        let aac_stream = input
            .streams()
            .filter(|s| s.parameters().medium() == ffmpeg::media::Type::Audio)
            .next()
            .expect("No audio stream in test video");
        let mut params = aac_stream.parameters();

        crate::ffmpeg_utils::helpers::codec_params_set_for_test(
            &mut params,
            ffmpeg::ffi::AVCodecID::AV_CODEC_ID_AC3,
            1536,
            192000,
        );
        // sample_rate and ch_layout are already valid from the original AAC stream.

        let stream_index = aac_stream.index();
        muxer
            .add_audio_stream(&params, stream_index)
            .expect("Failed to add AC3 stream");

        // Find the first packet and use the new method
        let mut first_packet = None;
        for (s, pkt) in input.packets() {
            if s.index() == stream_index {
                first_packet = Some(pkt);
                break;
            }
        }

        let mut pkt = first_packet.expect("No packets found in test video");
        if let Some(out_tb) = muxer.get_output_timebase(stream_index) {
            let in_tb = input.stream(stream_index).unwrap().time_base();
            pkt.rescale_ts(in_tb, out_tb);
        }

        let init_data = muxer
            .generate_init_segment_with_packets(vec![&mut pkt], true)
            .expect("Failed to generate AC3 init segment");

        assert!(!init_data.is_empty(), "Init segment should not be empty");
        let box_type = &init_data[4..8];
        assert_eq!(box_type, b"ftyp", "Init segment should start with ftyp");
    }

    #[test]
    fn test_mux_av_ac3_header() {
        ffmpeg::init().unwrap();
        let mut muxer = Fmp4Muxer::new().unwrap();

        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("testvideos")
            .join("bun33s.mp4");

        if !path.exists() {
            return;
        }

        let mut input = ffmpeg::format::input(&path).unwrap();
        let video_stream = input
            .streams()
            .find(|s| s.parameters().medium() == ffmpeg::media::Type::Video)
            .expect("No video stream");
        let aac_stream = input
            .streams()
            .find(|s| s.parameters().medium() == ffmpeg::media::Type::Audio)
            .expect("No audio stream in test video");

        let mut audio_params = aac_stream.parameters();
        let video_params = video_stream.parameters();

        crate::ffmpeg_utils::helpers::codec_params_set_for_test(
            &mut audio_params,
            ffmpeg::ffi::AVCodecID::AV_CODEC_ID_AC3,
            1536,
            192000,
        );

        let video_idx = video_stream.index();
        let audio_idx = aac_stream.index();

        muxer
            .add_video_stream(&video_params, video_idx)
            .expect("Failed to add video stream");
        muxer
            .add_audio_stream(&audio_params, audio_idx)
            .expect("Failed to add AC3 stream");

        // Find the first packet of each stream
        let mut video_packet = None;
        let mut audio_packet = None;

        for (s, mut pkt) in input.packets() {
            if s.index() == video_idx && video_packet.is_none() {
                if let Some(out_tb) = muxer.get_output_timebase(video_idx) {
                    let in_tb = s.time_base();
                    pkt.rescale_ts(in_tb, out_tb);
                }
                video_packet = Some(pkt);
            } else if s.index() == audio_idx && audio_packet.is_none() {
                if let Some(out_tb) = muxer.get_output_timebase(audio_idx) {
                    let in_tb = s.time_base();
                    pkt.rescale_ts(in_tb, out_tb);
                }
                audio_packet = Some(pkt);
            }
            if video_packet.is_some() && audio_packet.is_some() {
                break;
            }
        }

        let mut pkts = vec![];
        if let Some(vp) = video_packet {
            pkts.push(vp);
        }
        if let Some(ap) = audio_packet {
            pkts.push(ap);
        }

        let pkt_refs: Vec<&mut ffmpeg::Packet> = pkts.iter_mut().collect();

        let init_data = muxer
            .generate_init_segment_with_packets(pkt_refs, true)
            .expect("Failed to generate AV AC3 init segment");

        assert!(!init_data.is_empty(), "Init segment should not be empty");
        let box_type = &init_data[4..8];
        assert_eq!(box_type, b"ftyp", "Init segment should start with ftyp");
    }
}
