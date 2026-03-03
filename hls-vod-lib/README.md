# hls-vod-lib 🦀

A high-performance Rust library for on-the-fly HLS (HTTP Live Streaming) transmuxing and transcoding.

## 🌟 Overview

`hls-vod-lib` is a specialized media processing engine that generates HLS master playlists, variant playlists, and fMP4 media segments directly from source files (MKV, MP4, WebM, etc.) in memory. It eliminates the traditional need to pre-segment files or use external CLI tools like `ffmpeg` to write chunks to disk.

While originally designed as the core engine for `jellyfin-transmux-proxy`, this library is built to be modular and suitable for any standalone Rust project that requires dynamic HLS generation.

## ✨ Features

- **🚀 In-Memory Transmuxing**: Converts container formats to HLS-compliant fMP4 segments on-the-fly without temporary disk storage.
- **🔊 Audio Transcoding**: Built-in support for transcoding audio streams to AAC (using `ffmpeg-next` / C-API) when the source codec is incompatible with HLS/Safari.
- **🎬 Interleaved A/V Streams**: Supports generating single fMP4 segments containing both audio and video tracks, perfectly interleaved by DTS.
- **📄 Dynamic Playlists**: Generates HLS Master and Media playlists based on source file probing and requested constraints.
- **⚖️ Seeking Support**: Provides frame-accurate segment generation, enabling ultra-low latency seeking in players.
- **🛠️ FFmpeg Integration**: Deep integration with the FFmpeg libraries via `ffmpeg-next` for robust demuxing, decoding, and encoding.

## 🎯 Use Cases

- **Media Proxies**: Build lightweight edge servers that "trick" clients into seeing optimized streams (like `jellyfin-transmux-proxy`).
- **Custom Streaming Servers**: Integrate directly into Rust-based media servers or content delivery platforms.
- **Jellyfin Integration**: Could potentially serve as the foundation for a more efficient, native media delivery branch in Rust-compatible Jellyfin forks or associated tools.

## 💻 Technical Details

The library focuses on:
- **Zero-Copy philosophy**: Minimizing memory allocations and avoiding disk I/O for generated segments.
- **Async Efficiency**: Designed to work seamlessly within the `tokio` ecosystem.
- **Precision Timestamps**: Careful management of PTS/DTS and encoder delays to ensure smooth playback across segment boundaries.

## 🚧 Status

Currently supports **transmuxing** for both Video and Audio, and **transcoding** for Audio. Video transcoding is a planned future addition.
