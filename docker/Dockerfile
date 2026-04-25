FROM rust:1.94-slim AS builder

WORKDIR /app
COPY . .
RUN cargo build --release --workspace

FROM debian:slim AS runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY --from=builder /app/target/release/rss-ai-news /usr/local/bin/rss-ai-news

CMD ["rss-ai-news", "--help"]
