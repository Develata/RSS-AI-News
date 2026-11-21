#!/usr/bin/env sh
# Registry adapter for release-promotion.sh (contract documented there),
# backed by `docker buildx imagetools` and jq.
#
#   inspect <image>:<tag>  -> `<digest> <version-label> <revision-label>`; exit 3
#                             only when the tag's own top-level manifest request
#                             answers "not found" / "manifest unknown"; once it
#                             resolves, any digest / child / config read
#                             failure is a read error (R3G-B1).
#   copy <image>@<digest> <image>:<tag>
#                          -> `imagetools create --prefer-index=false`: a single
#                             source manifest or index is copied as is, so the
#                             tag's digest is the source digest (R3-22; without
#                             the flag a plain manifest would be wrapped in a new
#                             index with another digest).

set -eu

die() {
    echo "release-registry: $*" >&2
    exit 1
}

command="${1:-}"
case "$command" in
    inspect)
        [ "$#" -eq 2 ] || die "usage: inspect <image>:<tag>"
        ref="$2"
        err="$(mktemp)"
        trap 'rm -f "$err"' EXIT
        # Existence is decided by the top-level manifest request alone (R3G-B1).
        # `--raw` resolves the tag and fetches only that manifest (Buildx
        # v0.38.2 `Printer.Print` returns before any child manifest or config
        # is loaded), so a "not found" here is about the tag itself. Any other
        # failure (auth, network, rate limit) is a read error.
        if ! docker buildx imagetools inspect "$ref" --raw >/dev/null 2>"$err"; then
            if grep -Eqi 'not found|manifest unknown' "$err"; then
                exit 3
            fi
            cat "$err" >&2
            die "inspect $ref failed"
        fi
        # The tag exists. `--format` also loads child manifests and image
        # configs, whose errors say "not found" too: from here on every
        # failure is a hard error, never "missing".
        manifest="$(docker buildx imagetools inspect "$ref" --format '{{json .Manifest}}')" ||
            die "reading the manifest of $ref failed (the tag exists)"
        digest="$(printf '%s' "$manifest" | jq -er '.digest')" || die "no digest for $ref"
        # Read the labels from the resolved digest, not the tag again, so a tag
        # moved in between cannot mix two images' facts. `.Image` is the image
        # config for a single manifest, and a platform -> config map for an
        # index (attestation manifests carry no config and are skipped).
        image_json="$(docker buildx imagetools inspect "${ref%:*}@$digest" --format '{{json .Image}}')" ||
            die "reading the config of ${ref%:*}@$digest failed"
        labels="$(printf '%s' "$image_json" | jq -c '
            if type == "object" and has("config") then .config.Labels
            else [ .[]? | objects | select(has("config")) | .config.Labels ] | first
            end // {}')" || die "unexpected image config for $ref"
        version="$(printf '%s' "$labels" | jq -r '.["org.opencontainers.image.version"] // "-" | if . == "" then "-" else . end')"
        revision="$(printf '%s' "$labels" | jq -r '.["org.opencontainers.image.revision"] // "-" | if . == "" then "-" else . end')"
        printf '%s %s %s\n' "$digest" "$version" "$revision"
        ;;
    copy)
        [ "$#" -eq 3 ] || die "usage: copy <image>@<digest> <image>:<tag>"
        docker buildx imagetools create --prefer-index=false -t "$3" "$2"
        ;;
    *)
        die "usage: release-registry.sh inspect|copy ..."
        ;;
esac
