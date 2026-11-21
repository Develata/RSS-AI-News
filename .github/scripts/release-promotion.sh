#!/usr/bin/env sh
# Release image policy for release.yml: version-tag reuse and alias
# reconciliation. POSIX sh; every registry access goes through the adapter
# script $RELEASE_REGISTRY (default: release-registry.sh next to this file;
# the tests substitute a file-backed fake), so the policy is tested offline.
#
# Adapter contract (`sh "$RELEASE_REGISTRY" <command> ...`):
#   inspect <image>:<tag>   exit 0: one line `<digest> <version-label> <revision-label>`
#                           (`-` for a missing label); exit 3: the tag does not
#                           exist; any other exit: a read error.
#   copy <image>@<digest> <image>:<tag>
#                           point the tag at exactly that manifest or index.
#
# Usage:
#   release-promotion.sh tag-commit <X.Y.Z> <sha> < ls-remote-output
#       stdin: `git ls-remote origin refs/tags/vX.Y.Z 'refs/tags/vX.Y.Z^{}'`.
#       Succeeds only when the remote tag `vX.Y.Z` still names <sha> (the
#       peeled commit of an annotated tag). A re-run keeps its original
#       GITHUB_SHA, so after the tag moved it would push the old commit's
#       images under the version tag first and block the new commit's run
#       (R3G-B4); publish-image runs this before its first push. A missing
#       tag (empty listing) fails too.
#
#   release-promotion.sh existing <image> <X.Y.Z> <revision>
#       Version tags are immutable (R3-7). Prints `runtime-digest=<d>` and
#       `scheduler-digest=<d>` for $GITHUB_OUTPUT: the digest of an existing
#       `X.Y.Z` / `X.Y.Z-scheduler` tag built from <revision> (re-use it), or
#       empty when that tag does not exist yet (build it). An existing tag with
#       another revision or another version label than its role's (`X.Y.Z`
#       for runtime, `X.Y.Z-scheduler` for scheduler, R3G-B2) fails: it is
#       never overwritten.
#
#   release-promotion.sh reconcile <image> <X.Y.Z> < ls-remote-output
#       stdin: `git ls-remote --tags origin 'refs/tags/v*'`. Reconciles the
#       moving tags to the highest *released* stable version, whichever tag
#       triggered the run (R3-2, R3-3):
#         latest[-scheduler]  -> highest released version overall
#         X.Y[-scheduler]     -> highest released version of the trigger's line
#                                and of the line `latest` points at
#       A version is released when `V` carries version label exactly `V` and
#       `V-scheduler` exactly `V-scheduler` (the labels metadata-action
#       writes), both with revision label = the tag's commit (the peeled
#       `^{}` SHA of an annotated tag). Aliases follow the same role rule: a
#       runtime alias carries `C`, a scheduler alias `C-scheduler` (R3G-B2).
#       An alias is never moved to a version lower than its current
#       `org.opencontainers.image.version` label, so a deleted higher tag
#       cannot move it backwards; such a retained higher pair must have equal
#       revisions and be exactly the `C` / `C-scheduler` version tags'
#       images, else the run fails (R3G-B2). Every alias the run
#       touches (runtime and scheduler, every line) is read and validated
#       before the first write, so a read error, an alias without a valid
#       version label, or a runtime / scheduler pair that would end at
#       different versions fails the run with nothing written (R3G-B3). Then
#       runtime aliases are written before scheduler aliases, each with
#       `--prefer-index=false` semantics (adapter), and every written alias
#       is read back (R3-4, R3-22). A re-run completes a reconcile
#       interrupted between two writes (see docs/operations/ci-and-release.md
#       for the case where the higher tag is deleted in between).
#
# Versions: stable `X.Y.Z` only, each component `0` or `[1-9][0-9]*` (SemVer
# 2.0.0 §2), of any length, like the Rust acceptance validator
# (`validate_version`). Without leading zeros a longer component is larger
# and equal-length components compare digit by digit, so versions are
# compared as strings, never as (floating-point) numbers (R3G-S8).

set -eu

script_dir="$(dirname "$0")"
registry="${RELEASE_REGISTRY:-$script_dir/release-registry.sh}"
VERSION_ERE='^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$'

die() {
    echo "release-promotion: $*" >&2
    exit 1
}

is_version() {
    printf '%s\n' "$1" | grep -Eq "$VERSION_ERE"
}

