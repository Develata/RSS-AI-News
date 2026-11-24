#!/usr/bin/env sh
# GitHub Release policy for release.yml's github-release job. POSIX sh; every
# GitHub access goes through the adapter script $RELEASE_GITHUB (default:
# release-github-api.sh next to this file; the tests substitute a
# file-backed fake), so the policy is tested offline.
#
# Adapter contract (`sh "$RELEASE_GITHUB" <command> ...`):
#   find <tag>        exit 0: one line `<draft> <prerelease>` (each `true` or
#                     `false`) per Release whose tag is <tag>, nothing when
#                     there is none; any other exit: a read error. Absence is
#                     decided from the full Release listing, never from a 404.
#   create <tag> <title> <notes-file> <latest>
#                     publishes a Release (not a draft, not a prerelease) for
#                     the tag <tag>, which must already be on the remote (the
#                     adapter never creates a tag), with the body read from
#                     <notes-file> and make_latest = <latest> (`true`/`false`).
#   set-latest <tag>  marks the published Release of <tag> as Latest.
#   latest            exit 0: the tag of the Release GitHub marks Latest.
#
# Usage:
#   release-github.sh notes <X.Y.Z> <report> [<report-url>]
#       Prints the Release body: the lines of <report> strictly between the
#       line `<!-- release-notes:start -->` and the line
#       `<!-- release-notes:end -->` (each exactly once, whole line, start
#       first), without leading or trailing blank lines; then, when
#       <report-url> is given, a line linking the full report. Fails when the
#       report is missing, a marker is missing or repeated, the end comes
#       first, or the markers enclose only blank lines. The release
#       acceptance lane runs it on the candidate's report, so a tag whose
#       report has no notes fails verify-release and never reaches the images.
#
#   release-github.sh ensure <X.Y.Z> <latest-version> <notes-file>
#       Makes the Release of vX.Y.Z exist, published. <latest-version> is the
#       version the `latest` image alias carries (release-promotion.sh
#       alias-version), read after this run's promote-tags: the Release is
#       marked Latest exactly when that is X.Y.Z, so the GitHub Release and
#       the image alias follow one policy (the highest released stable
#       version). Otherwise it is created with make_latest=false and GitHub's
#       current Latest stays.
#         - no Release of vX.Y.Z: create it, title `vX.Y.Z`, body <notes-file>;
#         - a published one (a re-run, or one written by hand): its title and
#           body are left as they are; only Latest is applied when due;
#         - a draft or a prerelease: fail with nothing written (publishing or
#           replacing it is a maintainer's decision);
#         - more than one Release of vX.Y.Z (drafts may share a tag): fail.
#       Then reads back: the Release exists, published and not a prerelease,
#       and, when due, GitHub's Latest is vX.Y.Z. The caller holds one
#       serialized concurrency group across tags, so the `latest` alias read
#       and the Latest write of two runs never interleave.

set -eu

script_dir="$(dirname "$0")"
github="${RELEASE_GITHUB:-$script_dir/release-github-api.sh}"
VERSION_ERE='^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$'
NOTES_START='<!-- release-notes:start -->'
NOTES_END='<!-- release-notes:end -->'

die() {
    echo "release-github: $*" >&2
    exit 1
}

is_version() {
    printf '%s\n' "$1" | grep -Eq "$VERSION_ERE"
}

cmd_notes() {
    if [ "$#" -ne 2 ] && [ "$#" -ne 3 ]; then
        echo "usage: release-github.sh notes <X.Y.Z> <report> [<report-url>]" >&2
        exit 2
    fi
    is_version "$1" || { echo "release-github: expected X.Y.Z, got '$1'" >&2; exit 2; }
    [ -f "$2" ] || die "$2 does not exist; write the v$1 release report before the tag (docs/operations/ci-and-release.md)"
    # Errors are exit codes, not awk output: 2 = marker count, 3 = order,
    # 4 = only blank lines between the markers.
    set +e
    body="$(LC_ALL=C awk -v s="$NOTES_START" -v e="$NOTES_END" '
        $0 == s { ns++; if (ne > 0) late = 1; inside = 1; next }
        $0 == e { ne++; inside = 0; next }
        inside { n++; line[n] = $0 }
        END {
            if (ns != 1 || ne != 1) exit 2
            if (late) exit 3
            first = 1
            while (first <= n && line[first] ~ /^[ \t]*$/) first++
            last = n
            while (last >= first && line[last] ~ /^[ \t]*$/) last--
            if (first > last) exit 4
            for (i = first; i <= last; i++) print line[i]
        }' "$2")"
    rc=$?
    set -e
    case "$rc" in
        0) ;;
        2) die "$2 must contain the line '$NOTES_START' and the line '$NOTES_END' exactly once each" ;;
        3) die "$2: '$NOTES_END' comes before '$NOTES_START'" ;;
        4) die "$2: no release notes between '$NOTES_START' and '$NOTES_END'" ;;
        *) die "reading $2 failed (exit $rc)" ;;
    esac
    printf '%s\n' "$body"
    if [ "$#" -eq 3 ]; then
        printf '\n完整发布报告（含 tag 后补录的发布证据）：%s\n' "$3"
    fi
}

