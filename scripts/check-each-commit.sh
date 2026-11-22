#!/usr/bin/env bash
# Check that every commit on the current branch formats, builds, and passes
# tests.
#
# Usage: scripts/check-each-commit.sh [BASE [TIP]]
#
# BASE defaults to origin/main and TIP to HEAD. In a colocated jj repo, pass the
# working-copy commit's ID as TIP, since Git's HEAD is its parent:
#
#     scripts/check-each-commit.sh origin/main "$(jj log -r @ --no-graph -T commit_id)"
#
# Each commit in BASE..TIP is checked out in a temporary worktree, so the
# current checkout and its uncommitted changes are left alone. The build shares
# this checkout's target directory to avoid rebuilding from scratch for every
# commit.

set -uo pipefail

base=${1:-origin/main}
tip=${2:-HEAD}
root=$(git rev-parse --show-toplevel) || exit 1
commits=$(git rev-list --reverse "$base..$tip") || exit 1
if [[ -z $commits ]]; then
    echo "No commits in $base..$tip"
    exit 0
fi

tmp=$(mktemp -d "${TMPDIR:-/tmp}/check-each-commit.XXXXXX") || exit 1
cleanup() {
    [[ -d $tmp/tree ]] && git worktree remove --force "$tmp/tree"
    rm -rf "$tmp"
}
trap cleanup EXIT
git worktree add --quiet --detach "$tmp/tree" "$tip" || exit 1

export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$root/target}
failed=0
for commit in $commits; do
    summary=$(git log -1 --format='%h %s' "$commit")
    if ! git -C "$tmp/tree" checkout --quiet --detach "$commit"; then
        echo "FAIL  $summary: checkout failed"
        failed=1
        continue
    fi
    problems=()
    warnings=0

    if ! (cd "$tmp/tree" && cargo +nightly fmt --check -- --color never) >"$tmp/fmt.log" 2>&1; then
        problems+=("fmt")
    fi
    if (cd "$tmp/tree" && cargo build --all-targets) >"$tmp/build.log" 2>&1; then
        warnings=$(grep '^warning' "$tmp/build.log" | grep -cvE 'generated [0-9]+ warnings?')
        if ! (cd "$tmp/tree" && cargo test) >"$tmp/tests.log" 2>&1; then
            problems+=("tests")
        fi
    else
        problems+=("build")
    fi

    note=""
    if [[ $warnings -eq 1 ]]; then
        note=" (1 build warning)"
    elif [[ $warnings -gt 1 ]]; then
        note=" ($warnings build warnings)"
    fi
    if [[ ${#problems[@]} -eq 0 ]]; then
        echo "ok    $summary$note"
    else
        echo "FAIL  $summary: $(IFS=,; echo "${problems[*]}") failed$note"
        for problem in "${problems[@]}"; do
            echo "      $problem:"
            if [[ $problem == fmt ]]; then
                head -20 "$tmp/fmt.log"
            else
                grep -E '^(error|test .* FAILED|---- )' -A6 "$tmp/$problem.log" | head -40
            fi | sed 's/^/        /'
        done
        failed=1
    fi
done
exit $failed
