//! pve-meta-traefik: a Traefik HTTP provider endpoint fed from pve-meta.
//!
//! Every poll from Traefik asks the cluster which running guests carry the
//! configured prefix in their metadata, reads each subtree, finds each guest's
//! address, and answers with one dynamic configuration document. There is no
//! state unless `cache_ttl_seconds` asks for some. An upstream failure is a
//! 502, never an empty document: Traefik keeps its last configuration on an
//! error and would drop every route on an empty one.

mod config;
mod pve;
mod render;

use std::collections::BTreeSet;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use clap::Parser;
use ipnet::IpNet;
use serde_json::{Map, Value};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use config::Config;
use pve::{Client, Resource};
use render::Guest;

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// The configuration file.
    #[arg(short, long, default_value = "/etc/pve-meta-traefik/config.yaml")]
    config: PathBuf,

    /// Render the document once to stdout and exit, instead of serving it.
    #[arg(long)]
    once: bool,
}

struct App {
    config: Config,
    client: Client,
    /// Held across a whole render, so concurrent polls collapse into one
    /// round of cluster requests.
    cache: Mutex<Option<(Instant, Arc<String>)>>,
    /// The warnings the last render produced. A problem is logged when it
    /// appears and when it goes away, not on every poll in between.
    warnings: std::sync::Mutex<BTreeSet<String>>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_target(false)
        .init();

    let cli = Cli::parse();
    let config = Config::load(&cli.config)?;
    let client = Client::new(&config.pve)?;
    let app = Arc::new(App {
        config,
        client,
        cache: Mutex::new(None),
        warnings: Default::default(),
    });

    if cli.once {
        let (doc, warnings) = app.render().await?;
        for w in warnings {
            warn!("{w}");
        }
        println!("{}", serde_json::to_string_pretty(&doc)?);
        return Ok(());
    }

    let router = Router::new()
        .route("/", get(provide))
        .route("/healthz", get(|| async { "ok" }))
        .with_state(app.clone());
    let listener = tokio::net::TcpListener::bind(app.config.listen)
        .await
        .with_context(|| format!("binding {}", app.config.listen))?;
    info!(
        "serving prefix '{}' from {} on http://{}",
        app.config.prefix, app.config.pve.url, app.config.listen
    );
    axum::serve(listener, router).await?;
    Ok(())
}

async fn provide(State(app): State<Arc<App>>) -> Response {
    match app.document().await {
        Ok(body) => (
            [(header::CONTENT_TYPE, "application/json")],
            body.as_str().to_owned(),
        )
            .into_response(),
        Err(err) => {
            warn!("{err:#}");
            (StatusCode::BAD_GATEWAY, format!("{err:#}\n")).into_response()
        }
    }
}

impl App {
    /// The document as a string, from the cache while it is fresh.
    async fn document(&self) -> Result<Arc<String>> {
        let ttl = Duration::from_secs(self.config.cache_ttl_seconds);
        let mut cache = self.cache.lock().await;
        if let Some((at, body)) = cache.as_ref() {
            if at.elapsed() < ttl {
                return Ok(body.clone());
            }
        }
        let (doc, warnings) = self.render().await?;
        self.report(warnings);
        let body = Arc::new(serde_json::to_string(&doc)?);
        *cache = Some((Instant::now(), body.clone()));
        Ok(body)
    }

    fn report(&self, warnings: Vec<String>) {
        let now: BTreeSet<String> = warnings.into_iter().collect();
        let mut last = self.warnings.lock().unwrap();
        for w in now.difference(&last) {
            warn!("{w}");
        }
        for w in last.difference(&now) {
            info!("resolved: {w}");
        }
        *last = now;
    }

    /// One round: discovery, then every guest in parallel, then the merge.
    async fn render(&self) -> Result<(Map<String, Value>, Vec<String>)> {
        let prefix = self.config.prefix.as_str();
        let (resources, with_prefix) = tokio::try_join!(
            self.client.cluster_resources(),
            self.client.meta_guests_with(prefix)
        )?;
        let with_prefix: BTreeSet<u64> = with_prefix.into_iter().collect();

        let mut guests: Vec<Resource> = resources
            .into_iter()
            .filter(|r| with_prefix.contains(&r.vmid))
            .collect();
        guests.sort_by_key(|r| r.vmid);

        let mut tasks = Vec::new();
        for resource in guests {
            tasks.push(async move {
                let vmid = resource.vmid;
                (vmid, self.render_guest(resource).await)
            });
        }
        let results = futures_util::future::join_all(tasks).await;

        let mut warnings = Vec::new();
        let mut shares = Vec::new();
        for (vmid, result) in results {
            match result {
                Ok(Some(share)) => shares.push((vmid, share)),
                Ok(None) => {}
                Err(Skip(reason)) => warnings.push(format!("guest {vmid}: {reason}")),
                Err(Fail(err)) => return Err(err.context(format!("guest {vmid}")))?,
            }
        }
        let doc = render::merge(shares, &mut warnings);
        Ok((doc, warnings))
    }

