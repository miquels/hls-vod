use crate::media::StreamIndex;
use std::path::PathBuf;

#[test]
fn debug_video_alex_pts_with_audio() {
    let video_path = PathBuf::from("/Users/mikevs/Devel/hls-server/video-alex.mp4");
    if !video_path.exists() {
        println!("File not found: {:?}", video_path);
        return;
    }

    let index = StreamIndex::open(&video_path, None).expect("Failed to scan file");
    let video_idx = index.primary_video().unwrap().stream_index;

    println!("Generating Video Segment 0...");
    let data = crate::segment::generator::generate_video_segment(&index, video_idx, 0, &video_path)
        .expect("Failed to generate segment");

    if let Some(pos) = data.windows(4).position(|w| w == b"tfdt") {
        let tfdt_box = &data[pos - 4..pos + 24];
        println!("Video Segment 0 tfdt box: {:02x?}", tfdt_box);
    }

    println!("Generating Audio Segment 0 (track 1)...");
    let audio_data =
        crate::segment::generator::generate_audio_segment(&index, 1, 0, &video_path, None)
            .expect("Failed to generate audio segment");

    if let Some(pos) = audio_data.windows(4).position(|w| w == b"tfdt") {
        let tfdt_box = &audio_data[pos - 4..pos + 24];
        println!("Audio Segment 0 tfdt box: {:02x?}", tfdt_box);
    }
}
