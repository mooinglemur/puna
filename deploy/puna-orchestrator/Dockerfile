# Runtime image for puna-orchestrator. Holds the Kubernetes credential; takes no inbound
# internet traffic beyond its own /healthz, /readyz and /metrics on :9090.
#
# As with puna-web, the binary is built outside and staged as `build-result`.
FROM docker.io/library/debian:13-slim

ENV DEBIAN_FRONTEND=noninteractive
# libpq5 for diesel; ca-certificates to verify the Kubernetes API server and room certificates.
# `kube` is built with default-features = false + rustls-tls precisely so no openssl is needed.
RUN apt update && \
    apt install -y --no-install-recommends libpq5 ca-certificates && \
    rm -rf /var/lib/apt/lists/*

WORKDIR /usr/local/bin

COPY --chmod=755 build-result /usr/local/bin/puna-orchestrator
CMD ["/usr/local/bin/puna-orchestrator"]
