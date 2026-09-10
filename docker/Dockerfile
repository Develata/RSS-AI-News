# syntax=docker/dockerfile:1.7

FROM rust:1.97.0-slim-bookworm AS builder
WORKDIR /app
COPY . .
# Cache mounts accelerate repeated builds on the same BuildKit builder.
# Export the executable outside the target mount for later stages.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/app/target \
    cargo build --release --locked --bin rss-ai-news && \
    cp target/release/rss-ai-news /app/rss-ai-news && \
    strip /app/rss-ai-news

############################
# Production runtime
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

COPY --from=builder /app/rss-ai-news /usr/local/bin/rss-ai-news

USER appuser

ENTRYPOINT ["/usr/local/bin/rss-ai-news"]
CMD ["--help"]

############################
# Debug image (built on top of runtime layers)
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
# Scheduler image (常驻容器 + 内置 cron 周期触发 single-shot CLI)
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

ARG SUPERCRONIC_VERSION=0.2.49
# SHA-256 verified against GitHub release asset.digest for v0.2.49.
# https://github.com/aptible/supercronic/releases/tag/v0.2.49
ARG SUPERCRONIC_SHA256_AMD64=a53ae236602c7338aba3fbaff40bda6300eae3b9fedb8261eb06cfe3724430c1
ARG SUPERCRONIC_SHA256_ARM64=02aa0cb229ba09050cba6638059dadb9eedc2276632ea43d6a57a2f8c1629dd5

RUN set -eux; \
    apt-get update; \
    apt-get install -y --no-install-recommends ca-certificates wget; \
    arch="$(dpkg --print-architecture)"; \
    case "$arch" in \
        amd64) sc_arch=amd64; sc_sha=$SUPERCRONIC_SHA256_AMD64;; \
        arm64) sc_arch=arm64; sc_sha=$SUPERCRONIC_SHA256_ARM64;; \
        *) echo "unsupported arch: $arch" >&2; exit 1;; \
    esac; \
    wget -q -O /usr/local/bin/supercronic \
        "https://github.com/aptible/supercronic/releases/download/v${SUPERCRONIC_VERSION}/supercronic-linux-${sc_arch}"; \
    echo "${sc_sha}  /usr/local/bin/supercronic" | sha256sum -c -; \
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
