#!/usr/bin/env sh
# Registry adapter for release-promotion.sh (contract documented there),
# backed by curl, `docker buildx imagetools` and jq.
#
#   inspect <image>:<tag>  -> `<digest> <version-label> <revision-label>`; exit 3
#                             only when the exact manifest request
#                             `HEAD /v2/<name>/manifests/<tag>` answers 404
#                             (OCI distribution-spec v1.1.1 "Checking if
#                             content exists in the registry"). A 200 pins the
#                             tag's digest from its `Docker-Content-Digest`
#                             header; every later read goes to `<image>@<digest>`
#                             and any failure there, like any other status
#                             (401/403/429/5xx, 3xx, a 200 without a digest) or
#                             a transport error, is a read error (exit 1,
#                             R3G-B1, R4-2).
#   copy <image>@<digest> <image>:<tag>
#                          -> `imagetools create --prefer-index=false`: a single
#                             source manifest or index is copied as is, so the
#                             tag's digest is the source digest (R3-22; without
#                             the flag a plain manifest would be wrapped in a new
#                             index with another digest).
#
# Why not `imagetools inspect --raw` for existence (R4-2): Buildx resolves the
# tag (HEAD) and then fetches the manifest by the returned digest (GET); a 404
# on that second request also reads "not found", so stderr cannot tell a
# missing tag from a tag whose manifest is unreadable.
#
# Authentication follows the registry's challenge (distribution token auth):
# a 401 with `WWW-Authenticate: Bearer realm=...,service=...,scope=...` is
# answered by a token request to the https realm with Basic credentials from
# REGISTRY_USERNAME / REGISTRY_PASSWORD (release.yml: github.actor and
# GITHUB_TOKEN, the same credential its docker/login-action uses), then the
# HEAD is repeated with the bearer token. A challenged request without those
# variables fails: an anonymous answer for a private package proves nothing.
# For the same reason, with those variables set an unchallenged 404 is a read
# error: "missing" then needs the authenticated HEAD's 404.
# Credentials and tokens go to curl through `-K -` (stdin), never argv, and
# never into messages. A registry without auth (a local `registry:2`) answers
# the first HEAD directly; RELEASE_REGISTRY_SCHEME=http reaches one over plain
# HTTP (tests only; default https).

set -eu

die() {
    echo "release-registry: $*" >&2
    exit 1
}

# All manifest kinds the release pushes or copies: without the index types a
# registry may answer 404 for an existing index.
ACCEPT='application/vnd.oci.image.index.v1+json, application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.list.v2+json, application/vnd.docker.distribution.manifest.v2+json'

# curl_quote VALUE -> VALUE escaped for a double-quoted curl config string.
curl_quote() {
    printf '%s' "$1" | sed 's/[\\"]/\\&/g'
}

# header_value FILE NAME -> the value of header NAME (lowercase) in the last
# response of FILE (a proxy's CONNECT response may precede it).
header_value() {
    awk -v name="$2" '
        /^HTTP\// { value = ""; next }
        {
            line = $0
            sub(/\r$/, "", line)
            i = index(line, ":")
            if (i > 0 && tolower(substr(line, 1, i - 1)) == name) {
                v = substr(line, i + 1)
                sub(/^[ \t]+/, "", v)
                sub(/[ \t]+$/, "", v)
                value = v
            }
        }
        END { print value }' "$1"
}

# challenge_param CHALLENGE KEY -> the quoted KEY="..." value, or nothing.
challenge_param() {
    printf '%s\n' "$1" | sed -n "s/.*[ ,]$2=\"\\([^\"]*\\)\".*/\\1/p"
}

# head_manifest URL [CURL-CONFIG] -> prints the HTTP status; headers in $hdr.
head_manifest() {
    printf '%s\n' "${2:-}" |
        curl -sS -I --suppress-connect-headers -K - -D "$hdr" -o /dev/null \
            -w '%{http_code}' -H "Accept: $ACCEPT" "$1"
}

