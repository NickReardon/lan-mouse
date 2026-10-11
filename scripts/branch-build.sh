#!/usr/bin/env bash
set -euo pipefail

usage() {
    cat <<'EOF'
Usage: bash scripts/branch-build.sh BRANCH [--build | --launch]
       bash scripts/branch-build.sh --run-id ID [--launch]
Options:
  --build             Reuse a current build or request one, then exit.
  --launch            Download, extract, and launch (quit other instances first).
  --run-id ID         Download a specific successful manual build on each machine.
  --repo OWNER/REPO   Default: NickReardon/lan-mouse
  --dest DIRECTORY    Default: ~/Downloads (creates a unique directory per download)
  -h, --help          Show this help.
Requires GitHub CLI (gh auth login); macOS also needs unzip.
EOF
}

branch='' run_id='' build=false launch=false
repo='NickReardon/lan-mouse'
destination="$HOME/Downloads"
while (($#)); do
    case "$1" in
        --build) build=true; shift ;;
        --launch) launch=true; shift ;;
        --run-id|--repo|--dest)
            if (($# < 2)) || [[ -z "$2" ]]; then echo "Missing value for $1" >&2; exit 1; fi
            case "$1" in
                --run-id) run_id="$2" ;;
                --repo) repo="$2" ;;
                --dest) destination="$2" ;;
            esac
            shift 2 ;;
        -h|--help) usage; exit 0 ;;
        -*) echo "Unknown option: $1" >&2; exit 1 ;;
        *) if [[ -n "$branch" ]]; then usage >&2; exit 1; fi; branch="$1"; shift ;;
    esac
done
if [[ -z "$branch" && -z "$run_id" ]] || [[ -n "$branch" && -n "$run_id" ]]; then usage >&2; exit 1; fi
if [[ -n "$run_id" && ! "$run_id" =~ ^[0-9]+$ ]]; then echo 'Run ID must be numeric.' >&2; exit 1; fi
if $build && { $launch || [[ -n "$run_id" ]]; }; then echo '--build requires a branch and cannot use --launch.' >&2; exit 1; fi
command -v gh >/dev/null || { echo 'Install GitHub CLI (https://cli.github.com) and run gh auth login.' >&2; exit 1; }

os=$(uname -s)
arch=$(uname -m)
case "$os/$arch" in
    Darwin/arm64) artifact=lan-mouse-macos-arm64 ;;
    Darwin/x86_64) artifact=lan-mouse-macos-intel ;;
    Linux/x86_64) artifact=lan-mouse-linux-x86_64 ;;
    Linux/aarch64|Linux/arm64) artifact=lan-mouse-linux-arm64 ;;
    *) echo "Unsupported platform: $os/$arch" >&2; exit 1 ;;
esac

template='{{.databaseId}} {{.headSha}} {{.url}} {{.headBranch}}'
if [[ -n "$run_id" ]]; then
    status=$(gh run view "$run_id" -R "$repo" --json conclusion,event,workflowName \
        --template '{{.conclusion}}|{{.event}}|{{.workflowName}}')
    if [[ "$status" != 'success|workflow_dispatch|Release' ]]; then
        echo 'Choose a successful manual Release workflow run.' >&2; exit 1
    fi
    metadata=$(gh run view "$run_id" -R "$repo" --json databaseId,headSha,url,headBranch --template "$template")
else
    encoded_branch=${branch//%/%25}
    encoded_branch=${encoded_branch//#/%23}
    head_commit=$(gh api "repos/$repo/git/ref/heads/$encoded_branch" --jq '.object.sha')
    if [[ ! "$head_commit" =~ ^[0-9a-f]{40,64}$ ]]; then echo 'Could not resolve the remote branch commit.' >&2; exit 1; fi
    echo "Remote branch: $branch | Commit: $head_commit"
    candidates=$(gh run list -R "$repo" -w release.yml --branch "$branch" --event workflow_dispatch \
        --commit "$head_commit" --status success --limit 100 --json databaseId,headSha,url,headBranch \
        --template "{{range .}}$template{{\"\\n\"}}{{end}}")
    metadata=''
    while read -r candidate_id candidate_commit candidate_url candidate_branch; do
        [[ -n "$candidate_id" && "$candidate_commit" == "$head_commit" ]] || continue
        artifact_ids=$(gh api "repos/$repo/actions/runs/$candidate_id/artifacts" --paginate \
            --jq ".artifacts[] | select(.name == \"$artifact\" and .expired == false) | .id")
        if [[ -n "$artifact_ids" ]]; then
            metadata="$candidate_id $candidate_commit $candidate_url $candidate_branch"
            break
        fi
    done <<< "$candidates"
    if [[ -z "$metadata" ]]; then
        active=$(gh run list -R "$repo" -w release.yml --branch "$branch" --event workflow_dispatch \
            --commit "$head_commit" --limit 100 --json headSha,status,url \
            --jq '[.[] | select(.status != "completed")][0] | if . == null then empty else "\(.headSha) \(.status) \(.url)" end')
        if [[ -n "$active" ]]; then
            read -r active_commit active_status active_url <<< "$active"
            if [[ "$active_commit" != "$head_commit" ]]; then echo 'Unexpected build commit.' >&2; exit 1; fi
            echo "Build already $active_status: $active_url"
            if $build; then exit 0; fi
            echo 'The current commit is still building. Download after it completes.' >&2; exit 1
        fi
        if ! $build; then echo 'No downloadable build for the current commit. Request one with --build.' >&2; exit 1; fi
        gh workflow run release.yml -R "$repo" --ref "$branch"
        echo "Build requested for $branch ($head_commit). Download after it completes."
        exit 0
    fi
    if $build; then echo "Reusing successful build: $metadata"; exit 0; fi
fi
if [[ -z "$metadata" ]]; then echo "No successful manual build for $branch. Request one with --build." >&2; exit 1; fi
read -r run_id commit url branch <<< "$metadata"
echo "Branch: $branch | Run: $run_id | Commit: $commit"
echo "$url"
if [[ -z "${head_commit:-}" ]]; then echo 'Pinned run selected; this may predate the current branch commit.'; fi
mkdir -p "$destination"
package_dir=$(mktemp -d "$destination/lan-mouse-$run_id.XXXXXX")
gh run download "$run_id" -R "$repo" -n "$artifact" -D "$package_dir"
if [[ "$os" == Darwin ]]; then
    unzip -q "$package_dir/$artifact.zip" -d "$package_dir/app"
    executable="$package_dir/app/Lan Mouse.app"
    [[ -d "$executable" ]] || { echo "Missing app: $executable" >&2; exit 1; }
else
    executable="$package_dir/$artifact"
    [[ -f "$executable" ]] || { echo "Missing executable: $executable" >&2; exit 1; }
    chmod +x "$executable"
fi
echo "Ready: $executable"
if $launch; then
    echo 'Exit any other Lan Mouse instance before testing.'
    if [[ "$os" == Darwin ]]; then open "$executable"; else "$executable"; fi
fi
