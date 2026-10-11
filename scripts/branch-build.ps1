<#
.SYNOPSIS
Build a local branch or download a GitHub branch package (Windows).
.EXAMPLE
./scripts/branch-build.ps1 build codex/edge-position -CreateWorktree
.EXAMPLE
./scripts/branch-build.ps1 -Branch feature/MyChange -Build
.EXAMPLE
./scripts/branch-build.ps1 -Branch feature/MyChange -Launch
.EXAMPLE
./scripts/branch-build.ps1 -RunId 123456789 -Launch
#>
[CmdletBinding(DefaultParameterSetName = 'Branch')]
param(
    [Parameter(Mandatory, Position = 0, ParameterSetName = 'Local')]
    [ValidateSet('build')]
    [string]$Action,
    [Parameter(Mandatory, ParameterSetName = 'Branch')]
    [Parameter(Mandatory, Position = 1, ParameterSetName = 'Local')]
    [ValidateNotNullOrEmpty()]
    [string]$Branch,
    [Parameter(Mandatory, ParameterSetName = 'Run')]
    [ValidatePattern('^[0-9]+$')]
    [string]$RunId,
    [Parameter(ParameterSetName = 'Branch')]
    [switch]$Build,
    [switch]$Launch,
    [Parameter(ParameterSetName = 'Local')]
    [switch]$CreateWorktree,
    [string]$Repo = 'NickReardon/lan-mouse',
    [string]$Destination = (Join-Path $HOME 'Downloads')
)

$ErrorActionPreference = 'Stop'
if ($env:OS -ne 'Windows_NT') { throw 'Use branch-build.sh on macOS or Linux.' }
if ($Action -eq 'build') {
    & (Join-Path $PSScriptRoot 'local-build.ps1') -Branch $Branch -CreateWorktree:$CreateWorktree
    return
}
if (-not (Get-Command gh -ErrorAction SilentlyContinue)) {
    throw 'Install GitHub CLI (https://cli.github.com) and run gh auth login first.'
}

function Invoke-GitHub {
    $result = & gh @args
    if ($LASTEXITCODE -ne 0) { throw "GitHub CLI failed (exit $LASTEXITCODE)." }
    return $result
}

if ($Build -and $Launch) { throw 'Use -Build first, then -Launch after the build completes.' }

if ($RunId) {
    $run = Invoke-GitHub run view $RunId -R $Repo --json 'databaseId,headSha,url,headBranch,conclusion,event,workflowName' |
        ConvertFrom-Json
    if ($run.conclusion -ne 'success' -or $run.event -ne 'workflow_dispatch' -or $run.workflowName -ne 'Release') {
        throw 'Choose a successful manual Release workflow run.'
    }
} else {
    $encodedBranch = [uri]::EscapeDataString($Branch)
    $headCommit = Invoke-GitHub api "repos/$Repo/git/ref/heads/$encodedBranch" --jq '.object.sha'
    if ($headCommit -notmatch '^[0-9a-f]{40,64}$') { throw 'Could not resolve the remote branch commit.' }
    Write-Host "Remote branch: $Branch | Commit: $headCommit"
    $runs = @(Invoke-GitHub run list -R $Repo -w release.yml --branch $Branch --event workflow_dispatch `
        --commit $headCommit --limit 100 --json 'databaseId,headSha,url,headBranch,status,conclusion' | ConvertFrom-Json)
    $run = $null
    foreach ($candidate in $runs) {
        if ($candidate.conclusion -ne 'success' -or $candidate.headSha -ne $headCommit) { continue }
        $pages = Invoke-GitHub api "repos/$Repo/actions/runs/$($candidate.databaseId)/artifacts" --paginate --slurp | ConvertFrom-Json
        $artifacts = @($pages | ForEach-Object { $_.artifacts } | Where-Object {
            $_.name -eq 'lan-mouse-windows-x86_64' -and $_.expired -eq $false
        })
        if ($artifacts.Count -gt 0) { $run = $candidate; break }
    }
    if (-not $run) {
        $active = $runs | Where-Object { $_.headSha -eq $headCommit -and $_.status -ne 'completed' } | Select-Object -First 1
        if ($active) {
            Write-Host "Build already $($active.status): $($active.url)"
            if ($Build) { exit 0 }
            throw 'The current commit is still building. Download after it completes.'
        }
        if (-not $Build) { throw 'No downloadable build for the current commit. Request one with -Build.' }
        Invoke-GitHub workflow run release.yml -R $Repo --ref $Branch
        Write-Host "Build requested for $Branch ($headCommit). Download after it completes."
        exit 0
    }
    if ($Build) {
        Write-Host "Reusing successful build $($run.databaseId): $($run.url)"
        exit 0
    }
}

Write-Host "Branch: $($run.headBranch) | Run: $($run.databaseId) | Commit: $($run.headSha)"
Write-Host $run.url
if ($RunId) { Write-Host 'Pinned run selected; this may predate the current branch commit.' }
$packageDir = Join-Path $Destination "lan-mouse-$($run.databaseId)-$([guid]::NewGuid().ToString('N'))"
New-Item -ItemType Directory -Path $packageDir | Out-Null
Invoke-GitHub run download $run.databaseId -R $Repo -n lan-mouse-windows-x86_64 -D $packageDir
$appDir = Join-Path $packageDir 'app'
Expand-Archive -LiteralPath (Join-Path $packageDir 'lan-mouse-windows-x86_64.zip') -DestinationPath $appDir
$executable = Join-Path $appDir 'lan-mouse.exe'
if (-not (Test-Path -LiteralPath $executable -PathType Leaf)) { throw "Missing executable: $executable" }
Write-Host "Ready: $executable"
if ($Launch) {
    Write-Host 'Exit any other Lan Mouse instance before testing.'
    Start-Process -FilePath $executable -WorkingDirectory $appDir
}
