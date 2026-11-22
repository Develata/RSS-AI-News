#!/usr/bin/env sh
# File-backed stand-in for release-registry.sh, used by
# release-promotion-test.sh. State lives in $FAKE_REGISTRY:
#   tags/<tag>        the digest the tag points at
#   blobs/<digest>    `<version-label> <revision-label>` of that manifest
#   writes.log        one line per successful copy
# Fault injection: FAKE_FAIL_INSPECT=<tag> makes inspecting that tag a read
# error; FAKE_FAIL_COPY=<tag> makes writing it fail; FAKE_COPY_WRONG=<tag>
# makes the write point it at another digest (read-back check).

set -eu

dir="${FAKE_REGISTRY:?FAKE_REGISTRY is not set}"

case "${1:-}" in
    inspect)
        tag="${2##*:}"
        [ "$tag" != "${FAKE_FAIL_INSPECT:-}" ] || { echo "fake: read error for $tag" >&2; exit 1; }
        [ -f "$dir/tags/$tag" ] || exit 3
        digest="$(cat "$dir/tags/$tag")"
        printf '%s %s\n' "$digest" "$(cat "$dir/blobs/$digest")"
        ;;
    copy)
        digest="${2##*@}"
        tag="${3##*:}"
        [ "$tag" != "${FAKE_FAIL_COPY:-}" ] || { echo "fake: write error for $tag" >&2; exit 1; }
        [ -f "$dir/blobs/$digest" ] || { echo "fake: unknown digest $digest" >&2; exit 1; }
        if [ "$tag" = "${FAKE_COPY_WRONG:-}" ]; then
            digest="sha256:wrong"
            echo "- -" >"$dir/blobs/$digest"
        fi
        printf '%s\n' "$digest" >"$dir/tags/$tag"
        echo "$tag $digest" >>"$dir/writes.log"
        ;;
    *)
        echo "fake: unknown command '${1:-}'" >&2
        exit 2
        ;;
esac
