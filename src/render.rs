//! From one guest's subtree to its share of the dynamic configuration, and
//! from all shares to the one document Traefik reads.
//!
//! The subtree is Traefik's file-provider format, verbatim, plus three things
//! the file format does not have:
//!
//! * `ip` at the top: the guest's address, trusted as given. Otherwise the
//!   address is resolved from the cluster (see `main.rs`).
//! * `${ip}`, `${name}` and `${vmid}` in any string.
//! * A server without `url` (HTTP) or `address` (TCP, UDP): `port` and an
//!   optional `scheme` build one from the guest's address, the way the
//!   Docker provider's `server.port` label does.
//!
//! Nothing else is touched. Every option stays where Traefik documents it.

use std::net::IpAddr;

use anyhow::{anyhow, bail, Result};
use serde_json::{Map, Value};

/// What a guest's rendered configuration is about.
pub struct Guest {
    pub vmid: u64,
    pub name: String,
    pub ip: Option<IpAddr>,
}

impl Guest {
    fn ip(&self) -> Result<IpAddr> {
        self.ip
            .ok_or_else(|| anyhow!("needs an address and none could be resolved"))
    }

    fn host(&self) -> Result<String> {
        Ok(match self.ip()? {
            IpAddr::V4(v4) => v4.to_string(),
            IpAddr::V6(v6) => format!("[{v6}]"),
        })
    }
}

/// Renders one guest's subtree, `ip` already taken out. Fails when the
/// subtree needs the guest's address and there is none, or when a server
/// entry cannot be completed.
pub fn render(subtree: Map<String, Value>, guest: &Guest) -> Result<Map<String, Value>> {
    let mut out = Map::new();
    for (key, value) in subtree {
        out.insert(key, substitute(value, guest)?);
    }
    for (section, needs_url) in [("http", true), ("tcp", false), ("udp", false)] {
        let Some(services) =
            get_mut_ci_map(&mut out, section).and_then(|s| get_mut_ci(s, "services"))
        else {
            continue;
        };
        let Some(services) = services.as_object_mut() else {
            continue;
        };
        for (name, service) in services.iter_mut() {
            let Some(servers) =
                get_mut_ci(service, "loadBalancer").and_then(|lb| get_mut_ci(lb, "servers"))
            else {
                continue;
            };
            let Some(servers) = servers.as_array_mut() else {
                continue;
            };
            for server in servers.iter_mut() {
                complete_server(server, needs_url, guest)
                    .map_err(|e| anyhow!("{section}.services.{name}: {e}"))?;
            }
        }
    }
    Ok(out)
}

/// `${ip}`, `${name}`, `${vmid}` in every string, at any depth.
fn substitute(value: Value, guest: &Guest) -> Result<Value> {
    Ok(match value {
        Value::String(s) => {
            let mut s = s;
            if s.contains("${ip}") {
                s = s.replace("${ip}", &guest.ip()?.to_string());
            }
            if s.contains("${name}") {
                s = s.replace("${name}", &guest.name);
            }
            if s.contains("${vmid}") {
                s = s.replace("${vmid}", &guest.vmid.to_string());
            }
            Value::String(s)
        }
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(|v| substitute(v, guest))
                .collect::<Result<_>>()?,
        ),
        Value::Object(map) => {
            let mut out = Map::new();
            for (k, v) in map {
                out.insert(k, substitute(v, guest)?);
            }
            Value::Object(out)
        }
        other => other,
    })
}