# find_release TAG -> prints `<draft> <prerelease>`, or nothing when TAG has
# no Release; fails on a read error, a malformed answer or several Releases.
find_release() {
    set +e
    found="$(sh "$github" find "$1")"
    rc=$?
    set -e
    [ "$rc" -eq 0 ] || die "listing the Releases failed (exit $rc); nothing is written on an unreadable state"
    [ -n "$found" ] || return 0
    count="$(printf '%s\n' "$found" | wc -l | tr -d ' ')"
    [ "$count" -eq 1 ] || die "$count Releases name $1 (drafts may share a tag); resolve them by hand, then re-run"
    case "$found" in
        'false false' | 'false true' | 'true false' | 'true true') ;;
        *) die "unexpected Release state for $1: '$found'" ;;
    esac
    printf '%s\n' "$found"
}

cmd_ensure() {
    [ "$#" -eq 3 ] || { echo "usage: release-github.sh ensure <X.Y.Z> <latest-version> <notes-file>" >&2; exit 2; }
    version="$1"
    latest_version="$2"
    notes_file="$3"
    is_version "$version" || { echo "release-github: expected X.Y.Z, got '$version'" >&2; exit 2; }
    is_version "$latest_version" || die "the latest alias version '$latest_version' is not X.Y.Z"
    [ -s "$notes_file" ] || die "notes file '$notes_file' is missing or empty"
    tag="v$version"
    if [ "$latest_version" = "$version" ]; then
        make_latest=true
    else
        make_latest=false
    fi

    state="$(find_release "$tag")" || exit 1
    case "$state" in
        '')
            sh "$github" create "$tag" "$tag" "$notes_file" "$make_latest" >&2 ||
                die "creating the Release of $tag failed; re-run the github-release job"
            echo "release $tag: created (latest=$make_latest; the latest image alias is at $latest_version)" >&2
            ;;
        'false false')
            if [ "$make_latest" = true ]; then
                sh "$github" set-latest "$tag" >&2 ||
                    die "marking the Release of $tag Latest failed; re-run the github-release job"
                echo "release $tag: exists, title and notes left as published; marked Latest" >&2
            else
                echo "release $tag: exists, left unchanged (the latest image alias is at $latest_version)" >&2
            fi
            ;;
        'true '*)
            die "the Release of $tag is a draft; publish or delete it by hand, then re-run the github-release job"
            ;;
        *)
            die "the Release of $tag is a prerelease; a stable tag gets a stable Release: fix it by hand, then re-run the github-release job"
            ;;
    esac

    state="$(find_release "$tag")" || exit 1
    [ "$state" = 'false false' ] ||
        die "the Release of $tag reads back as '${state:-missing}', expected published and not a prerelease"
    if [ "$make_latest" = true ]; then
        set +e
        current="$(sh "$github" latest)"
        rc=$?
        set -e
        [ "$rc" -eq 0 ] || die "reading GitHub's Latest Release failed (exit $rc)"
        [ "$current" = "$tag" ] || die "GitHub marks '$current' as Latest, expected $tag"
    fi
    echo "release=$tag latest=$make_latest"
}

command="${1:-}"
[ "$#" -gt 0 ] && shift
case "$command" in
    notes) cmd_notes "$@" ;;
    ensure) cmd_ensure "$@" ;;
    *)
        echo "usage: release-github.sh notes|ensure ..." >&2
        exit 2
        ;;
esac