# inspect_tag REF -> prints `missing`, or `<digest> <version> <revision>`;
# any failure exits non-zero (mapped to 1 by the caller, so a stray exit 3
# from curl or jq can never read as "missing").
inspect_tag() {
    set -eu
    ref="$1"
    tag="${ref##*:}"
    repo="${ref%:*}"
    host="${repo%%/*}"
    name="${repo#*/}"
    case "$ref" in *@*) die "expected <image>:<tag>, got '$ref'" ;; esac
    printf '%s\n' "$tag" | grep -Eq '^[A-Za-z0-9_][A-Za-z0-9._-]{0,127}$' ||
        die "expected <image>:<tag>, got '$ref'"
    [ "$host" != "$repo" ] && [ -n "$name" ] || die "expected <registry>/<name>:<tag>, got '$ref'"
    url="${RELEASE_REGISTRY_SCHEME:-https}://$host/v2/$name/manifests/$tag"

    code="$(head_manifest "$url")" || die "HEAD $url failed (transport error)"
    if [ "$code" = 401 ]; then
        challenge="$(header_value "$hdr" www-authenticate)"
        case "$challenge" in
            [Bb]earer\ *) ;;
            *) die "HEAD $url: unsupported authentication challenge '$challenge'" ;;
        esac
        realm="$(challenge_param "$challenge" realm)"
        service="$(challenge_param "$challenge" service)"
        scope="$(challenge_param "$challenge" scope)"
        case "$realm" in
            https://*) ;;
            *) die "HEAD $url: refusing to send credentials to token realm '$realm' (not https)" ;;
        esac
        [ -n "${REGISTRY_USERNAME:-}" ] && [ -n "${REGISTRY_PASSWORD:-}" ] ||
            die "$host requires authentication; set REGISTRY_USERNAME and REGISTRY_PASSWORD"
        [ -n "$scope" ] || scope="repository:$name:pull"
        set -- --data-urlencode "scope=$scope"
        [ -z "$service" ] || set -- "$@" --data-urlencode "service=$service"
        token_code="$(
            printf 'user = "%s:%s"\n' "$(curl_quote "$REGISTRY_USERNAME")" "$(curl_quote "$REGISTRY_PASSWORD")" |
                curl -sS -G -K - -o "$token_file" -w '%{http_code}' "$@" "$realm"
        )" || die "token request to $realm failed (transport error)"
        [ "$token_code" = 200 ] || die "token request to $realm answered $token_code"
        token="$(jq -er '(.token // .access_token) | strings | select(. != "")' "$token_file")" ||
            die "token response from $realm has no token"
        code="$(head_manifest "$url" "header = \"Authorization: Bearer $(curl_quote "$token")\"")" ||
            die "HEAD $url failed (transport error)"
    fi
    case "$code" in
        404)
            # With credentials configured (release.yml), only an
            # authenticated answer may prove absence: an unchallenged 404
            # could be a registry hiding a package from anonymous clients.
            if [ -z "${token:-}" ] && [ -n "${REGISTRY_USERNAME:-}${REGISTRY_PASSWORD:-}" ]; then
                die "HEAD $url answered 404 without an authentication challenge although credentials are set; refusing to treat it as missing"
            fi
            echo missing
            return 0
            ;;
        200) ;;
        *) die "HEAD $url answered $code; nothing is decided on an unreadable state" ;;
    esac
    digest="$(header_value "$hdr" docker-content-digest)"
    printf '%s\n' "$digest" | grep -Eq '^sha256:[0-9a-f]{64}$' ||
        die "HEAD $url answered 200 without a valid Docker-Content-Digest ('$digest')"

    # The tag exists: read its labels from the pinned digest, so a tag moved
    # in between cannot mix two images' facts and a missing child object is a
    # read error. `.Image` is the image config for a single manifest, and a
    # platform -> config map for an index (attestation manifests carry no
    # config and are skipped).
    image_json="$(docker buildx imagetools inspect "$repo@$digest" --format '{{json .Image}}')" ||
        die "reading the config of $repo@$digest failed (the tag exists)"
    labels="$(printf '%s' "$image_json" | jq -c '
        if type == "object" and has("config") then .config.Labels
        else [ .[]? | objects | select(has("config")) | .config.Labels ] | first
        end // {}')" || die "unexpected image config for $ref"
    version="$(printf '%s' "$labels" | jq -r '.["org.opencontainers.image.version"] // "-" | if . == "" then "-" else . end')" ||
        die "unexpected labels for $ref"
    revision="$(printf '%s' "$labels" | jq -r '.["org.opencontainers.image.revision"] // "-" | if . == "" then "-" else . end')" ||
        die "unexpected labels for $ref"
    printf '%s %s %s\n' "$digest" "$version" "$revision"
}

command="${1:-}"
case "$command" in
    inspect)
        [ "$#" -eq 2 ] || die "usage: inspect <image>:<tag>"
        work="$(mktemp -d)"
        trap 'rm -rf "$work"' EXIT
        hdr="$work/headers"
        token_file="$work/token"
        # Not in an `if` / `||` context, so `set -e` stays active inside.
        set +e
        result="$(inspect_tag "$2")"
        rc=$?
        set -e
        [ "$rc" -eq 0 ] || exit 1
        if [ "$result" = missing ]; then
            exit 3
        fi
        printf '%s\n' "$result"
        ;;
    copy)
        [ "$#" -eq 3 ] || die "usage: copy <image>@<digest> <image>:<tag>"
        docker buildx imagetools create --prefer-index=false -t "$3" "$2"
        ;;
    *)
        die "usage: release-registry.sh inspect|copy ..."
        ;;
esac
