#!/usr/bin/env sh
# GitHub adapter for release-github.sh (contract documented there), backed by
# the gh CLI (preinstalled on GitHub-hosted runners). The job passes GH_TOKEN
# (its GITHUB_TOKEN) and GITHUB_REPOSITORY (`owner/name`). Tested with a stub
# gh that applies the --jq filters with jq (release-github-test.sh).
#
#   find <tag>        `GET /repos/{repo}/releases`, every page (`--paginate`;
#                     a failed page fails the command), keeping the Releases
#                     whose tag_name is <tag>: one `<draft> <prerelease>` line
#                     each. Each page must be an array of objects with a
#                     string tag_name and boolean draft / prerelease, else the
#                     filter raises an error (gh exits non-zero), so a
#                     malformed answer never reads as "absent". Not
#                     `GET /releases/tags/{tag}`: it serves published Releases
#                     only and answers 404 for a draft as for a missing
#                     Release, so a 404 cannot prove absence.
#   create <tag> <title> <notes-file> <latest> <commit>
#                     `gh release create --verify-tag --target <commit>`:
#                     aborts when the tag is not on the remote. gh checks the
#                     tag and creates the Release in two requests; if the tag
#                     is deleted in between, GitHub creates it again from the
#                     target, here the run's commit rather than the default
#                     branch. Not a draft, not a prerelease; `--latest=<latest>`
#                     is always explicit, never GitHub's date-and-version
#                     default.
#   set-latest <tag>  `gh release edit --latest`.
#   latest            tag_name of `GET /repos/{repo}/releases/latest`.

set -eu

die() {
    echo "release-github-api: $*" >&2
    exit 1
}

repo="${GITHUB_REPOSITORY:?GITHUB_REPOSITORY is not set}"

# The tag is interpolated into a jq filter: accept only vX.Y.Z, one line.
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
        gh api --paginate "repos/$repo/releases?per_page=100" --jq '
            if type != "array" then error("the Releases listing is not an array")
            else .[]
                | if type != "object" or (.tag_name | type) != "string"
                     or (.draft | type) != "boolean" or (.prerelease | type) != "boolean"
                  then error("malformed Release entry in the listing")
                  elif .tag_name == "'"$2"'" then "\(.draft) \(.prerelease)"
                  else empty end
            end'
        ;;
    create)
        [ "$#" -eq 6 ] || die "usage: create <tag> <title> <notes-file> <latest> <commit>"
        check_tag "$2"
        [ -f "$4" ] || die "notes file '$4' does not exist"
        case "$5" in
            true | false) ;;
            *) die "latest must be true or false, got '$5'" ;;
        esac
        case "$6" in '' | *[!0-9a-f]*) die "expected a commit SHA, got '$6'" ;; esac
        gh release create "$2" --repo "$repo" --verify-tag --target "$6" \
            --title "$3" --notes-file "$4" --latest="$5"
        ;;
    set-latest)
        [ "$#" -eq 2 ] || die "usage: set-latest <tag>"
        check_tag "$2"
        gh release edit "$2" --repo "$repo" --latest
        ;;
    latest)
        [ "$#" -eq 1 ] || die "usage: latest"
        gh api "repos/$repo/releases/latest" --jq .tag_name
        ;;
    *)
        die "usage: release-github-api.sh find|create|set-latest|latest ..."
        ;;
esac