/// A server entry with `url`/`address` is left alone. One without gets it
/// from `port` and `scheme`, which are then removed so Traefik never sees a
/// field its file format does not accept.
fn complete_server(server: &mut Value, needs_url: bool, guest: &Guest) -> Result<()> {
    let Some(map) = server.as_object_mut() else {
        bail!("server entry is not a map");
    };
    let target = if needs_url { "url" } else { "address" };
    if find_ci(map, target).is_some() {
        return Ok(());
    }
    let port = match remove_ci(map, "port") {
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::String(s)) => s,
        Some(_) => bail!("'port' must be a number"),
        None => bail!("server has neither '{target}' nor 'port'"),
    };
    if port.parse::<u16>().is_err() {
        bail!("'port' {port:?} is not a port number");
    }
    let scheme = remove_ci(map, "scheme");
    let host = guest.host()?;
    let value = if needs_url {
        let scheme = match scheme {
            Some(Value::String(s)) => s,
            Some(_) => bail!("'scheme' must be a string"),
            None => "http".to_string(),
        };
        format!("{scheme}://{host}:{port}")
    } else {
        if scheme.is_some() {
            bail!("'scheme' has no meaning for a TCP or UDP server");
        }
        format!("{host}:{port}")
    };
    map.insert(target.to_string(), Value::String(value));
    Ok(())
}

// Traefik matches its keys case-insensitively (`loadbalancer` and
// `loadBalancer` are the same field), so these do too.

fn find_ci<'a>(map: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    map.keys()
        .find(|k| k.eq_ignore_ascii_case(key))
        .map(String::as_str)
}

fn get_mut_ci_map<'a>(map: &'a mut Map<String, Value>, key: &str) -> Option<&'a mut Value> {
    let found = find_ci(map, key)?.to_string();
    map.get_mut(&found)
}

fn get_mut_ci<'a>(value: &'a mut Value, key: &str) -> Option<&'a mut Value> {
    get_mut_ci_map(value.as_object_mut()?, key)
}

fn remove_ci(map: &mut Map<String, Value>, key: &str) -> Option<Value> {
    let found = find_ci(map, key)?.to_string();
    map.remove(&found)
}

