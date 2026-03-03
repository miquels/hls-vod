# Jellyfin HLS Proxy Status

## Milestone 1: Project Initialization & Basic Reverse Proxy
**Status**: Completed

**Summary:**
- Configured Cargo workspace and initialized the `jellyfin-hls-proxy` project (`Cargo.toml`).
- Added dependencies: `axum`, `tokio`, `clap`, and `reqwest`.
- Implemented `src/main.rs` with a basic CLI using `clap` (listening port and upstream URL).
- Set up an Axum server with a `fallback` route that transparently proxies all HTTP requests to the upstream Jellyfin backend using `reqwest`.
- Fixed `Cargo.toml` workspace missing member error and verified successful compilation (`cargo check`).

## Milestone 2: Intercepting PlaybackInfo Requests
**Status**: Completed

**Summary:**
- Switched the HTTP server implementation from `axum::serve` to `axum_server::bind` (v0.8.0).
- Handled the `/Items/:item_id/PlaybackInfo` route specifically to intercept client device capabilities.
- Deserialized the incoming proxy request using `serde_json` and mutated the root `DeviceProfile`.
- Automatically injected a custom `DirectPlayProfile` for all standard containers (`"mp4,m4v,mkv,webm"`) and codecs (`"h264,h265,vp9"` / `"aac,mp3,ac3,eac3,opus"`).
- Explicitly cleared out the `TranscodingProfiles` to force Jellyfin to rely on the proxy.
- Forwarded the modified request downstream to the backend.

## Milestone 3: Processing PlaybackInfo Responses
**Status**: Completed

**Summary:**
- Intercepted the downstream Jellyfin JSON response in the `playback_info_handler`.
- Parsed the Jellyfin device capability evaluation to locate the `MediaSources` array.
- Extracted the physical file `Path` for each media source.
- Generated a mapped proxy `TranscodingUrl` (`/proxymedia/...`) that points to our standalone HLS handlers.
- Modified proxy response indicating that `TranscodingContainer` is `ts` (to mimic standard behavior) and set `SupportsTranscoding` to `true`, forcing the Jellyfin client to use our server for streaming.
- Successfully repacked and returned the modified JSON body with adjusted `CONTENT_LENGTH` to the requesting client.

## Milestone 4: HLS Playlist and Segment Handlers
**Status**: Completed

**Summary:**
- Implemented a dedicated Axum handler for `GET /proxymedia/*path`.
- Extracted and decoded the upstream media `path` from the URL parameter.
- Integrated `hls_vod_lib::HlsParams::parse` to parse the requested HLS entity (MainPlaylist, Audio/Video Segments, VTT subtitles).
- Invoked `HlsVideo::open` and `hls_video.generate()` to seamlessly scan the video file on-the-fly and perform required segmentation/transmuxing without shelling out to `ffmpeg`.
- Correctly parsed incoming proxy query parameters (`codecs`, `tracks`, `interleave`) to filter and stream the custom track combinations desired by the client.
- Handled MIME types, `CACHE-CONTROL`, and binary HLS generation synchronously within a `tokio::task::spawn_blocking` thread to prevent starving the Axum async executor.

## Milestone 5: Polish & Testing
**Status**: Completed

**Summary:**
- Refactored the JSON mutation logic in `main.rs` into isolated functions (`mutate_playback_info_request` and `mutate_playback_info_response`).
- Brought in `tower_http::trace::TraceLayer` and applied it to the Axum Router to provide detailed, structured HTTP request/response logging via `tracing`.
- Created basic unit/integration tests to guarantee that device profiles are correctly injected before hitting Jellyfin, and that the server correctly maps responses to the `TranscodingUrl`.
- Verified the build via `cargo check` and `cargo test`, successfully passing all assertions and effectively finishing the proxy implementation.

## Milestone 6: Bugfix - Empty PlaybackInfo Reply
**Status**: Completed

**Summary:**
- Fixed a "Connection Drop / Empty Reply" issue on `POST /PlaybackInfo`.
- The root cause was identified as `hyper` (the underlying HTTP library) aborting the response because the proxy was merging upstream `Transfer-Encoding: chunked` headers with a manually calculated `Content-Length` for the mutated JSON.
- Implemented a robust "hop-by-hop" header stripping mechanism in the proxy handler to ensure standard-compliant HTTP responses.
- Refactored `playback_info_handler` to use Axum extractors (`Bytes`, `Method`, `Uri`) and added defensive JSON parsing to handle various client request shapes safely.
- Verified the fix with the user's reproduction `curl` command, successfully receiving the mutated JSON.

