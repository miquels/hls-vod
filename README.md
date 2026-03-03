# HLS Transmuxer Suite

A collection of high-performance Rust tools and libraries designed for on-the-fly HLS transmuxing and audio transcoding.

## Why.

This project was born out of a specific need for a more efficient way to handle Jellyfin streaming. The standard pipeline of spawning external FFmpeg processes and writing segments to disk often results in high I/O overhead and sluggish seeking. 

This suite provides a native, in-memory alternative that significantly lowers server load and makes seeking feel instantaneous. While developed with Jellyfin in mind, the underlying library is modular enough for any Rust-based media project—perhaps even a future native Rust implementation of a media server.

---

## Primary Project: [Jellyfin Transmux Proxy](./jellyfin-transmux-proxy/README.md)

This is a specialized edge proxy that sits in front of your Jellyfin server. It intelligently intercepts playback requests and handles the media stream internally.

- **✅ No External FFmpeg**: Everything stays within the proxy process.
- **✅ No Disk Thrashing**: Segments are generated and served from memory.
- **✅ Instant Seek**: Experience the fastest seeking you've ever had in a web player.
- **✅ Easy Integration**: Works with existing Jellyfin setups by just changing a few settings.

👉 **[Read the Full Proxy README](./jellyfin-transmux-proxy/README.md)** for setup instructions and features.

---

## Components

### [hls-vod-lib](./hls-vod-lib/README.md)
The engine under the hood. A standalone Rust crate for demuxing source files and packaging them into HLS-compliant fMP4 fragments. 🦀
- **Audio Transcoding**: Supports AC-3 to AAC conversion on-the-fly.
- **A/V Interleaving**: Perfectly synced streams for modern browser compatibility.

### [hls-vod-server](./hls-vod-server/README.md)
A lightweight reference implementation.
- **Proof-of-Concept**: Demonstrates how to use the library in a simple Axum server environment.
- **Disclaimer**: For testing and reference only; use the Transmux Proxy for production.

---

## Getting Started

1. Clone the repository.
2. Build the workspace: `cargo build --release`
3. Configure your proxy using `jellyfix-transmux-proxy.toml`.
4. Update your Jellyfin user settings to optimize for transmuxing.
