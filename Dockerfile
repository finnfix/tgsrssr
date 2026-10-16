# ===== Build stage =====
FROM rust:1.98-slim AS builder

WORKDIR /app

# Mirror acceleration (optional). Defaults to official sources.
# China users can override at build time:
#   docker build --build-arg APT_MIRROR=mirrors.aliyun.com \
#                --build-arg CARGO_REGISTRY=rsproxy .
ARG APT_MIRROR=""
ARG CARGO_REGISTRY=""

RUN if [ -n "$APT_MIRROR" ]; then \
      sed -i "s|deb.debian.org|${APT_MIRROR}|g" /etc/apt/sources.list.d/debian.sources; \
    fi \
    && apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

# Optional cargo registry override (e.g. rsproxy sparse index)
RUN if [ -n "$CARGO_REGISTRY" ]; then \
      mkdir -p /usr/local/cargo && printf '%s\n' \
      '[source.crates-io]' \
      'replace-with = "mirror-sparse"' \
      '[source.mirror]' \
      "registry = \"https://${CARGO_REGISTRY}/crates.io-index\"" \
      '[source.mirror-sparse]' \
      "registry = \"sparse+https://${CARGO_REGISTRY}/index/\"" \
      '[registries.mirror]' \
      "index = \"https://${CARGO_REGISTRY}/crates.io-index\"" \
      '[net]' \
      'git-fetch-with-cli = true' \
      > /usr/local/cargo/config.toml; \
    fi

# Cargo.lock is optional on first build; wildcard avoids failure if absent
COPY Cargo.toml Cargo.lock* ./

# Create dummy source to cache dependency builds
RUN mkdir -p src && echo "fn main() {}" > src/main.rs
RUN cargo build --release || true
RUN rm -rf src

# Copy real source
COPY src ./src
COPY migrations ./migrations
COPY build.rs ./
RUN touch src/main.rs

ENV SQLX_OFFLINE=true
# Disable incremental compilation: container builds once triggered a rustc ICE
ENV CARGO_INCREMENTAL=0
RUN cargo build --release --locked

# ===== Runtime stage =====
# Must match the builder stage Debian version (rust:1.98-slim is trixie-based, glibc 2.39)
FROM debian:trixie-slim

# Version label (passed by CI: --build-arg VERSION=x.y.z)
ARG VERSION=""
LABEL org.opencontainers.image.version="${VERSION}"

ARG APT_MIRROR=""
RUN if [ -n "$APT_MIRROR" ]; then \
      sed -i "s|deb.debian.org|${APT_MIRROR}|g" /etc/apt/sources.list.d/debian.sources; \
    fi \
    && apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    ffmpeg \
    && rm -rf /var/lib/apt/lists/*

# Runs as root (bind-mounted data/media/logs dirs are created by docker as root;
# a non-root user would lack write permission and panic; for a personal bot,
# container isolation is considered sufficient).

WORKDIR /app

COPY --from=builder /app/target/release/tgsrssr /usr/local/bin/tgsrssr

ENV DB_PATH=/app/data/rss_bot.db
ENV MEDIA_DIR=/app/media
ENV LOG_DIR=/app/logs

CMD ["tgsrssr"]
