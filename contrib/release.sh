#!/usr/bin/env bash
# SPDX-License-Identifier: ISC
#
# Release a new pitpat version (R5): bump the version everywhere, run the
# tests, commit and tag — but do NOT push (the owner pushes).
#
#   contrib/release.sh X.Y.Z
#
# Requirements it enforces up front: a clean tree, the `trunk` branch, and
# a version strictly newer than the current one (numeric, not string,
# compare). It sets the version in Cargo.toml, refreshes Cargo.lock with
# cargo (never by hand) and rewrites the plasmoid's metadata.json with jq;
# then `cargo test`, a `release X.Y.Z` commit and an annotated tag
# `X.Y.Z` with the message `Version X.Y.Z`, like the existing tags.
#
# The last local gate against a wrong version becoming public is the
# pre-push hook in contrib/githooks (see README's Releasing section);
# the tag itself is created here, already correct.
set -euo pipefail

if [ $# -ne 1 ] || ! [[ "$1" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    echo "usage: contrib/release.sh X.Y.Z" >&2
    exit 1
fi
new="$1"

cd "$(dirname "$0")/.."

branch="$(git symbolic-ref --short HEAD 2>/dev/null || true)"
if [ "$branch" != "trunk" ]; then
    echo "refusing: releases are cut from trunk (on '$branch')" >&2
    exit 1
fi
if [ -n "$(git status --porcelain --untracked-files=no)" ]; then
    echo "refusing: the working tree is not clean" >&2
    exit 1
fi

cur="$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)"
if ! [ "$cur" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]; then
    echo "refusing: cannot read the current version from Cargo.toml" >&2
    exit 1
fi
# numeric compare, so 1.10.0 > 1.9.0 (a plain sort would say otherwise)
if [ "$(printf '%s\n%s\n' "$new" "$cur" | sort -t. -k1,1n -k2,2n -k3,3n | head -1)" != "$cur" ] \
    || [ "$new" = "$cur" ]; then
    echo "refusing: $new is not newer than the current version $cur" >&2
    exit 1
fi

sed -i "0,/^version = \"$cur\"$/s//version = \"$new\"/" Cargo.toml
cargo update -p pitpat --offline >/dev/null
jq --arg v "$new" '.KPlugin.Version = $v' \
    plasmoid/it.lapo.pitpat/metadata.json > metadata.json.new
mv metadata.json.new plasmoid/it.lapo.pitpat/metadata.json

echo "version: $cur -> $new, running cargo test"
cargo test

git add Cargo.toml Cargo.lock plasmoid/it.lapo.pitpat/metadata.json
git commit -m "release $new"
git tag -a "$new" -m "Version $new"

echo
echo "released $new: review the commit and the tag, then push them:"
echo "  git push --follow-tags"
echo "(the pre-push hook in contrib/githooks refuses a tag whose version"
echo " does not match the Cargo.toml of the tagged commit)"
