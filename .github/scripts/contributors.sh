#!/usr/bin/env bash
# Prints a "**Contributors:** @a, @b, …" line for the release at the given tag:
# everyone who authored or co-authored a commit since the previous release tag,
# most commits first. GitHub resolves commit authors and Co-authored-by
# trailers to accounts by their email address; a contributor whose email isn't
# linked to an account is listed by name instead. Merge commits and the
# release commit cargo-release makes are not counted.
#
# Usage: contributors.sh <tag>   (needs gh, jq and the full history with tags)
set -euo pipefail

tag=$1
repo=${GITHUB_REPOSITORY:-$(gh repo view --json nameWithOwner -q .nameWithOwner)}
owner=${repo%/*}
name=${repo#*/}

if prev=$(git describe --tags --abbrev=0 --match 'v[0-9]*' "$tag^" 2>/dev/null); then
    range="$prev..$tag"
else
    range=$tag
fi

mapfile -t commits < <(git log --no-merges --format='%H %s' "$range" |
    grep -v '^[0-9a-f]* chore: Release ' | cut -d' ' -f1)

# One GraphQL request per 50 commits, each commit under its own alias.
authors=$(
    for ((i = 0; i < ${#commits[@]}; i += 50)); do
        query="{ repository(owner: \"$owner\", name: \"$name\") {"
        for oid in "${commits[@]:i:50}"; do
            query+=" c$oid: object(oid: \"$oid\") { ... on Commit { authors(first: 20) { nodes { name user { login } } } } }"
        done
        query+=" } }"
        # One line per commit and person, so a person is counted once per commit.
        gh api graphql -f query="$query" --jq '
            .data.repository[]
            | [.authors.nodes[] | if .user then "@" + .user.login else .name end]
            | unique[]'
    done
)

[ -n "$authors" ] || exit 0

printf '%s\n' "$authors" | jq -Rrs '
    split("\n") | map(select(. != ""))
    | group_by(.) | map({who: .[0], n: length})
    | sort_by(-.n, (.who | ascii_downcase))
    | "**Contributors:** " + (map(.who) | join(", "))'
