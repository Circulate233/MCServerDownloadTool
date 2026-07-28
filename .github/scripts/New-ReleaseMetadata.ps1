[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [ValidateNotNullOrEmpty()]
    [string] $AssetDirectory,

    [Parameter(Mandatory)]
    [ValidateNotNullOrEmpty()]
    [string] $OutputDirectory,

    [Parameter(Mandatory)]
    [ValidateNotNullOrEmpty()]
    [string] $Repository,

    [Parameter(Mandatory)]
    [ValidateNotNullOrEmpty()]
    [string] $Tag,

    [Parameter(Mandatory)]
    [ValidateNotNullOrEmpty()]
    [string] $CommitSha,

    [Parameter(Mandatory)]
    [ValidateNotNullOrEmpty()]
    [string] $RunId,

    [Parameter(Mandatory)]
    [ValidateNotNullOrEmpty()]
    [string] $RunAttempt,

    [Parameter()]
    [string] $ServerUrl = "https://github.com"
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$expectedPlatforms = @("linux-x86_64", "macos-aarch64", "windows-x86_64")
$expectedTargets = @{
    "linux-x86_64"  = "x86_64-unknown-linux-musl"
    "macos-aarch64" = "aarch64-apple-darwin"
    "windows-x86_64" = "x86_64-pc-windows-msvc"
}
$expectedAssets = @{
    "linux-x86_64"   = "MCServerDownloadTool-linux-x86_64"
    "macos-aarch64"  = "MCServerDownloadTool-macos-aarch64"
    "windows-x86_64" = "MCServerDownloadTool-windows-x86_64.exe"
}
$metadataFiles = @(Get-ChildItem -LiteralPath $AssetDirectory -Recurse -File -Filter "*.metadata.json")
if ($metadataFiles.Count -ne 3) {
    throw "Expected metadata for exactly three release assets, found $($metadataFiles.Count)."
}

$records = @(
    foreach ($metadataFile in $metadataFiles) {
        try {
            $record = Get-Content -LiteralPath $metadataFile.FullName -Raw | ConvertFrom-Json -Depth 20
        }
        catch {
            throw "Invalid asset metadata '$($metadataFile.FullName)': $($_.Exception.Message)"
        }

        if ([int] $record.schemaVersion -ne 1) {
            throw "Unsupported asset metadata schema in '$($metadataFile.FullName)': $($record.schemaVersion)"
        }
        foreach ($requiredField in @("package", "version", "asset", "checksum", "platform", "target", "sha256")) {
            if ([string]::IsNullOrWhiteSpace([string] $record.$requiredField)) {
                throw "Asset metadata '$($metadataFile.FullName)' has an empty '$requiredField' field."
            }
        }
        if ([IO.Path]::GetFileName([string] $record.asset) -cne [string] $record.asset) {
            throw "Asset metadata must reference a file name without path components: $($record.asset)"
        }
        if (-not $expectedTargets.ContainsKey([string] $record.platform)) {
            throw "Unexpected release platform in '$($metadataFile.FullName)': $($record.platform)"
        }
        if ([string] $record.target -cne $expectedTargets[[string] $record.platform]) {
            throw "Target '$($record.target)' does not match platform '$($record.platform)'."
        }
        if ([string] $record.asset -cne $expectedAssets[[string] $record.platform]) {
            throw "Asset '$($record.asset)' does not match the required name for platform '$($record.platform)'."
        }
        if ([string] $record.checksum -cne "$($record.asset).sha256") {
            throw "Checksum file '$($record.checksum)' must be named '$($record.asset).sha256'."
        }

        $assetPath = Join-Path $metadataFile.DirectoryName ([string] $record.asset)
        if (-not (Test-Path -LiteralPath $assetPath -PathType Leaf)) {
            throw "Asset metadata references a missing file: $assetPath"
        }

        $actualHash = (Get-FileHash -LiteralPath $assetPath -Algorithm SHA256).Hash.ToLowerInvariant()
        if ($actualHash -cne [string] $record.sha256) {
            throw "SHA-256 mismatch for '$assetPath': metadata=$($record.sha256), actual=$actualHash"
        }

        if ((Get-Item -LiteralPath $assetPath).Length -ne [long] $record.size) {
            throw "Size mismatch for '$assetPath'."
        }

        $checksumPath = Join-Path $metadataFile.DirectoryName ([string] $record.checksum)
        if (-not (Test-Path -LiteralPath $checksumPath -PathType Leaf)) {
            throw "Asset metadata references a missing checksum file: $checksumPath"
        }
        $expectedChecksumLine = "$actualHash  $($record.asset)"
        $actualChecksumLine = (Get-Content -LiteralPath $checksumPath -Raw).Trim()
        if ($actualChecksumLine -cne $expectedChecksumLine) {
            throw "Checksum file '$checksumPath' does not match its release asset."
        }

        [PSCustomObject]@{
            Package  = [string] $record.package
            Version  = [string] $record.version
            Asset    = [string] $record.asset
            Checksum = [string] $record.checksum
            Platform = [string] $record.platform
            Target   = [string] $record.target
            Size     = [long] $record.size
            Sha256   = $actualHash
            Path     = $assetPath
            ChecksumPath = $checksumPath
        }
    }
)

$actualPlatforms = @($records.Platform | Sort-Object -Unique)
$platformDifferences = @(Compare-Object -ReferenceObject $expectedPlatforms -DifferenceObject $actualPlatforms)
if ($platformDifferences.Count -ne 0) {
    throw "Release platforms do not match the required matrix. Expected: $($expectedPlatforms -join ', '); actual: $($actualPlatforms -join ', ')."
}

$packages = @($records.Package | Sort-Object -Unique)
$versions = @($records.Version | Sort-Object -Unique)
if ($packages.Count -ne 1 -or $versions.Count -ne 1) {
    throw "All release assets must have one package and version. Packages: $($packages -join ', '); versions: $($versions -join ', ')."
}
if ("v$($versions[0])" -cne $Tag) {
    throw "Downloaded asset version '$($versions[0])' does not match release tag '$Tag'."
}

if (Test-Path -LiteralPath $OutputDirectory) {
    $existingOutput = @(Get-ChildItem -LiteralPath $OutputDirectory -Force)
    if ($existingOutput.Count -ne 0) {
        throw "Release output directory must be empty: $OutputDirectory"
    }
}
else {
    New-Item -ItemType Directory -Path $OutputDirectory -Force | Out-Null
}
$releaseAssets = @()
foreach ($record in ($records | Sort-Object Platform)) {
    $destination = Join-Path $OutputDirectory $record.Asset
    Copy-Item -LiteralPath $record.Path -Destination $destination -Force
    Copy-Item -LiteralPath $record.ChecksumPath -Destination (Join-Path $OutputDirectory $record.Checksum) -Force
    $releaseAssets += [PSCustomObject]@{
        name     = $record.Asset
        version  = $record.Version
        platform = $record.Platform
        target   = $record.Target
        size     = $record.Size
        sha256   = $record.Sha256
        url      = "$ServerUrl/$Repository/releases/download/$Tag/$($record.Asset)"
    }
}

$index = [ordered]@{
    schemaVersion = 1
    package       = $packages[0]
    version       = $versions[0]
    tag           = $Tag
    commit        = $CommitSha
    assets        = $releaseAssets
    metadata      = [ordered]@{
        provenance = "provenance.sigstore.json"
        workflow = "$ServerUrl/$Repository/actions/runs/$RunId/attempts/$RunAttempt"
    }
}
$indexPath = Join-Path $OutputDirectory "release-index.json"
$index | ConvertTo-Json -Depth 20 | Set-Content -LiteralPath $indexPath -Encoding utf8NoBOM

Write-Host "Prepared $($releaseAssets.Count) release assets, checksums, and release index in $OutputDirectory."
