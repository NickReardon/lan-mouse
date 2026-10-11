[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$GtkPrefix,
    [Parameter(Mandatory)][string]$Destination
)
$ErrorActionPreference = 'Stop'
$gtkBin = Join-Path $GtkPrefix 'bin'
$schemas = Join-Path $GtkPrefix 'share/glib-2.0/schemas/gschemas.compiled'
if (-not (Test-Path -LiteralPath $schemas -PathType Leaf)) {
    throw "Missing compiled GTK settings schemas: $schemas"
}
New-Item -ItemType Directory -Force -Path $Destination | Out-Null
function Get-RuntimeHash([string]$Path) {
    $stream = [System.IO.File]::OpenRead($Path)
    $hash = [System.Security.Cryptography.SHA256]::Create()
    try {
        return [Convert]::ToBase64String($hash.ComputeHash($stream))
    } finally {
        $hash.Dispose()
        $stream.Dispose()
    }
}
foreach ($dll in (Get-ChildItem -LiteralPath $gtkBin -Filter '*.dll' -File)) {
    $target = Join-Path $Destination $dll.Name
    # Leave unchanged DLLs alone, including those loaded by a running app.
    if (-not (Test-Path -LiteralPath $target) -or
        (Get-RuntimeHash $dll.FullName) -ne (Get-RuntimeHash $target)) {
        Copy-Item -LiteralPath $dll.FullName -Destination $target -Force
    }
}
# GLib searches the installation's share/ directory relative to its DLLs.
$schemaDir = Join-Path $Destination 'share/glib-2.0/schemas'
New-Item -ItemType Directory -Force -Path $schemaDir | Out-Null
Copy-Item -LiteralPath $schemas -Destination (Join-Path $schemaDir 'gschemas.compiled') -Force
