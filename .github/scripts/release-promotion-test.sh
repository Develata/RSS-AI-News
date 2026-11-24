#!/usr/bin/env sh
# Tests for release-promotion.sh against a file-backed fake registry
# (release-registry-fake.sh); run by the static acceptance lane.
set -eu

here="$(cd "$(dirname "$0")" && pwd)"
script="$here/release-promotion.sh"
RELEASE_REGISTRY="$here/release-registry-fake.sh"
export RELEASE_REGISTRY
IMAGE=ghcr.io/example/rss
failures=0
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
case_no=0

fail() {
    echo "FAIL - $description: $*"
    failures=$((failures + 1))
}

# Starts a case with an empty registry.
start() {
    description="$1"
    case_no=$((case_no + 1))
    FAKE_REGISTRY="$work/$case_no"
    export FAKE_REGISTRY
    mkdir -p "$FAKE_REGISTRY/tags" "$FAKE_REGISTRY/blobs"
    : >"$FAKE_REGISTRY/writes.log"
    unset FAKE_FAIL_INSPECT FAKE_FAIL_COPY FAKE_COPY_WRONG || true
}

done_case() {
    if [ "$case_failures" -eq "$failures" ]; then
        echo "ok   - $description"
    fi
}

# publish VERSION COMMIT: both version tags, as publish-image pushes them.
publish() {
    printf '%s %s\n' "$1" "$2" >"$FAKE_REGISTRY/blobs/sha256:rt-$1"
    printf 'sha256:rt-%s\n' "$1" >"$FAKE_REGISTRY/tags/$1"
    printf '%s-scheduler %s\n' "$1" "$2" >"$FAKE_REGISTRY/blobs/sha256:sc-$1"
    printf 'sha256:sc-%s\n' "$1" >"$FAKE_REGISTRY/tags/$1-scheduler"
}

# point ALIAS VERSION: ALIAS and ALIAS-scheduler at VERSION's images.
point() {
    printf 'sha256:rt-%s\n' "$2" >"$FAKE_REGISTRY/tags/$1"
    printf 'sha256:sc-%s\n' "$2" >"$FAKE_REGISTRY/tags/$1-scheduler"
}

# tag VERSION COMMIT -> a lightweight tag's ls-remote line.
tag() {
    printf '%s\trefs/tags/v%s\n' "$2" "$1"
}

# reconcile VERSION < listing: runs the script; sets out and rc.
reconcile() {
    set +e
    out="$(sh "$script" reconcile "$IMAGE" "$1" 2>"$work/stderr")"
    rc=$?
    set -e
}

expect_rc() {
    [ "$rc" -eq "$1" ] || fail "exit $rc, expected $1 (stderr: $(cat "$work/stderr"))"
}

# alias_at ALIAS VERSION: both aliases point at VERSION's images.
alias_at() {
    runtime="$(cat "$FAKE_REGISTRY/tags/$1" 2>/dev/null || echo missing)"
    scheduler="$(cat "$FAKE_REGISTRY/tags/$1-scheduler" 2>/dev/null || echo missing)"
    [ "$runtime" = "sha256:rt-$2" ] || fail "$1 is '$runtime', expected $2"
    [ "$scheduler" = "sha256:sc-$2" ] || fail "$1-scheduler is '$scheduler', expected $2"
}

absent() {
    [ ! -e "$FAKE_REGISTRY/tags/$1" ] || fail "$1 exists"
}

no_writes() {
    [ ! -s "$FAKE_REGISTRY/writes.log" ] || fail "unexpected writes: $(tr '\n' ' ' <"$FAKE_REGISTRY/writes.log")"
}

# --- reconcile ---------------------------------------------------------------

start "highest release creates latest and X.Y"
case_failures=$failures
publish 0.10.3 c3
publish 0.10.4 c4
point latest 0.10.3
point 0.10 0.10.3
{ tag 0.9.1 c1; tag 0.10.3 c3; tag 0.10.4 c4; } >"$work/tags"
reconcile 0.10.4 <"$work/tags"
expect_rc 0
alias_at latest 0.10.4
alias_at 0.10 0.10.4
done_case

start "numeric, not lexical, order (0.10 > 0.9)"
case_failures=$failures
publish 0.9.9 c9
publish 0.10.0 c10
{ tag 0.9.9 c9; tag 0.10.0 c10; } >"$work/tags"
reconcile 0.9.9 <"$work/tags"
expect_rc 0
alias_at latest 0.10.0
alias_at 0.9 0.9.9
alias_at 0.10 0.10.0
done_case

