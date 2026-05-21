#!/usr/bin/env sh
# rss-ai-news scheduler 容器入口。
#
# 设计前提（blueprint §14）：rss-ai-news 二进制本身不内置定时器；
# 「调度」职责完全外化给这个 entrypoint + supercronic
# （github.com/aptible/supercronic）。
#
# 两种调度形态（优先级从高到低）：
#
# 1. 多行 crontab 文件（外挂模式，推荐用于分离 ingest/publish 节奏）
#    RSS_CRONTAB_FILE  指向一个 supercronic 兼容的 crontab 文件，
#                      默认 /app/crontab。文件存在且非空时被直接使用，
#                      跳过 RSS_CRON_SCHEDULE / RSS_CRON_COMMAND。
#    文件每一行格式：<五段 cron>  <要在容器内执行的命令>
#    例：
#      0 */3 * * * sh -c '/usr/local/bin/rss-ai-news --config-dir /app/configs ingest && /usr/local/bin/rss-ai-news --config-dir /app/configs ai-run'
#      30 21 * * * /usr/local/bin/rss-ai-news --config-dir /app/configs publish
#
# 2. 单行 env 模式（向后兼容，简单场景）
#    RSS_CRON_SCHEDULE  五段 cron，例如 "15 8 * * *"（每天 08:15）
#                       或 "0 */6 * * *"（每 6 小时）
#    RSS_CRON_COMMAND   rss-ai-news 子命令字符串，例如：
#                         "run"                     完整 ingest+ai-run+publish
#                         "ingest --batch-size 50"  只抓取
#                         "run --max-batches 3"     限制批数
#
# log：supercronic 把 cron job 的 stdout/stderr 透传到自身 stdout，
# `docker logs <container>` 可见。

set -eu

RSS_CRONTAB_FILE="${RSS_CRONTAB_FILE:-/app/crontab}"

if [ -s "$RSS_CRONTAB_FILE" ]; then
    # 外挂模式：直接把用户挂载的 crontab 交给 supercronic。
    # 用户自己负责完整命令前缀（含 binary 路径、--config-dir 等）。
    echo "[scheduler] using mounted crontab: $RSS_CRONTAB_FILE"
    echo "[scheduler] crontab contents:"
    cat "$RSS_CRONTAB_FILE"
    exec /usr/local/bin/supercronic "$RSS_CRONTAB_FILE"
fi

# 单行 env 模式 fallback。
: "${RSS_CRON_SCHEDULE:?RSS_CRON_SCHEDULE must be set (e.g. '15 8 * * *') when no $RSS_CRONTAB_FILE is mounted}"
: "${RSS_CRON_COMMAND:?RSS_CRON_COMMAND must be set (e.g. 'run') when no $RSS_CRONTAB_FILE is mounted}"

CRONTAB_FILE="$(mktemp)"
# 用 `sh -c` 包一层，确保多 token 命令（如 "run --max-batches 3"）也能跑。
cat > "$CRONTAB_FILE" <<EOF
${RSS_CRON_SCHEDULE} sh -c '/usr/local/bin/rss-ai-news --config-dir /app/configs ${RSS_CRON_COMMAND}'
EOF

echo "[scheduler] generated crontab from env:"
cat "$CRONTAB_FILE"
echo "[scheduler] supercronic starting (schedule='${RSS_CRON_SCHEDULE}', command='${RSS_CRON_COMMAND}')"

# `-passthrough-logs` 已是 supercronic 默认（since v0.2.x），无需显式传。
exec /usr/local/bin/supercronic "$CRONTAB_FILE"
