# syntax=docker/dockerfile:1@sha256:4edf897a3ffa55b89f906fc8cc78afdb3f1834cc9c7083565e611a8a7d5fe99e

FROM rust:1.99-alpine3.24@sha256:b1d5dd5a22a09404d3e798c0cf051b689077360374eb505fa5bb93c479693d87 AS builder

WORKDIR /build

COPY Cargo.toml Cargo.lock README.md LICENSE ./
COPY crates ./crates

RUN cargo build --locked --release --package rs-suno --bin suno

FROM alpine:3.24@sha256:71e9b7bae160ba167fa17414401b94cbcdaf3c32feeab9298bd794a2a0614c35

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
