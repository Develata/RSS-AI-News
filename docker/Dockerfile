# syntax=docker/dockerfile:1.7

############################
# Stage 1: deps cache
############################
FROM rust:1.94-slim-bookworm AS deps

WORKDIR /app

# Copy workspace + crate manifests (and Cargo.lock) only.
COPY Cargo.toml Cargo.lock ./
COPY crates/cli/Cargo.toml         crates/cli/Cargo.toml
COPY crates/domain/Cargo.toml      crates/domain/Cargo.toml
COPY crates/config/Cargo.toml      crates/config/Cargo.toml
COPY crates/runtime/Cargo.toml     crates/runtime/Cargo.toml
COPY crates/storage/Cargo.toml     crates/storage/Cargo.toml
COPY crates/feed/Cargo.toml        crates/feed/Cargo.toml
COPY crates/extractor/Cargo.toml   crates/extractor/Cargo.toml
COPY crates/ai/Cargo.toml          crates/ai/Cargo.toml
COPY crates/report/Cargo.toml      crates/report/Cargo.toml
COPY crates/publish/Cargo.toml     crates/publish/Cargo.toml
COPY crates/observability/Cargo.toml crates/observability/Cargo.toml

# Create stub sources so cargo can resolve and pre-build deps.
RUN set -eux; \
    mkdir -p src; \
    echo 'fn main() {}' > src/main.rs; \
    for c in cli domain config runtime storage feed extractor ai report publish observability; do \
        mkdir -p "crates/$c/src"; \
        : > "crates/$c/src/lib.rs"; \
    done

# Pre-build dependencies. Allowed to fail on stub bodies; the goal is to populate
# the registry / git / target caches.
RUN cargo build --release --workspace --bin rss-ai-news || true

############################
# Stage 2: full build
############################
FROM deps AS builder

WORKDIR /app

# Bring in the real workspace sources. .dockerignore filters target/ etc.
COPY . .

# Force re-build now that real sources replace the stubs. The deps stage produced
# .rmeta/.rlib for stub lib.rs files; nuke their fingerprints so cargo rebuilds the
# workspace crates against the real sources, then bump every .rs mtime so cargo
# also detects source changes regardless of how COPY normalized them.
RUN set -eux; \
    rm -rf target/release/.fingerprint/rss-ai-news* \
           target/release/deps/librss_ai_news* \
           target/release/deps/rss_ai_news*; \
    find . -path ./target -prune -o -name '*.rs' -print -exec touch {} +; \
    cargo build --release --bin rss-ai-news; \
    strip target/release/rss-ai-news || true

############################
# Stage 3: production runtime
############################
FROM debian:bookworm-slim AS runtime

ENV TZ=Asia/Shanghai \
    RUST_LOG=info

RUN set -eux; \
    apt-get update; \
    apt-get install -y --no-install-recommends \
        ca-certificates \
        tzdata; \
    rm -rf /var/lib/apt/lists/*; \
    groupadd --system --gid 10001 appgroup; \
    useradd --system --uid 10001 --gid appgroup \
        --home-dir /app --shell /usr/sbin/nologin appuser; \
    mkdir -p /app/data /app/configs; \
    chown -R appuser:appgroup /app

WORKDIR /app

COPY --from=builder /app/target/release/rss-ai-news /usr/local/bin/rss-ai-news

USER appuser

ENTRYPOINT ["/usr/local/bin/rss-ai-news"]
CMD ["--help"]

############################
# Stage 4: debug image (built on top of runtime layers)
############################
FROM runtime AS debug

USER root

RUN set -eux; \
    apt-get update; \
    apt-get install -y --no-install-recommends \
        bash \
        curl \
        sqlite3 \
        less \
        jq \
        procps \
        tini; \
    rm -rf /var/lib/apt/lists/*

USER appuser

ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/rss-ai-news"]
CMD ["--help"]
