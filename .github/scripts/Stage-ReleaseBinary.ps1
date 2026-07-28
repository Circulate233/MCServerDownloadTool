[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [ValidateNotNullOrEmpty()]
    [string] $Target,

    [Parameter(Mandatory)]
    [ValidateSet("windows-x86_64", "linux-x86_64", "macos-aarch64")]
    [string] $Platform,

    [Parameter(Mandatory)]
    [ValidateNotNullOrEmpty()]
    [string] $ExpectedVersion,

    [Parameter()]
    [string] $RepositoryRoot = (Resolve-Path (Join-Path $PSScriptRoot "../..")).Path,

    [Parameter()]
    [string] $OutputDirectory = (Join-Path $RepositoryRoot "dist/release-assets")
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
. (Join-Path $PSScriptRoot "CargoProject.ps1")

$project = Get-CargoProject -RepositoryRoot $RepositoryRoot
if (-not (Test-StrictReleaseVersion -Version $ExpectedVersion)) {
    throw "Expected release version '$ExpectedVersion' is not strict X.Y.Z."
}

$expectedTargets = @{
    "windows-x86_64" = "x86_64-pc-windows-msvc"
    "linux-x86_64"   = "x86_64-unknown-linux-musl"
    "macos-aarch64"  = "aarch64-apple-darwin"
}
if ($Target -cne $expectedTargets[$Platform]) {
    throw "Target '$Target' does not match platform '$Platform'."
}

$sourceExtension = if ($Platform -eq "windows-x86_64") { ".exe" } else { "" }
$source = Join-Path $RepositoryRoot "target/$Target/release/$($project.BinaryName)$sourceExtension"
$actualVersion = Get-BuiltBinaryVersion -BinaryPath $source -BinaryName $project.BinaryName
if ($actualVersion -cne $ExpectedVersion) {
    throw "Release binary reported '$actualVersion'; expected verified tag version '$ExpectedVersion'."
}

$assetNames = @{
    "windows-x86_64" = "MCServerDownloadTool-windows-x86_64.exe"
    "linux-x86_64"   = "MCServerDownloadTool-linux-x86_64"
    "macos-aarch64"  = "MCServerDownloadTool-macos-aarch64"
}
$assetName = $assetNames[$Platform]

New-Item -ItemType Directory -Path $OutputDirectory -Force | Out-Null
$assetPath = Join-Path $OutputDirectory $assetName
Copy-Item -LiteralPath $source -Destination $assetPath -Force
if ($Platform -ne "windows-x86_64") {
    & chmod 755 $assetPath
    if ($LASTEXITCODE -ne 0) {
        throw "chmod failed with exit code $LASTEXITCODE for '$assetPath'."
    }
}

$assetHash = (Get-FileHash -LiteralPath $assetPath -Algorithm SHA256).Hash.ToLowerInvariant()
$checksumName = "$assetName.sha256"
$checksumPath = Join-Path $OutputDirectory $checksumName
"$assetHash  $assetName" | Set-Content -LiteralPath $checksumPath -Encoding utf8NoBOM

$assetMetadata = [ordered]@{
    schemaVersion = 1
    package       = $project.PackageName
    version       = $ExpectedVersion
    asset         = $assetName
    checksum      = $checksumName
    platform      = $Platform
    target        = $Target
    size          = (Get-Item -LiteralPath $assetPath).Length
    sha256        = $assetHash
}
$metadataPath = Join-Path $OutputDirectory "$assetName.metadata.json"
$assetMetadata | ConvertTo-Json -Depth 10 | Set-Content -LiteralPath $metadataPath -Encoding utf8NoBOM

if ($env:GITHUB_OUTPUT) {
    @(
        "artifact-name=release-$Platform"
        "artifact-path=$OutputDirectory"
        "asset-name=$assetName"
    ) | Out-File -FilePath $env:GITHUB_OUTPUT -Encoding utf8 -Append
}

Write-Host "Staged raw release asset $assetPath (sha256: $assetHash)."
