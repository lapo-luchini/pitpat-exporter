#!/usr/bin/env bash
# SPDX-License-Identifier: ISC
#
# Release a new pitpat version (R5): bump the version everywhere, run the
# tests, commit and tag — but do NOT push (the owner pushes).
#
#   contrib/release.sh X.Y.Z
#
# Requirements it enforces up front: a clean tree, HEAD on `trunk`
# (attached, or detached on trunk's commit — R6: how jj's colocated repo
# leaves git), and a version strictly newer than the current one
# (numeric, not string, compare). It sets the version in Cargo.toml,
# refreshes Cargo.lock with cargo (never by hand) and rewrites the
# plasmoid's metadata.json with jq; then `cargo test`, a `release X.Y.Z`
# commit and a signed tag `X.Y.Z` with the message `Version X.Y.Z`, like
# the existing tags.
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

# R7: on NixOS, cargo and jq usually live only in the nix dev shell. If
# any tool is missing, re-exec inside `nix develop` (guard variable so
# this cannot loop; --extra-experimental-features is harmless when
# flakes are already on). The check is on missing tools, not on NixOS,
# so it helps on any machine with nix and does nothing where the tools
# are installed.
missing=""
for tool in cargo jq; do
    command -v "$tool" >/dev/null 2>&1 || missing="$missing $tool"
done
if [ -n "$missing" ]; then
    if [ "${PITPAT_RELEASE_NIX:-}" = 1 ]; then
        echo "refusing: even inside the nix dev shell these tools are" \
            "missing:$missing" >&2
        exit 1
    fi
    if ! command -v nix >/dev/null 2>&1; then
        echo "refusing: missing tools:$missing and no nix on PATH to get" \
            "them (install cargo and jq, or run from a nix dev shell)" >&2
        exit 1
    fi
    self="$(readlink -f "$0")"
    echo "missing tools:$missing — re-running inside the nix dev shell"
    export PITPAT_RELEASE_NIX=1
    exec nix develop "$PWD" \
        --extra-experimental-features 'nix-command flakes' \
        --command "$self" "$@"
fi

# R6: the branch check accepts HEAD attached to trunk, or HEAD detached
# on trunk's commit (how a jj-colocated repo leaves git after
# `jj new trunk`); anything else is refused with the commit HEAD is on
detached=""
branch="$(git symbolic-ref --short HEAD 2>/dev/null || true)"
if [ "$branch" = "trunk" ]; then
    :
else
    head_commit="$(git rev-parse HEAD 2>/dev/null || true)"
    trunk_commit="$(git rev-parse refs/heads/trunk 2>/dev/null || true)"
    if [ -z "$branch" ] && [ -n "$head_commit" ] \
        && [ "$head_commit" = "$trunk_commit" ]; then
        detached=1
        old_commit="$head_commit"
    else
        # U3: one echo argument, so no stray space before the ';'; short
        # hash in the message
        where="detached $(git rev-parse --short HEAD 2>/dev/null || true)"
        [ -n "$branch" ] && where="branch '$branch'"
        echo "refusing: releases are cut from trunk (HEAD is on $where);" \
            "attach with 'git switch trunk', with jj: 'jj new trunk'" >&2
        exit 1
    fi
fi
if [ -n "$(git status --porcelain --untracked-files=no)" ]; then
    echo "refusing: the working tree is not clean" \
        "(with jj: 'jj new trunk', then check '@')" >&2
    exit 1
fi

cur="$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)"
if ! [[ "$cur" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    echo "refusing: cannot read the current version from Cargo.toml" >&2
    exit 1
fi
# numeric compare, so 1.10.0 > 1.9.0 (a plain sort would say otherwise)
if ! [[ "$new" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] \
    || [ "$(printf '%s\n%s\n' "$new" "$cur" | sort -t. -k1,1n -k2,2n -k3,3n | head -1)" != "$cur" ] \
    || [ "$new" = "$cur" ]; then
    echo "refusing: $new is not newer than the current version $cur" >&2
    exit 1
fi

# U2: refuse an already-released HEAD up front, before any change —
# these only surfaced after the bump and a full cargo test
if git rev-parse -q --verify "refs/tags/$new" >/dev/null 2>&1; then
    echo "refusing: tag $new already exists" >&2
    exit 1
fi
head_tag="$(git tag --points-at HEAD | grep -E '^[0-9]+\.[0-9]+\.[0-9]+$' | head -1 || true)"
if [ -n "$head_tag" ]; then
    echo "refusing: HEAD is already released as $head_tag; commit something first" >&2
    exit 1
fi

# Failing steps (e.g. `cargo test`) must not leave the tree with a
# half-applied bump: the next run would refuse it as dirty. The three
# files are tracked and the tree was verified clean, so restoring them
# from HEAD is a full rollback of the bump phase.
files="Cargo.toml Cargo.lock plasmoid/it.lapo.pitpat/metadata.json"
rollback() {
    echo
    echo "release failed (line $1): the tree is restored, nothing was" \
        "committed or tagged; fix the problem and run the script again" >&2
    # S6: the jq temp file is untracked, so checkout does not remove it
    rm -f metadata.json.new
    git reset -q HEAD -- $files
    git checkout -q -- $files
}
trap 'rollback $LINENO' ERR

sed -i "0,/^version = \"$cur\"$/s//version = \"$new\"/" Cargo.toml
cargo update -p pitpat --offline >/dev/null
jq --arg v "$new" '.KPlugin.Version = $v' \
    plasmoid/it.lapo.pitpat/metadata.json > metadata.json.new
mv metadata.json.new plasmoid/it.lapo.pitpat/metadata.json

echo "version: $cur -> $new, running cargo test"
cargo test
trap - ERR

git add $files
git commit -m "release $new" || {
    git reset -q HEAD -- $files
    git checkout -q -- $files
    echo "release failed: could not commit; the tree is restored" >&2
    exit 1
}
if ! git tag -s "$new" -m "Version $new"; then
    # the commit exists: drop it again (its content is exactly HEAD^ plus
    # the bump, and the bump is reverted with the commit). Signed tags
    # make this the likely failure point: a cancelled pinentry must not
    # leave the release commit behind
    git reset -q --hard HEAD~1
    echo "release failed: could not create signed tag $new; the release" \
        "commit was undone and the tree is restored" >&2
    exit 1
fi
# R6: on a detached HEAD the commit was made while HEAD stays detached,
# and the tag points at it — only now move the branch, with a
# compare-and-swap so a trunk that moved in the meantime cannot be
# clobbered. On an attached HEAD this does not run (the commit is
# already on trunk).
if [ -n "$detached" ]; then
    new_commit="$(git rev-parse HEAD)"
    if ! git update-ref refs/heads/trunk "$new_commit" "$old_commit"; then
        echo "release failed: trunk moved during the release; undoing" \
            "the commit and the tag" >&2
        git reset -q --hard "$old_commit"
        git tag -d "$new" >/dev/null
        exit 1
    fi
    echo "(trunk now points at the release commit; HEAD stays detached" \
        "on it — with jj the next jj command imports both)"
fi

echo
echo "released $new: review the commit and the tag, then push them:"
echo "  git push --follow-tags origin trunk"
echo "(the pre-push hook in contrib/githooks refuses a tag whose version"
echo " does not match the Cargo.toml of the tagged commit)"
