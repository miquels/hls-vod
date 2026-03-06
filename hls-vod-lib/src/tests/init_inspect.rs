use std::fs::File;
use std::io::Read;

#[test]
fn inspect_source_boxes_video_alex() {
    let video_path = "/Users/mikevs/Devel/hls-server/video-alex.mp4";
    let Ok(mut file) = File::open(video_path) else {
        println!("Skipping debug test - video-alex.mp4 not found");
        return;
    };

    // Read first 15MB which contains the moov
    let mut buffer = vec![0u8; 15_000_000];
    if file.read_exact(&mut buffer).is_err() {
        return;
    }

    println!("--- Source File Boxes (first 15MB) ---");
    for box_name in &[b"mvhd", b"tkhd", b"mdhd", b"hdlr", b"elst", b"edts"] {
        let mut start = 0;
        while let Some(pos) = buffer[start..].windows(4).position(|w| w == *box_name) {
            let actual_pos = start + pos;
            println!(
                "Found {:?} at offset {}",
                std::str::from_utf8(*box_name).unwrap(),
                actual_pos
            );

            // Print some bytes of the box
            let end = (actual_pos + 64).min(buffer.len());
            println!("  Data: {:02x?}", &buffer[actual_pos..end]);

            // For elst, decode entries
            if box_name == &b"elst" {
                decode_elst(&buffer[actual_pos..end]);
            }

            start = actual_pos + 4;
        }
    }
}

fn decode_elst(data: &[u8]) {
    if data.len() < 16 {
        return;
    }
    let version = data[8];
    let count = u32::from_be_bytes(data[12..16].try_into().unwrap());
    println!("  -> elst Version {}, Count {}", version, count);

    let mut pos = 16;
    for i in 0..count {
        if pos + 12 > data.len() {
            break;
        }
        if version == 0 {
            let dur = u32::from_be_bytes(data[pos..pos + 4].try_into().unwrap());
            let media_time = i32::from_be_bytes(data[pos + 4..pos + 8].try_into().unwrap());
            let rate = u32::from_be_bytes(data[pos + 8..pos + 12].try_into().unwrap());
            println!(
                "     Entry {}: dur={}, media_time={}, rate={}",
                i, dur, media_time, rate
            );
            pos += 12;
        } else {
            // Version 1 (64-bit)
            if pos + 20 > data.len() {
                break;
            }
            let dur = u64::from_be_bytes(data[pos..pos + 8].try_into().unwrap());
            let media_time = i64::from_be_bytes(data[pos + 8..pos + 16].try_into().unwrap());
            let rate = u32::from_be_bytes(data[pos + 16..pos + 20].try_into().unwrap());
            println!(
                "     Entry {}: dur={}, media_time={}, rate={}",
                i, dur, media_time, rate
            );
            pos += 20;
        }
    }
}
