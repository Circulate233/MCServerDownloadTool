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

if (-not $Tag.StartsWith("v", [StringComparison]::Ordinal)) {
    throw "Release tag '$Tag' must start with a lowercase 'v'."
}

$tagVersion = $Tag.Substring(1)
if (-not (Test-StrictReleaseVersion -Version $tagVersion)) {
    throw "Release tag '$Tag' must match strict vX.Y.Z (decimal components, no leading zeroes, no prerelease or build metadata)."
}

$project = Get-CargoProject -RepositoryRoot $RepositoryRoot

if ($env:GITHUB_OUTPUT) {
    @(
        "version=$tagVersion"
        "package-name=$($project.PackageName)"
        "binary-name=$($project.BinaryName)"
    ) | Out-File -FilePath $env:GITHUB_OUTPUT -Encoding utf8 -Append
}

Write-Host "Validated release tag $Tag for $($project.PackageName) $tagVersion (binary: $($project.BinaryName))."