start "a lower version's run still promotes the highest (displaced pending run)"
case_failures=$failures
publish 1.4.1 c141
publish 2.0.0 c200
{ tag 1.4.1 c141; tag 2.0.0 c200; } >"$work/tags"
reconcile 1.4.1 <"$work/tags"
expect_rc 0
alias_at latest 2.0.0
alias_at 2.0 2.0.0
alias_at 1.4 1.4.1
done_case

start "a deleted higher tag never moves latest or X.Y backwards"
case_failures=$failures
publish 1.4.3 c143
publish 1.4.4 c144
point latest 1.4.4
point 1.4 1.4.4
tag 1.4.3 c143 >"$work/tags"
reconcile 1.4.3 <"$work/tags"
expect_rc 0
alias_at latest 1.4.4
alias_at 1.4 1.4.4
no_writes
done_case

start "re-run of an old tag moves nothing"
case_failures=$failures
publish 0.10.3 c3
publish 0.10.4 c4
point latest 0.10.4
point 0.10 0.10.4
{ tag 0.10.3 c3; tag 0.10.4 c4; } >"$work/tags"
reconcile 0.10.3 <"$work/tags"
expect_rc 0
alias_at latest 0.10.4
no_writes
done_case

start "backport moves only its own line"
case_failures=$failures
publish 0.9.1 c91
publish 0.9.2 c92
publish 0.10.4 c4
point latest 0.10.4
point 0.10 0.10.4
point 0.9 0.9.1
{ tag 0.9.1 c91; tag 0.9.2 c92; tag 0.10.4 c4; } >"$work/tags"
reconcile 0.9.2 <"$work/tags"
expect_rc 0
alias_at latest 0.10.4
alias_at 0.9 0.9.2
done_case

start "a higher tag without both images is not released yet"
case_failures=$failures
publish 0.10.4 c4
printf '0.10.5 c5\n' >"$FAKE_REGISTRY/blobs/sha256:rt-0.10.5"
printf 'sha256:rt-0.10.5\n' >"$FAKE_REGISTRY/tags/0.10.5"
{ tag 0.10.4 c4; tag 0.10.5 c5; } >"$work/tags"
reconcile 0.10.4 <"$work/tags"
expect_rc 0
alias_at latest 0.10.4
done_case

start "annotated tags: the peeled commit is the revision"
case_failures=$failures
publish 0.10.4 commit4
{
    printf 'tagobject4\trefs/tags/v0.10.4\n'
    printf 'commit4\trefs/tags/v0.10.4^{}\n'
} >"$work/tags"
reconcile 0.10.4 <"$work/tags"
expect_rc 0
alias_at latest 0.10.4
# Whatever the line order, the peeled line wins over the tag object's SHA.
rm "$FAKE_REGISTRY/tags/latest" "$FAKE_REGISTRY/tags/latest-scheduler"
{
    printf 'commit4\trefs/tags/v0.10.4^{}\n'
    printf 'tagobject4\trefs/tags/v0.10.4\n'
} >"$work/tags"
reconcile 0.10.4 <"$work/tags"
expect_rc 0
alias_at latest 0.10.4
done_case

start "version image from another commit fails (immutable version tags)"
case_failures=$failures
publish 0.10.4 other-commit
tag 0.10.4 c4 >"$work/tags"
reconcile 0.10.4 <"$work/tags"
expect_rc 1
no_writes
done_case

start "alias without a valid version label is never moved"
case_failures=$failures
publish 0.10.4 c4
printf '%s\n' '- -' >"$FAKE_REGISTRY/blobs/sha256:unlabelled"
printf 'sha256:unlabelled\n' >"$FAKE_REGISTRY/tags/latest"
tag 0.10.4 c4 >"$work/tags"
reconcile 0.10.4 <"$work/tags"
expect_rc 1
no_writes
done_case

start "alias with a non-SemVer version label is never moved"
case_failures=$failures
publish 0.10.4 c4
printf '%s\n' '00.10.9 cx' >"$FAKE_REGISTRY/blobs/sha256:odd"
printf 'sha256:odd\n' >"$FAKE_REGISTRY/tags/latest"
tag 0.10.4 c4 >"$work/tags"
reconcile 0.10.4 <"$work/tags"
expect_rc 1
no_writes
done_case

start "a read error writes nothing"
case_failures=$failures
publish 0.10.4 c4
tag 0.10.4 c4 >"$work/tags"
FAKE_FAIL_INSPECT=latest-scheduler
export FAKE_FAIL_INSPECT
reconcile 0.10.4 <"$work/tags"
expect_rc 1
unset FAKE_FAIL_INSPECT
# Every alias is read before the first write (R3G-B3): a failed read of the
# scheduler alias leaves the runtime alias unwritten too.
no_writes
FAKE_FAIL_INSPECT=0.10.4
export FAKE_FAIL_INSPECT
: >"$FAKE_REGISTRY/writes.log"
rm -f "$FAKE_REGISTRY/tags/latest"
reconcile 0.10.4 <"$work/tags"
expect_rc 1
no_writes
done_case

