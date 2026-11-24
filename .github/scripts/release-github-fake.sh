#!/usr/bin/env sh
# File-backed stand-in for release-github-api.sh, used by
# release-github-test.sh. State lives in $FAKE_GITHUB:
#   tags              the remote's tags, one per line (create's --verify-tag)
#   releases/<tag>    `<draft> <prerelease>` of that tag's Release
#   title/<tag>, notes/<tag>, target/<tag>
#                     what create wrote
#   latest            the tag GitHub marks Latest (absent: none, a 404)
#   writes.log        one line per successful write
# Fault injection: FAKE_GH_FAIL=<command> makes that command fail;
# FAKE_GH_FAIL_AFTER_WRITE=<command> makes it fail once a write happened;
# FAKE_GH_DUPLICATE=1 lists every Release twice; FAKE_GH_CREATE_DRAFT=1
# leaves a created Release a draft; FAKE_GH_IGNORE_LATEST=1 makes create and
# set-latest leave Latest where it was (read-back checks).

set -eu

dir="${FAKE_GITHUB:?FAKE_GITHUB is not set}"
command="${1:-}"
[ "$command" != "${FAKE_GH_FAIL:-}" ] || { echo "fake: $command failed" >&2; exit 1; }
if [ "$command" = "${FAKE_GH_FAIL_AFTER_WRITE:-}" ] && [ -s "$dir/writes.log" ]; then
    echo "fake: $command failed after a write" >&2
    exit 1
fi

case "$command" in
    find)
        if [ -f "$dir/releases/$2" ]; then
            cat "$dir/releases/$2"
            [ -z "${FAKE_GH_DUPLICATE:-}" ] || cat "$dir/releases/$2"
        fi
        ;;
    create)
        grep -qx "$2" "$dir/tags" 2>/dev/null || { echo "fake: tag $2 is not on the remote" >&2; exit 1; }
        [ ! -e "$dir/releases/$2" ] || { echo "fake: a Release of $2 exists" >&2; exit 1; }
        if [ -n "${FAKE_GH_CREATE_DRAFT:-}" ]; then
            echo "true false" >"$dir/releases/$2"
        else
            echo "false false" >"$dir/releases/$2"
        fi
        printf '%s\n' "$3" >"$dir/title/$2"
        printf '%s\n' "$6" >"$dir/target/$2"
        cp "$4" "$dir/notes/$2"
        if [ "$5" = true ] && [ -z "${FAKE_GH_IGNORE_LATEST:-}" ]; then
            echo "$2" >"$dir/latest"
        fi
        echo "create $2 latest=$5" >>"$dir/writes.log"
        ;;
    set-latest)
        [ "$(cat "$dir/releases/$2" 2>/dev/null)" = "false false" ] ||
            { echo "fake: no published Release of $2" >&2; exit 1; }
        [ -n "${FAKE_GH_IGNORE_LATEST:-}" ] || echo "$2" >"$dir/latest"
        echo "set-latest $2" >>"$dir/writes.log"
        ;;
    latest)
        [ -f "$dir/latest" ] || { echo "fake: HTTP 404" >&2; exit 1; }
        cat "$dir/latest"
        ;;
    *)
        echo "fake: unknown command '$command'" >&2
        exit 2
        ;;
esac
