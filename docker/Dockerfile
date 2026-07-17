# syntax=docker/dockerfile:1.7

############################
# Stage 1: deps cache
############################
FROM rust:1.97.0-slim-bookworm AS deps

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

############################
# Stage 5: scheduler image (常驻容器 + 内置 cron 周期触发 single-shot CLI)
############################
# 设计意图：保持 binary 自身 single-shot 不变（blueprint §14 硬约束），
# 在外层套一个 supercronic（github.com/aptible/supercronic）按 crontab
# 表达式周期触发 `rss-ai-news <subcommand>`。supercronic 相比 cron 的优势：
#   - 非 root 友好（继承 appuser），不需要 setuid
#   - cron job 的 stdout/stderr 直接透传到 supercronic 自身的 stdout，
#     `docker logs` 可见，无需 syslog
#   - 单 binary 部署，零运行时依赖
FROM runtime AS scheduler

USER root

ARG SUPERCRONIC_VERSION=0.2.30
# SHA1 checksum 来源：https://github.com/aptible/supercronic/releases/tag/v0.2.30
# （supercronic 官方 release 用 sha1sum 校验，不是 sha256）
ARG SUPERCRONIC_SHA1_AMD64=9f27ad28c5c57cd133325b2a66bba69ba2235799
ARG SUPERCRONIC_SHA1_ARM64=d5e02aa760b3d434bc7b991777aa89ef4a503e49

RUN set -eux; \
    apt-get update; \
    apt-get install -y --no-install-recommends ca-certificates wget; \
    arch="$(dpkg --print-architecture)"; \
    case "$arch" in \
        amd64) sc_arch=amd64; sc_sha=$SUPERCRONIC_SHA1_AMD64;; \
        arm64) sc_arch=arm64; sc_sha=$SUPERCRONIC_SHA1_ARM64;; \
        *) echo "unsupported arch: $arch" >&2; exit 1;; \
    esac; \
    wget -q -O /usr/local/bin/supercronic \
        "https://github.com/aptible/supercronic/releases/download/v${SUPERCRONIC_VERSION}/supercronic-linux-${sc_arch}"; \
    echo "${sc_sha}  /usr/local/bin/supercronic" | sha1sum -c -; \
    chmod +x /usr/local/bin/supercronic; \
    apt-get purge -y wget; \
    apt-get autoremove -y; \
    rm -rf /var/lib/apt/lists/*

COPY docker/scheduler-entrypoint.sh /usr/local/bin/scheduler-entrypoint.sh
RUN chmod +x /usr/local/bin/scheduler-entrypoint.sh

USER appuser

# 默认 cron 表达式：每天 08:15（基于 container TZ）；可被 env 覆盖。
# RSS_CRON_COMMAND 默认 "run"（等价于 ingest -> ai-run -> publish 完整流程）。
ENV RSS_CRON_SCHEDULE="15 8 * * *" \
    RSS_CRON_COMMAND="run"

ENTRYPOINT ["/usr/local/bin/scheduler-entrypoint.sh"]
CMD []