start "an invalid scheduler alias stops the run before the runtime alias is written"
case_failures=$failures
publish 0.10.4 c4
publish 0.10.5 c5
point latest 0.10.4
point 0.10 0.10.4
printf '%s\n' 'garbage cx' >"$FAKE_REGISTRY/blobs/sha256:garbage"
printf 'sha256:garbage\n' >"$FAKE_REGISTRY/tags/latest-scheduler"
{ tag 0.10.4 c4; tag 0.10.5 c5; } >"$work/tags"
reconcile 0.10.5 <"$work/tags"
expect_rc 1
no_writes
[ "$(cat "$FAKE_REGISTRY/tags/latest")" = sha256:rt-0.10.4 ] || fail "runtime alias moved"
# A second run fails the same way and still writes nothing (no split pair).
reconcile 0.10.5 <"$work/tags"
expect_rc 1
no_writes
done_case

start "an invalid alias of a later line stops the run before any line is written"
case_failures=$failures
publish 0.10.4 c4
publish 0.10.5 c5
point latest 0.10.4
point 0.10 0.10.4
printf '%s\n' 'garbage cx' >"$FAKE_REGISTRY/blobs/sha256:garbage"
printf 'sha256:garbage\n' >"$FAKE_REGISTRY/tags/0.10-scheduler"
{ tag 0.10.4 c4; tag 0.10.5 c5; } >"$work/tags"
reconcile 0.10.5 <"$work/tags"
expect_rc 1
no_writes
alias_at latest 0.10.4
done_case

start "an interrupted promotion (scheduler write failed) is completed by a re-run"
case_failures=$failures
publish 0.10.3 c3
publish 0.10.4 c4
point latest 0.10.3
point 0.10 0.10.3
{ tag 0.10.3 c3; tag 0.10.4 c4; } >"$work/tags"
FAKE_FAIL_COPY=latest-scheduler
export FAKE_FAIL_COPY
reconcile 0.10.4 <"$work/tags"
expect_rc 1
[ "$(cat "$FAKE_REGISTRY/tags/latest")" = sha256:rt-0.10.4 ] || fail "runtime alias was not written first"
[ "$(cat "$FAKE_REGISTRY/tags/latest-scheduler")" = sha256:sc-0.10.3 ] || fail "scheduler alias changed"
unset FAKE_FAIL_COPY
reconcile 0.10.4 <"$work/tags"
expect_rc 0
alias_at latest 0.10.4
alias_at 0.10 0.10.4
done_case

start "a write that reads back another digest fails"
case_failures=$failures
publish 0.10.4 c4
tag 0.10.4 c4 >"$work/tags"
FAKE_COPY_WRONG=latest
export FAKE_COPY_WRONG
reconcile 0.10.4 <"$work/tags"
expect_rc 1
done_case

start "runtime and scheduler aliases left at different versions fail"
case_failures=$failures
publish 1.4.3 c143
publish 1.4.4 c144
point latest 1.4.3
printf 'sha256:rt-1.4.4\n' >"$FAKE_REGISTRY/tags/latest"
point 1.4 1.4.3
tag 1.4.3 c143 >"$work/tags"
reconcile 1.4.3 <"$work/tags"
expect_rc 1
no_writes
done_case

# swap_version_tags VERSION: V points at the scheduler image and vice versa.
swap_version_tags() {
    printf 'sha256:sc-%s\n' "$1" >"$FAKE_REGISTRY/tags/$1"
    printf 'sha256:rt-%s\n' "$1" >"$FAKE_REGISTRY/tags/$1-scheduler"
}

start "swapped version tags are not a released version"
case_failures=$failures
publish 0.10.4 c4
swap_version_tags 0.10.4
tag 0.10.4 c4 >"$work/tags"
reconcile 0.10.4 <"$work/tags"
expect_rc 1
no_writes
done_case

start "swapped aliases are never moved or kept"
case_failures=$failures
publish 0.10.4 c4
publish 0.10.5 c5
printf 'sha256:sc-0.10.4\n' >"$FAKE_REGISTRY/tags/latest"
printf 'sha256:rt-0.10.4\n' >"$FAKE_REGISTRY/tags/latest-scheduler"
{ tag 0.10.4 c4; tag 0.10.5 c5; } >"$work/tags"
reconcile 0.10.5 <"$work/tags"
expect_rc 1
no_writes
# The same swap at a newer version than the target is not kept either.
point 0.10 0.10.4
printf 'sha256:sc-0.10.5\n' >"$FAKE_REGISTRY/tags/latest"
printf 'sha256:rt-0.10.5\n' >"$FAKE_REGISTRY/tags/latest-scheduler"
tag 0.10.4 c4 >"$work/tags"
reconcile 0.10.4 <"$work/tags"
expect_rc 1
no_writes
done_case

