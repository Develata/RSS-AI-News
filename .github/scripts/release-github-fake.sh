#!/usr/bin/env sh
# File-backed stand-in for release-github-api.sh, used by
# release-github-test.sh. State lives in $FAKE_GITHUB:
#   tags              the remote's tags, one `<tag> <commit>` per line
#   releases/<tag>    `<id> <draft> <prerelease>` of that tag's Release
#   title/<tag>, notes/<tag>
#                     what create wrote
#   latest            the tag GitHub marks Latest (absent: none, a 404)
#   writes.log        one line per successful write
#   calls.log         every command, in call order, before it runs (so a
#                     write attempt that fails is still recorded)
# Fault injection: FAKE_GH_FAIL=<command> makes that command fail;
# FAKE_GH_FAIL_AFTER_WRITE=<command> makes it fail once a write happened;
# FAKE_GH_DUPLICATE=1 lists every Release twice; FAKE_GH_CREATE_DRAFT=1
# leaves a created Release a draft; FAKE_GH_IGNORE_LATEST=1 makes create and
# set-latest leave Latest where it was (read-back checks);
# FAKE_GH_MOVE_TAG_ON=<command> with FAKE_GH_MOVE_TAG_TO=<sha> points every
# tag at <sha> while that command runs (a tag moved between two reads).

set -eu

dir="${FAKE_GITHUB:?FAKE_GITHUB is not set}"
command="${1:-}"
echo "$command" >>"$dir/calls.log"
if [ "$command" = "${FAKE_GH_MOVE_TAG_ON:-}" ]; then
    awk -v c="${FAKE_GH_MOVE_TAG_TO:?}" '{ print $1, c }' "$dir/tags" >"$dir/tags.moved"
    mv "$dir/tags.moved" "$dir/tags"
fi
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
        [ "$#" -eq 5 ] || { echo "fake: create takes 4 arguments" >&2; exit 2; }
        awk -v t="$2" '$1 == t { found = 1 } END { exit !found }' "$dir/tags" ||
            { echo "fake: tag $2 is not on the remote" >&2; exit 1; }
        [ ! -e "$dir/releases/$2" ] || { echo "fake: a Release of $2 exists" >&2; exit 1; }
        id=$((900 + $(find "$dir/releases" -type f | wc -l)))
        if [ -n "${FAKE_GH_CREATE_DRAFT:-}" ]; then
            echo "$id true false" >"$dir/releases/$2"
        else
            echo "$id false false" >"$dir/releases/$2"
        fi
        printf '%s\n' "$3" >"$dir/title/$2"
        cp "$4" "$dir/notes/$2"
        if [ "$5" = true ] && [ -z "${FAKE_GH_IGNORE_LATEST:-}" ]; then
            echo "$2" >"$dir/latest"
        fi
        echo "create $2 latest=$5" >>"$dir/writes.log"
        ;;
    set-latest)
        tag=""
        for f in "$dir"/releases/*; do
            [ -f "$f" ] || continue
            if [ "$(cat "$f")" = "$2 false false" ]; then tag="$(basename "$f")"; fi
        done
        [ -n "$tag" ] || { echo "fake: no published Release with id $2" >&2; exit 1; }
        [ -n "${FAKE_GH_IGNORE_LATEST:-}" ] || echo "$tag" >"$dir/latest"
        echo "set-latest $tag" >>"$dir/writes.log"
        ;;
    latest)
        [ -f "$dir/latest" ] || { echo "fake: HTTP 404" >&2; exit 1; }
        cat "$dir/latest"
        ;;
    tag-commit)
        awk -v t="$2" '$1 == t { print $2 }' "$dir/tags"
        ;;
    *)
        echo "fake: unknown command '$command'" >&2
        exit 2
        ;;
esac
