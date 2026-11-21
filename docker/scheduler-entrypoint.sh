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
#    路径存在但不是普通文件（例如 bind-mount 的宿主文件不存在时 Docker
#    自动建出的目录）→ 报错退出，不静默降级到 env 模式。
#
# 2. 单行 env 模式（向后兼容，简单场景）
#    RSS_CRON_SCHEDULE  五段 cron，例如 "15 8 * * *"（每天 08:15）
#                       或 "0 */6 * * *"（每 6 小时）
#    RSS_CRON_COMMAND   rss-ai-news 全局 flag + 子命令，按 shell 命令行写，例如：
#                         "run"                     完整 ingest+ai-run+publish
#                         "ingest --batch-size 50"  只抓取
#                         "run --max-batches 3"     限制批数
#                         "rebuild-report --output 'daily report.md'"
#                                                   含空格的参数照常加引号
#    生成的一行是 `<RSS_CRON_SCHEDULE> /usr/local/bin/rss-ai-news --config-dir
#    /app/configs <RSS_CRON_COMMAND>`。supercronic 用 `/bin/sh -c` 执行每行命令，
#    所以 RSS_CRON_COMMAND 只被 shell 解析一次（不再包第二层 `sh -c '...'`，
#    引号不会被外层截断）。两个变量都不得含换行 / 回车（否则会多出 crontab 行）。
#
# log：supercronic 0.2.49 默认不原样透传（-passthrough-logs 默认 false）：
# job 的每行输出被包装成 supercronic 自己的一条日志（channel=stdout|stderr），
# 写到 supercronic 的 stderr，`docker logs <container>` 可见。
# crontab 语法可用 `supercronic -test <file>` 预检（不执行 job）。

set -eu

RSS_CRONTAB_FILE="${RSS_CRONTAB_FILE:-/app/crontab}"

# `-L` as well as `-e`: a dangling symlink fails both `-e` and `-f` and would
# otherwise fall through to the image's default env schedule (R3-16). A
# symlink to a regular file passes `-f` and is used as is.
if { [ -e "$RSS_CRONTAB_FILE" ] || [ -L "$RSS_CRONTAB_FILE" ]; } && [ ! -f "$RSS_CRONTAB_FILE" ]; then
    # 典型原因：bind-mount 的宿主文件不存在，Docker 在容器里建了同名目录；或悬空符号链接。
    echo "[scheduler] $RSS_CRONTAB_FILE exists but is not a regular file (a bind mount of a missing host file, or a dangling symlink?); refusing to start" >&2
    exit 1
fi

if [ -f "$RSS_CRONTAB_FILE" ] && [ -s "$RSS_CRONTAB_FILE" ]; then
    # 外挂模式：直接把用户挂载的 crontab 交给 supercronic。
    # 用户自己负责完整命令前缀（含 binary 路径、--config-dir 等）。
    echo "[scheduler] using mounted crontab: $RSS_CRONTAB_FILE"
    exec /usr/local/bin/supercronic "$RSS_CRONTAB_FILE"
fi

# 单行 env 模式 fallback。
: "${RSS_CRON_SCHEDULE:?RSS_CRON_SCHEDULE must be set (e.g. '15 8 * * *') when no $RSS_CRONTAB_FILE is mounted}"
: "${RSS_CRON_COMMAND:?RSS_CRON_COMMAND must be set (e.g. 'run') when no $RSS_CRONTAB_FILE is mounted}"

# 换行 / 回车会把一个变量拆成多条 crontab 行（额外的 job 或 env 行）。
newline='
'
carriage_return="$(printf '\r')"
for value in "$RSS_CRON_SCHEDULE" "$RSS_CRON_COMMAND"; do
    case "$value" in
        *"$newline"* | *"$carriage_return"*)
            echo "[scheduler] RSS_CRON_SCHEDULE and RSS_CRON_COMMAND must be single lines (no newline or carriage return)" >&2
            exit 1
            ;;
    esac
done

CRONTAB_FILE="$(mktemp)"
# 单层命令行：supercronic 以 `/bin/sh -c '<命令部分>'` 执行，RSS_CRON_COMMAND
# 只经这一次 shell 解析，其中的引号按用户所写生效。
printf '%s /usr/local/bin/rss-ai-news --config-dir /app/configs %s\n' \
    "$RSS_CRON_SCHEDULE" "$RSS_CRON_COMMAND" > "$CRONTAB_FILE"

echo "[scheduler] starting generated schedule"

# 不传 -passthrough-logs：保留 supercronic 的包装日志（带 job.command 与
# channel，便于在 docker logs 里区分各 job）；CI smoke 在这些行里找标记。
exec /usr/local/bin/supercronic "$CRONTAB_FILE"
