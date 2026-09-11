//! The few PVE API calls the provider needs, as typed results.

use std::net::IpAddr;

use anyhow::{anyhow, Context, Result};
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION};
use serde::Deserialize;
use serde_json::Value;

use crate::config::Pve;

pub struct Client {
    http: reqwest::Client,
    base: String,
}

/// One row of `GET /cluster/resources?type=vm`.
#[derive(Debug, Clone, Deserialize)]
pub struct Resource {
    pub vmid: u64,
    pub node: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub status: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub tags: Option<String>,
}

impl Resource {
    pub fn tags(&self) -> Vec<String> {
        self.tags
            .as_deref()
            .unwrap_or("")
            .split(';')
            .filter(|t| !t.is_empty())
            .map(str::to_string)
            .collect()
    }
}

#[derive(Deserialize)]
struct Envelope {
    data: Option<Value>,
}

impl Client {
    pub fn new(pve: &Pve) -> Result<Self> {
        let mut headers = HeaderMap::new();
        let token = format!(
            "PVEAPIToken={}={}",
            pve.token_id,
            pve.token_secret.as_deref().unwrap_or("")
        );
        let mut value = HeaderValue::from_str(&token)
            .context("token id or secret is not a valid header value")?;
        value.set_sensitive(true);
        headers.insert(AUTHORIZATION, value);

        let mut builder = reqwest::Client::builder()
            .default_headers(headers)
            .timeout(std::time::Duration::from_secs(pve.timeout_seconds))
            .danger_accept_invalid_certs(pve.insecure);
        if let Some(ca) = &pve.ca_file {
            let pem = std::fs::read(ca).with_context(|| format!("reading {}", ca.display()))?;
            let cert = reqwest::Certificate::from_pem(&pem)
                .context("pve.ca_file is not a PEM certificate")?;
            builder = builder
                .tls_built_in_root_certs(false)
                .add_root_certificate(cert);
        }
        Ok(Client {
            http: builder.build()?,
            base: format!("{}/api2/json", pve.url),
        })
    }

    /// `GET <path>` with `query`, returning the envelope's `data`.
    async fn get(&self, path: &str, query: &[(&str, &str)]) -> Result<Value> {
        let url = format!("{}{}", self.base, path);
        let response = self
            .http
            .get(&url)
            .query(query)
            .send()
            .await
            .with_context(|| format!("GET {path}"))?;
        let status = response.status();
        if !status.is_success() {
            let reason = status.canonical_reason().unwrap_or("");
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow!(
                "GET {path}: {} {reason} {}",
                status.as_u16(),
                body.trim()
            ));
        }
        let envelope: Envelope = response
            .json()
            .await
            .with_context(|| format!("GET {path}: decoding"))?;
        Ok(envelope.data.unwrap_or(Value::Null))
    }

    pub async fn cluster_resources(&self) -> Result<Vec<Resource>> {
        let data = self.get("/cluster/resources", &[("type", "vm")]).await?;
        serde_json::from_value(data).context("GET /cluster/resources: unexpected shape")
    }

    /// The vmids whose metadata has something at `prefix`, as far as the
    /// token may see.
    pub async fn meta_guests_with(&self, prefix: &str) -> Result<Vec<u64>> {
        let data = self.get("/meta/guests", &[("has", prefix)]).await?;
        let rows = data.as_array().context("GET /meta/guests: not a list")?;
        Ok(rows
            .iter()
            .filter_map(|r| r.get("vmid").and_then(Value::as_u64))
            .collect())
    }

    /// The subtree at `view` of one guest's document.
    pub async fn meta_view(&self, vmid: u64, view: &str) -> Result<Value> {
        let path = format!("/meta/guests/{vmid}");
        let data = self
            .get(&path, &[("view", view), ("format", "json")])
            .await?;
        Ok(data.get("data").cloned().unwrap_or(Value::Null))
    }

    /// The addresses a running container reports. No agent involved.
    pub async fn lxc_addresses(&self, node: &str, vmid: u64) -> Result<Vec<IpAddr>> {
        let path = format!("/nodes/{node}/lxc/{vmid}/interfaces");
        let data = self.get(&path, &[]).await?;
        let mut out = Vec::new();
        for iface in data.as_array().into_iter().flatten() {
            for key in ["inet", "inet6"] {
                if let Some(cidr) = iface.get(key).and_then(Value::as_str) {
                    if let Ok(ip) = cidr.split('/').next().unwrap_or("").parse::<IpAddr>() {
                        out.push(ip);
                    }
                }
            }
        }
        Ok(out)
    }

    /// The addresses a running VM's guest agent reports. Without an agent the
    /// call fails, and the caller treats that as "none".
    pub async fn qemu_addresses(&self, node: &str, vmid: u64) -> Result<Vec<IpAddr>> {
        let path = format!("/nodes/{node}/qemu/{vmid}/agent/network-get-interfaces");
        let data = self.get(&path, &[]).await?;
        let mut out = Vec::new();
        for iface in data
            .get("result")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            for addr in iface
                .get("ip-addresses")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let Some(ip) = addr.get("ip-address").and_then(Value::as_str) {
                    if let Ok(ip) = ip.parse::<IpAddr>() {
                        out.push(ip);
                    }
                }
            }
        }
        Ok(out)
    }
}
