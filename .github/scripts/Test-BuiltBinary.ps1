[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [ValidateNotNullOrEmpty()]
    [string] $Target,

    [Parameter(Mandatory)]
    [ValidateSet("windows-x86_64", "linux-x86_64", "macos-aarch64")]
    [string] $Platform,

    [Parameter()]
    [string] $ExpectedVersion,

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

$version = Get-BuiltBinaryVersion -BinaryPath $binary -BinaryName $project.BinaryName
if (-not [string]::IsNullOrEmpty($ExpectedVersion) -and $version -cne $ExpectedVersion) {
    throw "Version smoke test returned '$version'; expected release version '$ExpectedVersion'."
}

Write-Host "Smoke test passed: $($project.BinaryName) $version"
