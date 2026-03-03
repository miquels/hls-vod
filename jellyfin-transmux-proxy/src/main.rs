use std::io;
use std::sync::Arc;

use axum::{routing::any, Router};
use axum_server::tls_rustls::RustlsConfig;
use clap::Parser;
use reqwest::Client;
use socket2::{Domain, Protocol, Socket, Type};
use tower_http::cors::CorsLayer;
use tower_http::trace::{DefaultMakeSpan, DefaultOnRequest, DefaultOnResponse, TraceLayer};
use tracing::Level;

pub mod config;
pub mod hls;
pub mod playbackinfo;
pub mod proxy;
pub mod types;

use config::{listen_on_port, Config};
use hls::proxymedia_handler;
use playbackinfo::playback_info_handler;
use proxy::{proxy_handler, websocket_handler};

#[derive(Parser, Debug, Clone)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Jellyfin server URL to proxy to
    #[arg(short, long)]
    jellyfin_url: Option<String>,

    /// Listen port for HTTP (CLI override)
    #[arg(short, long)]
    port: Option<u16>,

    /// Media root directory to prepend to filesystem paths
    #[arg(short, long)]
    mediaroot: Option<String>,

    /// Path to config file
    #[arg(short, long, default_value = "jellyfix-transmux-proxy.toml")]
    config: String,
}

pub struct AppState {
    pub jellyfin_url: String,
    pub media_root: String,
    pub http_client: Client,
    pub safari_force_transcoding: bool,
}

// Helper to create a listener.
fn tcp_listener(addr: std::net::SocketAddr) -> io::Result<std::net::TcpListener> {
    let domain = Domain::for_address(addr);
    let socket = Socket::new(domain, Type::STREAM, Some(Protocol::TCP))?;

    if addr.is_ipv6() {
        socket.set_only_v6(true)?;
    }

    socket.set_reuse_address(true)?;
    socket.bind(&addr.into())?;
    socket.listen(128)?;
    socket.set_nonblocking(true)?;

    Ok(socket.into())
}

async fn watcher(watcher_config: RustlsConfig, cert: String, key: String) {
    let mut last_modified = tokio::fs::metadata(&cert)
        .await
        .and_then(|m| m.modified())
        .unwrap_or(std::time::SystemTime::UNIX_EPOCH);

    loop {
        // Sleep for 300 seconds (5 minutes)
        tokio::time::sleep(tokio::time::Duration::from_secs(300)).await;

        if let Ok(metadata) = tokio::fs::metadata(&cert).await {
            if let Ok(new_modified) = metadata.modified() {
                // Only reload if the file has been touched since our last check
                if new_modified > last_modified {
                    tracing::info!("Detected certificate change. Reloading...");

                    match watcher_config.reload_from_pem_file(&cert, &key).await {
                        Ok(_) => {
                            last_modified = new_modified;
                            tracing::info!("TLS Certificates reloaded successfully.");
                        }
                        Err(e) => tracing::error!("Failed to reload certificates: {}", e),
                    }
                }
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Explicitly install the ring crypto provider for rustls 0.23+
    // This avoids the "Could not automatically determine the process-level CryptoProvider" panic
    // when multiple providers are enabled or when the environment is ambiguous.
    let _ = rustls::crypto::ring::default_provider().install_default();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "jellyfin_transmux_proxy=info,tower_http=info".into()),
        )
        .init();

    let args = Args::parse();

    // Load config if it exists
    let mut config = if std::path::Path::new(&args.config).exists() {
        tracing::info!("Loading config from {}", args.config);
        Config::load(&args.config)?
    } else {
        tracing::info!("Config file {} not found, using CLI arguments", args.config);
        Config::empty_config()
    };

    // Merge config and args
    if let Some(jellyfin_url) = &args.jellyfin_url {
        config.jellyfin.jellyfin = jellyfin_url.to_string();
    };
    if let Some(mediaroot) = &args.mediaroot {
        config.jellyfin.mediaroot = Some(mediaroot.to_string());
    };
    if let Some(port) = &args.port {
        config.server.enable_http = true;
        config.server.enable_https = false;
        config.server.http_listen = listen_on_port(*port);
    }

    let http_client = Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()?;

    let state = Arc::new(AppState {
        jellyfin_url: config.jellyfin.jellyfin.clone(),
        media_root: config.jellyfin.mediaroot.clone().unwrap_or_default(),
        http_client,
        safari_force_transcoding: config.safari.force_transcoding,
    });

    let app = Router::new()
        .route(
            "/Items/{item_id}/PlaybackInfo",
            axum::routing::post(playback_info_handler),
        )
        .route(
            "/proxymedia/{*path}",
            axum::routing::get(proxymedia_handler),
        )
        .route("/socket", axum::routing::get(websocket_handler))
        .fallback(any(proxy_handler))
        .layer(CorsLayer::permissive())
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(DefaultMakeSpan::new().level(Level::INFO))
                .on_request(DefaultOnRequest::new().level(Level::INFO))
                .on_response(DefaultOnResponse::new().level(Level::INFO)),
        )
        .with_state(state);

    let mut listeners = Vec::new();

    // HTTP listeners
    if config.server.enable_http {
        for addr in &config.server.http_listen {
            let app_clone = app.clone();
            tracing::info!("Starting HTTP listener on {}", addr);
            let listener = tcp_listener(*addr)?;
            listeners.push(tokio::spawn(async move {
                axum_server::from_tcp(listener)?
                    .serve(app_clone.into_make_service())
                    .await
            }));
        }
    }

    // HTTPS listeners
    if config.server.enable_https {
        let rustls_config = axum_server::tls_rustls::RustlsConfig::from_pem_file(
            &config.server.tls_cert,
            &config.server.tls_key,
        )
        .await?;

        let cert = config.server.tls_cert.clone();
        let key = config.server.tls_key.clone();
        let rustls_config_clone = rustls_config.clone();
        tokio::spawn(async move {
            watcher(rustls_config_clone, cert, key).await;
        });

        for addr in &config.server.https_listen {
            tracing::info!("Starting HTTPS listener on {}", addr);
            let app_clone = app.clone();
            let rustls_config_clone = rustls_config.clone();
            let listener = tcp_listener(*addr)?;
            listeners.push(tokio::spawn(async move {
                axum_server::from_tcp_rustls(listener, rustls_config_clone)?
                    .serve(app_clone.into_make_service())
                    .await
            }));
        }
    }

    tracing::info!("Proxying to {}", config.jellyfin.jellyfin);

    if listeners.is_empty() {
        tracing::error!("No listeners configured!");
        return Err("No listeners configured".into());
    }

    // Wait for all listeners
    for handle in listeners {
        handle.await??;
    }

    Ok(())
}
