#!/usr/bin/env sh
# GitHub Release policy for release.yml's github-release job. POSIX sh; every
# GitHub access goes through the adapter script $RELEASE_GITHUB (default:
# release-github-api.sh next to this file; the tests substitute a
# file-backed fake), so the policy is tested offline.
#
# Adapter contract (`sh "$RELEASE_GITHUB" <command> ...`):
#   find <tag>        exit 0: one line `<id> <draft> <prerelease>` (id a
#                     number, the others `true` / `false`) per Release whose
#                     tag is <tag>, nothing when there is none; any other
#                     exit: a read error. Absence is decided from the full
#                     Release listing, never from a 404; a malformed listing
#                     or an answer without a body is a read error.
#   create <tag> <title> <notes-file> <latest>
#                     publishes a Release (not a draft, not a prerelease) for
#                     the tag <tag>, which must already be on the remote, with
#                     the body read from <notes-file> and make_latest =
#                     <latest> (`true` / `false`).
#   set-latest <id>   marks the published Release <id> as Latest, sending
#                     nothing else.
#   latest            exit 0: the tag of the Release GitHub marks Latest; any
#                     other exit: a read error (a repository without any
#                     published Release also answers 404).
#   tag-commit <tag>  exit 0: the commit the remote tag names (peeled), or
#                     nothing when the tag is not on the remote; any other
#                     exit: a read error.
#
# Usage:
#   release-github.sh notes <X.Y.Z> <report> [<report-url>]
#       Prints the Release body: the lines of <report> strictly between the
#       line `<!-- release-notes:start -->` and the line
#       `<!-- release-notes:end -->` (each exactly once, whole line, start
#       first; a trailing CR is ignored, so a CRLF checkout reads the same),
#       without leading or trailing blank lines; then, when <report-url> is
#       given, a Markdown link to the full report at the tag. Fails when the
#       report is missing, a marker is missing or repeated, the end comes
#       first, or the markers enclose only blank lines. The notes are copied
#       verbatim: links in them are not checked (telling relative links from
#       code or HTML needs a Markdown parser; a line-based check both missed
#       real links and rejected valid notes), so the rule that they must be
#       absolute https URLs is the report author's (docs/reports/README.md).
#       The release acceptance lane runs it on the candidate's report, so a
#       tag whose notes are missing fails verify-release and never reaches
#       the images.
#
#   release-github.sh ensure <X.Y.Z> <commit> <latest-version> <notes-file>
#       Makes the Release of vX.Y.Z exist, published. <latest-version> is the
#       version the `latest` image alias carries (release-promotion.sh
#       alias-version). The caller reads it and runs this under the same
#       concurrency group as promote-tags, so the alias cannot move between
#       that read and the writes here. GitHub's Latest follows the alias:
#         - X.Y.Z is the alias version: vX.Y.Z must end as Latest;
#         - otherwise GitHub's Latest must already be v<latest-version> and
#           stay there (make_latest=false). If it is not, the higher
#           version's Release is missing, unpublished, not Latest or not yet
#           done; the run fails before any write and names it.
#       Order: every read and check comes before the first write, so a read
#       error, a draft or prerelease Release of vX.Y.Z, several Releases of
#       it (drafts may share a tag) or the Latest mismatch above fail with
#       nothing written. The last check, also when nothing is to be written:
#       the remote tag vX.Y.Z still names <commit> (the notes and images are
#       that commit's), so a success never vouches for a moved tag.
#       Then at most one write: create (no Release yet) or set-latest by id
#       (published, Latest due and not already set); a published Release's
#       title and body are never rewritten. Then a read-back of the Release
#       state and of Latest; a failure there after a write is reported as
#       "written, not confirmed", and a re-run, which reads first, completes
#       or confirms it.

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

# One line of digits and dots only (grep -E alone would accept a multi-line
# argument with one matching line), then the X.Y.Z structure.
is_version() {
    case "$1" in '' | *[!0-9.]*) return 1 ;; esac
    printf '%s\n' "$1" | grep -Eq "$VERSION_ERE"
}

# A full commit SHA (SHA-1 or SHA-256 object format).
is_commit() {
    case "$1" in '' | *[!0-9a-f]*) return 1 ;; esac
    [ "${#1}" -eq 40 ] || [ "${#1}" -eq 64 ]
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
        { sub(/\r$/, "") }
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
        printf '\n完整发布报告（tag 时的版本；tag 后补录的发布证据在 main 分支的同一路径）：[%s](%s)\n' "$2" "$3"
    fi
}

# What a failed read means for the repository: before the write nothing
# changed; after it the write went through and only its confirmation failed.
phase_note() {
    if [ "$1" = before ]; then
        echo "nothing was written"
    else
        echo "the write went through but is not confirmed; re-run the github-release job, which reads first"
    fi
}

