#!/usr/bin/env sh
# Tests for release-promotion.sh; run by the static acceptance lane.
set -eu

script="$(dirname "$0")/release-promotion.sh"
failures=0

expect() {
    # expect <description> <expected stdout or "exit:N"> <version> <tags...>
    description="$1"
    expected="$2"
    version="$3"
    shift 3
    set +e
    actual="$(printf '%s\n' "$@" | sh "$script" "$version" 2>/dev/null)"
    code=$?
    set -e
    if [ "$code" -ne 0 ]; then
        actual="exit:$code"
    else
        actual="$(printf '%s' "$actual" | tr '\n' ' ')"
    fi
    if [ "$actual" = "$expected" ]; then
        echo "ok   - $description"
    else
        echo "FAIL - $description: expected '$expected', got '$actual'"
        failures=$((failures + 1))
    fi
}

both='latest=true minor=true'
expect "highest tag promotes both" "$both" 0.10.4 v0.9.1 v0.10.3 v0.10.4
expect "numeric, not lexical, order (0.10 > 0.9)" "$both" 0.10.0 v0.9.9 v0.10.0
expect "re-run of an old tag moves nothing" 'latest=false minor=false' \
    0.10.3 v0.10.3 v0.10.4
expect "backport is highest of its minor only" 'latest=false minor=true' \
    0.9.2 v0.9.1 v0.9.2 v0.10.4
expect "prerelease tags are ignored" "$both" 0.10.4 v0.10.4 v0.10.5-rc.1 v1.0.0-alpha.1
expect "ls-remote format with peeled lines" "$both" 0.10.4 \
    "$(printf 'aaa\trefs/tags/v0.10.3')" "$(printf 'bbb\trefs/tags/v0.10.4')" \
    "$(printf 'ccc\trefs/tags/v0.10.4^{}')"
expect "1.0.0 outranks 0.99.99" 'latest=false minor=true' 0.99.99 v0.99.99 v1.0.0
expect "version missing from the listing fails" 'exit:1' 0.10.5 v0.10.4
expect "empty listing fails" 'exit:1' 0.10.4 ''
expect "malformed version is rejected" 'exit:2' 0.10 v0.10.4
expect "prerelease version is rejected" 'exit:2' 0.10.5-rc.1 v0.10.5-rc.1

if [ "$failures" -ne 0 ]; then
    echo "$failures release-promotion case(s) failed" >&2
    exit 1
fi