# version_cmp A B -> prints -1, 0 or 1. Pre: A and B are versions.
version_cmp() {
    # `split` yields numeric strings, which awk would compare as doubles;
    # concatenating "" forces a string comparison (by length first).
    LC_ALL=C awk -v a="$1" -v b="$2" 'BEGIN {
        split(a, x, "."); split(b, y, ".")
        for (i = 1; i <= 3; i++) {
            p = x[i] ""; q = y[i] ""
            if (length(p) < length(q)) { print -1; exit }
            if (length(p) > length(q)) { print 1; exit }
            if (p < q) { print -1; exit }
            if (p > q) { print 1; exit }
        }
        print 0
    }'
}

minor_line() {
    printf '%s\n' "${1%.*}"
}

# registry_inspect REF -> sets inspect_rc and, on success, inspect_digest,
# inspect_version, inspect_revision.
registry_inspect() {
    inspect_digest=""
    inspect_version=""
    inspect_revision=""
    set +e
    inspect_out="$(sh "$registry" inspect "$1")"
    inspect_rc=$?
    set -e
    case "$inspect_rc" in
        0) ;;
        3) return 0 ;;
        *) die "reading $1 failed (exit $inspect_rc); nothing is promoted on an unreadable state" ;;
    esac
    # shellcheck disable=SC2086 # the adapter prints three space-separated fields
    set -- $inspect_out
    [ "$#" -eq 3 ] || die "unexpected inspect output for $1: '$inspect_out'"
    case "$1" in
        sha256:*) ;;
        *) die "unexpected digest '$1'" ;;
    esac
    inspect_digest="$1"
    inspect_version="$2"
    inspect_revision="$3"
}

# Parses ls-remote output on stdin into `X.Y.Z <commit>` lines, highest first.
# An annotated tag's commit is its peeled `^{}` line.
stable_tags() {
    awk -F '\t' 'NF == 2 && $2 ~ /^refs\/tags\/v/ {
        name = $2
        sub(/^refs\/tags\/v/, "", name)
        if (sub(/\^\{\}$/, "", name)) { peeled[name] = $1 } else { plain[name] = $1 }
        seen[name] = 1
    }
    END {
        for (name in seen) print name, ((name in peeled) ? peeled[name] : plain[name])
    }' |
        while read -r name commit; do
            if is_version "$name"; then
                printf '%s %s\n' "$name" "$commit"
            fi
        done |
        # Sort keys: each component's length, then its digits as a string
        # (exact for any length; `sort -n` may go through floating point).
        awk '{ split($1, v, "."); print length(v[1]), v[1], length(v[2]), v[2], length(v[3]), v[3], $0 }' |
        LC_ALL=C sort -k1,1nr -k2,2r -k3,3nr -k4,4r -k5,5nr -k6,6r |
        cut -d ' ' -f 7-
}

# role_tag ROLE NAME -> NAME for runtime, NAME-scheduler for scheduler: both
# the tag name and the exact version label of that role.
role_tag() {
    if [ "$1" = runtime ]; then printf '%s\n' "$2"; else printf '%s\n' "$2-scheduler"; fi
}

# check_version_tag IMAGE VERSION KIND REVISION -> sets checked_digest (empty
# when the tag does not exist); fails on a tag with other labels. The version
# label must be exactly the role's (`V` / `V-scheduler`), so swapped images
# never count as a release (R3G-B2).
check_version_tag() {
    tag="$(role_tag "$3" "$2")"
    registry_inspect "$1:$tag"
    checked_digest=""
    [ "$inspect_rc" -eq 0 ] || return 0
    if [ "$inspect_version" != "$tag" ] || [ "$inspect_revision" != "$4" ]; then
        die "$1:$tag exists with version '$inspect_version' and revision '$inspect_revision', not $2 at $4; version tags are immutable"
    fi
    checked_digest="$inspect_digest"
}

cmd_existing() {
    [ "$#" -eq 3 ] || { echo "usage: release-promotion.sh existing <image> <X.Y.Z> <revision>" >&2; exit 2; }
    is_version "$2" || { echo "release-promotion: expected X.Y.Z, got '$2'" >&2; exit 2; }
    check_version_tag "$1" "$2" runtime "$3"
    runtime_digest="$checked_digest"
    check_version_tag "$1" "$2" scheduler "$3"
    echo "runtime-digest=$runtime_digest"
    echo "scheduler-digest=$checked_digest"
}