    /// `Ok(None)`: nothing to contribute, silently (stopped). `Skip`: nothing
    /// to contribute and worth a warning. `Fail`: the whole poll fails, so
    /// Traefik keeps what it has rather than getting a document with a guest
    /// missing.
    async fn render_guest(
        &self,
        resource: Resource,
    ) -> std::result::Result<Option<Map<String, Value>>, GuestError> {
        if resource.status != "running" {
            debug!("guest {}: {}, skipped", resource.vmid, resource.status);
            return Ok(None);
        }
        let vmid = resource.vmid;
        let subtree = self
            .client
            .meta_view(vmid, &self.config.prefix)
            .await
            .map_err(Fail)?;
        let Value::Object(mut subtree) = subtree else {
            return Err(Skip(format!("'{}' is not a map", self.config.prefix)));
        };
        let ip = match subtree.remove("ip") {
            Some(Value::String(s)) => Some(
                s.parse::<IpAddr>()
                    .map_err(|_| Skip(format!("'ip' {s:?} is not an address")))?,
            ),
            Some(Value::Null) | None => self.resolve(&resource).await,
            Some(_) => return Err(Skip("'ip' must be a string".into())),
        };
        let guest = Guest {
            vmid,
            name: resource.name.clone().unwrap_or_else(|| vmid.to_string()),
            ip,
        };
        render::render(subtree, &guest)
            .map(Some)
            .map_err(|e| Skip(format!("{e:#}")))
    }

    /// The guest's address from the cluster: what the container or the
    /// guest agent reports, else an address the IP-tag script left as a tag.
    async fn resolve(&self, resource: &Resource) -> Option<IpAddr> {
        let live = match resource.kind.as_str() {
            "lxc" => {
                self.client
                    .lxc_addresses(&resource.node, resource.vmid)
                    .await
            }
            "qemu" => {
                self.client
                    .qemu_addresses(&resource.node, resource.vmid)
                    .await
            }
            other => {
                debug!("guest {}: unknown type {other}", resource.vmid);
                Ok(Vec::new())
            }
        };
        let live = match live {
            Ok(addresses) => addresses,
            Err(err) => {
                debug!("guest {}: no live addresses: {err:#}", resource.vmid);
                Vec::new()
            }
        };
        let tagged: Vec<IpAddr> = resource
            .tags()
            .iter()
            .filter_map(|t| t.parse().ok())
            .collect();
        pick(&live, &self.config.cidrs).or_else(|| pick(&tagged, &self.config.cidrs))
    }
}

enum GuestError {
    Skip(String),
    Fail(anyhow::Error),
}
use GuestError::{Fail, Skip};

/// The first candidate inside the first matching `cidrs` entry, so the list's
/// order is a preference. With no `cidrs`, the first global address, IPv4
/// before IPv6.
fn pick(candidates: &[IpAddr], cidrs: &[IpNet]) -> Option<IpAddr> {
    if cidrs.is_empty() {
        let global = |ip: &&IpAddr| match ip {
            IpAddr::V4(v4) => !v4.is_loopback() && !v4.is_link_local() && !v4.is_unspecified(),
            IpAddr::V6(v6) => {
                !v6.is_loopback() && !v6.is_unicast_link_local() && !v6.is_unspecified()
            }
        };
        return candidates
            .iter()
            .filter(global)
            .find(|ip| ip.is_ipv4())
            .or_else(|| candidates.iter().find(global))
            .copied();
    }
    cidrs
        .iter()
        .find_map(|net| candidates.iter().find(|ip| net.contains(*ip)).copied())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ips(list: &[&str]) -> Vec<IpAddr> {
        list.iter().map(|s| s.parse().unwrap()).collect()
    }

    #[test]
    fn pick_prefers_the_cidr_order() {
        let cidrs: Vec<IpNet> = vec![
            "10.10.10.0/23".parse().unwrap(),
            "172.17.0.0/16".parse().unwrap(),
        ];
        assert_eq!(
            pick(&ips(&["172.17.0.2", "10.10.10.5"]), &cidrs),
            Some("10.10.10.5".parse().unwrap())
        );
        assert_eq!(
            pick(&ips(&["172.17.0.2"]), &cidrs),
            Some("172.17.0.2".parse().unwrap())
        );
        assert_eq!(pick(&ips(&["192.168.1.1"]), &cidrs), None);
    }

    #[test]
    fn pick_without_cidrs_takes_the_first_global_ipv4() {
        assert_eq!(
            pick(&ips(&["127.0.0.1", "fe80::1", "fd00::1", "10.0.0.1"]), &[]),
            Some("10.0.0.1".parse().unwrap())
        );
        assert_eq!(
            pick(&ips(&["127.0.0.1", "fd00::1"]), &[]),
            Some("fd00::1".parse().unwrap())
        );
        assert_eq!(pick(&ips(&["127.0.0.1"]), &[]), None);
    }
}
