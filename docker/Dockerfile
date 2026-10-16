# syntax=docker/dockerfile:1.7

############################
# Builder (cargo-chef dependency layer)
############################
# CI and release persist BuildKit layers through `cache-to: type=gha`, which
# does not export `RUN --mount=type=cache` contents. cargo-chef turns the
# dependency build into an ordinary layer keyed by a manifest-only recipe
# (Cargo.lock + manifests, workspace member versions masked), so a source edit
# rebuilds only the workspace crates instead of every dependency.
FROM rust:1.97.0-slim-bookworm AS chef

ARG CARGO_CHEF_VERSION=0.1.78
# SHA-256 verified against the release's .sha256 assets for v0.1.78.
# https://github.com/LukeMathWalker/cargo-chef/releases/tag/v0.1.78
ARG CARGO_CHEF_SHA256_AMD64=aca691abfbfbbe00d482e0ed2249eec3091b65e96a0ce92947fb5b254d48b16d
ARG CARGO_CHEF_SHA256_ARM64=cd59b90fce5fa84c4189648fec33404ec5491791eebdb812df5e7fca8336408d

RUN set -eux; \
    apt-get update; \
    apt-get install -y --no-install-recommends ca-certificates wget xz-utils; \
    arch="$(dpkg --print-architecture)"; \
    case "$arch" in \
        amd64) chef_target=x86_64-unknown-linux-musl; chef_sha=$CARGO_CHEF_SHA256_AMD64;; \
        arm64) chef_target=aarch64-unknown-linux-musl; chef_sha=$CARGO_CHEF_SHA256_ARM64;; \
        *) echo "unsupported arch: $arch" >&2; exit 1;; \
    esac; \
    wget -q -O /tmp/cargo-chef.tar.xz \
        "https://github.com/LukeMathWalker/cargo-chef/releases/download/v${CARGO_CHEF_VERSION}/cargo-chef-${chef_target}.tar.xz"; \
    echo "${chef_sha}  /tmp/cargo-chef.tar.xz" | sha256sum -c -; \
    tar -xJf /tmp/cargo-chef.tar.xz -C /tmp; \
    install -m 0755 "/tmp/cargo-chef-${chef_target}/cargo-chef" /usr/local/cargo/bin/cargo-chef; \
    rm -rf /tmp/cargo-chef* /var/lib/apt/lists/*

WORKDIR /app
# Install what rust-toolchain.toml pins (toolchain + components) once, in a
# cached layer; otherwise rustup re-downloads the components in every later
# stage that copies the file in.
COPY rust-toolchain.toml ./
RUN rustup toolchain install --no-self-update

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
# Same profile, flags and binary as the final build, so the cooked dependency
# artifacts are reused rather than rebuilt.
RUN cargo chef cook --release --locked --bin rss-ai-news --recipe-path recipe.json
COPY . .
RUN cargo build --release --locked --bin rss-ai-news && \
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
