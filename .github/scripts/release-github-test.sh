#!/usr/bin/env sh
# Tests for release-github.sh against a file-backed fake GitHub
# (release-github-fake.sh), and for the real adapter release-github-api.sh
# with a stub gh; run by the static acceptance lane (needs jq).
set -eu

here="$(cd "$(dirname "$0")" && pwd)"
script="$here/release-github.sh"
adapter="$here/release-github-api.sh"
RELEASE_GITHUB="$here/release-github-fake.sh"
export RELEASE_GITHUB
failures=0
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
case_no=0
START='<!-- release-notes:start -->'
END='<!-- release-notes:end -->'
SHA=0123456789abcdef0123456789abcdef01234567

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
    mkdir -p "$FAKE_GITHUB/releases" "$FAKE_GITHUB/title" "$FAKE_GITHUB/notes" "$FAKE_GITHUB/target"
    : >"$FAKE_GITHUB/tags"
    : >"$FAKE_GITHUB/writes.log"
    for t in "$@"; do echo "$t" >>"$FAKE_GITHUB/tags"; done
    unset FAKE_GH_FAIL FAKE_GH_FAIL_AFTER_WRITE FAKE_GH_DUPLICATE FAKE_GH_CREATE_DRAFT FAKE_GH_IGNORE_LATEST || true
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

