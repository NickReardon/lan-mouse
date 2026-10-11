# Called by branch-build.ps1 build BRANCH. All generated files belong to the
# primary clone, even when this script is invoked from a linked worktree.
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$Branch,
    [switch]$CreateWorktree
)
$ErrorActionPreference = 'Stop'

function Invoke-Git {
    $result = & git @args
    if ($LASTEXITCODE -ne 0) { throw "Git failed (exit $LASTEXITCODE)." }
    return $result
}

Invoke-Git check-ref-format --branch $Branch | Out-Null
$commonDir = Invoke-Git -C $PSScriptRoot rev-parse --path-format=absolute --git-common-dir
$baseRepo = Split-Path -Parent $commonDir
if ($Branch.StartsWith('origin/')) {
    & git -C $baseRepo show-ref --verify --quiet "refs/heads/$Branch"
    if ($LASTEXITCODE -ne 0) { $Branch = $Branch.Substring(7) }
}
Invoke-Git check-ref-format --branch $Branch | Out-Null
# Older base branches may not have the new .gitignore entries yet.
foreach ($generatedDir in @('.worktrees', 'builds')) {
    & git -C $baseRepo check-ignore --quiet -- "$generatedDir/"
    if ($LASTEXITCODE -eq 1) {
        Add-Content -LiteralPath (Join-Path $commonDir 'info/exclude') -Value "`n/$generatedDir/"
    } elseif ($LASTEXITCODE -ne 0) { throw 'Could not check generated directory ignore rules.' }
}
$sourceDir = $null
$candidate = $null
foreach ($line in (Invoke-Git -C $baseRepo worktree list --porcelain)) {
    if ($line.StartsWith('worktree ')) { $candidate = $line.Substring(9) }
    if ($line -ceq "branch refs/heads/$Branch") { $sourceDir = $candidate; break }
}
if (-not $sourceDir) {
    if (-not $CreateWorktree) {
        throw "No checkout for '$Branch'. Use -CreateWorktree to create one under the base repo's .worktrees/."
    }
    $sourceDir = Join-Path $baseRepo ".worktrees/$Branch"
    & git -C $baseRepo show-ref --verify --quiet "refs/heads/$Branch"
    if ($LASTEXITCODE -eq 0) {
        Invoke-Git -C $baseRepo worktree add -- $sourceDir $Branch | Out-Host
    } else {
        & git -C $baseRepo fetch --no-tags origin "refs/heads/${Branch}:refs/remotes/origin/$Branch"
        if ($LASTEXITCODE -ne 0) { throw "Could not fetch branch '$Branch' from origin. Check the branch name and your connection." }
        Invoke-Git -C $baseRepo worktree add --track -b $Branch -- $sourceDir "origin/$Branch" | Out-Host
    }
}
$basePath = [IO.Path]::GetFullPath($baseRepo)
$sourcePath = [IO.Path]::GetFullPath($sourceDir)
if ($sourcePath -ne $basePath -and -not $sourcePath.StartsWith($basePath + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) {
    throw "The branch checkout is outside the base repository: $sourceDir. Move it into .worktrees/ first."
}

# Honor a configured tool, then try the standard gvsbuild installation or PATH.
$pkgConfig = $env:PKG_CONFIG
if (-not $pkgConfig) {
    $standardTool = 'C:\gtk-build\gtk\x64\release\bin\pkgconf.exe'
    if (Test-Path -LiteralPath $standardTool) { $pkgConfig = $standardTool }
    else {
        $tool = Get-Command pkg-config,pkgconf -ErrorAction SilentlyContinue | Select-Object -First 1
        if ($tool) { $pkgConfig = $tool.Source }
    }
}
if (-not $pkgConfig) { throw 'GTK development tools are required. Set PKG_CONFIG to your pkgconf.exe path.' }
$gtkPrefix = & $pkgConfig --variable=prefix gtk4
if ($LASTEXITCODE -ne 0 -or -not $gtkPrefix) { throw 'Could not locate GTK using pkg-config.' }
$gtkBin = Join-Path $gtkPrefix 'bin'
$oldPath = $env:PATH
$oldPkgConfig = $env:PKG_CONFIG
try {
    $env:PKG_CONFIG = $pkgConfig
    $env:PATH = "$gtkBin;$oldPath"
    $targetDir = Join-Path $baseRepo "builds/$Branch"
    $commit = Invoke-Git -C $sourceDir rev-parse HEAD
    Write-Host "Building $Branch ($commit) from $sourceDir"
    # Run from the selected checkout so Cargo uses its configuration/build data.
    Push-Location -LiteralPath $sourceDir
    try {
        & cargo build --locked -p lan-mouse --target-dir $targetDir
        if ($LASTEXITCODE -ne 0) { throw "Cargo build failed (exit $LASTEXITCODE)." }
    } finally { Pop-Location }
    Write-Host "Build output: $targetDir"
} finally {
    $env:PATH = $oldPath
    $env:PKG_CONFIG = $oldPkgConfig
}
