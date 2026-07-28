[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [ValidateNotNullOrEmpty()]
    [string] $Target,

    [Parameter(Mandatory)]
    [ValidateSet("windows-x86_64", "linux-x86_64", "macos-aarch64")]
    [string] $Platform,

    [Parameter()]
    [string] $RepositoryRoot = (Resolve-Path (Join-Path $PSScriptRoot "../..")).Path,

    [Parameter()]
    [string] $OutputDirectory = (Join-Path $RepositoryRoot "dist/ci")
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
. (Join-Path $PSScriptRoot "CargoProject.ps1")

$project = Get-CargoProject -RepositoryRoot $RepositoryRoot
$extension = if ($Platform -eq "windows-x86_64") { ".exe" } else { "" }
$source = Join-Path $RepositoryRoot "target/$Target/release/$($project.BinaryName)$extension"
if (-not (Test-Path -LiteralPath $source -PathType Leaf)) {
    throw "Built binary was not found at expected path: $source"
}

$platformDirectory = Join-Path $OutputDirectory $Platform
New-Item -ItemType Directory -Path $platformDirectory -Force | Out-Null
$destination = Join-Path $platformDirectory "$($project.BinaryName)$extension"
Copy-Item -LiteralPath $source -Destination $destination -Force
$version = Get-BuiltBinaryVersion -BinaryPath $source -BinaryName $project.BinaryName

$hash = (Get-FileHash -LiteralPath $destination -Algorithm SHA256).Hash.ToLowerInvariant()
$metadata = [ordered]@{
    schemaVersion = 1
    package       = $project.PackageName
    version       = $version
    binary        = [IO.Path]::GetFileName($destination)
    platform      = $Platform
    target        = $Target
    sha256        = $hash
    commit        = $env:GITHUB_SHA
}
$metadata | ConvertTo-Json -Depth 10 | Set-Content -LiteralPath (Join-Path $platformDirectory "build-metadata.json") -Encoding utf8NoBOM

$artifactName = "$($project.PackageName)-$version-$Platform"
if ($env:GITHUB_OUTPUT) {
    @(
        "artifact-name=$artifactName"
        "artifact-path=$platformDirectory"
    ) | Out-File -FilePath $env:GITHUB_OUTPUT -Encoding utf8 -Append
}

Write-Host "Staged $destination (sha256: $hash)."