start "a retained higher pair with different revisions fails"
case_failures=$failures
publish 1.4.3 c143
publish 1.4.4 c144
point latest 1.4.4
point 1.4 1.4.4
printf '1.4.4-scheduler other\n' >"$FAKE_REGISTRY/blobs/sha256:sc-other"
printf 'sha256:sc-other\n' >"$FAKE_REGISTRY/tags/latest-scheduler"
tag 1.4.3 c143 >"$work/tags"
reconcile 1.4.3 <"$work/tags"
expect_rc 1
no_writes
done_case

start "a retained higher pair must be the exact version tags' images"
case_failures=$failures
publish 1.4.3 c143
publish 1.4.4 c144
point latest 1.4.4
point 1.4 1.4.4
# Same labels as 1.4.4, another image (e.g. a rebuild pushed to the alias).
printf '1.4.4 c144\n' >"$FAKE_REGISTRY/blobs/sha256:rt-rebuilt"
printf 'sha256:rt-rebuilt\n' >"$FAKE_REGISTRY/tags/latest"
tag 1.4.3 c143 >"$work/tags"
reconcile 1.4.3 <"$work/tags"
expect_rc 1
no_writes
done_case

start "a retained higher pair without its version tags fails closed"
case_failures=$failures
publish 1.4.3 c143
publish 1.4.4 c144
point latest 1.4.4
point 1.4 1.4.4
rm "$FAKE_REGISTRY/tags/1.4.4" "$FAKE_REGISTRY/tags/1.4.4-scheduler"
tag 1.4.3 c143 >"$work/tags"
reconcile 1.4.3 <"$work/tags"
expect_rc 1
no_writes
done_case

start "prerelease tags are ignored"
case_failures=$failures
publish 0.10.4 c4
{ tag 0.10.4 c4; tag 0.10.5-rc.1 c5; tag 1.0.0-alpha.1 c6; } >"$work/tags"
reconcile 0.10.4 <"$work/tags"
expect_rc 0
alias_at latest 0.10.4
done_case

start "tags with leading-zero components are not versions (cannot block promotion)"
case_failures=$failures
publish 1.4.3 c143
{ tag 1.4.3 c143; tag 01.4.99 cx; tag 1.04.0 cy; tag 1.4.007 cz; } >"$work/tags"
reconcile 1.4.3 <"$work/tags"
expect_rc 0
alias_at latest 1.4.3
alias_at 1.4 1.4.3
reconcile 01.4.99 <"$work/tags"
expect_rc 2
done_case

# 2^53 + 1 and 2^53 are equal as doubles: components must compare exactly,
# with no digit cap (R3G-S8, same rule as the Rust acceptance validator).
start "components beyond 2^53 and 9 digits compare exactly"
case_failures=$failures
big_hi=9007199254740993.0.0
big_lo=9007199254740992.0.0
publish "$big_hi" chi
publish "$big_lo" clo
publish 1000000000.0.0 cg
{ tag "$big_lo" clo; tag "$big_hi" chi; tag 1000000000.0.0 cg; } >"$work/tags"
reconcile "$big_lo" <"$work/tags"
expect_rc 0
alias_at latest "$big_hi"
alias_at 9007199254740992.0 "$big_lo"
alias_at 9007199254740993.0 "$big_hi"
done_case

start "an alias beyond 2^53 is not moved down to a version it rounds to"
case_failures=$failures
publish "$big_hi" chi
publish "$big_lo" clo
point latest "$big_hi"
point 9007199254740992.0 "$big_lo"
tag "$big_lo" clo >"$work/tags"
reconcile "$big_lo" <"$work/tags"
expect_rc 0
alias_at latest "$big_hi"
no_writes
done_case

start "same-length components compare digit by digit"
case_failures=$failures
publish 1.19.0 c119
publish 1.20.0 c120
publish 1.9.0 c19
{ tag 1.9.0 c19; tag 1.19.0 c119; tag 1.20.0 c120; } >"$work/tags"
reconcile 1.9.0 <"$work/tags"
expect_rc 0
alias_at latest 1.20.0
alias_at 1.9 1.9.0
done_case

