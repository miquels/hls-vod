//! Integration testing module
//!
//! End-to-end tests for the HLS streaming server:
//! - Stream creation and indexing
//! - Playlist generation and validation
//! - Segment generation
//! - Audio track switching
//! - Subtitle synchronization
//! - Performance benchmarks

pub mod dts_debug;
pub mod e2e;
pub mod fixtures;
pub mod init_inspect;
pub mod playlist_dump;
pub mod pts_debug;
pub mod test_audio_bug;
pub mod test_context_reuse;
pub mod test_send;
pub mod validation;
pub mod validator_debug;
pub mod dump_test;