# highest_released IMAGE TAGS [LINE] -> prints `V runtime-digest scheduler-digest`
# of the highest released version (of LINE when given), or nothing. A
# version with only one of its two images is not released (skipped).
highest_released() {
    # A here-document, not a pipeline: the loop runs in this shell, so
    # `return` ends the function and a failure in check_version_tag exits.
    while read -r version commit; do
        [ -n "$version" ] || continue
        if [ -n "${3:-}" ] && [ "$(minor_line "$version")" != "$3" ]; then
            continue
        fi
        check_version_tag "$1" "$version" runtime "$commit"
        runtime_digest="$checked_digest"
        [ -n "$runtime_digest" ] || continue
        check_version_tag "$1" "$version" scheduler "$commit"
        [ -n "$checked_digest" ] || continue
        printf '%s %s %s\n' "$version" "$runtime_digest" "$checked_digest"
        return 0
    done <<EOF
$2
EOF
}

# plan_alias IMAGE ALIAS ROLE VERSION DIGEST -> reads the alias and prints
# one line `<final-version> <action> <current-digest> <current-revision>`,
# where action is `keep` (already at a newer version), `unchanged`, `create`
# or `update:<current-version>` (write DIGEST). The alias's version label
# must be its role's (`C` / `C-scheduler`, R3G-B2). Writes nothing.
plan_alias() {
    registry_inspect "$1:$2"
    if [ "$inspect_rc" -eq 3 ]; then
        echo "$4 create - -"
        return 0
    fi
    if [ "$3" = runtime ]; then
        current="$inspect_version"
    else
        current="${inspect_version%-scheduler}"
    fi
    if ! is_version "$current" || [ "$inspect_version" != "$(role_tag "$3" "$current")" ]; then
        die "$1:$2 exists without a valid $3 org.opencontainers.image.version label ('$inspect_version'); refusing to move it"
    fi
    if [ "$(version_cmp "$current" "$4")" -eq 1 ]; then
        echo "$current keep $inspect_digest $inspect_revision"
    elif [ "$inspect_digest" = "$5" ]; then
        echo "$4 unchanged $inspect_digest $inspect_revision"
    else
        echo "$4 update:$current $inspect_digest $inspect_revision"
    fi
}

# check_retained_pair IMAGE ALIAS VERSION RUNTIME-DIGEST RUNTIME-REVISION
#     SCHEDULER-DIGEST SCHEDULER-REVISION: a pair kept at a version above the
# target must be one release: equal revisions, and exactly the images of the
# `VERSION` / `VERSION-scheduler` tags (which must still exist, R3G-B2).
check_retained_pair() {
    if [ "$5" = - ] || [ "$5" != "$7" ]; then
        die "$1:$2 is kept at $3 with revision '$5' but $1:$2-scheduler with '$7'; refusing to keep a mixed pair"
    fi
    check_version_tag "$1" "$3" runtime "$5"
    [ "$checked_digest" = "$4" ] ||
        die "$1:$2 ($4) is not the $3 version tag's image ('${checked_digest:-missing}'); refusing to keep it"
    check_version_tag "$1" "$3" scheduler "$5"
    [ "$checked_digest" = "$6" ] ||
        die "$1:$2-scheduler ($6) is not the $3-scheduler version tag's image ('${checked_digest:-missing}'); refusing to keep it"
}

