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
#   release-promotion.sh existing <image> <X.Y.Z> <revision>
#       Version tags are immutable (R3-7). Prints `runtime-digest=<d>` and
#       `scheduler-digest=<d>` for $GITHUB_OUTPUT: the digest of an existing
#       `X.Y.Z` / `X.Y.Z-scheduler` tag built from <revision> (re-use it), or
#       empty when that tag does not exist yet (build it). An existing tag with
#       another revision or version label fails: it is never overwritten.
#
#   release-promotion.sh reconcile <image> <X.Y.Z> < ls-remote-output
#       stdin: `git ls-remote --tags origin 'refs/tags/v*'`. Reconciles the
#       moving tags to the highest *released* stable version, whichever tag
#       triggered the run (R3-2, R3-3):
#         latest[-scheduler]  -> highest released version overall
#         X.Y[-scheduler]     -> highest released version of the trigger's line
#                                and of the line `latest` points at
#       A version is released when both `V` and `V-scheduler` exist and carry
#       version label V and revision label = the tag's commit (the peeled
#       `^{}` SHA of an annotated tag). An alias is never moved to a version
#       lower than its current `org.opencontainers.image.version` label, so a
#       deleted higher tag cannot move it backwards. Runtime aliases are
#       written before scheduler aliases, each with `--prefer-index=false`
#       semantics (adapter), and every written alias is read back (R3-4,
#       R3-22). A re-run completes an interrupted reconcile. Any read error,
#       an alias without a valid version label, or a runtime / scheduler pair
#       left at different versions fails the run.
#
# Versions: stable `X.Y.Z` only, each component `0` or `[1-9][0-9]*` (SemVer
# 2.0.0 §2; at most 9 digits so awk compares them exactly).

set -eu

script_dir="$(dirname "$0")"
registry="${RELEASE_REGISTRY:-$script_dir/release-registry.sh}"
VERSION_ERE='^(0|[1-9][0-9]{0,8})\.(0|[1-9][0-9]{0,8})\.(0|[1-9][0-9]{0,8})$'

die() {
    echo "release-promotion: $*" >&2
    exit 1
}

is_version() {
    printf '%s\n' "$1" | grep -Eq "$VERSION_ERE"
}

# version_cmp A B -> prints -1, 0 or 1.
version_cmp() {
    awk -v a="$1" -v b="$2" 'BEGIN {
        split(a, x, "."); split(b, y, ".")
        for (i = 1; i <= 3; i++) {
            if (x[i] + 0 < y[i] + 0) { print -1; exit }
            if (x[i] + 0 > y[i] + 0) { print 1; exit }
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
        sort -t . -k1,1nr -k2,2nr -k3,3nr
}

# check_version_tag IMAGE VERSION KIND REVISION -> sets checked_digest (empty
# when the tag does not exist); fails on a tag with other labels.
check_version_tag() {
    if [ "$3" = runtime ]; then tag="$2"; else tag="$2-scheduler"; fi
    registry_inspect "$1:$tag"
    checked_digest=""
    [ "$inspect_rc" -eq 0 ] || return 0
    label="${inspect_version%-scheduler}"
    if [ "$label" != "$2" ] || [ "$inspect_revision" != "$4" ]; then
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

# reconcile_alias IMAGE ALIAS VERSION DIGEST -> prints the alias's final version.
reconcile_alias() {
    registry_inspect "$1:$2"
    action=""
    if [ "$inspect_rc" -eq 3 ]; then
        action=created
    else
        current="${inspect_version%-scheduler}"
        is_version "$current" ||
            die "$1:$2 exists without a valid org.opencontainers.image.version label ('$inspect_version'); refusing to move it"
        case "$(version_cmp "$current" "$3")" in
            1)
                echo "alias $2: kept at newer $current (highest released tag is $3)" >&2
                echo "$current"
                return 0
                ;;
            *)
                if [ "$inspect_digest" = "$4" ]; then
                    echo "alias $2: unchanged at $3" >&2
                    echo "$3"
                    return 0
                fi
                action="updated from $current"
                ;;
        esac
    fi
    sh "$registry" copy "$1@$4" "$1:$2" >&2 || die "writing $1:$2 failed; re-run to complete the promotion"
    registry_inspect "$1:$2"
    if [ "$inspect_rc" -ne 0 ] || [ "$inspect_digest" != "$4" ]; then
        die "$1:$2 reads back as '${inspect_digest:-missing}', expected $4"
    fi
    echo "alias $2: $action -> $3 ($4)" >&2
    echo "$3"
}

# reconcile_pair IMAGE ALIAS TARGET-LINE: runtime alias, then scheduler alias.
reconcile_pair() {
    # shellcheck disable=SC2086 # `V runtime-digest scheduler-digest`
    set -- "$1" "$2" $3
    runtime_at="$(reconcile_alias "$1" "$2" "$3" "$4")"
    scheduler_at="$(reconcile_alias "$1" "$2-scheduler" "$3" "$5")"
    if [ "$runtime_at" != "$scheduler_at" ]; then
        die "$1:$2 is at $runtime_at but $1:$2-scheduler at $scheduler_at; promote the matching release to pair them"
    fi
    echo "$2=$runtime_at"
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

    reconcile_pair "$image" latest "$latest_target"
    reconcile_pair "$image" "$trigger_line" "$line_target"
    if [ "$latest_line" != "$trigger_line" ]; then
        reconcile_pair "$image" "$latest_line" "$latest_target"
    fi
}

command="${1:-}"
[ "$#" -gt 0 ] && shift
case "$command" in
    existing) cmd_existing "$@" ;;
    reconcile) cmd_reconcile "$@" ;;
    *)
        echo "usage: release-promotion.sh existing|reconcile ..." >&2
        exit 2
        ;;
esac
