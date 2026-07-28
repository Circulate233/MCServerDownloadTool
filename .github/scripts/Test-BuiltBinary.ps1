[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [ValidateNotNullOrEmpty()]
    [string] $Target,

    [Parameter(Mandatory)]
    [ValidateSet("windows-x86_64", "linux-x86_64", "macos-aarch64")]
    [string] $Platform,

    [Parameter()]
    [string] $RepositoryRoot = (Resolve-Path (Join-Path $PSScriptRoot "../..")).Path
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
. (Join-Path $PSScriptRoot "CargoProject.ps1")

$expectedTargets = @{
    "windows-x86_64" = "x86_64-pc-windows-msvc"
    "linux-x86_64"   = "x86_64-unknown-linux-musl"
    "macos-aarch64"  = "aarch64-apple-darwin"
}
if ($Target -cne $expectedTargets[$Platform]) {
    throw "Target '$Target' does not match platform '$Platform'."
}

$project = Get-CargoProject -RepositoryRoot $RepositoryRoot
$extension = if ($Platform -eq "windows-x86_64") { ".exe" } else { "" }
$binary = Join-Path $RepositoryRoot "target/$Target/release/$($project.BinaryName)$extension"
if (-not (Test-Path -LiteralPath $binary -PathType Leaf)) {
    throw "Built binary was not found at expected path: $binary"
}

$versionOutput = (& $binary --version 2>&1 | Out-String).Trim()
if ($LASTEXITCODE -ne 0) {
    throw "Version smoke test failed with exit code $LASTEXITCODE for '$binary': $versionOutput"
}
$expectedOutput = "$($project.BinaryName) $($project.Version)"
if ($versionOutput -cne $expectedOutput) {
    throw "Version smoke test returned '$versionOutput'; expected '$expectedOutput'."
}

Write-Host "Smoke test passed: $versionOutput"
