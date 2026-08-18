# Runtime image for puna-orchestrator. Holds the Kubernetes credential; takes no inbound
# internet traffic beyond its own /healthz, /readyz and /metrics on :9090.
#
# As with puna-web, the binary is built outside and staged as `build-result`.
FROM docker.io/library/debian:13-slim

ENV DEBIAN_FRONTEND=noninteractive
# ca-certificates only -- needed to verify the Kubernetes API server and room certificates.
# No libpq: Postgres is reached via diesel-async -> tokio-postgres, pure Rust, with diesel built
# as `postgres_backend` rather than the libpq-backed `postgres` feature. No openssl: `kube` is
# built default-features = false with rustls-tls + ring. Confirmed with ldd.
RUN apt update && \
    apt install -y --no-install-recommends ca-certificates && \
    rm -rf /var/lib/apt/lists/*

WORKDIR /usr/local/bin

COPY --chmod=755 build-result /usr/local/bin/puna-orchestrator
CMD ["/usr/local/bin/puna-orchestrator"]
