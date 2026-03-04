//! Configuration file support
//!
//! Loads server configuration from TOML files.

use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::config::ServerConfig;

/// Configuration file format
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigFile {
    /// Server settings
    pub server: ServerSettings,
    /// Cache settings
    pub cache: CacheSettings,
    /// Segment settings
    pub segment: SegmentSettings,
    /// Audio settings
    pub audio: AudioSettings,
    /// Logging settings
    pub logging: Option<LoggingSettings>,
    /// Limits settings
    pub limits: Option<LimitsSettings>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerSettings {
    /// Host address to bind to
    pub host: String,
    /// Port to listen on
    pub port: u16,
    /// Enable CORS
    pub cors_enabled: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheSettings {
    /// Maximum memory usage in MB
    pub max_memory_mb: usize,
    /// Maximum number of cached segments
    pub max_segments: usize,
    /// TTL for cached segments in seconds
    pub ttl_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SegmentSettings {
    /// Target segment duration in seconds
    pub target_duration_secs: f64,
    /// Minimum segment duration
    pub min_duration_secs: Option<f64>,
    /// Maximum segment duration
    pub max_duration_secs: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioSettings {
    /// Target sample rate for AAC output
    pub target_sample_rate: u32,
    /// AAC bitrate in bps
    pub aac_bitrate: u64,
    /// Enable audio transcoding
    pub enable_transcoding: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoggingSettings {
    /// Log level (trace, debug, info, warn, error)
    pub level: String,
    /// Output format (json, pretty)
    pub format: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LimitsSettings {
    /// Maximum concurrent streams
    pub max_concurrent_streams: Option<usize>,
    /// Rate limit requests per second
    pub rate_limit_rps: Option<u32>,
    /// Maximum request body size in MB
    pub max_request_size_mb: Option<usize>,
}

impl ConfigFile {
    /// Load configuration from a TOML file
    pub fn from_file<P: AsRef<Path>>(path: P) -> Result<Self, Box<dyn std::error::Error>> {
        let content = std::fs::read_to_string(path.as_ref())?;
        let config: ConfigFile = toml::from_str(&content)?;
        Ok(config)
    }

    /// Save configuration to a TOML file
    pub fn to_file<P: AsRef<Path>>(&self, path: P) -> Result<(), Box<dyn std::error::Error>> {
        let content = toml::to_string_pretty(self)?;
        std::fs::write(path.as_ref(), content)?;
        Ok(())
    }

    /// Generate default configuration file
    pub fn default_config() -> Self {
        Self {
            server: ServerSettings {
                host: "0.0.0.0".to_string(),
                port: 3000,
                cors_enabled: Some(true),
            },
            cache: CacheSettings {
                max_memory_mb: 512,
                max_segments: 100,
                ttl_secs: 300,
            },
            segment: SegmentSettings {
                target_duration_secs: 4.0,
                min_duration_secs: Some(3.0),
                max_duration_secs: Some(6.0),
            },
            audio: AudioSettings {
                target_sample_rate: 48000,
                aac_bitrate: 128000,
                enable_transcoding: Some(true),
            },
            logging: Some(LoggingSettings {
                level: "info".to_string(),
                format: Some("pretty".to_string()),
            }),
            limits: Some(LimitsSettings {
                max_concurrent_streams: Some(100),
                rate_limit_rps: Some(100),
                max_request_size_mb: Some(10),
            }),
        }
    }

    /// Convert to ServerConfig
    pub fn into_server_config(self) -> ServerConfig {
        ServerConfig {
            host: self.server.host,
            port: self.server.port,
            cache: crate::config::SegmentCacheConfig {
                max_memory_mb: self.cache.max_memory_mb,
                max_segments: self.cache.max_segments,
                ttl_secs: self.cache.ttl_secs,
                lookahead: 0,
            },
            segment: crate::config::SegmentConfig {
                target_duration_secs: self.segment.target_duration_secs,
                min_duration_secs: self.segment.min_duration_secs.unwrap_or(3.0),
                max_duration_secs: self.segment.max_duration_secs.unwrap_or(6.0),
            },
            audio: crate::config::AudioConfig {
                target_sample_rate: self.audio.target_sample_rate,
                aac_bitrate: self.audio.aac_bitrate,
                enable_transcoding: self.audio.enable_transcoding.unwrap_or(true),
            },
            cors_enabled: self.server.cors_enabled.unwrap_or(true),
            log_level: self
                .logging
                .map(|l| l.level)
                .unwrap_or_else(|| "info".to_string()),
            max_concurrent_streams: self.limits.as_ref().and_then(|l| l.max_concurrent_streams),
            rate_limit_rps: self.limits.as_ref().and_then(|l| l.rate_limit_rps),
        }
    }
}

/// Generate default configuration file at the specified path
pub fn generate_default_config<P: AsRef<Path>>(path: P) -> Result<(), Box<dyn std::error::Error>> {
    let config = ConfigFile::default_config();
    config.to_file(path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_default_config() {
        let config = ConfigFile::default_config();
        assert_eq!(config.server.port, 3000);
        assert_eq!(config.cache.max_memory_mb, 512);
        assert_eq!(config.segment.target_duration_secs, 4.0);
    }

    #[test]
    fn test_config_file_roundtrip() {
        let config = ConfigFile::default_config();

        let mut temp_file = NamedTempFile::new().unwrap();
        let content = toml::to_string_pretty(&config).unwrap();
        temp_file.write_all(content.as_bytes()).unwrap();

        let loaded = ConfigFile::from_file(temp_file.path()).unwrap();
        assert_eq!(loaded.server.port, config.server.port);
        assert_eq!(loaded.cache.max_memory_mb, config.cache.max_memory_mb);
    }

    #[test]
    fn test_into_server_config() {
        let config_file = ConfigFile::default_config();
        let server_config = config_file.into_server_config();

        assert_eq!(server_config.port, 3000);
        assert_eq!(server_config.cache.max_memory_mb, 512);
        assert_eq!(server_config.segment.target_duration_secs, 4.0);
    }

    #[test]
    fn test_generate_default_config() {
        let temp_file = NamedTempFile::new().unwrap();
        let path = temp_file.path().to_path_buf();

        generate_default_config(&path).unwrap();

        assert!(path.exists());
        let loaded = ConfigFile::from_file(&path).unwrap();
        assert_eq!(loaded.server.port, 3000);
    }
}
