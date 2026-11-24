#!/usr/bin/env sh
# GitHub adapter for release-github.sh (contract documented there), backed by
# the gh CLI (preinstalled on GitHub-hosted runners). The job passes GH_TOKEN
# (its GITHUB_TOKEN) and GITHUB_REPOSITORY (`owner/name`).
#
#   find <tag>        `GET /repos/{repo}/releases`, every page, keeping the
#                     Releases whose tag_name is <tag>: one `<draft>
#                     <prerelease>` line each. Not `GET /releases/tags/{tag}`:
#                     it serves published Releases only and answers 404 for
#                     a draft as for a missing Release, so a 404 cannot
#                     prove absence.
#   create <tag> <title> <notes-file> <latest>
#                     `gh release create --verify-tag`: aborts when the tag is
#                     not on the remote, so publishing a Release never creates
#                     a tag (a bare `POST /releases` would create it from the
#                     default branch). Not a draft, not a prerelease;
#                     `--latest=<latest>` is always explicit, never GitHub's
#                     date-and-version default.
#   set-latest <tag>  `gh release edit --latest`.
#   latest            tag_name of `GET /repos/{repo}/releases/latest`.

set -eu

die() {
    echo "release-github-api: $*" >&2
    exit 1
}

repo="${GITHUB_REPOSITORY:?GITHUB_REPOSITORY is not set}"

# The tag is interpolated into a jq filter: accept only vX.Y.Z.
check_tag() {
    printf '%s\n' "$1" | grep -Eq '^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$' ||
        die "expected vX.Y.Z, got '$1'"
}

case "${1:-}" in
    find)
        [ "$#" -eq 2 ] || die "usage: find <tag>"
        check_tag "$2"
        gh api --paginate "repos/$repo/releases?per_page=100" \
            --jq ".[] | select(.tag_name == \"$2\") | \"\\(.draft) \\(.prerelease)\""
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
