#!/usr/bin/env sh
# Decide which moving image tags a release may promote.
#
# Usage: release-promotion.sh X.Y.Z < tags
#   stdin: the repository's tags, one per line, as `vX.Y.Z`, `refs/tags/vX.Y.Z`
#          or `git ls-remote --tags` output (`<sha>\trefs/tags/vX.Y.Z`; peeled
#          `^{}` lines are ignored). Only stable `vX.Y.Z` tags count.
#   stdout: `latest=true|false` and `minor=true|false`, ready for
#           $GITHUB_OUTPUT.
#
# `latest` (and `latest-scheduler`) moves only when X.Y.Z is the highest
# stable tag in the repository; `X.Y` (and `X.Y-scheduler`) only when it is
# the highest stable tag of its X.Y line. Re-running an old tag therefore
# never moves either alias backwards. The version must itself be among the
# tags, so an empty or truncated listing fails instead of promoting.

set -eu

version="${1:-}"
case "$version" in
    *[!0-9.]* | .* | *. | *..* | "")
        echo "release-promotion: expected X.Y.Z, got '$version'" >&2
        exit 2
        ;;
esac
if [ "$(printf '%s' "$version" | tr -cd '.' | wc -c | tr -d ' ')" -ne 2 ]; then
    echo "release-promotion: expected X.Y.Z, got '$version'" >&2
    exit 2
fi

# Stable versions only, without the leading `v`, numerically sorted.
stable="$(
    sed -n 's#^.*refs/tags/##; s#^v\([0-9][0-9]*\.[0-9][0-9]*\.[0-9][0-9]*\)$#\1#p' |
        sort -t . -k1,1n -k2,2n -k3,3n -u
)"

if ! printf '%s\n' "$stable" | grep -Fqx "$version"; then
    echo "release-promotion: v$version is not among the repository's stable tags" >&2
    exit 1
fi

highest="$(printf '%s\n' "$stable" | tail -n 1)"
major="${version%%.*}"
minor="${version#*.}"
minor="${minor%.*}"
highest_in_minor="$(
    printf '%s\n' "$stable" | awk -F . -v x="$major" -v y="$minor" '$1 == x && $2 == y' | tail -n 1
)"

if [ "$version" = "$highest" ]; then
    echo "latest=true"
else
    echo "latest=false"
fi
if [ "$version" = "$highest_in_minor" ]; then
    echo "minor=true"
else
    echo "minor=false"
fi
