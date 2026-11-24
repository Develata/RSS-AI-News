#!/usr/bin/env sh
# Tests for release-github.sh against a file-backed fake GitHub
# (release-github-fake.sh); run by the static acceptance lane.
set -eu

here="$(cd "$(dirname "$0")" && pwd)"
script="$here/release-github.sh"
RELEASE_GITHUB="$here/release-github-fake.sh"
export RELEASE_GITHUB
failures=0
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
case_no=0
START='<!-- release-notes:start -->'
END='<!-- release-notes:end -->'

fail() {
    echo "FAIL - $description: $*"
    failures=$((failures + 1))
}

# Starts a case with an empty fake GitHub whose remote has the given tags.
start() {
    description="$1"
    shift
    case_no=$((case_no + 1))
    case_failures=$failures
    FAKE_GITHUB="$work/$case_no"
    export FAKE_GITHUB
    mkdir -p "$FAKE_GITHUB/releases" "$FAKE_GITHUB/title" "$FAKE_GITHUB/notes"
    : >"$FAKE_GITHUB/tags"
    : >"$FAKE_GITHUB/writes.log"
    for t in "$@"; do echo "$t" >>"$FAKE_GITHUB/tags"; done
    unset FAKE_GH_FAIL FAKE_GH_DUPLICATE FAKE_GH_CREATE_DRAFT FAKE_GH_IGNORE_LATEST || true
    printf 'notes for this release\n' >"$work/notes.md"
}

done_case() {
    if [ "$case_failures" -eq "$failures" ]; then
        echo "ok   - $description"
    fi
}

# release TAG STATE: an existing Release (`<draft> <prerelease>`).
release() {
    echo "$2" >"$FAKE_GITHUB/releases/$1"
    echo "existing body" >"$FAKE_GITHUB/notes/$1"
}

latest_is() {
    actual="$(cat "$FAKE_GITHUB/latest" 2>/dev/null || echo none)"
    [ "$actual" = "$1" ] || fail "Latest is '$actual', expected $1"
}

state_is() {
    actual="$(cat "$FAKE_GITHUB/releases/$1" 2>/dev/null || echo missing)"
    [ "$actual" = "$2" ] || fail "the Release of $1 is '$actual', expected '$2'"
}

no_writes() {
    [ ! -s "$FAKE_GITHUB/writes.log" ] || fail "unexpected writes: $(tr '\n' ' ' <"$FAKE_GITHUB/writes.log")"
}

writes_are() {
    actual="$(tr '\n' ';' <"$FAKE_GITHUB/writes.log")"
    [ "$actual" = "$1" ] || fail "writes are '$actual', expected '$1'"
}

# ensure VERSION LATEST [NOTES]: runs the script; sets out and rc.
ensure() {
    set +e
    out="$(sh "$script" ensure "$1" "$2" "${3:-$work/notes.md}" 2>"$work/stderr")"
    rc=$?
    set -e
}

# notes ARGS...: runs `notes` on $work/report.md; sets out and rc.
notes() {
    set +e
    out="$(sh "$script" notes "$@" 2>"$work/stderr")"
    rc=$?
    set -e
}

expect_rc() {
    [ "$rc" -eq "$1" ] || fail "exit $rc, expected $1 (stderr: $(cat "$work/stderr"))"
}

expect_out() {
    [ "$out" = "$1" ] || fail "output is '$out', expected '$1'"
}

stderr_has() {
    grep -qF -- "$1" "$work/stderr" || fail "stderr lacks '$1': $(cat "$work/stderr")"
}

# --- notes -------------------------------------------------------------------

report="$work/report.md"

start "notes: the lines between the markers, trimmed, then the report link"
printf '# Release v1.2.3\n\n- 状态：draft\n%s\n\n## 核心交付\n\n- 修复 A\n  续行\n\n%s\n\n## 验收证据\n' "$START" "$END" >"$report"
notes 1.2.3 "$report" https://example.test/r.md
expect_rc 0
expected="$(printf '## 核心交付\n\n- 修复 A\n  续行\n\n完整发布报告（含 tag 后补录的发布证据）：https://example.test/r.md')"
expect_out "$expected"
done_case

