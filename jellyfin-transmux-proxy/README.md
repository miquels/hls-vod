# 🚀 Jellyfin Transmux Proxy

A high-performance, specialized edge proxy for Jellyfin that replaces the heavy "FFmpeg-to-disk" pipeline with a lightning-fast, in-memory transmuxing engine. ⚡️

## 🎯 Why this exists

Standard Jellyfin handles HLS by spawning external `ffmpeg` processes that write media segments to your disk before serving them to the client. While robust, this introduces significant I/O overhead, startup latency, and a "choppy" seeking experience. 

The **Jellyfin Transmux Proxy** changes the game by intercepting these requests and handling the heavy lifting natively.

### 🌟 Advantages over standard Jellyfin
- **Zero-Copy Transmuxing**: Media is read from disk and transmuxed to fMP4 in-memory. No more temporary `.ts` or `.m4s` files cluttering your drive. 📂
- **Lower CPU & I/O Overhead**: By eliminating the need to spawn and manage external processes for every stream, your server's load remains low even with multiple active viewers. 📉
- **Instant Seeking**: Because the engine can calculate and generate any segment on-demand, seeking is nearly instantaneous. No more waiting for the "buffer" to catch up as FFmpeg restarts. ⏩

### 🛠️ Advantages over Nginx
While Nginx is a world-class reverse proxy, it is "dumb" regarding media logic. It simply passes bytes along.
- **Intelligent Interception**: This proxy understands Jellyfin's `PlaybackInfo` API. It modifies the negotiation in real-time to "trick" the client into using our optimized HLS engine. 🧠
- **On-the-fly Audio Transcoding**: Unlike a passive proxy, this server can decode and re-encode audio (e.g., AC-3 to AAC) during the transmuxing process if the client requires it. 🔊
- **Internal Media Engine**: It isn't just a router; it's a specialized media server written in Rust that speaks the same language as your media files. 🏗️

## ✨ Features

- **🌐 HTTPS Edge Support**: Act as a secure frontend with native TLS (Rustls) support. No need for an extra layer if you don't want one.
- **🎥 Native FFmpeg Integration**: Uses the `ffmpeg-next` libraries to interface directly with the FFmpeg C API for maximum performance and compatibility.
- **🔄 Dual-Mode Operation**: Seamlessly proxies all standard Jellyfin traffic (API, Web UI, WebSockets) while surgically intercepting only the streaming components.
- **🍏 Safari Optimized**: Includes built-in overrides to handle Safari's specific HLS quirks and player constraints.

## 💻 Technical Snapshot

- **🦀 Language**: Core logic written in **Rust** for memory safety and zero-cost abstractions.
- **⏱️ Concurrency**: Built on the **Tokio** async runtime, capable of handling hundreds of concurrent connections with minimal footprint.
- **📦 Transmuxing**: Handles the transition from various containers to HLS-compliant fMP4 natively.
- **🎵 Transcoding**: Supported for **Audio** (Video transcoding is planned but not yet implemented).

## ⚙️ Configuration

The proxy is configured via `jellyfix-transmux-proxy.toml`. Simply point it at your upstream Jellyfin instance and your media library, and it handles the rest.

```toml
[server]
http_listen_port = 8064
https_listen_port = 443
tls_cert = "/etc/certs/fullchain.pem"
tls_key = "/etc/certs/privkey.pem"

[jellyfin]
jellyfin = "http://localhost:8096"
# mediaroot = "/"
```

## 🚧 Status

This project is in active development. It currently excels at **transmuxing** and **audio transcoding**. High-performance video transcoding is on the roadmap. 🗺️
