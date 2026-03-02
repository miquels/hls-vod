use crate::index::scanner::{scan_file_with_options, IndexOptions};
use crate::segment::generator::generate_audio_segment;
use std::path::PathBuf;

#[test]
fn test_audio_segment_3_bug() {
    crate::ffmpeg_utils::init().unwrap();
    let video_path = PathBuf::from("/Users/mikevs/Devel/hls-server/video-alex.mp4");
    if !video_path.exists() {
        return;
    }
    let options = IndexOptions {
        segment_duration_secs: 4.0,
        index_segments: true,
    };
    let index = scan_file_with_options(&video_path, &options).unwrap();
    println!("Audio streams: {:?}", index.audio_streams);

    // Test generating segment 0, track 3
    let res = generate_audio_segment(&index, 3, 0, &video_path, None);
    println!("Audio segment 3 result: {:?}", res.map(|b| b.len()));
}