/// Every guest's share into one document. Names are the author's: a router,
/// service or middleware two guests both declare is a conflict, reported and
/// resolved in favour of the lower vmid. Lists (`tls.certificates`) are
/// concatenated.
pub fn merge(
    shares: Vec<(u64, Map<String, Value>)>,
    conflicts: &mut Vec<String>,
) -> Map<String, Value> {
    let mut doc: Map<String, Value> = Map::new();
    let mut owners: std::collections::HashMap<String, u64> = Default::default();
    for (vmid, share) in shares {
        for (section, kinds) in share {
            let Value::Object(kinds) = kinds else {
                conflicts.push(format!("guest {vmid}: '{section}' is not a map; ignored"));
                continue;
            };
            let target = doc
                .entry(section.clone())
                .or_insert_with(|| Value::Object(Map::new()));
            let target = target.as_object_mut().expect("sections are maps");
            for (kind, entries) in kinds {
                match entries {
                    Value::Object(entries) => {
                        let slot = target
                            .entry(kind.clone())
                            .or_insert_with(|| Value::Object(Map::new()));
                        let Some(slot) = slot.as_object_mut() else {
                            conflicts.push(format!(
                                "guest {vmid}: '{section}.{kind}' clashes with a list; ignored"
                            ));
                            continue;
                        };
                        for (name, entry) in entries {
                            let path = format!("{section}.{kind}.{name}");
                            if let Some(owner) = owners.get(&path) {
                                conflicts.push(format!("guest {vmid}: '{path}' is already declared by guest {owner}; ignored"));
                                continue;
                            }
                            owners.insert(path, vmid);
                            slot.insert(name, entry);
                        }
                    }
                    Value::Array(items) => {
                        let slot = target
                            .entry(kind.clone())
                            .or_insert_with(|| Value::Array(Vec::new()));
                        let Some(slot) = slot.as_array_mut() else {
                            conflicts.push(format!(
                                "guest {vmid}: '{section}.{kind}' clashes with a map; ignored"
                            ));
                            continue;
                        };
                        slot.extend(items);
                    }
                    _ => conflicts.push(format!(
                        "guest {vmid}: '{section}.{kind}' is neither a map nor a list; ignored"
                    )),
                }
            }
        }
    }
    doc
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn guest(ip: Option<&str>) -> Guest {
        Guest {
            vmid: 200,
            name: "ct".into(),
            ip: ip.map(|s| s.parse().unwrap()),
        }
    }

    fn map(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn port_and_scheme_become_a_url() {
        let out = render(
            map(json!({"http": {"services": {"a": {"loadBalancer": {"servers": [{"port": 3000}, {"port": "9090", "scheme": "https"}]}}}}})),
            &guest(Some("10.0.0.5")),
        )
        .unwrap();
        assert_eq!(
            out["http"]["services"]["a"]["loadBalancer"]["servers"],
            json!([{"url": "http://10.0.0.5:3000"}, {"url": "https://10.0.0.5:9090"}])
        );
    }

    #[test]
    fn tcp_gets_an_address_and_ipv6_is_bracketed() {
        let out = render(
            map(json!({"tcp": {"services": {"db": {"loadbalancer": {"servers": [{"port": 5432}]}}}}})),
            &guest(Some("fd00::5")),
        )
        .unwrap();
        assert_eq!(
            out["tcp"]["services"]["db"]["loadbalancer"]["servers"],
            json!([{"address": "[fd00::5]:5432"}])
        );
    }

    #[test]
    fn urls_and_other_options_are_untouched() {
        let input = json!({"http": {"services": {"a": {"loadBalancer": {"passHostHeader": false, "servers": [{"url": "http://x:1"}]}}}}});
        let out = render(map(input.clone()), &guest(None)).unwrap();
        assert_eq!(Value::Object(out), input);
    }

    #[test]
    fn placeholders() {
        let out = render(
            map(json!({"http": {"routers": {"r": {"rule": "Host(`${name}-${vmid}`)", "service": "s"}}, "services": {"s": {"loadBalancer": {"servers": [{"url": "http://${ip}:80"}]}}}}})),
            &guest(Some("10.0.0.5")),
        )
        .unwrap();
        assert_eq!(out["http"]["routers"]["r"]["rule"], "Host(`ct-200`)");
        assert_eq!(
            out["http"]["services"]["s"]["loadBalancer"]["servers"][0]["url"],
            "http://10.0.0.5:80"
        );
    }

    #[test]
    fn needing_an_address_without_one_fails() {
        let err = render(
            map(json!({"http": {"services": {"a": {"loadBalancer": {"servers": [{"port": 1}]}}}}})),
            &guest(None),
        )
        .unwrap_err();
        assert!(err.to_string().contains("http.services.a"), "{err}");
        let err = render(map(json!({"x": "${ip}"})), &guest(None)).unwrap_err();
        assert!(err.to_string().contains("address"), "{err}");
    }

    #[test]
    fn a_server_without_port_fails() {
        let err = render(
            map(json!({"http": {"services": {"a": {"loadBalancer": {"servers": [{}]}}}}})),
            &guest(Some("10.0.0.5")),
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("neither 'url' nor 'port'"),
            "{err}"
        );
    }

    #[test]
    fn merge_keeps_the_first_and_reports_the_second() {
        let mut conflicts = Vec::new();
        let doc = merge(
            vec![
                (
                    100,
                    map(
                        json!({"http": {"routers": {"web": {"rule": "a"}}}, "tls": {"certificates": [{"certFile": "a"}]}}),
                    ),
                ),
                (
                    200,
                    map(
                        json!({"http": {"routers": {"web": {"rule": "b"}, "api": {"rule": "c"}}}, "tls": {"certificates": [{"certFile": "b"}]}}),
                    ),
                ),
            ],
            &mut conflicts,
        );
        assert_eq!(doc["http"]["routers"]["web"]["rule"], "a");
        assert_eq!(doc["http"]["routers"]["api"]["rule"], "c");
        assert_eq!(doc["tls"]["certificates"].as_array().unwrap().len(), 2);
        assert_eq!(
            conflicts,
            vec!["guest 200: 'http.routers.web' is already declared by guest 100; ignored"]
        );
    }
}
