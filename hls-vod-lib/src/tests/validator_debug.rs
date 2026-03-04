#[cfg(test)]
mod tests {
    use crate::ffmpeg_utils::ffmpeg;
    use crate::media::{SegmentInfo, StreamIndex};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicI64, AtomicU64};
    use std::sync::Arc;

    #[test]
    fn test_reproduce_mdat_mismatch() {
        let _ = ffmpeg::init();

        let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        path.push("testvideos");
        path.push("bun33s.mp4");

        if !path.exists() {
            eprintln!("Test video not found, skipping");
            return;
        }

        let mut index = StreamIndex {
            stream_id: "test-id".to_string(),
            source_path: path.clone(),
            duration_secs: 60.0,
            video_timebase: crate::ffmpeg_utils::ffmpeg::Rational::new(1, 90000),
            video_streams: Vec::new(),
            audio_streams: Vec::new(),
            subtitle_streams: Vec::new(),
            segments: Vec::new(),
            indexed_at: std::time::SystemTime::now(),
            last_accessed: AtomicU64::new(0),
            segment_first_pts: Arc::new(Vec::new()),
            cached_context: None,
            cache_enabled: true,
            last_requested_segment: AtomicI64::new(-1),
            lookahead_queue: std::sync::Mutex::new(std::collections::VecDeque::new()),
        };

        let segment = SegmentInfo {
            sequence: 1,
            start_pts: 360000,
            end_pts: 720000, // 8 seconds
            duration_secs: 4.0,
            is_keyframe: true,
            video_byte_offset: 0,
        };
        index.segments.push(segment);

        // Initialize segment_first_pts
        let n = index.segments.len();
        let v: Vec<AtomicI64> = (0..n).map(|_| AtomicI64::new(i64::MIN)).collect();
        index.segment_first_pts = Arc::new(v);

        let bytes = crate::segment::generator::generate_video_segment(&index, 0, 1, &path).unwrap();
        let data = bytes.as_ref();

        // Parse moof and trun
        use crate::segment::muxer::find_box;
        let moof_pos = find_box(data, b"moof").expect("moof not found");
        let moof_size = u32::from_be_bytes([
            data[moof_pos],
            data[moof_pos + 1],
            data[moof_pos + 2],
            data[moof_pos + 3],
        ]) as usize;
        let moof_data = &data[moof_pos..moof_pos + moof_size];

        let traf_pos = find_box(&moof_data[8..], b"traf").expect("traf not found") + 8;
        let traf_size = u32::from_be_bytes([
            moof_data[traf_pos],
            moof_data[traf_pos + 1],
            moof_data[traf_pos + 2],
            moof_data[traf_pos + 3],
        ]) as usize;
        let traf_data = &moof_data[traf_pos..traf_pos + traf_size];

        let trun_pos = find_box(&traf_data[8..], b"trun").expect("trun not found") + 8;
        let trun_flags = u32::from_be_bytes([
            traf_data[trun_pos + 8],
            traf_data[trun_pos + 9],
            traf_data[trun_pos + 10],
            traf_data[trun_pos + 11],
        ]) & 0x00FFFFFF;

        if trun_flags & 0x01 != 0 {
            let data_offset = i32::from_be_bytes([
                traf_data[trun_pos + 16],
                traf_data[trun_pos + 17],
                traf_data[trun_pos + 18],
                traf_data[trun_pos + 19],
            ]);
            println!("trun DataOffset: {}", data_offset);
        }

        // Find tfdt
        if let Some(tfdt_offset) = find_box(&traf_data[8..], b"tfdt") {
            let tfdt_pos = tfdt_offset + 8;
            let version = traf_data[tfdt_pos + 8];
            let base_time = if version == 1 {
                u64::from_be_bytes(traf_data[tfdt_pos + 12..tfdt_pos + 20].try_into().unwrap())
            } else {
                u32::from_be_bytes(traf_data[tfdt_pos + 12..tfdt_pos + 16].try_into().unwrap())
                    as u64
            };
            println!("tfdt baseMediaDecodeTime: {}", base_time);
        }

        let sample_count = u32::from_be_bytes([
            traf_data[trun_pos + 12],
            traf_data[trun_pos + 13],
            traf_data[trun_pos + 14],
            traf_data[trun_pos + 15],
        ]) as usize;

        let mut total_sample_size: u64 = 0;
        let mut offset = 16;
        if trun_flags & 0x01 != 0 {
            offset += 4;
        } // data_offset
        if trun_flags & 0x04 != 0 {
            offset += 4;
        } // first_sample_flags

        for _ in 0..sample_count {
            if trun_flags & 0x100 != 0 {
                offset += 4;
            } // duration
            if trun_flags & 0x200 != 0 {
                let size = u32::from_be_bytes([
                    traf_data[trun_pos + offset],
                    traf_data[trun_pos + offset + 1],
                    traf_data[trun_pos + offset + 2],
                    traf_data[trun_pos + offset + 3],
                ]);
                total_sample_size += size as u64;
                offset += 4;
            }
            if trun_flags & 0x400 != 0 {
                offset += 4;
            } // flags
            if trun_flags & 0x800 != 0 {
                offset += 4;
            } // composition offset
        }

        println!("trun claimed total sample size: {}", total_sample_size);

        // Find mdat
        let mdat_pos = find_box(data, b"mdat").expect("mdat not found");
        let mdat_size = u32::from_be_bytes([
            data[mdat_pos],
            data[mdat_pos + 1],
            data[mdat_pos + 2],
            data[mdat_pos + 3],
        ]) as u64;
        let actual_data_size = mdat_size - 8;
        println!("mdat actual data size: {}", actual_data_size);

        assert_eq!(
            total_sample_size, actual_data_size,
            "Mismatch between trun sample sizes and mdat data size!"
        );
    }
}
