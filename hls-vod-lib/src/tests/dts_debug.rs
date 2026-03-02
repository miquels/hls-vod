//! DTS diagnostic integration test

#[cfg(test)]
#[allow(dead_code, unused_variables)]
mod tests {
    use crate::ffmpeg_utils::ffmpeg;
    use crate::segment::generator::{
        generate_audio_segment, generate_video_init_segment, generate_video_segment,
    };
    use crate::segment::muxer::find_box;
    use std::path::PathBuf;

    // ── helpers ──────────────────────────────────────────────────────────────

    fn u32_be(data: &[u8], offset: usize) -> u32 {
        u32::from_be_bytes(data[offset..offset + 4].try_into().unwrap())
    }

    fn u64_be(data: &[u8], offset: usize) -> u64 {
        u64::from_be_bytes(data[offset..offset + 8].try_into().unwrap())
    }

    fn find_box_recursive<'a>(data: &'a [u8], tag: &[u8; 4]) -> Option<usize> {
        let mut pos = 0;
        while pos + 8 <= data.len() {
            let size = u32_be(data, pos) as usize;
            if size < 8 || pos + size > data.len() {
                break;
            }
            let btype: &[u8] = &data[pos + 4..pos + 8];
            if btype == tag.as_ref() {
                return Some(pos);
            }
            const CONTAINERS: &[&[u8]] = &[
                b"moov", b"trak", b"mdia", b"minf", b"stbl", b"mvex", b"moof", b"traf",
            ];
            if CONTAINERS.iter().any(|c| btype == *c) {
                if let Some(inner) = find_box_recursive(&data[pos + 8..pos + size], tag) {
                    return Some(pos + 8 + inner);
                }
            }
            pos += size;
        }
        None
    }

    fn parse_mdhd_timescales(init: &[u8]) -> Vec<(u32, u32)> {
        let mut results = Vec::new();
        parse_mdhd_in(init, &mut results);
        results
    }

    fn parse_mdhd_in(data: &[u8], out: &mut Vec<(u32, u32)>) {
        let mut pos = 0;
        while pos + 8 <= data.len() {
            let size = u32_be(data, pos) as usize;
            if size < 8 || pos + size > data.len() {
                break;
            }
            let btype: &[u8] = &data[pos + 4..pos + 8];
            match btype {
                b"moov" | b"trak" | b"mdia" => {
                    parse_mdhd_in(&data[pos + 8..pos + size], out);
                }
                b"mdhd" => {
                    if size >= 24 {
                        let version = data[pos + 8];
                        let timescale = if version == 1 && size >= 32 {
                            u32_be(data, pos + 28)
                        } else {
                            u32_be(data, pos + 20)
                        };
                        out.push((0, timescale));
                    }
                }
                _ => {}
            }
            pos += size;
        }
    }

    fn parse_trex_default_duration(init: &[u8]) -> u32 {
        let mut pos = 0;
        while pos + 8 <= init.len() {
            let size = u32_be(init, pos) as usize;
            if size < 8 || pos + size > init.len() {
                break;
            }
            let btype: &[u8] = &init[pos + 4..pos + 8];
            match btype {
                b"moov" | b"mvex" => {
                    let result = parse_trex_default_duration(&init[pos + 8..pos + size]);
                    if result > 0 {
                        return result;
                    }
                }
                b"trex" => {
                    if size >= 28 {
                        return u32_be(init, pos + 20);
                    }
                }
                _ => {}
            }
            pos += size;
        }
        0
    }

    struct SegmentTiming {
        frag_seq: u32,
        base_decode_time: u64,
        tfdt_version: u8,
        total_trun_duration: u64,
        sample_count: u32,
        sample_durations: Vec<u32>,
    }

    fn parse_media_segment(data: &[u8]) -> SegmentTiming {
        let data = if data.len() >= 8 && &data[4..8] == b"styp" {
            let styp_size = u32_be(data, 0) as usize;
            &data[styp_size..]
        } else {
            data
        };

        let moof_pos = find_box(data, b"moof").expect("moof not found in segment");
        let moof_size = u32_be(data, moof_pos) as usize;
        let moof = &data[moof_pos..moof_pos + moof_size];

        let mfhd_pos = find_box_recursive(moof, b"mfhd").expect("mfhd not found");
        let frag_seq = u32_be(moof, mfhd_pos + 12);

        let traf_pos = find_box_recursive(moof, b"traf").expect("traf not found");
        let traf_size = u32_be(moof, traf_pos) as usize;
        let traf = &moof[traf_pos..traf_pos + traf_size];

        let tfdt_pos = find_box_recursive(traf, b"tfdt").expect("tfdt not found");
        let tfdt_version = traf[tfdt_pos + 8];
        let base_decode_time = if tfdt_version == 1 {
            u64_be(traf, tfdt_pos + 12)
        } else {
            u32_be(traf, tfdt_pos + 12) as u64
        };

        let trun_pos = find_box_recursive(traf, b"trun").expect("trun not found");
        let trun_flags = u32_be(traf, trun_pos + 8) & 0x00FF_FFFF;
        let sample_count = u32_be(traf, trun_pos + 12);

        let mut entry_offset = 16;
        if trun_flags & 0x0001 != 0 {
            entry_offset += 4;
        }
        if trun_flags & 0x0004 != 0 {
            entry_offset += 4;
        }

        let mut per_sample_size = 0usize;
        if trun_flags & 0x0100 != 0 {
            per_sample_size += 4;
        }
        if trun_flags & 0x0200 != 0 {
            per_sample_size += 4;
        }
        if trun_flags & 0x0400 != 0 {
            per_sample_size += 4;
        }
        if trun_flags & 0x0800 != 0 {
            per_sample_size += 4;
        }

        let has_duration = trun_flags & 0x0100 != 0;
        let mut total_trun_duration: u64 = 0;
        let mut sample_durations = Vec::new();
        let mut off = trun_pos + entry_offset;

        for _ in 0..sample_count {
            if off + per_sample_size > traf.len() {
                break;
            }
            if has_duration {
                let dur = u32_be(traf, off);
                total_trun_duration += duration_as_u64(dur);
                if sample_durations.len() < 5 {
                    sample_durations.push(dur);
                }
            }
            off += per_sample_size;
        }

        SegmentTiming {
            frag_seq,
            base_decode_time,
            tfdt_version,
            total_trun_duration,
            sample_count,
            sample_durations,
        }
    }

    fn duration_as_u64(dur: u32) -> u64 {
        dur as u64
    }

    #[test]
    fn test_dts_continuity_across_segments() {
        let _ = ffmpeg::init();

        let mut asset_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        asset_path.push("testvideos");
        asset_path.push("bun33s.mp4");

        if !asset_path.exists() {
            eprintln!("⚠  Test asset not found at {:?} — skipping", asset_path);
            return;
        }

        let media = crate::media::StreamIndex::open(&asset_path, None).expect("Parsing failed");
        let stream_id = media.stream_id.clone();
        let index = &media;

        if index.segments.len() < 2 {
            eprintln!(
                "⚠  Need at least 2 segments; got {} — skipping",
                index.segments.len()
            );
            return;
        }

        let init_bytes =
            generate_video_init_segment(index).expect("Failed to generate init segment");
        let timescales = parse_mdhd_timescales(&init_bytes);

        let seg0_bytes =
            generate_video_segment(index, 0, 0, &asset_path).expect("Failed to generate segment 0");
        let seg1_bytes =
            generate_video_segment(index, 0, 1, &asset_path).expect("Failed to generate segment 1");

        let seg0 = parse_media_segment(&seg0_bytes);
        let seg1 = parse_media_segment(&seg1_bytes);

        let trex_default_dur = parse_trex_default_duration(&init_bytes);
        println!("  trex.default_sample_duration: {}", trex_default_dur);

        let seg0_effective_dur = if seg0.total_trun_duration > 0 {
            seg0.total_trun_duration
        } else {
            seg0.sample_count as u64 * trex_default_dur as u64
        };

        let expected_tfdt_seg1 = seg0.base_decode_time + seg0_effective_dur;
        let actual_tfdt_seg1 = seg1.base_decode_time;

        assert!(seg1.base_decode_time > seg0.base_decode_time);
        assert_eq!(actual_tfdt_seg1, expected_tfdt_seg1);
    }

    #[test]
    fn test_audio_tfdt_timescale() {
        let _ = ffmpeg::init();

        let mut asset_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        asset_path.push("testvideos");
        asset_path.push("bun33s.mp4");

        if !asset_path.exists() {
            eprintln!("⚠  Test asset not found at {:?} — skipping", asset_path);
            return;
        }

        let media = crate::media::StreamIndex::open(&asset_path, None).expect("Parsing failed");
        let stream_id = media.stream_id.clone();
        let index = &media;
        let audio_stream = index.audio_streams.first().expect("No audio stream found");

        let seg0_bytes = generate_audio_segment(index, 1, 0, &asset_path, None)
            .expect("Failed to generate audio seg 0");
        let seg0 = parse_media_segment(&seg0_bytes);
        assert_eq!(seg0.base_decode_time, 0);

        let seg1_bytes = generate_audio_segment(index, 1, 1, &asset_path, None)
            .expect("Failed to generate audio seg 1");
        let seg1 = parse_media_segment(&seg1_bytes);

        // Basic check that it's increasing
        assert!(seg1.base_decode_time > 0);
    }
}
