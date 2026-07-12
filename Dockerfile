# syntax=docker/dockerfile:1

ARG RUST_VERSION=1.96
ARG ALPINE_VERSION=3.24

FROM rust:${RUST_VERSION}-alpine${ALPINE_VERSION} AS builder

WORKDIR /build

COPY Cargo.toml Cargo.lock README.md LICENSE ./
COPY crates ./crates

RUN cargo build --locked --release --package rs-suno --bin suno

FROM alpine:${ALPINE_VERSION}

RUN apk add --no-cache ca-certificates ffmpeg \
    && addgroup -S -g 10001 suno \
    && adduser -S -D -H -u 10001 -G suno suno \
    && install -d -o suno -g suno /config /library

COPY --from=builder /build/target/release/suno /usr/local/bin/suno
COPY LICENSE /usr/share/licenses/rs-suno/LICENSE

LABEL org.opencontainers.image.source="https://github.com/teh-hippo/rs-suno" \
    org.opencontainers.image.description="A download-only command-line tool for mirroring a Suno library." \
    org.opencontainers.image.licenses="MIT AND GPL-2.0-or-later AND LGPL-2.1-or-later"

ENV HOME=/config \
    XDG_CONFIG_HOME=/config

WORKDIR /library
USER 10001:10001

ENTRYPOINT ["/usr/local/bin/suno"]
CMD ["--help"]
