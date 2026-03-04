use serde::{de, Deserialize, Deserializer};
use std::fs;
use std::io::{self, ErrorKind};
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::Path;
use std::str::FromStr;

#[derive(Debug, Deserialize, Clone, Default)]
pub struct Config {
    pub server: ServerConfig,
    pub jellyfin: JellyfinConfig,
    #[serde(default)]
    pub safari: SafariConfig,
    #[serde(default)]
    pub cache: hls_vod_lib::cache::SegmentCacheConfig,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct ServerConfig {
    #[serde(default)]
    pub enable_http: bool,
    #[serde(default)]
    pub enable_https: bool,
    #[serde(default = "default_http", deserialize_with = "flex_socketaddr_list")]
    pub http_listen: Vec<SocketAddr>,
    #[serde(default = "default_https", deserialize_with = "flex_socketaddr_list")]
    pub https_listen: Vec<SocketAddr>,
    #[serde(default)]
    pub tls_cert: String,
    #[serde(default)]
    pub tls_key: String,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct SafariConfig {
    #[serde(default)]
    pub force_transcoding: bool,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct JellyfinConfig {
    pub jellyfin: String,
    #[serde(default)]
    pub mediaroot: Option<String>,
}

fn default_http() -> Vec<SocketAddr> {
    listen_on_port(80)
}

fn default_https() -> Vec<SocketAddr> {
    listen_on_port(443)
}

pub fn listen_on_port(port: u16) -> Vec<SocketAddr> {
    vec![
        SocketAddr::new(Ipv4Addr::new(0, 0, 0, 0).into(), port),
        SocketAddr::new(Ipv6Addr::from_bits(0u128).into(), port),
    ]
}

impl Config {
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self, Box<dyn std::error::Error>> {
        let content = fs::read_to_string(path.as_ref())?;
        let config: Config = toml::from_str(&content)?;
        if !config.server.enable_http && !config.server.enable_https {
            return Err(Box::new(io::Error::new(
                ErrorKind::InvalidInput,
                format!(
                    "{:?}: at least one of enable_http or enable_https must be set to true",
                    path.as_ref()
                ),
            )));
        }
        if config.server.enable_https
            && (config.server.tls_cert == "" || config.server.tls_key == "")
        {
            return Err(Box::new(io::Error::new(
                ErrorKind::InvalidInput,
                format!("{:?}: tls_cert and tls_key must be set", path.as_ref()),
            )));
        }

        Ok(config)
    }

    pub fn empty_config() -> Self {
        Config {
            server: ServerConfig {
                enable_http: true,
                http_listen: listen_on_port(8097),
                ..Default::default()
            },
            safari: Default::default(),
            jellyfin: JellyfinConfig {
                jellyfin: "http://localhost:8096".to_string(),
                ..Default::default()
            },
            cache: Default::default(),
        }
    }
}

// Single address.
#[derive(Deserialize)]
#[serde(untagged)]
enum RawAddr {
    Port(u16),
    Full(String),
}

// Address or list.
#[derive(Deserialize)]
#[serde(untagged)]
enum FlexibleInput {
    Single(RawAddr),
    Multiple(Vec<RawAddr>),
}

// Parse the raw address into a Vec<SocketAddr>.
fn parse_rawaddr(rawaddr: RawAddr) -> Result<Vec<SocketAddr>, String> {
    match rawaddr {
        RawAddr::Port(p) => Ok(vec![
            SocketAddr::new(Ipv4Addr::new(0, 0, 0, 0).into(), p),
            SocketAddr::new(Ipv6Addr::from_bits(0u128).into(), p),
        ]),
        RawAddr::Full(s) => {
            // Could be just a port.
            if let Ok(port) = u16::from_str(&s) {
                return parse_rawaddr(RawAddr::Port(port));
            }
            // Translate *:port to 0.0.0.0:port
            let s = if s.starts_with("*:") {
                let (_, r) = s.split_once(":").unwrap();
                format!("0.0.0.0:{}", r)
            } else {
                s
            };
            // Now parse as v4 or v6.
            let sa = SocketAddr::from_str(&s).map_err(|e| e.to_string())?;
            Ok(vec![sa])
        }
    }
}

// Deserialize a number, string, or list of number or string into a Vec<SocketAddr>.
fn flex_socketaddr_list<'de, D>(deserializer: D) -> Result<Vec<SocketAddr>, D::Error>
where
    D: Deserializer<'de>,
{
    // Deserialize the input into our "One or Many" enum
    let input = FlexibleInput::deserialize(deserializer)?;

    // Flatten the result into a Vec<SocketAddr>
    match input {
        FlexibleInput::Single(raw) => parse_rawaddr(raw).map_err(de::Error::custom),
        FlexibleInput::Multiple(raw_vec) => raw_vec
            .into_iter()
            .map(parse_rawaddr)
            .collect::<Result<Vec<Vec<_>>, _>>()
            .map(|v| v.into_iter().flatten().collect())
            .map_err(de::Error::custom),
    }
}
