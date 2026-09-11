//! The configuration file: where the PVE API is, which prefix holds the
//! Traefik configuration, and which addresses count.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use ipnet::IpNet;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub pve: Pve,

    /// The dotted key path in each guest's metadata document that holds its
    /// Traefik dynamic configuration. The prefix a pve-meta permission file
    /// grants the token read access to.
    #[serde(default = "default_prefix")]
    pub prefix: String,

    /// Which guest addresses may be used, in order of preference. Empty means
    /// any global address, IPv4 first. A container running Docker has a
    /// bridge address too; this is how it never wins.
    #[serde(default)]
    pub cidrs: Vec<IpNet>,

    /// Where Traefik polls.
    #[serde(default = "default_listen")]
    pub listen: SocketAddr,

    /// How long a rendered document is served before the cluster is asked
    /// again. Zero, the default, means every poll asks the cluster: nothing
    /// is ever stale by more than Traefik's own poll interval. A large
    /// deployment can trade freshness for API load here.
    #[serde(default)]
    pub cache_ttl_seconds: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pve {
    /// `https://host:8006`, any cluster node.
    pub url: String,

    /// `user@realm!tokenname`.
    pub token_id: String,

    /// The token's secret. One of `token_secret`, `token_secret_file` or the
    /// `PVE_META_TRAEFIK_TOKEN_SECRET` environment variable.
    #[serde(default)]
    pub token_secret: Option<String>,
    #[serde(default)]
    pub token_secret_file: Option<PathBuf>,

    /// Skip TLS verification (the PVE default certificate is self-signed).
    #[serde(default)]
    pub insecure: bool,

    /// A PEM file to trust instead of the system roots.
    #[serde(default)]
    pub ca_file: Option<PathBuf>,

    #[serde(default = "default_timeout")]
    pub timeout_seconds: u64,
}

fn default_prefix() -> String {
    "traefik".into()
}

fn default_listen() -> SocketAddr {
    "127.0.0.1:8087".parse().unwrap()
}

fn default_timeout() -> u64 {
    10
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let mut config: Config = serde_yaml_ng::from_str(&text)
            .with_context(|| format!("parsing {}", path.display()))?;
        config.pve.url = config.pve.url.trim_end_matches('/').to_string();
        if let Ok(secret) = std::env::var("PVE_META_TRAEFIK_TOKEN_SECRET") {
            config.pve.token_secret = Some(secret);
        } else if config.pve.token_secret.is_none() {
            let file = config
                .pve
                .token_secret_file
                .as_ref()
                .context("pve.token_secret, pve.token_secret_file or PVE_META_TRAEFIK_TOKEN_SECRET is required")?;
            let secret = std::fs::read_to_string(file)
                .with_context(|| format!("reading {}", file.display()))?;
            config.pve.token_secret = Some(secret.trim().to_string());
        }
        Ok(config)
    }
}
