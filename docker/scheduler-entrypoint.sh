#!/usr/bin/env sh
# rss-ai-news scheduler 容器入口。
#
# 把环境变量 RSS_CRON_SCHEDULE + RSS_CRON_COMMAND 拼成一行 crontab，
# 交给 supercronic（github.com/aptible/supercronic）周期触发底层
# `rss-ai-news` single-shot CLI。
#
# 设计前提（blueprint §14）：rss-ai-news 二进制本身不内置定时器；
# 「调度」职责完全外化给这个 entrypoint + supercronic。
#
# 可调环境变量：
#   RSS_CRON_SCHEDULE  五段式 cron 表达式，例如 "15 8 * * *"（每天 08:15）
#                      或 "0 */6 * * *"（每 6 小时）。默认见 Dockerfile ENV。
#   RSS_CRON_COMMAND   rss-ai-news 子命令字符串，例如：
#                        "run"                       — 完整 ingest+ai-run+publish
#                        "ingest --batch-size 50"   — 只抓取
#                        "run --max-batches 3"      — 限制批数
#                      默认 "run"。
#
# log：supercronic 把 cron job 的 stdout/stderr 透传到自身 stdout，
# `docker logs <container>` 可见。

set -eu

# busybox `sh` 兼容写法（Alpine 也能跑；Debian 实际是 dash）。
: "${RSS_CRON_SCHEDULE:?RSS_CRON_SCHEDULE must be set (e.g. '15 8 * * *')}"
: "${RSS_CRON_COMMAND:?RSS_CRON_COMMAND must be set (e.g. 'run')}"

CRONTAB_FILE="$(mktemp)"
# eval 让 RSS_CRON_COMMAND 里的 shell 转义被正确解析，但 supercronic 本身
# 用 exec(3) 运行命令，所以 entry 里的 word splitting 也得交给 sh -c。
# 用 `sh -c` 包一层，确保多 token 命令（如 "run --max-batches 3"）也能跑。
cat > "$CRONTAB_FILE" <<EOF
${RSS_CRON_SCHEDULE} sh -c '/usr/local/bin/rss-ai-news --config-dir /app/configs ${RSS_CRON_COMMAND}'
EOF

echo "[scheduler] crontab:"
cat "$CRONTAB_FILE"
echo "[scheduler] supercronic starting (schedule='${RSS_CRON_SCHEDULE}', command='${RSS_CRON_COMMAND}')"

# `-passthrough-logs` 已是 supercronic 默认（since v0.2.x），无需显式传。
exec /usr/local/bin/supercronic "$CRONTAB_FILE"