latest() {
    echo "$1" >"$FAKE_GITHUB/latest"
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

# ensure VERSION LATEST [NOTES]: runs the script at commit $SHA; sets out and rc.
ensure() {
    set +e
    out="$(sh "$script" ensure "$1" "$SHA" "$2" "${3:-$work/notes.md}" 2>"$work/stderr")"
    rc=$?
    set -e
}

# notes ARGS...: runs `notes`; sets out and rc.
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
printf '# Release v1.2.3\n\n- 状态：draft\n%s\n\n## 核心交付\n\n- 修复 A  \n  续行，见 [plan](https://example.test/p.md#s) 与 [下文](#x)\n\n%s\n\n## 验收证据\n' "$START" "$END" >"$report"
notes 1.2.3 "$report" https://example.test/r.md
expect_rc 0
expected="$(printf '## 核心交付\n\n- 修复 A  \n  续行，见 [plan](https://example.test/p.md#s) 与 [下文](#x)\n\n完整发布报告（含 tag 后补录的发布证据）：[%s](https://example.test/r.md)' "$report")"
expect_out "$expected"
done_case

start "notes: without a URL, no link line"
printf '%s\nbody\n%s\n' "$START" "$END" >"$report"
notes 1.2.3 "$report"
expect_rc 0
expect_out "body"
done_case

start "notes: a CRLF checkout reads like LF (one trailing CR per line)"
printf '%s\r\nbody\r\n%s\r\n' "$START" "$END" >"$report"
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

for link in '见 [plan/02](../../plan/02-extract.md)' '![图](img.png)' '[ref]: ../x.md' '[a](https://ok.test) 与 [b](rel.md)'; do
    start "notes: a relative link fails ($link)"
    printf '%s\n%s\n%s\n' "$START" "$link" "$END" >"$report"
    notes 1.2.3 "$report"
    expect_rc 1
    stderr_has "use an absolute https URL"
    done_case
done

start "notes: absolute, anchor and mailto links pass"
printf '%s\n[a](https://x.test) [b](#c) [d](mailto:e@x.test) [f](<https://g.test>)\n[ref]: https://h.test\n%s\n' "$START" "$END" >"$report"
notes 1.2.3 "$report"
expect_rc 0
done_case

start "notes: a malformed version is a usage error"
printf '%s\nbody\n%s\n' "$START" "$END" >"$report"
for bad in 1.2.3-rc.1 01.2.3 "$(printf '1.2.3\ninvalid')"; do
    notes "$bad" "$report"
    expect_rc 2
done
done_case

# --- ensure ------------------------------------------------------------------

start "ensure: the latest version gets a new Release marked Latest, at the run's commit" v0.10.4 v0.10.5
release v0.10.4 "false false"
latest v0.10.4
ensure 0.10.5 0.10.5
expect_rc 0
expect_out "release=v0.10.5 latest=true"
state_is v0.10.5 "false false"
latest_is v0.10.5
[ "$(cat "$FAKE_GITHUB/title/v0.10.5")" = v0.10.5 ] || fail "title is '$(cat "$FAKE_GITHUB/title/v0.10.5")'"
[ "$(cat "$FAKE_GITHUB/target/v0.10.5")" = "$SHA" ] || fail "target is '$(cat "$FAKE_GITHUB/target/v0.10.5")'"
cmp -s "$work/notes.md" "$FAKE_GITHUB/notes/v0.10.5" || fail "the body is not the notes file"
writes_are "create v0.10.5 latest=true;"
done_case

start "ensure: an older line's release is created without taking Latest" v0.9.2 v0.10.5
release v0.10.5 "false false"
latest v0.10.5
ensure 0.9.2 0.10.5
expect_rc 0
expect_out "release=v0.9.2 latest=false"
state_is v0.9.2 "false false"
latest_is v0.10.5
writes_are "create v0.9.2 latest=false;"
done_case

start "ensure: an older tag still Latest while the alias is higher fails before any write" v1.0.0
release v1.0.0 "false false"
latest v1.0.0
ensure 1.0.0 2.0.0
expect_rc 1
stderr_has "GitHub marks v1.0.0 as Latest, but the latest image alias is at 2.0.0"
stderr_has "nothing was written"
latest_is v1.0.0
no_writes
done_case

start "ensure: a higher version without any Release blocks a lower one's create" v0.9.9
ensure 0.9.9 0.10.0
expect_rc 1
stderr_has "nothing was written"
state_is v0.9.9 missing
no_writes
done_case

start "ensure: a re-run on a published Release keeps its notes and re-applies Latest" v0.10.5
release v0.10.5 "false false"
latest v0.10.4
ensure 0.10.5 0.10.5
expect_rc 0
expect_out "release=v0.10.5 latest=true"
[ "$(cat "$FAKE_GITHUB/notes/v0.10.5")" = "existing body" ] || fail "the published body was rewritten"
latest_is v0.10.5
writes_are "set-latest v0.10.5;"
done_case

start "ensure: a re-run when everything is in place writes nothing" v0.10.5
release v0.10.5 "false false"
latest v0.10.5
ensure 0.10.5 0.10.5
expect_rc 0
expect_out "release=v0.10.5 latest=true"
no_writes
done_case

start "ensure: a re-run of an older release writes nothing" v0.10.4
release v0.10.4 "false false"
latest v0.10.5
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
stderr_has "listing the Releases failed (exit 1); nothing was written"
no_writes
done_case

for setup in published older; do
    start "ensure: a Latest read error before the write fails with nothing written ($setup)" v0.10.4 v0.10.5
    if [ "$setup" = published ]; then
        release v0.10.5 "false false"
        version=0.10.5
    else
        version=0.10.4
    fi
    FAKE_GH_FAIL="latest"
    export FAKE_GH_FAIL
    ensure "$version" 0.10.5
    expect_rc 1
    stderr_has "reading GitHub's Latest Release failed"
    stderr_has "nothing was written"
    no_writes
    done_case
done

start "ensure: a failed create fails" v0.10.5
FAKE_GH_FAIL="create"
export FAKE_GH_FAIL
ensure 0.10.5 0.10.5
expect_rc 1
stderr_has "creating the Release of v0.10.5 failed (it may or may not exist now)"
done_case

start "ensure: a tag missing on the remote is never created by the Release"
ensure 0.10.5 0.10.5
expect_rc 1
stderr_has "creating the Release of v0.10.5 failed"
state_is v0.10.5 missing
done_case

start "ensure: a failed set-latest fails" v0.10.5
release v0.10.5 "false false"
latest v0.10.4
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
stderr_has "the write went through but is not confirmed"
done_case

start "ensure: Latest that does not read back fails" v0.10.5
latest v0.10.4
FAKE_GH_IGNORE_LATEST=1
export FAKE_GH_IGNORE_LATEST
ensure 0.10.5 0.10.5
expect_rc 1
stderr_has "GitHub marks v0.10.4 as Latest, expected v0.10.5"
done_case

start "ensure: a create that takes Latest although it should not fails on read-back" v0.9.2 v0.10.5
release v0.10.5 "false false"
latest v0.10.5
# The fake honours --latest; simulate GitHub moving Latest anyway.
cat >"$work/sneaky.sh" <<'EOF'
#!/usr/bin/env sh
set -eu
sh "$REAL_FAKE" "$@"
if [ "$1" = create ]; then echo "$2" >"$FAKE_GITHUB/latest"; fi
EOF
REAL_FAKE="$RELEASE_GITHUB"
RELEASE_GITHUB="$work/sneaky.sh"
export REAL_FAKE RELEASE_GITHUB
ensure 0.9.2 0.10.5
RELEASE_GITHUB="$REAL_FAKE"
export RELEASE_GITHUB
expect_rc 1
stderr_has "GitHub marks v0.9.2 as Latest, expected v0.10.5"
done_case

for command in find latest; do
    start "ensure: a $command error after the write reports it as written, not confirmed" v0.10.5
    FAKE_GH_FAIL_AFTER_WRITE="$command"
    export FAKE_GH_FAIL_AFTER_WRITE
    ensure 0.10.5 0.10.5
    expect_rc 1
    stderr_has "the write went through but is not confirmed"
    state_is v0.10.5 "false false"
    done_case
done

start "ensure: an empty or missing notes file fails with nothing written" v0.10.5
: >"$work/empty.md"
ensure 0.10.5 0.10.5 "$work/empty.md"
expect_rc 1
ensure 0.10.5 0.10.5 "$work/absent.md"
expect_rc 1
no_writes
done_case

start "ensure: malformed versions and commits are rejected with nothing written" v0.10.5
ensure 0.10.5-rc.1 0.10.5
expect_rc 2
ensure "$(printf '0.10.5\nx')" 0.10.5
expect_rc 2
ensure 0.10.5 latest
expect_rc 1
stderr_has "is not X.Y.Z"
set +e
sh "$script" ensure 0.10.5 abc123 0.10.5 "$work/notes.md" 2>"$work/stderr"
rc=$?
set -e
expect_rc 2
no_writes
done_case

# --- release-github-api.sh (the real adapter) with a stub gh ----------------
# Stub gh logs its argv (one argument per line, then `--`) to $STUB_GH/argv.
# `gh api --paginate <path> --jq <filter>` applies the filter with jq to each
# $STUB_GH/page<N>.json in turn (as gh does per page); page<N>.rc makes that
# page's request fail after the earlier pages printed. `gh api <path> --jq
# <filter>` reads $STUB_GH/single.json. `gh release ...` exits with
# $STUB_GH/write.rc (default 0).

mkdir -p "$work/bin"
cat >"$work/bin/gh" <<'EOF'
#!/usr/bin/env sh
set -eu
for a in "$@"; do printf '%s\n' "$a" >>"$STUB_GH/argv"; done
echo -- >>"$STUB_GH/argv"
if [ "$1" = release ]; then
    exit "$(cat "$STUB_GH/write.rc" 2>/dev/null || echo 0)"
fi
[ "$1" = api ] || exit 9
shift
paginate=false
if [ "$1" = --paginate ]; then paginate=true; shift; fi
path="$1"
[ "$2" = --jq ] || exit 9
filter="$3"
: "$path"
if [ "$paginate" = true ]; then
    n=1
    while [ -f "$STUB_GH/page$n.json" ]; do
        if [ -f "$STUB_GH/page$n.rc" ]; then exit "$(cat "$STUB_GH/page$n.rc")"; fi
        jq -r "$filter" "$STUB_GH/page$n.json" || exit 1
        n=$((n + 1))
    done
else
    jq -r "$filter" "$STUB_GH/single.json" || exit 1
fi
EOF
chmod +x "$work/bin/gh"

start_adapter() {
    description="adapter: $1"
    case_no=$((case_no + 1))
    case_failures=$failures
    STUB_GH="$work/$case_no"
    export STUB_GH
    mkdir -p "$STUB_GH"
    : >"$STUB_GH/argv"
}

# adapter ARGS...: runs the real adapter with the stub gh; sets out and rc.
adapter() {
    set +e
    out="$(PATH="$work/bin:$PATH" GITHUB_REPOSITORY=o/r sh "$adapter" "$@" 2>"$work/stderr")"
    rc=$?
    set -e
}

argv_is() {
    actual="$(tr '\n' ' ' <"$STUB_GH/argv")"
    [ "$actual" = "$1" ] || fail "gh argv is '$actual', expected '$1'"
}

not_called() {
    [ ! -s "$STUB_GH/argv" ] || fail "gh was called: $(tr '\n' ' ' <"$STUB_GH/argv")"
}

entry() {
    printf '{"tag_name":"%s","draft":%s,"prerelease":%s}' "$1" "$2" "$3"
}

start_adapter "find reads every page and matches on page 2"
printf '[%s,%s]\n' "$(entry v0.9.0 false false)" "$(entry v0.10.4 false false)" >"$STUB_GH/page1.json"
printf '[%s]\n' "$(entry v0.10.5 true false)" >"$STUB_GH/page2.json"
adapter find v0.10.5
expect_rc 0
expect_out "true false"
head -3 "$STUB_GH/argv" | tr '\n' ' ' | grep -qx 'api --paginate repos/o/r/releases?per_page=100 ' ||
    fail "gh argv starts '$(head -3 "$STUB_GH/argv" | tr '\n' ' ')'"
done_case

start_adapter "find with no match prints nothing and succeeds"
printf '[%s]\n' "$(entry v0.9.0 false false)" >"$STUB_GH/page1.json"
printf '[]\n' >"$STUB_GH/page2.json"
adapter find v0.10.5
expect_rc 0
expect_out ""
done_case

start_adapter "find fails when a later page fails"
printf '[%s]\n' "$(entry v0.9.0 false false)" >"$STUB_GH/page1.json"
printf '[]\n' >"$STUB_GH/page2.json"
echo 1 >"$STUB_GH/page2.rc"
adapter find v0.10.5
[ "$rc" -ne 0 ] || fail "exit 0"
done_case

for body in '[{"draft":true,"prerelease":false}]' '[{"tag_name":"v0.10.5","draft":"false","prerelease":false}]' '{"message":"Not Found"}' '[1]'; do
    start_adapter "find rejects a malformed listing: $body"
    printf '%s\n' "$body" >"$STUB_GH/page1.json"
    adapter find v0.10.5
    [ "$rc" -ne 0 ] || fail "exit 0, output '$out'"
    done_case
done

for bad in 0.10.5 v0.10 "$(printf 'v0.10.5\nx')" 'v0.10.5"'; do
    start_adapter "find refuses tag '$(printf '%s' "$bad" | tr '\n' '|')' without calling gh"
    adapter find "$bad"
    expect_rc 1
    not_called
    done_case
done

for flag in false true; do
    start_adapter "create passes --verify-tag, --target and --latest=$flag"
    echo body >"$STUB_GH/notes.md"
    adapter create v0.10.5 v0.10.5 "$STUB_GH/notes.md" "$flag" "$SHA"
    expect_rc 0
    argv_is "release create v0.10.5 --repo o/r --verify-tag --target $SHA --title v0.10.5 --notes-file $STUB_GH/notes.md --latest=$flag -- "
    done_case
done

start_adapter "create refuses a bad latest value or commit without calling gh"
echo body >"$STUB_GH/notes.md"
adapter create v0.10.5 v0.10.5 "$STUB_GH/notes.md" yes "$SHA"
expect_rc 1
adapter create v0.10.5 v0.10.5 "$STUB_GH/notes.md" true "main"
expect_rc 1
not_called
done_case

start_adapter "create reports gh's failure"
echo body >"$STUB_GH/notes.md"
echo 1 >"$STUB_GH/write.rc"
adapter create v0.10.5 v0.10.5 "$STUB_GH/notes.md" true "$SHA"
[ "$rc" -ne 0 ] || fail "exit 0"
done_case

start_adapter "set-latest edits the Release with --latest"
adapter set-latest v0.10.5
expect_rc 0
argv_is "release edit v0.10.5 --repo o/r --latest -- "
done_case

start_adapter "latest reads the tag of releases/latest"
printf '{"tag_name":"v0.9.0"}\n' >"$STUB_GH/single.json"
adapter latest
expect_rc 0
expect_out v0.9.0
argv_is "api repos/o/r/releases/latest --jq .tag_name -- "
done_case

if [ "$failures" -ne 0 ]; then
    echo "$failures release-github check(s) failed" >&2
    exit 1
fi
echo "all release-github cases passed"
