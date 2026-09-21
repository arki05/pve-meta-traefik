# pve-meta-traefik

Traefik reads its routes from the guests of a Proxmox VE cluster, the way it
reads them from Docker labels. Each guest's Traefik configuration lives in its
[pve-meta](https://github.com/arki05/pve-meta) document; this service turns all
of them into one dynamic configuration document that Traefik polls through its
built-in [HTTP provider](https://doc.traefik.io/traefik/providers/http/).
Nothing is installed into Traefik, and Traefik can run anywhere that reaches
the PVE API.

## How it works

On every poll from Traefik the service asks the cluster which running guests
have a `traefik` subtree in their metadata, reads each subtree, fills in the
guest's address, and answers with the merged document. There is no state: a
guest that stops, or loses its subtree, is gone from the next answer. If the
cluster cannot be asked, the answer is an error rather than an empty document,
so Traefik keeps the routes it has.

The subtree is Traefik's own file-provider format, verbatim. Four things are
added on top:

* A server with `port` (and optionally `scheme`) instead of `url` gets the
  guest's address filled in, like the Docker provider's `server.port` label.
  For TCP and UDP services the same rule builds `address`.
* `port` (and optionally `scheme`) on a router: the router's service is one
  such server, so the common case is one router and nothing else. The service
  takes the router's name, or the name the router gives in `service`. A name
  that is also declared under `services` is refused, since the document would
  say two things about it.
* `${ip}`, `${name}` and `${vmid}` are substituted in any string.
* `ip` at the top of the subtree is the guest's address, trusted as given, for
  guests with several addresses or none the cluster can see.

```yaml
# CT 200's metadata document, subtree `traefik`
traefik:
  http:
    routers:
      grafana:
        rule: Host(`grafana.example.net`)
        middlewares: [auth@file]
        port: 3000
```

is the same as

```yaml
traefik:
  http:
    routers:
      grafana:
        rule: Host(`grafana.example.net`)
        middlewares: [auth@file]
        service: grafana
    services:
      grafana:
        loadBalancer:
          servers: [{ port: 3000 }]
```

Everything else stays where Traefik documents it, with every option available.
Names are the author's: a router or service two guests both declare is logged
and the lower vmid wins.

Two settings on the Traefik side keep documents this short. An entry point
marked `asDefault` takes every router that names none, and a TLS block on that
entry point gives every such router its certificate resolver; a router that
says `entryPoints` or `tls` itself still overrides both. And a middleware
declared once in Traefik's file provider, say a `chain` named `auth`, lets every
document say `auth@file` and lets the chain's target change in one place:

```toml
# traefik static configuration
[entryPoints.websecure]
  address = ":443"
  asDefault = true
  [entryPoints.websecure.http.tls]
    certResolver = "le"
```

```yaml
# traefik file provider
http:
  middlewares:
    auth:
      chain:
        middlewares: [authentik@docker]
```

## Addresses

Without `ip`, the guest's address comes from the cluster, in this order:

1. Containers: what the container reports (`/nodes/{node}/lxc/{vmid}/interfaces`).
   Works for every running container, no agent needed.
2. VMs: what the QEMU guest agent reports. VMs without an agent need `ip`,
   or the next source.
3. A tag that is an IP address, as left by the community IP-tag script.

The `cidrs` list in the configuration says which addresses count, in order of
preference. A container running Docker reports its bridge address too; listing
your LAN keeps it from winning.

## Install

On the machine running Traefik (Debian 13):

```sh
curl -fsSL https://apt.arki05.com/arki05.gpg -o /etc/apt/keyrings/arki05.gpg
echo 'deb [signed-by=/etc/apt/keyrings/arki05.gpg] https://apt.arki05.com trixie main' \
  > /etc/apt/sources.list.d/arki05.list
apt update && apt install pve-meta-traefik
```

Or take the `.deb` from a [release](https://github.com/arki05/pve-meta-traefik/releases).

Or run the container image, a static binary on `scratch`, for amd64 and arm64:

```yaml
services:
  pve-meta-traefik:
    image: ghcr.io/arki05/pve-meta-traefik:0.1.2
    environment: { PVE_META_TRAEFIK_TOKEN_SECRET: "${PVE_META_TRAEFIK_TOKEN_SECRET}" }
    configs: [{ source: pve-meta-traefik, target: /config.yaml }]
configs:
  pve-meta-traefik:
    content: |
      pve: { url: https://pve.example.net:8006, token_id: traefik@pve!meta, insecure: true }
      prefix: traefik
      cidrs: [10.10.10.0/23]
      listen: 0.0.0.0:8087
```

Traefik in the same compose project reaches it as `http://pve-meta-traefik:8087/`.
The secret comes from the environment, so the configuration holds none. The
image reads `/config.yaml` by default (`PVE_META_TRAEFIK_CONFIG`), so
`docker compose run --rm pve-meta-traefik --once` shows the document.

### A token on the cluster

The service needs a PVE API token that may list guests and read their
addresses and metadata documents: full read of a guest's document is
`VM.Audit`, which is all this service ever needs (pve-meta 0.2 grants no
per-prefix access; there is no permission file to write):

```sh
pveum user add traefik@pve
pveum role add TraefikMeta --privs 'VM.Audit VM.GuestAgent.Audit'
pveum acl modify /vms --users traefik@pve --roles TraefikMeta
pveum user token add traefik@pve meta --privsep 0
```

### Configuration

```sh
install -m 0600 /usr/share/doc/pve-meta-traefik/config.example.yaml /etc/pve-meta-traefik/config.yaml
$EDITOR /etc/pve-meta-traefik/config.yaml
systemctl enable --now pve-meta-traefik
pve-meta-traefik --once      # the document as Traefik will see it
```

Then in Traefik's static configuration:

```yaml
providers:
  http:
    endpoint: http://127.0.0.1:8087/
    pollInterval: 10s
```

`cache_ttl_seconds` in the configuration makes the service answer from a
cached document for that long instead of asking the cluster on every poll,
for clusters where the per-poll requests add up. The default is zero.
