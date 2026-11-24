#!/usr/bin/env sh
# GitHub adapter for release-github.sh (contract documented there), backed by
# the gh CLI and git (both preinstalled on GitHub-hosted runners). The job
# passes GH_TOKEN (its GITHUB_TOKEN) and GITHUB_REPOSITORY (`owner/name`);
# `origin` is the checkout's remote. Tested with a stub gh that applies the
# --jq filters with jq, and a stub git (release-github-test.sh).
#
#   find <tag>        `GET /repos/{repo}/releases`, every page (`--paginate`;
#                     a failed page fails the command), keeping the Releases
#                     whose tag_name is <tag>: one `<id> <draft> <prerelease>`
#                     line each. Each page must be an array of objects with a
#                     number id, a string tag_name and boolean draft /
#                     prerelease, else the filter raises an error (gh exits
#                     non-zero). Every parsed page also emits a marker line:
#                     gh returns an answer without a body (HTTP 204) without
#                     running the filter, so no marker at all means nothing
#                     was read, never "no Release". This covers the first
#                     page only: a later page answering 204 would end the
#                     listing early, unnoticed (GitHub does not document
#                     such an answer for this endpoint). Not `GET /releases/tags/{tag}`: it
#                     serves published Releases only and answers 404 for a
#                     draft as for a missing Release.
#   create <tag> <title> <notes-file> <latest>
#                     `gh release create --verify-tag`: aborts when the tag is
#                     not on the remote. Not a draft, not a prerelease;
#                     `--latest=<latest>` is always explicit, never GitHub's
#                     date-and-version default. No `--target`: GitHub checks
#                     the target commit (default: the default branch's head)
#                     for workflow-file changes against the default branch and
#                     answers 404 to a GITHUB_TOKEN when there are any
#                     (REST "Create a release"), which a tag behind main or on
#                     an older line would hit. gh checks the tag and creates
#                     the Release in two requests; a tag deleted in between
#                     would be recreated from the default branch, which only a
#                     tag ruleset excludes (docs/operations/ci-and-release.md).
#   set-latest <id>   `PATCH /repos/{repo}/releases/{id}` with only
#                     make_latest=true: no lookup by tag inside gh, and the
#                     tag, target, title and body are not sent.
#   latest            tag_name of `GET /repos/{repo}/releases/latest`; an
#                     answer that is not an object with a non-empty string
#                     tag_name free of whitespace and control characters, or
#                     whose `refs/tags/<tag_name>` fails `git check-ref-format`,
#                     is an error.
#   tag-commit <tag>  the commit the remote tag names (the peeled `^{}` line
#                     of an annotated tag), from `git ls-remote origin`;
#                     nothing when the tag is not on the remote; a git error
#                     fails.

set -eu

die() {
    echo "release-github-api: $*" >&2
    exit 1
}

repo="${GITHUB_REPOSITORY:?GITHUB_REPOSITORY is not set}"

# The tag is interpolated into a jq filter and a ref: accept only vX.Y.Z,
# one line.
check_tag() {
    case "$1" in v*) ;; *) die "expected vX.Y.Z, got '$1'" ;; esac
    case "${1#v}" in '' | *[!0-9.]*) die "expected vX.Y.Z, got '$1'" ;; esac
    printf '%s\n' "$1" | grep -Eq '^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$' ||
        die "expected vX.Y.Z, got '$1'"
}

case "${1:-}" in
    find)
        [ "$#" -eq 2 ] || die "usage: find <tag>"
        check_tag "$2"
        listing="$(gh api --paginate "repos/$repo/releases?per_page=100" --jq '
            if type != "array" then error("the Releases listing is not an array")
            else "page",
                (.[]
                | if type != "object" or (.id | type) != "number" or (.tag_name | type) != "string"
                     or (.draft | type) != "boolean" or (.prerelease | type) != "boolean"
                  then error("malformed Release entry in the listing")
                  elif .tag_name == "'"$2"'" then "\(.id) \(.draft) \(.prerelease)"
                  else empty end)
            end')" || die "listing the Releases failed"
        printf '%s\n' "$listing" | grep -qx page || die "the Releases listing returned no page (an answer without a body?)"
        # awk, not `grep -v || true`: a filter error must fail, not read as
        # "no Release".
        printf '%s\n' "$listing" | awk '$0 != "page"'
        ;;
    create)
        [ "$#" -eq 5 ] || die "usage: create <tag> <title> <notes-file> <latest>"
        check_tag "$2"
        [ -f "$4" ] || die "notes file '$4' does not exist"
        case "$5" in
            true | false) ;;
            *) die "latest must be true or false, got '$5'" ;;
        esac
        gh release create "$2" --repo "$repo" --verify-tag --title "$3" --notes-file "$4" --latest="$5"
        ;;
    set-latest)
        [ "$#" -eq 2 ] || die "usage: set-latest <id>"
        case "$2" in '' | *[!0-9]*) die "expected a numeric Release id, got '$2'" ;; esac
        gh api -X PATCH "repos/$repo/releases/$2" -f make_latest=true --jq .id
        ;;
    latest)
        [ "$#" -eq 1 ] || die "usage: latest"
        tag="$(gh api "repos/$repo/releases/latest" --jq '
            if type == "object" and (.tag_name | type) == "string" and (.tag_name | length) > 0
               and ((.tag_name | explode | any(. < 33 or . == 127)) | not) and ((.tag_name | test("\\s")) | not)
            then .tag_name
            else error("unexpected answer from releases/latest") end')" || die "reading releases/latest failed"
        git check-ref-format "refs/tags/$tag" || die "releases/latest names '$tag', not a valid tag name"
        printf '%s\n' "$tag"
        ;;
    tag-commit)
        [ "$#" -eq 2 ] || die "usage: tag-commit <tag>"
        check_tag "$2"
        refs="$(git ls-remote origin "refs/tags/$2" "refs/tags/$2^{}")" || die "git ls-remote origin failed"
        printf '%s\n' "$refs" | awk -v r="refs/tags/$2" '
            $2 == r { plain = $1 }
            $2 == r "^{}" { peeled = $1 }
            END { if (peeled != "") print peeled; else if (plain != "") print plain }'
        ;;
    *)
        die "usage: release-github-api.sh find|create|set-latest|latest|tag-commit ..."
        ;;
esac