# find_release TAG PHASE -> prints `<id> <draft> <prerelease>`, or nothing
# when TAG has no Release; fails on a read error, a malformed answer or
# several Releases.
find_release() {
    set +e
    found="$(sh "$github" find "$1")"
    rc=$?
    set -e
    [ "$rc" -eq 0 ] || die "listing the Releases failed (exit $rc); $(phase_note "$2")"
    [ -n "$found" ] || return 0
    count="$(printf '%s\n' "$found" | wc -l | tr -d ' ')"
    [ "$count" -eq 1 ] || die "$count Releases name $1 (drafts may share a tag); resolve them by hand, then re-run; $(phase_note "$2")"
    printf '%s\n' "$found" | grep -Eqx '[0-9]+ (true|false) (true|false)' ||
        die "unexpected Release state for $1: '$found'; $(phase_note "$2")"
    printf '%s\n' "$found"
}

# read_latest PHASE [HINT] -> prints the tag GitHub marks Latest.
read_latest() {
    set +e
    current="$(sh "$github" latest)"
    rc=$?
    set -e
    [ "$rc" -eq 0 ] ||
        die "reading GitHub's Latest Release failed (exit $rc; a repository without a published Release answers 404 too)${2:+; $2}; $(phase_note "$1")"
    # The adapter checks the ref format; here: one non-empty word.
    case "$current" in
        '' | *[[:space:]]* | *[[:cntrl:]]*) die "unexpected Latest Release tag '$current'; $(phase_note "$1")" ;;
    esac
    printf '%s\n' "$current"
}

cmd_ensure() {
    [ "$#" -eq 4 ] || { echo "usage: release-github.sh ensure <X.Y.Z> <commit> <latest-version> <notes-file>" >&2; exit 2; }
    version="$1"
    commit="$2"
    latest_version="$3"
    notes_file="$4"
    is_version "$version" || { echo "release-github: expected X.Y.Z, got '$version'" >&2; exit 2; }
    is_commit "$commit" || { echo "release-github: expected a full commit SHA, got '$commit'" >&2; exit 2; }
    is_version "$latest_version" || die "the latest alias version '$latest_version' is not X.Y.Z; nothing was written"
    [ -s "$notes_file" ] || die "notes file '$notes_file' is missing or empty; nothing was written"
    tag="v$version"
    latest_tag="v$latest_version"
    if [ "$latest_version" = "$version" ]; then
        make_latest=true
    else
        make_latest=false
    fi
    behind="the latest image alias is at $latest_version, so GitHub's Latest should be $latest_tag: complete that Release first (re-run the github-release job of $latest_tag, wait for it if it is still queued, or create it by hand if its run predates this job), then re-run this one"

    # 1. Every read and check before the first write.
    state="$(find_release "$tag" before)" || exit 1
    id=""
    case "$state" in
        '') ;;
        *' false false') id="${state%% *}" ;;
        *' true '*) die "the Release of $tag is a draft; publish or delete it by hand, then re-run the github-release job; nothing was written" ;;
        *) die "the Release of $tag is a prerelease; a stable tag gets a stable Release: fix it by hand, then re-run the github-release job; nothing was written" ;;
    esac
    if [ "$make_latest" = true ]; then
        if [ -z "$id" ]; then
            action=create
        else
            current="$(read_latest before)" || exit 1
            if [ "$current" = "$tag" ]; then action=none; else action=set-latest; fi
        fi
    else
        current="$(read_latest before "$behind")" || exit 1
        [ "$current" = "$latest_tag" ] || die "GitHub marks $current as Latest, but $behind; nothing was written"
        if [ -z "$id" ]; then action=create; else action=none; fi
    fi
    set +e
    remote="$(sh "$github" tag-commit "$tag")"
    rc=$?
    set -e
    [ "$rc" -eq 0 ] || die "reading the remote tag $tag failed (exit $rc); nothing was written"
    [ -n "$remote" ] || die "$tag is not on the remote (deleted?); nothing was written"
    [ "$remote" = "$commit" ] ||
        die "$tag now names $remote, not this run's commit $commit (moved tag?); nothing was written, run the release for the tag's current commit"

    # 2. At most one write.
    case "$action" in
        create)
            sh "$github" create "$tag" "$tag" "$notes_file" "$make_latest" >&2 ||
                die "creating the Release of $tag failed (it may or may not exist now); re-run the github-release job, which reads the state first"
            echo "release $tag: created (latest=$make_latest; the latest image alias is at $latest_version)" >&2
            ;;
        set-latest)
            sh "$github" set-latest "$id" >&2 ||
                die "marking the Release of $tag Latest failed (it may or may not be Latest now); re-run the github-release job, which reads the state first"
            echo "release $tag: exists, title and notes left as published; marked Latest" >&2
            ;;
        none)
            echo "release $tag: exists, nothing to write (Latest is $current; the latest image alias is at $latest_version)" >&2
            ;;
    esac

    # 3. Read back (after no write, a failure here still changed nothing).
    if [ "$action" = none ]; then phase=before; else phase=after; fi
    state="$(find_release "$tag" "$phase")" || exit 1
    case "$state" in
        *' false false') ;;
        *) die "the Release of $tag reads back as '${state:-missing}', expected published and not a prerelease; $(phase_note "$phase")" ;;
    esac
    if [ "$make_latest" = true ]; then expected="$tag"; else expected="$latest_tag"; fi
    current="$(read_latest "$phase")" || exit 1
    [ "$current" = "$expected" ] || die "GitHub marks $current as Latest, expected $expected; $(phase_note "$phase")"
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