start "version missing from the listing fails"
case_failures=$failures
publish 0.10.5 c5
tag 0.10.4 c4 >"$work/tags"
reconcile 0.10.5 <"$work/tags"
expect_rc 1
no_writes
done_case

start "empty listing fails"
case_failures=$failures
: >"$work/tags"
reconcile 0.10.4 <"$work/tags"
expect_rc 1
done_case

start "malformed and prerelease versions are rejected"
case_failures=$failures
reconcile 0.10 </dev/null
expect_rc 2
reconcile 0.10.5-rc.1 </dev/null
expect_rc 2
done_case

# --- existing (version-tag reuse) --------------------------------------------

existing() {
    set +e
    out="$(sh "$script" existing "$IMAGE" "$1" "$2" 2>"$work/stderr")"
    rc=$?
    set -e
}

start "re-run reuses existing version tags built from the same commit"
case_failures=$failures
publish 0.10.4 c4
existing 0.10.4 c4
expect_rc 0
[ "$out" = "$(printf 'runtime-digest=sha256:rt-0.10.4\nscheduler-digest=sha256:sc-0.10.4')" ] ||
    fail "got '$out'"
done_case

start "a missing version tag is built (only the missing side)"
case_failures=$failures
publish 0.10.4 c4
rm "$FAKE_REGISTRY/tags/0.10.4-scheduler"
existing 0.10.4 c4
expect_rc 0
[ "$out" = "$(printf 'runtime-digest=sha256:rt-0.10.4\nscheduler-digest=')" ] || fail "got '$out'"
done_case

start "an existing version tag from another commit is never overwritten"
case_failures=$failures
publish 0.10.4 other
existing 0.10.4 c4
expect_rc 1
done_case

start "swapped version tags from the same commit are never reused"
case_failures=$failures
publish 0.10.4 c4
swap_version_tags 0.10.4
existing 0.10.4 c4
expect_rc 1
done_case

start "a runtime tag with a scheduler label is never reused"
case_failures=$failures
publish 0.10.4 c4
printf '0.10.4-scheduler c4\n' >"$FAKE_REGISTRY/blobs/sha256:rt-0.10.4"
existing 0.10.4 c4
expect_rc 1
done_case

# --- tag-commit (the remote tag still names the run's commit) ---------------

tag_commit() {
    set +e
    out="$(sh "$script" tag-commit "$1" "$2" <"$work/tags" 2>"$work/stderr")"
    rc=$?
    set -e
}

start "tag-commit: a lightweight tag at the run's commit passes"
case_failures=$failures
tag 1.0.0 c100 >"$work/tags"
tag_commit 1.0.0 c100
expect_rc 0
done_case

start "tag-commit: an annotated tag is compared by its peeled commit"
case_failures=$failures
{
    printf 'tagobject\trefs/tags/v1.0.0\n'
    printf 'c100\trefs/tags/v1.0.0^{}\n'
} >"$work/tags"
tag_commit 1.0.0 c100
expect_rc 0
tag_commit 1.0.0 tagobject
expect_rc 1
done_case

start "tag-commit: a tag moved to another commit stops the publish"
case_failures=$failures
tag 1.0.0 c-moved >"$work/tags"
tag_commit 1.0.0 c100
expect_rc 1
done_case

start "tag-commit: a deleted tag stops the publish"
case_failures=$failures
: >"$work/tags"
tag_commit 1.0.0 c100
expect_rc 1
# Only the exact tag counts, not a prerelease or longer name at that commit.
{ tag 1.0.0-rc.1 c100; tag 11.0.0 c100; } >"$work/tags"
tag_commit 1.0.0 c100
expect_rc 1
done_case

start "tag-commit: a malformed version is rejected"
case_failures=$failures
tag 1.0.0 c100 >"$work/tags"
tag_commit 1.0 c100
expect_rc 2
tag_commit "$(printf '1.0.0\nx')" c100
expect_rc 2
done_case

# --- alias-version (the version the latest alias carries) -------------------

alias_version() {
    set +e
    out="$(sh "$script" alias-version "$IMAGE" "$1" 2>"$work/stderr")"
    rc=$?
    set -e
}

start "alias-version: prints the runtime alias's version label"
case_failures=$failures
publish 0.10.5 c5
point latest 0.10.5
alias_version latest
expect_rc 0
[ "$out" = 0.10.5 ] || fail "printed '$out', expected 0.10.5"
no_writes
done_case

start "alias-version: a missing alias fails"
case_failures=$failures
alias_version latest
expect_rc 1
done_case

