#!/usr/bin/env bash
# Called by branch-build.sh build BRANCH.
set -euo pipefail
if (($# < 1 || $# > 2)) || { (($# == 2)) && [[ "$2" != --create-worktree ]]; }; then
    echo 'Usage: branch-build.sh build BRANCH [--create-worktree]' >&2
    exit 1
fi
branch=$1
git check-ref-format --branch "$branch" >/dev/null
script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
common_dir=$(git -C "$script_dir" rev-parse --path-format=absolute --git-common-dir)
base_repo=$(dirname -- "$common_dir")
if [[ "$branch" == origin/* ]] && ! git -C "$base_repo" show-ref --verify --quiet "refs/heads/$branch"; then
    branch=${branch#origin/}
fi
git check-ref-format --branch "$branch" >/dev/null
# Keep generated files ignored even when the base checkout predates this script.
folder_name=${branch//\//-}
for generated_dir in .worktrees builds; do
    if git -C "$base_repo" check-ignore --quiet -- "$generated_dir/"; then
        continue
    else
        status=$?
        if ((status != 1)); then exit "$status"; fi
        printf '\n/%s/\n' "$generated_dir" >> "$common_dir/info/exclude"
    fi
done
source_dir=''
candidate=''
while IFS= read -r line; do
    case "$line" in
        'worktree '*) candidate=${line#worktree } ;;
        "branch refs/heads/$branch") source_dir=$candidate; break ;;
    esac
done < <(git -C "$base_repo" worktree list --porcelain)
if [[ -z "$source_dir" ]]; then
    if [[ "${2:-}" != --create-worktree ]]; then
        echo "No checkout for '$branch'. Use --create-worktree to create one under the base repo's .worktrees/." >&2
        exit 1
    fi
    source_dir="$base_repo/.worktrees/$folder_name"
    if git -C "$base_repo" show-ref --verify --quiet "refs/heads/$branch"; then
        git -C "$base_repo" worktree add -- "$source_dir" "$branch"
    else
        if ! git -C "$base_repo" fetch --no-tags origin "refs/heads/$branch:refs/remotes/origin/$branch"; then
            echo "Could not fetch branch '$branch' from origin. Check the branch name and your connection." >&2
            exit 1
        fi
        git -C "$base_repo" worktree add --track -b "$branch" -- "$source_dir" "origin/$branch"
    fi
fi
case "$source_dir" in
    "$base_repo"|"$base_repo/"*) ;;
    *) echo "The branch checkout is outside the base repository: $source_dir. Move it into .worktrees/ first." >&2; exit 1 ;;
esac
case "$(uname -s)" in
    Darwin|Linux) ;;
    *) echo 'Use branch-build.ps1 on Windows.' >&2; exit 1 ;;
esac
target_dir="$base_repo/builds/$folder_name"
if [[ -f "$target_dir/.branch" && "$(cat "$target_dir/.branch")" != "$branch" ]]; then
    echo "Build folder '$target_dir' already belongs to another branch." >&2
    exit 1
fi
mkdir -p -- "$target_dir"
printf '%s\n' "$branch" > "$target_dir/.branch"
commit=$(git -C "$source_dir" rev-parse HEAD)
echo "Building $branch ($commit) from $source_dir"
# A subshell preserves the caller's working directory and uses branch-local config.
(
    cd -- "$source_dir"
    cargo build --locked -p lan-mouse --target-dir "$target_dir"
)
echo "Build output: $target_dir"
