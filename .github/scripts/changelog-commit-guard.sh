#!/usr/bin/env bash
#
# Guard against release-plz silently dropping commits from the generated changelog.
#
# WHY THIS EXISTS
# ---------------
# release-plz does not ask git for "the commits since the last tag". It walks
# history one commit at a time (release_plz_core::updater::get_package_diff):
#
#     start:  git log --format=%H -n 1 -- <package path>   # then `git checkout` it
#     step:   git log --format=%H -n 2 -- <package path>   # take the 2nd, checkout, repeat
#
# Because each step re-roots `git log` at the commit it just checked out, the walk
# collapses history into ONE ancestry chain. At a merge it follows whichever parent
# `git log` happens to list first (commit-date order) and the other parent's commits
# become unreachable for the rest of the walk — they are dropped from the changelog
# with no warning and no failure.
#
# That bites whenever a PR branch is merged without being up to date with `main`
# (two PRs cut from the same base, merged back to back). It cost us the
# `fix(cli): --quote-replies` entry in v0.5.4 and the `fix(send-delay)` entry in
# v0.5.2. The real fix is a merge policy — squash-merge, or "require branches to be
# up to date before merging" so every merge is a fast-forward of `main`. This script
# is the mechanical enforcement: it replays release-plz's walk and fails the release
# job when a commit in the release window would not make it into the changelog.
#
# Usage: changelog-commit-guard.sh [<ref>]        (default: HEAD)
set -euo pipefail

ref="${1:-HEAD}"
head_commit="$(git rev-parse "$ref^{commit}")"

# The release window starts at the last release tag (release-plz's `v{version}`).
if ! last_tag="$(git describe --tags --abbrev=0 --match 'v[0-9]*' "$head_commit" 2>/dev/null)"; then
    echo "changelog-commit-guard: no v* tag reachable from $ref — nothing to check."
    exit 0
fi
tag_commit="$(git rev-list -n 1 "$last_tag")"

if [ "$tag_commit" = "$head_commit" ]; then
    echo "changelog-commit-guard: $ref is the $last_tag commit — nothing to check."
    exit 0
fi

# `recorded` = release-plz would put this commit in the changelog. It records a
# commit only when `git show --name-only` reports files, which is empty for a
# clean merge commit (git shows the combined diff, and a conflict-free merge
# changes nothing relative to all parents).
recorded() {
    [ -n "$(git show --name-only --pretty=format: "$1")" ]
}

# --- Replay release-plz's walk ------------------------------------------------
# `covered` is the set of commits whose content reaches the changelog: every
# commit the walk records, plus — for a recorded merge — the commits that merge
# brought in (their PR shows up as the merge's own entry).
declare -A covered=()
cur="$(git log --format=%H -n 1 "$head_commit" -- .)"
# Bound the walk: it can only ever visit commits in the repository.
max_steps="$(git rev-list --count "$head_commit")"
for ((i = 0; i < max_steps; i++)); do
    if recorded "$cur"; then
        covered["$cur"]=1
        if [ "$(git rev-list --no-walk --count --min-parents=2 "$cur")" -gt 0 ]; then
            while read -r merged; do
                [ -n "$merged" ] && covered["$merged"]=1
            done < <(git rev-list "$cur^1..$cur^2")
        fi
    fi
    [ "$cur" = "$tag_commit" ] && break
    git merge-base --is-ancestor "$cur" "$tag_commit" && break
    next="$(git log --format=%H -n 2 "$cur" -- . | sed -n 2p)"
    [ -z "$next" ] && break
    cur="$next"
done

# --- Compare against the commits that are actually in the release window ------
dropped=()
while read -r sha; do
    [ -n "$sha" ] || continue
    [ -n "${covered[$sha]:-}" ] || dropped+=("$sha")
done < <(git rev-list --no-merges "$tag_commit..$head_commit")

if [ ${#dropped[@]} -eq 0 ]; then
    echo "changelog-commit-guard: OK — every commit in $last_tag..${ref} reaches the changelog."
    exit 0
fi

cat >&2 <<EOF
changelog-commit-guard: FAILED

release-plz's commit walk cannot reach the following commits in $last_tag..${ref},
so the generated changelog and the GitHub Release notes will be missing them:

$(git log --no-walk --format='  %h %s' "${dropped[@]}")

This happens when a PR branch is merged into main while it is behind main, so the
merge has two parents with divergent history. Fix the merge policy (squash-merge,
or require branches to be up to date before merging) and, for the release that is
already affected, add the missing entries to the release PR by hand.
EOF
exit 1
