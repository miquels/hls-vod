//! ISOBMFF (MP4) box parsing and manipulation utilities.
//! Centralizes boilerplate for traversing MP4 structures in memory.

/// Walk all top-level boxes in a buffer, and recursively traverse specified container boxes.
/// `callback` is invoked for EVERY box in pre-order traversal.
/// The callback signature is `|box_type: &[u8; 4], payload: &[u8]|`.
/// Mutable version of `walk_boxes`.
/// `callback` is invoked for EVERY box in pre-order traversal, with a mutable payload slice.
pub fn walk_boxes_mut<F>(data: &mut [u8], containers: &[&[u8; 4]], callback: &mut F)
where
    F: FnMut(&[u8; 4], &mut [u8]),
{
    let mut pos = 0;
    let len = data.len();
    while pos + 8 <= len {
        let size =
            u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as usize;
        if size < 8 || pos + size > len {
            break;
        }
        let btype: [u8; 4] = data[pos + 4..pos + 8].try_into().unwrap();

        let payload = &mut data[pos + 8..pos + size];
        callback(&btype, payload);

        if containers.contains(&&btype) {
            walk_boxes_mut(payload, containers, callback);
        }

        pos += size;
    }
}

/// Fix default_sample_duration in trex boxes
/// FFmpeg with stream copy sets duration to 1, but players need reasonable values
pub fn fix_trex_durations(data: &mut Vec<u8>, duration: u32) {
    walk_boxes_mut(data, &[b"moov", b"mvex"], &mut |btype, payload| {
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
pub fn patch_tfdts(media_data: &mut Vec<u8>, target_time: u64, start_frag_seq: u32) {
    let mut tfdt_delta: Option<i64> = None;
    let mut frag_count = 0;

    walk_boxes_mut(
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
pub fn patch_tfdts_per_track(
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

    walk_boxes_mut(
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
