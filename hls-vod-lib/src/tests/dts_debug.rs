//! DTS diagnostic integration test

#[cfg(test)]
#[allow(dead_code, unused_variables, unused_imports, unused_mut)]
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

    /// Dump interleaved segments for alex.mp4 (video=0, audio=3, non-transcoded) to /tmp.
    /// Run with: cargo test -p hls-vod-lib -- --test-threads=1 dump_alex_interleaved 2>&1
    #[test]
    fn dump_alex_interleaved() {
        let _ = ffmpeg::init();
        let asset_path = std::path::PathBuf::from("/Users/mikevs/Devel/hls-server/tests/assets/alex.mp4");
        if !asset_path.exists() {
            eprintln!("⚠  alex.mp4 not found — skipping");
            return;
        }

        let media = crate::media::StreamIndex::open(&asset_path, None).expect("scan failed");
        let video_idx = 0usize;
        let audio_idx = 3usize;

        let audio_sample_rate = media.audio_streams
            .iter()
            .find(|a| a.stream_index == audio_idx)
            .map(|a| a.sample_rate as u64)
            .unwrap_or(48000);

        let init = crate::segment::generator::generate_interleaved_init_segment(
            &media, video_idx, audio_idx, None,
        ).expect("init failed");
        std::fs::write("/tmp/alex_av_init.mp4", &init).unwrap();
        eprintln!("init: {} bytes", init.len());

        // Generate segments 0 and 1 and measure cross-segment audio continuity
        for seg_idx in 0..media.segments.len().min(3) {
            let seg = crate::segment::generator::generate_interleaved_segment(
                &media, video_idx, audio_idx, &media.segments[seg_idx], &asset_path, None,
            ).expect("seg failed");
            std::fs::write(format!("/tmp/alex_av{}.m4s", seg_idx), &seg).unwrap();
            eprintln!("seg{}: {} bytes", seg_idx, seg.len());

            // Parse all moofs in this segment
            let mut all_moofs = parse_all_moofs(&seg);
            eprintln!("  {} fragment(s)", all_moofs.len());
            for (fi, (v_tfdt, a_tfdt, v_cnt, a_cnt, v_dur, a_def_dur)) in all_moofs.iter().enumerate() {
                let v_end = v_tfdt + v_dur;
                let a_end = a_tfdt + a_cnt * a_def_dur;
                eprintln!("  frag{}: video tfdt={} end={} ({:.3}s-{:.3}s)  audio tfdt={} end={} ({:.3}s-{:.3}s) cnt={}",
                    fi,
                    v_tfdt, v_end, *v_tfdt as f64/90000.0, v_end as f64/90000.0,
                    a_tfdt, a_end, *a_tfdt as f64/audio_sample_rate as f64, a_end as f64/audio_sample_rate as f64,
                    a_cnt,
                );
            }
            // Total audio end across all frags
            if let Some(last) = all_moofs.last() {
                let (_, a_tfdt, _, a_cnt, _, a_def_dur) = last;
                let total_a_end = a_tfdt + a_cnt * a_def_dur;
                eprintln!("  seg{} audio total end sample={} ({:.3}s)", seg_idx, total_a_end, total_a_end as f64/audio_sample_rate as f64);
            }
        }
    }

    /// Parse all moof boxes in a segment: (video_tfdt, audio_tfdt, video_count, audio_count, video_total_dur, audio_default_dur)
    fn parse_all_moofs(data: &[u8]) -> Vec<(u64, u64, u64, u64, u64, u64)> {
        let mut results = Vec::new();
        let mut pos = 0;
        while pos + 8 <= data.len() {
            let size = u32_be(data, pos) as usize;
            let btype = &data[pos + 4..pos + 8];
            if size < 8 || pos + size > data.len() {
                break;
            }
            if btype == b"moof" {
                let moof = &data[pos..pos + size];
                let mut v_tfdt = 0u64;
                let mut a_tfdt = 0u64;
                let mut v_cnt = 0u64;
                let mut a_cnt = 0u64;
                let mut v_dur = 0u64;
                let mut a_def_dur = 0u64;

                let mut tpos = 8usize;
                while tpos + 8 <= moof.len() {
                    let tsz = u32_be(moof, tpos) as usize;
                    if tsz < 8 || tpos + tsz > moof.len() { break; }
                    if &moof[tpos+4..tpos+8] == b"traf" {
                        let traf = &moof[tpos..tpos+tsz];
                        // tfhd → track_id and default_sample_duration
                        let tfhd_pos = find_box_recursive(traf, b"tfhd").unwrap_or(0);
                        let track_id = u32_be(traf, tfhd_pos + 12);
                        // flags
                        let tfhd_flags = (u32_be(traf, tfhd_pos + 8)) & 0xFFFFFF;
                        let mut fp = tfhd_pos + 16; // after version+flags+track_id
                        if tfhd_flags & 0x01 != 0 { fp += 8; }
                        if tfhd_flags & 0x02 != 0 { fp += 4; }
                        let def_dur = if tfhd_flags & 0x08 != 0 { u32_be(traf, fp) as u64 } else { 1024 };

                        let tfdt_pos = find_box_recursive(traf, b"tfdt").unwrap_or(0);
                        let tfdt_ver = traf[tfdt_pos + 8];
                        let tfdt = if tfdt_ver == 1 { u64_be(traf, tfdt_pos + 12) } else { u32_be(traf, tfdt_pos + 12) as u64 };

                        let (total_dur, count) = sum_trun_info(traf);

                        if track_id == 1 {
                            v_tfdt = tfdt;
                            v_cnt = count as u64;
                            v_dur = total_dur;
                        } else {
                            a_tfdt = tfdt;
                            a_cnt = count as u64;
                            a_def_dur = def_dur;
                        }
                    }
                    tpos += tsz;
                }
                results.push((v_tfdt, a_tfdt, v_cnt, a_cnt, v_dur, a_def_dur));
            }
            pos += size;
        }
        results
    }

    #[test]
    fn test_interleaved_tfdt_continuity() {
        let _ = ffmpeg::init();

        let mut asset_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        asset_path.push("../tests/assets/bun33s.mp4");

        if !asset_path.exists() {
            eprintln!("⚠  Test asset not found at {:?} — skipping", asset_path);
            return;
        }

        let media = crate::media::StreamIndex::open(&asset_path, None).expect("Parsing failed");

        if media.video_streams.is_empty() || media.audio_streams.is_empty() {
            eprintln!("⚠  Need both video and audio streams — skipping");
            return;
        }
        if media.segments.len() < 2 {
            eprintln!(
                "⚠  Need at least 2 segments; got {} — skipping",
                media.segments.len()
            );
            return;
        }

        let video_idx = media.video_streams[0].stream_index;
        let audio_idx = media.audio_streams[0].stream_index;
        let audio_sample_rate = media.audio_streams[0].sample_rate as u64;

        let seg0_bytes = crate::segment::generator::generate_interleaved_segment(
            &media,
            video_idx,
            audio_idx,
            &media.segments[0],
            &asset_path,
            None,
        )
        .expect("Failed to generate interleaved segment 0");

        let seg1_bytes = crate::segment::generator::generate_interleaved_segment(
            &media,
            video_idx,
            audio_idx,
            &media.segments[1],
            &asset_path,
            None,
        )
        .expect("Failed to generate interleaved segment 1");

        // Also generate the init segment so we can read trex default sample durations.
        // These are needed to compute effective segment duration when trun omits per-sample
        // durations (which happens with delay_moov=false for constant-frame-rate video).
        let init_bytes = crate::segment::generator::generate_interleaved_init_segment(
            &media,
            video_idx,
            audio_idx,
            None,
        )
        .expect("Failed to generate interleaved init segment");

        let (v_trex_dur, a_trex_dur) = parse_interleaved_trex_defaults(&init_bytes);
        println!("  trex video default_dur={} audio default_dur={}", v_trex_dur, a_trex_dur);

        let (seg0_v_tfdt, seg0_a_tfdt, seg0_v_trun, seg0_a_trun, seg0_v_count, seg0_a_count) =
            parse_interleaved_segment(&seg0_bytes);
        let (seg1_v_tfdt, seg1_a_tfdt, _seg1_v_trun, _seg1_a_trun, _seg1_v_count, _seg1_a_count) =
            parse_interleaved_segment(&seg1_bytes);

        // Effective duration = trun total when per-sample durations are present,
        // otherwise fall back to sample_count × trex.default_sample_duration.
        let seg0_v_eff_dur = if seg0_v_trun > 0 {
            seg0_v_trun
        } else {
            seg0_v_count as u64 * v_trex_dur as u64
        };
        let seg0_a_eff_dur = if seg0_a_trun > 0 {
            seg0_a_trun
        } else {
            seg0_a_count as u64 * a_trex_dur as u64
        };

        println!(
            "seg0 video  tfdt={} eff_dur={} (trun={} count={} trex={})",
            seg0_v_tfdt, seg0_v_eff_dur, seg0_v_trun, seg0_v_count, v_trex_dur
        );
        println!(
            "seg0 audio  tfdt={} eff_dur={} (trun={} count={} trex={})",
            seg0_a_tfdt, seg0_a_eff_dur, seg0_a_trun, seg0_a_count, a_trex_dur
        );
        println!("seg1 video  tfdt={}", seg1_v_tfdt);
        println!("seg1 audio  tfdt={}", seg1_a_tfdt);

        // Video tfdt must be strictly increasing
        assert!(
            seg1_v_tfdt > seg0_v_tfdt,
            "Video tfdt did not increase: seg0={} seg1={}",
            seg0_v_tfdt,
            seg1_v_tfdt
        );

        // Video continuity: seg1.tfdt == seg0.tfdt + seg0.effective_duration
        assert_eq!(
            seg1_v_tfdt,
            seg0_v_tfdt + seg0_v_eff_dur,
            "Video tfdt discontinuity: expected {} got {}",
            seg0_v_tfdt + seg0_v_eff_dur,
            seg1_v_tfdt
        );

        // Audio tfdt must be strictly increasing
        assert!(
            seg1_a_tfdt > seg0_a_tfdt,
            "Audio tfdt did not increase: seg0={} seg1={}",
            seg0_a_tfdt,
            seg1_a_tfdt
        );

        // Audio continuity: seg1.tfdt == seg0.tfdt + seg0.effective_duration
        assert_eq!(
            seg1_a_tfdt,
            seg0_a_tfdt + seg0_a_eff_dur,
            "Audio tfdt discontinuity: expected {} got {}",
            seg0_a_tfdt + seg0_a_eff_dur,
            seg1_a_tfdt
        );

        // Cross-track A/V alignment: both tracks should start within 30ms of each other
        let video_timescale = 90_000u64;
        let seg1_v_sec = seg1_v_tfdt as f64 / video_timescale as f64;
        let seg1_a_sec = seg1_a_tfdt as f64 / audio_sample_rate as f64;
        let av_delta_ms = (seg1_v_sec - seg1_a_sec).abs() * 1000.0;
        assert!(
            av_delta_ms < 30.0,
            "A/V start misalignment in seg1: video={:.3}s audio={:.3}s delta={:.1}ms",
            seg1_v_sec,
            seg1_a_sec,
            av_delta_ms
        );
    }

    /// Parse per-track tfdts, trun total durations, and sample counts from an
    /// interleaved (multi-traf) segment.
    /// Returns (video_tfdt, audio_tfdt, video_trun_dur, audio_trun_dur, video_count, audio_count).
    /// Assumes track_id==1 is video, track_id==2 is audio.
    fn parse_interleaved_segment(data: &[u8]) -> (u64, u64, u64, u64, u32, u32) {
        let data = if data.len() >= 8 && &data[4..8] == b"styp" {
            let styp_size = u32_be(data, 0) as usize;
            &data[styp_size..]
        } else {
            data
        };

        let moof_pos = find_box(data, b"moof").expect("moof not found in interleaved segment");
        let moof_size = u32_be(data, moof_pos) as usize;
        let moof = &data[moof_pos..moof_pos + moof_size];

        let mut video_tfdt = 0u64;
        let mut audio_tfdt = 0u64;
        let mut video_dur = 0u64;
        let mut audio_dur = 0u64;
        let mut video_count = 0u32;
        let mut audio_count = 0u32;

        let mut pos = 8usize; // skip moof size+type header
        while pos + 8 <= moof.len() {
            let size = u32_be(moof, pos) as usize;
            if size < 8 || pos + size > moof.len() {
                break;
            }
            if &moof[pos + 4..pos + 8] == b"traf" {
                let traf = &moof[pos..pos + size];

                let tfhd_pos = find_box_recursive(traf, b"tfhd").expect("tfhd in traf");
                let track_id = u32_be(traf, tfhd_pos + 12);

                let tfdt_pos = find_box_recursive(traf, b"tfdt").expect("tfdt in traf");
                let tfdt_version = traf[tfdt_pos + 8];
                let base_dt = if tfdt_version == 1 {
                    u64_be(traf, tfdt_pos + 12)
                } else {
                    u32_be(traf, tfdt_pos + 12) as u64
                };

                let (total_dur, count) = sum_trun_info(traf);

                if track_id == 1 {
                    video_tfdt = base_dt;
                    video_dur = total_dur;
                    video_count = count;
                } else {
                    audio_tfdt = base_dt;
                    audio_dur = total_dur;
                    audio_count = count;
                }
            }
            pos += size;
        }

        (video_tfdt, audio_tfdt, video_dur, audio_dur, video_count, audio_count)
    }

    /// Returns (total_duration_from_trun, sample_count).
    /// total_duration is 0 when per-sample durations are absent (use trex default instead).
    fn sum_trun_info(traf: &[u8]) -> (u64, u32) {
        let trun_pos = match find_box_recursive(traf, b"trun") {
            Some(p) => p,
            None => return (0, 0),
        };
        let trun_flags = u32_be(traf, trun_pos + 8) & 0x00FF_FFFF;
        let sample_count = u32_be(traf, trun_pos + 12);

        let mut entry_offset = 16usize;
        if trun_flags & 0x0001 != 0 {
            entry_offset += 4;
        }
        if trun_flags & 0x0004 != 0 {
            entry_offset += 4;
        }

        let has_duration = trun_flags & 0x0100 != 0;
        if !has_duration {
            return (0, sample_count);
        }

        let mut per_sample_size = 4usize;
        if trun_flags & 0x0200 != 0 {
            per_sample_size += 4;
        }
        if trun_flags & 0x0400 != 0 {
            per_sample_size += 4;
        }
        if trun_flags & 0x0800 != 0 {
            per_sample_size += 4;
        }

        let mut total = 0u64;
        let mut off = trun_pos + entry_offset;
        for _ in 0..sample_count {
            if off + per_sample_size > traf.len() {
                break;
            }
            total += u32_be(traf, off) as u64;
            off += per_sample_size;
        }
        (total, sample_count)
    }

    /// Parse trex default_sample_duration for the video (track_id=1) and audio (track_id=2)
    /// tracks from an interleaved init segment.
    fn parse_interleaved_trex_defaults(init: &[u8]) -> (u32, u32) {
        let mut video_dur = 0u32;
        let mut audio_dur = 0u32;
        parse_trex_defaults_in(init, &mut video_dur, &mut audio_dur);
        (video_dur, audio_dur)
    }

    fn parse_trex_defaults_in(data: &[u8], video: &mut u32, audio: &mut u32) {
        let mut pos = 0;
        while pos + 8 <= data.len() {
            let size = u32_be(data, pos) as usize;
            if size < 8 || pos + size > data.len() {
                break;
            }
            let btype = &data[pos + 4..pos + 8];
            match btype {
                b"moov" | b"mvex" => {
                    parse_trex_defaults_in(&data[pos + 8..pos + size], video, audio);
                }
                b"trex" => {
                    if size >= 28 {
                        let track_id = u32_be(data, pos + 12);
                        let default_dur = u32_be(data, pos + 20);
                        if track_id == 1 {
                            *video = default_dur;
                        } else {
                            *audio = default_dur;
                        }
                    }
                }
                _ => {}
            }
            pos += size;
        }
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
