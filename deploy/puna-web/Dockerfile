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
# libpq5 for diesel's postgres backend; ca-certificates for Discord's API and for verifying
# room certificates when the tracker proxies. No openssl: rustls is used throughout.
RUN apt update && \
    apt install -y --no-install-recommends libpq5 ca-certificates && \
    rm -rf /var/lib/apt/lists/*

WORKDIR /usr/local/bin

COPY --chmod=755 build-result /usr/local/bin/puna-web
CMD ["/usr/local/bin/puna-web"]
