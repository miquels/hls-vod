use axum::{body::Body, extract::State, http::StatusCode, response::Response};
use std::sync::Arc;

use crate::AppState;

pub async fn proxymedia_handler(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(path): axum::extract::Path<String>,
    axum::extract::Query(query_params): axum::extract::Query<
        std::collections::HashMap<String, String>,
    >,
) -> Result<Response, StatusCode> {
    tracing::info!("Proxymedia request for path: {}", path);
    // Path comes in like Users/mikevs/Devel/...
    let mut clean_path = path.clone();
    if !clean_path.starts_with('/') {
        clean_path = format!("/{}", clean_path);
    }

    // Fallback to removing the leading slash if parsing fails (for the relative paths)
    let mut hls_url = match hls_vod_lib::HlsParams::parse(&clean_path) {
        Some(params) => params,
        None => hls_vod_lib::HlsParams::parse(&path).ok_or_else(|| {
            tracing::error!("Invalid HLS request: {}", path);
            StatusCode::BAD_REQUEST
        })?,
    };

    if let Some(stream_id) = query_params.get("stream_id") {
        hls_url.session_id = Some(stream_id.clone());
    }

    tracing::info!("Parsed HLS URL: {:?}", hls_url);

    let mut media_path = std::path::PathBuf::from(&hls_url.video_url);

    // If media_root is set, prepend it to the path
    if !state.media_root.is_empty() {
        let root = std::path::Path::new(&state.media_root);
        // We want to join them. If hls_url.video_url starts with /, we might need to be careful
        // depending on if we want it to be relative to root.
        // Usually joining an absolute path with another path makes it absolute.
        // Let's trim leading slash if we have a root.
        let video_url = hls_url.video_url.trim_start_matches('/');
        media_path = root.join(video_url);
        tracing::info!("Prepended media_root: {:?}", media_path);
    }

    if !media_path.exists() {
        tracing::error!("Media file not found: {:?}", media_path);
        return Err(StatusCode::NOT_FOUND);
    }

    tokio::task::spawn_blocking(move || {
        let mut hls_video = hls_vod_lib::HlsVideo::open(&media_path, hls_url).map_err(|e| {
            tracing::error!("Failed to open media: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

        if let hls_vod_lib::HlsVideo::MainPlaylist(p) = &mut hls_video {
            let codecs: Vec<String> = query_params
                .get("codecs")
                .map(|s| s.split(',').map(|c| c.trim().to_string()).collect())
                .unwrap_or_default();
            p.filter_codecs(&codecs);

            let tracks: Vec<usize> = query_params
                .get("tracks")
                .map(|s| {
                    s.split(',')
                        .filter_map(|s| s.parse::<usize>().ok())
                        .collect::<Vec<usize>>()
                })
                .unwrap_or_default();
            if !tracks.is_empty() {
                p.enable_tracks(&tracks);
            }

            // Always use interleaving.
            let interleave: bool = query_params
                .get("interleave")
                .map(|s| s == "true")
                .unwrap_or_default();
            if interleave {
                p.interleave();
            }
        }

        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            axum::http::header::CONTENT_TYPE,
            axum::http::HeaderValue::from_static(hls_video.mime_type()),
        );
        headers.insert(
            axum::http::header::CACHE_CONTROL,
            axum::http::HeaderValue::from_static(hls_video.cache_control()),
        );

        let bytes = hls_video.generate().map_err(|e| {
            tracing::error!("Failed to generate HLS data: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

        let mut response = Response::new(Body::from(bytes));
        *response.headers_mut() = headers;
        Ok(response)
    })
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
}
