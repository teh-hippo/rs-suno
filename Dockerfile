# syntax=docker/dockerfile:1@sha256:ecfaec9ed6d810b56388c508f4121597bfbba70d41a6dfeee4d8cad5f295fc32

FROM rust:1.98-alpine3.24@sha256:1716b3aa042d735f4566d14dc54e8037de9d69556e2d5dd58131d93a613d173d AS builder

WORKDIR /build

COPY Cargo.toml Cargo.lock README.md LICENSE ./
COPY crates ./crates

RUN cargo build --locked --release --package rs-suno --bin suno

FROM alpine:3.24@sha256:28bd5fe8b56d1bd048e5babf5b10710ebe0bae67db86916198a6eec434943f8b

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
