[CmdletBinding()]
param(
    [Parameter()]
    [string] $Tag = $env:GITHUB_REF_NAME,

    [Parameter()]
    [string] $RepositoryRoot = (Resolve-Path (Join-Path $PSScriptRoot "../..")).Path
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
. (Join-Path $PSScriptRoot "CargoProject.ps1")

if ([string]::IsNullOrWhiteSpace($Tag)) {
    throw "A release tag is required. Pass -Tag or set GITHUB_REF_NAME."
}

# SemVer 2.0.0: numeric identifiers have no leading zeroes; prerelease/build identifiers are ASCII only.
$semVerPattern = '^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(?:-((?:0|[1-9][0-9]*|[0-9A-Za-z-]*[A-Za-z-][0-9A-Za-z-]*)(?:\.(?:0|[1-9][0-9]*|[0-9A-Za-z-]*[A-Za-z-][0-9A-Za-z-]*))*))?(?:\+([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?$'
if (-not $Tag.StartsWith("v", [StringComparison]::Ordinal)) {
    throw "Release tag '$Tag' must start with a lowercase 'v'."
}

$tagVersion = $Tag.Substring(1)
if ($tagVersion -cnotmatch $semVerPattern) {
    throw "Release tag '$Tag' is not strict SemVer 2.0.0 (expected vMAJOR.MINOR.PATCH with optional prerelease/build metadata)."
}

$project = Get-CargoProject -RepositoryRoot $RepositoryRoot
if ($tagVersion -cne $project.Version) {
    throw "Tag version '$tagVersion' does not exactly match Cargo.toml package version '$($project.Version)'."
}

if ($env:GITHUB_OUTPUT) {
    @(
        "version=$tagVersion"
        "package-name=$($project.PackageName)"
        "binary-name=$($project.BinaryName)"
    ) | Out-File -FilePath $env:GITHUB_OUTPUT -Encoding utf8 -Append
}

Write-Host "Validated release tag $Tag for $($project.PackageName) $tagVersion (binary: $($project.BinaryName))."