start "notes: without a URL, no link line"
printf '%s\nbody\n%s\n' "$START" "$END" >"$report"
notes 1.2.3 "$report"
expect_rc 0
expect_out "body"
done_case

start "notes: a missing report fails"
notes 1.2.3 "$work/absent.md"
expect_rc 1
stderr_has "write the v1.2.3 release report"
done_case

start "notes: a report without markers fails"
printf '# Release v1.2.3\n\nbody\n' >"$report"
notes 1.2.3 "$report"
expect_rc 1
stderr_has "exactly once each"
done_case

start "notes: a repeated start marker fails"
printf '%s\na\n%s\nb\n%s\n' "$START" "$START" "$END" >"$report"
notes 1.2.3 "$report"
expect_rc 1
stderr_has "exactly once each"
done_case

start "notes: a repeated end marker fails"
printf '%s\na\n%s\nb\n%s\n' "$START" "$END" "$END" >"$report"
notes 1.2.3 "$report"
expect_rc 1
stderr_has "exactly once each"
done_case

start "notes: the end marker before the start fails"
printf '%s\na\n%s\nb\n' "$END" "$START" >"$report"
notes 1.2.3 "$report"
expect_rc 1
stderr_has "comes before"
done_case

start "notes: only blank lines between the markers fail"
printf '%s\n\n  \n\t\n%s\n' "$START" "$END" >"$report"
notes 1.2.3 "$report"
expect_rc 1
stderr_has "no release notes between"
done_case

start "notes: a marker is a whole line (indented or trailing text does not count)"
printf '  %s\nbody\n%s trailing\n' "$START" "$END" >"$report"
notes 1.2.3 "$report"
expect_rc 1
stderr_has "exactly once each"
done_case

start "notes: a CRLF marker line does not count"
printf '%s\r\nbody\r\n%s\r\n' "$START" "$END" >"$report"
notes 1.2.3 "$report"
expect_rc 1
done_case

start "notes: a malformed version is a usage error"
printf '%s\nbody\n%s\n' "$START" "$END" >"$report"
notes 1.2.3-rc.1 "$report"
expect_rc 2
notes 01.2.3 "$report"
expect_rc 2
done_case

# --- ensure ------------------------------------------------------------------

start "ensure: the latest version gets a new Release marked Latest" v0.10.4 v0.10.5
release v0.10.4 "false false"
echo v0.10.4 >"$FAKE_GITHUB/latest"
ensure 0.10.5 0.10.5
expect_rc 0
expect_out "release=v0.10.5 latest=true"
state_is v0.10.5 "false false"
latest_is v0.10.5
[ "$(cat "$FAKE_GITHUB/title/v0.10.5")" = v0.10.5 ] || fail "title is '$(cat "$FAKE_GITHUB/title/v0.10.5")'"
cmp -s "$work/notes.md" "$FAKE_GITHUB/notes/v0.10.5" || fail "the body is not the notes file"
writes_are "create v0.10.5 latest=true;"
done_case

start "ensure: an older line's release is created without taking Latest" v0.9.2 v0.10.5
release v0.10.5 "false false"
echo v0.10.5 >"$FAKE_GITHUB/latest"
ensure 0.9.2 0.10.5
expect_rc 0
expect_out "release=v0.9.2 latest=false"
state_is v0.9.2 "false false"
latest_is v0.10.5
writes_are "create v0.9.2 latest=false;"
done_case

start "ensure: numeric order is the alias's business, the script compares for equality only" v0.9.9
ensure 0.9.9 0.10.0
expect_rc 0
expect_out "release=v0.9.9 latest=false"
latest_is none
done_case

start "ensure: a re-run on a published Release keeps its notes and re-applies Latest" v0.10.5
release v0.10.5 "false false"
echo v0.10.4 >"$FAKE_GITHUB/latest"
ensure 0.10.5 0.10.5
expect_rc 0
expect_out "release=v0.10.5 latest=true"
[ "$(cat "$FAKE_GITHUB/notes/v0.10.5")" = "existing body" ] || fail "the published body was rewritten"
latest_is v0.10.5
writes_are "set-latest v0.10.5;"
done_case