start "alias-version: a scheduler or unlabelled image under the alias fails"
case_failures=$failures
publish 0.10.5 c5
printf 'sha256:sc-0.10.5\n' >"$FAKE_REGISTRY/tags/latest"
alias_version latest
expect_rc 1
echo "- -" >"$FAKE_REGISTRY/blobs/sha256:bare"
printf 'sha256:bare\n' >"$FAKE_REGISTRY/tags/latest"
alias_version latest
expect_rc 1
done_case

start "alias-version: a read error fails"
case_failures=$failures
publish 0.10.5 c5
point latest 0.10.5
FAKE_FAIL_INSPECT=latest
export FAKE_FAIL_INSPECT
alias_version latest
expect_rc 1
done_case


# --- release-registry.sh (the real adapter) with stub curl and docker --------
# Stub curl answers per request kind from $STUB_HTTP/<kind>.{code,headers,body,rc}:
#   head-anon   HEAD of the manifest URL without a bearer token
#   head-auth   HEAD of the manifest URL with `Authorization: Bearer stub-token`
#   token       GET of the token realm with Basic user `ci:s3cret`
#               (other credentials get 401)
# A kind without .code answers 404 (head-*) / 500 (token); .rc is curl's
# exit status (a transport error). Stub docker answers `imagetools inspect
# <image>@<digest> --format '{{json .Image}}'` from $STUB_HTTP/image.{rc,out,err}.
# Both log their requests to $STUB_HTTP/calls, and curl its argv to
# $STUB_HTTP/argv (credentials must never appear there).

adapter="$here/release-registry.sh"
DIGEST="sha256:$(printf '%064d' 0 | tr 0 a)"
REALM="https://auth.example.test/token"
mkdir -p "$work/bin"
cat >"$work/bin/curl" <<'STUB'
#!/usr/bin/env sh
set -eu
echo "$*" >>"$STUB_HTTP/argv"
config="$(cat)"
method=GET hdr="" out="" url="" data=""
while [ "$#" -gt 0 ]; do
    case "$1" in
        -I) method=HEAD ;;
        -D) hdr="$2"; shift ;;
        -o) out="$2"; shift ;;
        --data-urlencode) data="$data&$2"; shift ;;
        -H | -w | -K) shift ;;
        http://* | https://*) url="$1" ;;
    esac
    shift
