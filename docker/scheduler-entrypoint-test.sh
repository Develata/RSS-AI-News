#!/usr/bin/env sh
# Tests for scheduler-entrypoint.sh; run by the static acceptance lane.
# The entrypoint is run from a copy whose supercronic path points at a stub
# that prints its arguments, so no real scheduler is ever started.
set -eu

here="$(cd "$(dirname "$0")" && pwd)"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
failures=0

printf '#!/bin/sh\necho "supercronic-stub $*"\n' >"$work/supercronic"
chmod +x "$work/supercronic"
sed "s#/usr/local/bin/supercronic#$work/supercronic#g" "$here/scheduler-entrypoint.sh" >"$work/entrypoint.sh"

# expect <description> <exit code> <output substring> [VAR=value ...]
expect() {
    description="$1"
    want_code="$2"
    want_text="$3"
    shift 3
    set +e
    output="$(env -i PATH="$PATH" "$@" sh "$work/entrypoint.sh" 2>&1)"
    code=$?
    set -e
    if [ "$code" -eq "$want_code" ] && printf '%s' "$output" | grep -Fq -- "$want_text"; then
        echo "ok   - $description"
    else
        echo "FAIL - $description: exit $code (want $want_code), output: $output"
        failures=$((failures + 1))
    fi
}

env_schedule="RSS_CRON_SCHEDULE=15 8 * * *"
env_command="RSS_CRON_COMMAND=run"

mkdir "$work/crontab-dir"
expect "a directory crontab is refused" 1 "not a regular file" \
    "RSS_CRONTAB_FILE=$work/crontab-dir" "$env_schedule" "$env_command"

ln -s "$work/missing-target" "$work/dangling"
expect "a dangling symlink crontab is refused, not replaced by the env schedule" 1 \
    "not a regular file" "RSS_CRONTAB_FILE=$work/dangling" "$env_schedule" "$env_command"

printf '0 * * * * /usr/local/bin/rss-ai-news --config-dir /app/configs run\n' >"$work/crontab"
ln -s "$work/crontab" "$work/linked"
expect "a symlink to a regular crontab is used" 0 "supercronic-stub $work/linked" \
    "RSS_CRONTAB_FILE=$work/linked" "$env_schedule" "$env_command"
expect "a mounted crontab wins over the env schedule" 0 "using mounted crontab" \
    "RSS_CRONTAB_FILE=$work/crontab" "$env_schedule" "$env_command"

: >"$work/empty"
expect "an empty crontab falls back to the env schedule" 0 "starting generated schedule" \
    "RSS_CRONTAB_FILE=$work/empty" "$env_schedule" "$env_command"
expect "no crontab and no env schedule fails" 1 "RSS_CRON_SCHEDULE must be set" \
    "RSS_CRONTAB_FILE=$work/absent"
expect "a newline in the command is refused" 1 "single lines" \
    "RSS_CRONTAB_FILE=$work/absent" "$env_schedule" "RSS_CRON_COMMAND=run
ingest"
expect "a carriage return in the schedule is refused" 1 "single lines" \
    "RSS_CRONTAB_FILE=$work/absent" "RSS_CRON_SCHEDULE=15 8 * * *$(printf '\r')" "$env_command"

if [ "$failures" -ne 0 ]; then
    echo "$failures scheduler-entrypoint case(s) failed" >&2
    exit 1
fi
echo "all scheduler-entrypoint cases passed"