# plan_pair IMAGE ALIAS TARGET (`V runtime-digest scheduler-digest`) ->
# validates both aliases of the pair and prints its plan: `write <tag>
# <digest> <version> <from>` lines (runtime first), then `result
# <alias>=<version>`. Writes nothing.
plan_pair() {
    # shellcheck disable=SC2086 # `V runtime-digest scheduler-digest`
    set -- "$1" "$2" $3
    runtime_plan="$(plan_alias "$1" "$2" runtime "$3" "$4")" || exit 1
    scheduler_plan="$(plan_alias "$1" "$2-scheduler" scheduler "$3" "$5")" || exit 1
    # shellcheck disable=SC2086 # plan_alias prints four fields
    set -- "$@" $runtime_plan $scheduler_plan
    # $6..$9: runtime final / action / digest / revision; ${10}..${13}: scheduler.
    if [ "$6" != "${10}" ]; then
        die "$1:$2 would be at $6 but $1:$2-scheduler at ${10}; nothing was written. Promote the matching release to pair them"
    fi
    # Equal final versions above the target mean both sides are kept.
    if [ "$7" = keep ]; then
        check_retained_pair "$1" "$2" "$6" "$8" "$9" "${12}" "${13}"
    fi
    for side in runtime scheduler; do
        if [ "$side" = runtime ]; then
            tag="$2" action="$7" digest="$4"
        else
            tag="$2-scheduler" action="${11}" digest="$5"
        fi
        case "$action" in
            keep) echo "alias $tag: kept at newer $6 (highest released tag is $3)" >&2 ;;
            unchanged) echo "alias $tag: unchanged at $3" >&2 ;;
            create) echo "write $tag $digest $3 created" ;;
            update:*) echo "write $tag $digest $3 updated-from-${action#update:}" ;;
        esac
    done
    echo "result $2=$6"
}

# apply_plan IMAGE PLAN: performs the planned writes in order, reading each
# alias back, then prints the `<alias>=<version>` results.
apply_plan() {
    while read -r kind tag digest version action; do
        [ "$kind" = write ] || continue
        sh "$registry" copy "$1@$digest" "$1:$tag" >&2 || die "writing $1:$tag failed; re-run to complete the promotion"
        registry_inspect "$1:$tag"
        if [ "$inspect_rc" -ne 0 ] || [ "$inspect_digest" != "$digest" ]; then
            die "$1:$tag reads back as '${inspect_digest:-missing}', expected $digest"
        fi
        echo "alias $tag: $action -> $version ($digest)" >&2
    done <<EOF
$2
EOF
    printf '%s\n' "$2" | sed -n 's/^result //p'
}

cmd_tag_commit() {
    [ "$#" -eq 2 ] || { echo "usage: release-promotion.sh tag-commit <X.Y.Z> <sha> < ls-remote-output" >&2; exit 2; }
    is_version "$1" || { echo "release-promotion: expected X.Y.Z, got '$1'" >&2; exit 2; }
    remote_commit="$(stable_tags | awk -v v="$1" '$1 == v { print $2 }')"
    [ -n "$remote_commit" ] || die "v$1 is not on the remote (deleted?); nothing is published"
    [ "$remote_commit" = "$2" ] ||
        die "v$1 now names $remote_commit, not this run's commit $2 (moved tag?); nothing is published, run the release for the tag's current commit"
    echo "v$1 still names $2" >&2
}

cmd_reconcile() {
    [ "$#" -eq 2 ] || { echo "usage: release-promotion.sh reconcile <image> <X.Y.Z> < tags" >&2; exit 2; }
    image="$1"
    trigger="$2"
    is_version "$trigger" || { echo "release-promotion: expected X.Y.Z, got '$trigger'" >&2; exit 2; }
    tags="$(stable_tags)"
    printf '%s\n' "$tags" | awk -v v="$trigger" '$1 == v { found = 1 } END { exit !found }' ||
        die "v$trigger is not among the repository's stable tags (empty or truncated listing?)"

    latest_target="$(highest_released "$image" "$tags")"
    [ -n "$latest_target" ] || die "no released stable version found"
    trigger_line="$(minor_line "$trigger")"
    line_target="$(highest_released "$image" "$tags" "$trigger_line")"
    [ -n "$line_target" ] || die "no released version of the $trigger_line line (v$trigger's images are missing)"
    latest_line="$(minor_line "${latest_target%% *}")"

    # Read and validate every alias of every pair before the first write.
    plan="$(plan_pair "$image" latest "$latest_target")" || exit 1
    plan="$plan
$(plan_pair "$image" "$trigger_line" "$line_target")" || exit 1
    if [ "$latest_line" != "$trigger_line" ]; then
        plan="$plan
$(plan_pair "$image" "$latest_line" "$latest_target")" || exit 1
    fi
    apply_plan "$image" "$plan"
}

command="${1:-}"
[ "$#" -gt 0 ] && shift
case "$command" in
    tag-commit) cmd_tag_commit "$@" ;;
    existing) cmd_existing "$@" ;;
    reconcile) cmd_reconcile "$@" ;;
    *)
        echo "usage: release-promotion.sh tag-commit|existing|reconcile ..." >&2
        exit 2
        ;;
esac