start "ensure: a re-run of an older release writes nothing" v0.10.4
release v0.10.4 "false false"
echo v0.10.5 >"$FAKE_GITHUB/latest"
ensure 0.10.4 0.10.5
expect_rc 0
expect_out "release=v0.10.4 latest=false"
latest_is v0.10.5
no_writes
done_case

start "ensure: a draft Release fails with nothing written" v0.10.5
release v0.10.5 "true false"
ensure 0.10.5 0.10.5
expect_rc 1
stderr_has "is a draft"
state_is v0.10.5 "true false"
no_writes
done_case

start "ensure: a prerelease Release fails with nothing written" v0.10.5
release v0.10.5 "false true"
ensure 0.10.5 0.10.5
expect_rc 1
stderr_has "is a prerelease"
no_writes
done_case

start "ensure: several Releases of the tag fail with nothing written" v0.10.5
release v0.10.5 "true false"
FAKE_GH_DUPLICATE=1
export FAKE_GH_DUPLICATE
ensure 0.10.5 0.10.5
expect_rc 1
stderr_has "2 Releases name v0.10.5"
no_writes
done_case

start "ensure: a listing error fails with nothing written" v0.10.5
FAKE_GH_FAIL="find"
export FAKE_GH_FAIL
ensure 0.10.5 0.10.5
expect_rc 1
stderr_has "listing the Releases failed"
no_writes
done_case

start "ensure: a failed create fails" v0.10.5
FAKE_GH_FAIL="create"
export FAKE_GH_FAIL
ensure 0.10.5 0.10.5
expect_rc 1
stderr_has "creating the Release of v0.10.5 failed"
done_case

start "ensure: a tag missing on the remote is never created by the Release"
ensure 0.10.5 0.10.5
expect_rc 1
stderr_has "creating the Release of v0.10.5 failed"
state_is v0.10.5 missing
done_case

start "ensure: a failed set-latest fails" v0.10.5
release v0.10.5 "false false"
FAKE_GH_FAIL="set-latest"
export FAKE_GH_FAIL
ensure 0.10.5 0.10.5
expect_rc 1
stderr_has "marking the Release of v0.10.5 Latest failed"
done_case

start "ensure: a Release that reads back as a draft fails" v0.10.5
FAKE_GH_CREATE_DRAFT=1
export FAKE_GH_CREATE_DRAFT
ensure 0.10.5 0.10.5
expect_rc 1
stderr_has "reads back as 'true false'"
done_case

start "ensure: Latest that does not read back fails" v0.10.5
echo v0.10.4 >"$FAKE_GITHUB/latest"
FAKE_GH_IGNORE_LATEST=1
export FAKE_GH_IGNORE_LATEST
ensure 0.10.5 0.10.5
expect_rc 1
stderr_has "GitHub marks 'v0.10.4' as Latest, expected v0.10.5"
done_case

start "ensure: an unreadable Latest after the write fails" v0.10.5
FAKE_GH_FAIL="latest"
export FAKE_GH_FAIL
ensure 0.10.5 0.10.5
expect_rc 1
stderr_has "reading GitHub's Latest Release failed"
done_case

start "ensure: an empty or missing notes file fails with nothing written" v0.10.5
: >"$work/empty.md"
ensure 0.10.5 0.10.5 "$work/empty.md"
expect_rc 1
ensure 0.10.5 0.10.5 "$work/absent.md"
expect_rc 1
no_writes
done_case

start "ensure: malformed versions are rejected with nothing written" v0.10.5
ensure 0.10.5-rc.1 0.10.5
expect_rc 2
ensure 0.10.5 latest
expect_rc 1
stderr_has "is not X.Y.Z"
no_writes
done_case

if [ "$failures" -ne 0 ]; then
    echo "$failures release-github check(s) failed" >&2
    exit 1
fi
echo "all release-github cases passed"
