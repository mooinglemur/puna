# Runtime image for puna-web. Serves both PUNA_ROLE=web and PUNA_ROLE=tracker -- one image,
# two Deployments, so the roles can never drift to different builds.
#
# The binary is built OUTSIDE this image and staged here as `build-result`, matching the
# convention Archipelago-lobby's pipeline already uses. That keeps the cargo cache in the CI
# job (where it is keyed per branch and actually reused) rather than in a Docker layer.
#
# Static assets are compiled into the binary by rust-embed, so nothing else is copied.
FROM docker.io/library/debian:13-slim

ENV DEBIAN_FRONTEND=noninteractive
# ca-certificates only, and that is not an oversight. Puna reaches Postgres through
# diesel-async -> tokio-postgres, which is pure Rust: diesel is built with `postgres_backend`
# (types) and never the libpq-backed `postgres` feature, so there is no libpq to install.
# Confirmed with ldd: the binary links libc, libm and libgcc_s and nothing else. No openssl
# either -- rustls with the ring provider is used throughout.
RUN apt update && \
    apt install -y --no-install-recommends ca-certificates && \
    rm -rf /var/lib/apt/lists/*

WORKDIR /usr/local/bin

COPY --chmod=755 build-result /usr/local/bin/puna-web
CMD ["/usr/local/bin/puna-web"]
