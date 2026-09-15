# A static binary on scratch: no shell, no package manager, one process.
# Configuration is a mounted /config.yaml; the token secret may come from
# the PVE_META_TRAEFIK_TOKEN_SECRET environment variable instead of the file.
FROM rust:1-alpine AS build
RUN apk add --no-cache musl-dev build-base
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked && strip target/release/pve-meta-traefik

FROM scratch
COPY --from=build /src/target/release/pve-meta-traefik /pve-meta-traefik
COPY --from=build /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
ENV PVE_META_TRAEFIK_CONFIG=/config.yaml
USER 65534:65534
EXPOSE 8087
ENTRYPOINT ["/pve-meta-traefik"]
