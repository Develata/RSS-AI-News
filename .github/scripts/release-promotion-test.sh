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
done_case

# --- release-registry.sh (the real adapter) with a stub docker ---------------
# The stub answers `docker buildx imagetools inspect <ref> <flags>` per mode
# (raw = `--raw`, manifest = `--format '{{json .Manifest}}'`, image =
# `--format '{{json .Image}}'`) from $STUB_DOCKER/<mode>.{rc,out,err}; a mode
# without an .rc file succeeds with its .out.

adapter="$here/release-registry.sh"
mkdir -p "$work/bin"
cat >"$work/bin/docker" <<'STUB'
#!/usr/bin/env sh
set -eu
[ "$1 $2 $3" = "buildx imagetools inspect" ] || { echo "stub docker: unexpected $*" >&2; exit 2; }
shift 4
case "$*" in
    --raw) mode=raw ;;
    "--format {{json .Manifest}}") mode=manifest ;;
    "--format {{json .Image}}") mode=image ;;
    *) echo "stub docker: unexpected flags $*" >&2; exit 2 ;;
esac
echo "$mode" >>"$STUB_DOCKER/calls"
[ ! -f "$STUB_DOCKER/$mode.err" ] || cat "$STUB_DOCKER/$mode.err" >&2
[ ! -f "$STUB_DOCKER/$mode.out" ] || cat "$STUB_DOCKER/$mode.out"
exit "$(cat "$STUB_DOCKER/$mode.rc" 2>/dev/null || echo 0)"
STUB
chmod +x "$work/bin/docker"

# Starts an adapter case whose registry holds one healthy image.
start_adapter() {
    description="adapter: $1"
    case_no=$((case_no + 1))
    STUB_DOCKER="$work/$case_no"
    export STUB_DOCKER
    mkdir -p "$STUB_DOCKER"
    : >"$STUB_DOCKER/calls"
    echo '{"schemaVersion":2}' >"$STUB_DOCKER/raw.out"
    echo '{"digest":"sha256:abc"}' >"$STUB_DOCKER/manifest.out"
    echo '{"config":{"Labels":{"org.opencontainers.image.version":"1.2.3","org.opencontainers.image.revision":"c123"}}}' \
        >"$STUB_DOCKER/image.out"
    case_failures=$failures
}

# stub_fail MODE RC MESSAGE
stub_fail() {
    echo "$2" >"$STUB_DOCKER/$1.rc"
    echo "$3" >"$STUB_DOCKER/$1.err"
    rm -f "$STUB_DOCKER/$1.out"
}

adapter_inspect() {
    set +e
    out="$(PATH="$work/bin:$PATH" sh "$adapter" inspect "$IMAGE:1.2.3" 2>"$work/stderr")"
    rc=$?
    set -e
}

start_adapter "an existing tag prints digest, version and revision"
adapter_inspect
expect_rc 0
[ "$out" = "sha256:abc 1.2.3 c123" ] || fail "got '$out'"
done_case

start_adapter "a top-level 'not found' is a missing tag"
stub_fail raw 1 "ERROR: $IMAGE:1.2.3: not found"
adapter_inspect
expect_rc 3
done_case

start_adapter "a top-level 'manifest unknown' is a missing tag"
stub_fail raw 1 "ERROR: unexpected status 404: MANIFEST_UNKNOWN: manifest unknown"
adapter_inspect
expect_rc 3
done_case

start_adapter "a top-level auth or network error is a read error"
stub_fail raw 1 "ERROR: unexpected status 401 Unauthorized"
adapter_inspect
expect_rc 1
done_case

# Buildx loads child manifests and image configs for `--format`; once the
# top-level manifest resolved, their "not found" must not read as a missing tag.
start_adapter "a child manifest 'not found' after the top level resolved is a read error"
stub_fail manifest 1 "ERROR: sha256:child: not found"
adapter_inspect
expect_rc 1
done_case

start_adapter "an image config 'not found' after the top level resolved is a read error"
stub_fail image 1 "ERROR: blob sha256:config: not found"
adapter_inspect
expect_rc 1
done_case

if [ "$failures" -ne 0 ]; then
    echo "$failures release-promotion check(s) failed" >&2
    exit 1
fi
echo "all release-promotion cases passed"
