//! FFmpeg module - provides wrappers and utilities for FFmpeg library access
//!
//! This module handles:
//! - FFmpeg initialization
//! - Input/output context management
//! - Custom AVIOContext for in-memory writing
//! - Timebase conversion and other utilities

pub mod helpers;
pub mod index;
pub mod io;
pub mod utils;

pub use ffmpeg_next as ffmpeg;
#[allow(unused_imports)]
pub use utils::*;

/// Initialize the FFmpeg library.
///
/// This should be called exactly once at application startup before any other
/// FFmpeg-related functions (like `parse_file` or `generate_segment`) are used.
/// Returns an error if the underlying C library fails to initialize context structures.
pub fn init() -> Result<(), crate::error::FfmpegError> {
    ffmpeg::init().map_err(|e| {
        crate::error::FfmpegError::InitFailed(format!("ffmpeg::init() failed: {}", e))
    })?;

    crate::lookahead::init_workers();

    tracing::info!("FFmpeg & Lookahead Threadpool initialized");

    Ok(())
}

/// Install a custom FFmpeg log callback that suppresses known-noisy messages.
///
/// When muxing HLS streams on the fly (especially using `empty_moov` without `delay_moov`
/// to reduce latency), FFmpeg emits many warnings that are expected side-effects of this
/// deliberate muxer configuration. This function filters them out so they don't pollute the application log.
///
/// **Safety & Ordering:** Must be called after `init()` and before any threading begins,
/// because altering the global log callback is not thread-safe.
pub fn install_log_filter() {
    // SAFETY: both functions modify global FFmpeg state and are safe to call
    // after `ffmpeg::init()`.  They are called exactly once at startup before
    // any threads begin generating segments.
    unsafe {
        ffmpeg_next::ffi::av_log_set_level(ffmpeg_next::ffi::AV_LOG_WARNING);

        #[cfg(all(feature = "compat-ffmpeg7", target_os = "linux"))]
        {
            // On Linux/FFmpeg 7, the va_list type decays to a pointer in C but is an array in Rust.
            // We use transmute to bridge the gap between our *mut c_void and the expected *mut __va_list_tag.
            let callback: unsafe extern "C" fn(
                *mut std::ffi::c_void,
                std::ffi::c_int,
                *const std::ffi::c_char,
                *mut std::ffi::c_void,
            ) = ffmpeg_log_callback;
            ffmpeg_next::ffi::av_log_set_callback(Some(std::mem::transmute(callback)));
        }

        #[cfg(any(not(feature = "compat-ffmpeg7"), not(target_os = "linux")))]
        ffmpeg_next::ffi::av_log_set_callback(Some(ffmpeg_log_callback));
    }
}

/// Messages that are expected side-effects of our muxer design and should be suppressed.
const SUPPRESSED_MESSAGES: &[&str] = &[
    "No meaningful edit list will be written when using empty_moov without delay_moov",
    "starts with a nonzero dts",
    "Set the delay_moov flag to handle this case",
    "Could not update timestamps for skipped samples",
    "Could not update timestamps for discarded samples",
    "Error parsing Opus packet header",
];

unsafe extern "C" fn ffmpeg_log_callback(
    avcl: *mut std::ffi::c_void,
    level: std::ffi::c_int,
    fmt: *const std::ffi::c_char,
    #[cfg(all(feature = "compat-ffmpeg7", target_os = "linux"))] vl: *mut std::ffi::c_void,
    #[cfg(any(not(feature = "compat-ffmpeg7"), not(target_os = "linux")))]
    vl: ffmpeg_next::ffi::va_list,
) {
    use std::ffi::CStr;

    // Respect the configured log level
    if level > unsafe { ffmpeg_next::ffi::av_log_get_level() } {
        return;
    }

    // Format the message using FFmpeg's own vsnprintf helper
    let mut buf = [0i8; 1024];
    let mut print_prefix: std::ffi::c_int = 1;
    ffmpeg_next::ffi::av_log_format_line(
        avcl,
        level,
        fmt,
        vl as _,
        buf.as_mut_ptr(),
        buf.len() as std::ffi::c_int,
        &mut print_prefix,
    );

    let msg = CStr::from_ptr(buf.as_ptr()).to_string_lossy();

    // Drop messages that are known, benign side-effects of our design
    for suppressed in SUPPRESSED_MESSAGES {
        if msg.contains(suppressed) {
            return;
        }
    }

    eprint!("{}", msg);
}

/// Get the version information of the linked FFmpeg libraries.
/// Useful for debugging and reporting environment consistency.
pub fn version_info() -> String {
    // Return a simple version string since the API changed in FFmpeg 8.0
    "FFmpeg 8.0+".to_string()
}