## Milestone 7: --mediaroot support
**Status**: Completed

**Summary:**
- Added a new command line option `--mediaroot <directory>` to allow prepending a base path to all media resources.
- Updated `proxymedia_handler` to correctly join the provided root with the intercepted media path.
- Verified the implementation with debug logs showing successful path prefixing.

## Milestone 8: Typed PlaybackInfo Hub
**Status**: Completed

**Summary:**
- Refactored `playback_info_handler` to use strongly-typed `PlaybackInfoRequest` and `PlaybackInfoResponse` structs instead of raw JSON manipulation.
- Updated `mutate_playback_info_request` and `mutate_playback_info_response` to operate on These typed structures.
- Verified the refactor with unit tests (2 passed) ensuring parity with the previous logic.

## Milestone 9: TODO Implementation
**Status**: Completed

**Summary:**
- Added Safari H.265 detection to `mutate_playback_info_request`, injecting the `h265` codec into DirectPlay profiles for Safari clients.
- Disabled `ts` container transcoding globally to enforce proxy functionality.
- Adapted `mutate_playback_info_response` to decode Jellyfin's `TranscodingUrl` query string into our `HlsTranscodingParameters` struct and extract the appropriate `codecs` and `tracks` for our `/proxymedia` HLS proxy endpoints.
- Updated `proxy_handler` to utilize asynchronous request body streaming instead of holding entire payloads in memory blockingly.
- Added a `websocket_handler` utilizing `tokio-tungstenite` to transparently stream and maintain the WebSocket `/socket` connection between client and Jellyfin directly.

## Milestone 10: Session Management
**Status**: Completed

**Summary:**
- Configured `mutate_playback_info_response` to forward `play_session_id` as the `stream_id` parameter to the proxymedia endpoints.
- Patched `proxymedia_handler` to read the `stream_id` parameter and correctly hydrate `hls_url.session_id`.
- Intercepted the `DELETE /Videos/ActiveEncodings` request within `proxy.rs`, pulling out the `playSessionId` query variable.
- Invoked `hls_vod_lib::cache::remove_stream_by_id(session_id)` to gracefully clear active encoding tracking when playback is stopped early, returning `200 OK` on success, or appropriately falling back to the default proxy otherwise.

## Milestone 11: Interleaved Transcoding & Manual Packet Interleaving
**Status**: Completed

**Summary:**
- Resolved critical playback failures in Safari when streaming interleaved tracks with audio transcoding (AC-3 to AAC).
- Implemented a manual "write-ahead" interleaving algorithm that pre-transcodes audio packets and merges them into the video packet stream in strict DTS order.
- Fixed initialization segment generation for interleaved AC-3 tracks by enabling `delay_moov` and ingesting initial bitstream packets to construct a valid `moov` atom without `extradata`.
- Ensured codec suffixes (e.g., `-aac`) are correctly propagated through HLS playlists for interleaved variants.

## Milestone 12: Configuration System & Dual Listeners
**Status**: Completed

**Summary:**
- Created a comprehensive configuration module (`src/config.rs`) to load proxy settings from `jellyfix-transmux-proxy.toml`.
- Added support for concurrent HTTP and HTTPS listeners, allowing the proxy to act as a secure edge frontend.
- Implemented configuration-driven Safari hacks, specifically the `force_transcoding` flag which dynamically strips direct play profiles from `PlaybackInfo` to resolve browser-specific compatibility issues.
- Improved diagnostic logging to provide clear visibility into loaded configuration files and active listeners.

## Milestone 13: Upstream TLS & Crypto Provider Standardization
**Status**: Completed

**Summary:**
- Enabled full TLS support for upstream connections (`https://` and `wss://`) by configuring `rustls` features for `reqwest` and `tokio-tungstenite`.
- Resolved a `rustls v0.23` `CryptoProvider` panic by standardizing the entire dependency graph on the `ring` crypto engine, avoiding provider conflicts at startup.
- Finalized project documentation with descriptive READMEs for the proxy, the core `hls-vod-lib` engine, and the `hls-vod-server` reference POC.
- Verified the final build and test suite, ensuring all systems are robust and production-ready.