done
case "$url" in
    */manifests/*)
        case "$config" in
            *'header = "Authorization: Bearer stub-token"'*) kind=head-auth ;;
            *) kind=head-anon ;;
        esac
        default=404
        ;;
    "$STUB_REALM")
        kind=token
        default=500
        case "$config" in *'user = "ci:s3cret"'*) ;; *) kind=token-denied ;; esac
        ;;
    *) echo "stub curl: unexpected URL $url" >&2; exit 2 ;;
esac
echo "$method $url $kind$data" >>"$STUB_HTTP/calls"
rc="$(cat "$STUB_HTTP/$kind.rc" 2>/dev/null || echo 0)"
if [ "$rc" -ne 0 ]; then
    echo "curl: ($rc) stub transport error" >&2
    printf '000'
    exit "$rc"
fi
code="$(cat "$STUB_HTTP/$kind.code" 2>/dev/null || echo "$default")"
[ "$kind" != token-denied ] || code=401
if [ -n "$hdr" ]; then
    {
        printf 'HTTP/2 %s\r\n' "$code"
        [ ! -f "$STUB_HTTP/$kind.headers" ] || sed 's/$/\r/' "$STUB_HTTP/$kind.headers"
        printf '\r\n'
    } >"$hdr"
fi
if [ -n "$out" ] && [ "$out" != /dev/null ]; then
    cat "$STUB_HTTP/$kind.body" >"$out" 2>/dev/null || : >"$out"
fi
printf '%s' "$code"
STUB
cat >"$work/bin/docker" <<'STUB'
#!/usr/bin/env sh
set -eu
[ "$1 $2 $3" = "buildx imagetools inspect" ] || { echo "stub docker: unexpected $*" >&2; exit 2; }
[ "$5 $6" = "--format {{json .Image}}" ] || { echo "stub docker: unexpected flags $*" >&2; exit 2; }
echo "image $4" >>"$STUB_HTTP/calls"
[ ! -f "$STUB_HTTP/image.err" ] || cat "$STUB_HTTP/image.err" >&2
[ ! -f "$STUB_HTTP/image.out" ] || cat "$STUB_HTTP/image.out"
exit "$(cat "$STUB_HTTP/image.rc" 2>/dev/null || echo 0)"
STUB
chmod +x "$work/bin/curl" "$work/bin/docker"

# Starts an adapter case: a registry without auth holding one healthy image.
start_adapter() {
    description="adapter: $1"
    case_no=$((case_no + 1))
    STUB_HTTP="$work/$case_no"
    STUB_REALM="$REALM"
    export STUB_HTTP STUB_REALM
    mkdir -p "$STUB_HTTP"
    : >"$STUB_HTTP/calls"
    : >"$STUB_HTTP/argv"
    echo 200 >"$STUB_HTTP/head-anon.code"
    echo "Docker-Content-Digest: $DIGEST" >"$STUB_HTTP/head-anon.headers"
    echo '{"config":{"Labels":{"org.opencontainers.image.version":"1.2.3","org.opencontainers.image.revision":"c123"}}}' \
        >"$STUB_HTTP/image.out"
    unset REGISTRY_USERNAME REGISTRY_PASSWORD || true
    case_failures=$failures
}

# with_auth: the anonymous HEAD gets a bearer challenge, the token realm
# grants `stub-token` to ci:s3cret, and the authenticated HEAD finds the tag.
with_auth() {
    echo 401 >"$STUB_HTTP/head-anon.code"
    echo "Www-Authenticate: Bearer realm=\"$REALM\",service=\"registry.example.test\",scope=\"repository:example/rss:pull\"" \
        >"$STUB_HTTP/head-anon.headers"
    echo 200 >"$STUB_HTTP/token.code"
    echo '{"token":"stub-token"}' >"$STUB_HTTP/token.body"
    echo 200 >"$STUB_HTTP/head-auth.code"
    echo "Docker-Content-Digest: $DIGEST" >"$STUB_HTTP/head-auth.headers"
    REGISTRY_USERNAME=ci
    REGISTRY_PASSWORD=s3cret
    export REGISTRY_USERNAME REGISTRY_PASSWORD
}

adapter_inspect() {
    set +e
    out="$(PATH="$work/bin:$PATH" sh "$adapter" inspect "$IMAGE:1.2.3" 2>"$work/stderr")"
    rc=$?
    set -e
}

# called PATTERN: some stub request matches the extended regex PATTERN.
called() {
    grep -Eq "$1" "$STUB_HTTP/calls" || fail "no request matching '$1' (calls: $(tr '\n' ';' <"$STUB_HTTP/calls"))"
}

not_called() {
    ! grep -Eq "$1" "$STUB_HTTP/calls" || fail "unexpected request matching '$1' (calls: $(tr '\n' ';' <"$STUB_HTTP/calls"))"
}

TAG_URL="https://ghcr.io/v2/example/rss/manifests/1.2.3"

start_adapter "an existing tag prints digest, version and revision from the pinned digest"
adapter_inspect
expect_rc 0
[ "$out" = "$DIGEST 1.2.3 c123" ] || fail "got '$out'"
called "^HEAD $TAG_URL head-anon\$"
called "^image $IMAGE@$DIGEST\$"
done_case

for labels in '"1.2.3 ","c123"' '"1.2.3","c123 "' '"1.2.3\tx","c123"' '"1.2.3","c\n123"' '"1.2.3\n","c123"' '"1.2.3","c123\n"' '1.2,"c123"' '"1.2.3",false' '"1.2.\u00003","c123"' '"1.2.3","c123\u0007"'; do
    start_adapter "a malformed label is a read error ($(printf '%s' "$labels" | tr '\134' '/'))"
    printf '{"config":{"Labels":{"org.opencontainers.image.version":%s,"org.opencontainers.image.revision":%s}}}\n' \
        "${labels%%,*}" "${labels#*,}" >"$STUB_HTTP/image.out"
    adapter_inspect
    expect_rc 1
    done_case
done

start_adapter "alias-version through the real adapter rejects a label with trailing whitespace"
printf '{"config":{"Labels":{"org.opencontainers.image.version":"1.2.3 ","org.opencontainers.image.revision":"c123"}}}\n' \
    >"$STUB_HTTP/image.out"
set +e
out="$(PATH="$work/bin:$PATH" RELEASE_REGISTRY="$adapter" sh "$script" alias-version "$IMAGE" 1.2.3 2>"$work/stderr")"
rc=$?
set -e
expect_rc 1
done_case

start_adapter "an existing tag behind a bearer challenge (token auth)"
with_auth
adapter_inspect
expect_rc 0
[ "$out" = "$DIGEST 1.2.3 c123" ] || fail "got '$out'"
called "^GET $REALM token&scope=repository:example/rss:pull&service=registry.example.test\$"
called "^HEAD $TAG_URL head-auth\$"
called "^image $IMAGE@$DIGEST\$"
if grep -q s3cret "$STUB_HTTP/argv" || grep -q stub-token "$STUB_HTTP/argv"; then
    fail "a credential or token is on curl's argv"
fi
if grep -q s3cret "$work/stderr" || grep -q stub-token "$work/stderr"; then
    fail "a credential or token is in the adapter's messages"
fi
done_case

start_adapter "an exact-tag 404 is a missing tag (registry without auth)"
echo 404 >"$STUB_HTTP/head-anon.code"
adapter_inspect
expect_rc 3
not_called "^image "
done_case

start_adapter "an unchallenged 404 with credentials configured is a read error"
REGISTRY_USERNAME=ci
REGISTRY_PASSWORD=s3cret
export REGISTRY_USERNAME REGISTRY_PASSWORD
echo 404 >"$STUB_HTTP/head-anon.code"
adapter_inspect
expect_rc 1
not_called "^GET "
done_case

start_adapter "an authenticated exact-tag 404 is a missing tag"
with_auth
echo 404 >"$STUB_HTTP/head-auth.code"
adapter_inspect
expect_rc 3
not_called "^image "
done_case

# R4-2: Buildx resolves the tag, then GETs the manifest by digest; a 404 on
# that read must not read as a missing tag once the exact tag answered 200.
start_adapter "an exact-tag 200 with a digest read 'not found' is a read error"
with_auth
echo 1 >"$STUB_HTTP/image.rc"
echo "ERROR: httpReadSeeker: failed open: content at https://ghcr.io/v2/example/rss/manifests/$DIGEST not found: not found" \
    >"$STUB_HTTP/image.err"
rm -f "$STUB_HTTP/image.out"
adapter_inspect
expect_rc 1
done_case

start_adapter "an image config 'not found' after the tag resolved is a read error"
echo 1 >"$STUB_HTTP/image.rc"
echo "ERROR: blob sha256:config: not found" >"$STUB_HTTP/image.err"
rm -f "$STUB_HTTP/image.out"
adapter_inspect
expect_rc 1
done_case

for status in 401 403 429 500 503 301; do
    start_adapter "an authenticated exact-tag $status is a read error"
    with_auth
    echo "$status" >"$STUB_HTTP/head-auth.code"
    adapter_inspect
    expect_rc 1
    not_called "^image "
    done_case
done

for status in 403 500; do
    start_adapter "an anonymous exact-tag $status is a read error"
    echo "$status" >"$STUB_HTTP/head-anon.code"
    adapter_inspect
    expect_rc 1
    done_case
done

start_adapter "a challenge without REGISTRY_USERNAME / REGISTRY_PASSWORD is a read error"
with_auth
unset REGISTRY_USERNAME REGISTRY_PASSWORD
adapter_inspect
expect_rc 1
not_called "^GET $REALM"
done_case

start_adapter "wrong credentials at the token realm are a read error"
with_auth
REGISTRY_PASSWORD=wrong
adapter_inspect
expect_rc 1
not_called "head-auth"
done_case

start_adapter "a token response without a token is a read error"
with_auth
echo '{}' >"$STUB_HTTP/token.body"
adapter_inspect
expect_rc 1
done_case

start_adapter "a non-https token realm gets no credentials"
with_auth
echo 'Www-Authenticate: Bearer realm="http://auth.example.test/token",service="x"' >"$STUB_HTTP/head-anon.headers"
adapter_inspect
expect_rc 1
not_called "^GET "
done_case

start_adapter "a Basic challenge is a read error"
with_auth
echo 'Www-Authenticate: Basic realm="registry"' >"$STUB_HTTP/head-anon.headers"
adapter_inspect
expect_rc 1
done_case

start_adapter "a 200 without a Docker-Content-Digest is a read error"
: >"$STUB_HTTP/head-anon.headers"
adapter_inspect
expect_rc 1
not_called "^image "
done_case

start_adapter "a 200 with a malformed digest is a read error"
echo "Docker-Content-Digest: sha256:abc" >"$STUB_HTTP/head-anon.headers"
adapter_inspect
expect_rc 1
done_case

# curl exits 3 on a malformed URL: the adapter's exit 3 means "missing" only.
for kind in head-anon token head-auth; do
    start_adapter "a curl transport error (exit 3) on $kind is a read error, not missing"
    with_auth
    echo 3 >"$STUB_HTTP/$kind.rc"
    adapter_inspect
    expect_rc 1
    done_case
done

start_adapter "a malformed reference is a usage error"
set +e
out="$(PATH="$work/bin:$PATH" sh "$adapter" inspect "$IMAGE@$DIGEST" 2>"$work/stderr")"
rc=$?
set -e
expect_rc 1
done_case

if [ "$failures" -ne 0 ]; then
    echo "$failures release-promotion check(s) failed" >&2
    exit 1
fi
echo "all release-promotion cases passed"
