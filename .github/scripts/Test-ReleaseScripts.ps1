[CmdletBinding()]
param(
    [Parameter()]
    [string] $RepositoryRoot = (Resolve-Path (Join-Path $PSScriptRoot "../..")).Path
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$tagVersion = "7.8.9"
$tag = "v$tagVersion"
& (Join-Path $PSScriptRoot "Verify-ReleaseTag.ps1") -Tag $tag -RepositoryRoot $RepositoryRoot

$invalidTagRejected = $false
try {
    & (Join-Path $PSScriptRoot "Verify-ReleaseTag.ps1") -Tag "v01.0.0" -RepositoryRoot $RepositoryRoot
}
catch {
    $invalidTagRejected = $true
}
if (-not $invalidTagRejected) {
    throw "A non-strict release tag was accepted."
}

$nativeTarget = if ($IsWindows) {
    "x86_64-pc-windows-msvc"
}
elseif ($IsLinux) {
    "x86_64-unknown-linux-musl"
}
elseif ($IsMacOS) {
    "aarch64-apple-darwin"
}
else {
    throw "Unsupported test operating system."
}
& (Join-Path $PSScriptRoot "Test-RunnerArchitecture.ps1") -Target $nativeTarget

$mismatchedTarget = if ($nativeTarget -cne "aarch64-apple-darwin") {
    "aarch64-apple-darwin"
}
else {
    "x86_64-pc-windows-msvc"
}
$mismatchedArchitectureRejected = $false
try {
    & (Join-Path $PSScriptRoot "Test-RunnerArchitecture.ps1") -Target $mismatchedTarget
}
catch {
    $mismatchedArchitectureRejected = $true
}
if (-not $mismatchedArchitectureRejected) {
    throw "A non-native runner target was accepted."
}

$temporaryRoot = Join-Path ([IO.Path]::GetTempPath()) ("mcsdt-release-test-" + [Guid]::NewGuid().ToString("N"))
$assetDirectory = Join-Path $temporaryRoot "assets"
$outputDirectory = Join-Path $temporaryRoot "dist"

$specifications = @(
    [ordered]@{
        Platform = "windows-x86_64"
        Target   = "x86_64-pc-windows-msvc"
        Asset    = "MCServerDownloadTool-windows-x86_64.exe"
    },
    [ordered]@{
        Platform = "linux-x86_64"
        Target   = "x86_64-unknown-linux-musl"
        Asset    = "MCServerDownloadTool-linux-x86_64"
    },
    [ordered]@{
        Platform = "macos-aarch64"
        Target   = "aarch64-apple-darwin"
        Asset    = "MCServerDownloadTool-macos-aarch64"
    }
)

try {
    New-Item -ItemType Directory -Path $assetDirectory -Force | Out-Null
    foreach ($specification in $specifications) {
        $assetPath = Join-Path $assetDirectory $specification.Asset
        [IO.File]::WriteAllBytes($assetPath, [Text.Encoding]::UTF8.GetBytes("fixture-$($specification.Platform)"))
        $hash = (Get-FileHash -LiteralPath $assetPath -Algorithm SHA256).Hash.ToLowerInvariant()
        $checksumName = "$($specification.Asset).sha256"
        "$hash  $($specification.Asset)" | Set-Content -LiteralPath (Join-Path $assetDirectory $checksumName) -Encoding utf8NoBOM

        [ordered]@{
            schemaVersion = 1
            package       = "mc-server-download-tool"
            version       = $tagVersion
            asset         = $specification.Asset
            checksum      = $checksumName
            platform      = $specification.Platform
            target        = $specification.Target
            size          = (Get-Item -LiteralPath $assetPath).Length
            sha256        = $hash
        } | ConvertTo-Json -Depth 10 | Set-Content -LiteralPath (Join-Path $assetDirectory "$($specification.Asset).metadata.json") -Encoding utf8NoBOM
    }

    & (Join-Path $PSScriptRoot "New-ReleaseMetadata.ps1") `
        -AssetDirectory $assetDirectory `
        -OutputDirectory $outputDirectory `
        -Repository "example/MCServerDownloadTool" `
        -Tag $tag `
        -CommitSha "0123456789abcdef" `
        -RunId "123" `
        -RunAttempt "2"

    $expectedFiles = @(
        "MCServerDownloadTool-linux-x86_64",
        "MCServerDownloadTool-linux-x86_64.sha256",
        "MCServerDownloadTool-macos-aarch64",
        "MCServerDownloadTool-macos-aarch64.sha256",
        "MCServerDownloadTool-windows-x86_64.exe",
        "MCServerDownloadTool-windows-x86_64.exe.sha256",
        "release-index.json"
    ) | Sort-Object
    $actualFiles = @(Get-ChildItem -LiteralPath $outputDirectory -File | Select-Object -ExpandProperty Name | Sort-Object)
    if (@(Compare-Object -ReferenceObject $expectedFiles -DifferenceObject $actualFiles).Count -ne 0) {
        throw "Release metadata generated an unexpected file set: $($actualFiles -join ', ')"
    }

    $index = Get-Content -LiteralPath (Join-Path $outputDirectory "release-index.json") -Raw | ConvertFrom-Json -Depth 20
    if ([string] $index.version -cne $tagVersion -or [string] $index.tag -cne $tag -or @($index.assets).Count -ne 3) {
        throw "Release index version or asset count is invalid."
    }
    foreach ($asset in $index.assets) {
        if ([string] $asset.version -cne $tagVersion) {
            throw "Release index asset '$($asset.name)' does not carry the exact release tag version."
        }
        foreach ($field in @("version", "platform", "target", "size", "sha256", "url")) {
            if ($null -eq $asset.$field -or [string]::IsNullOrWhiteSpace([string] $asset.$field)) {
                throw "Release index asset '$($asset.name)' is missing '$field'."
            }
        }
    }
}
finally {
    if (Test-Path -LiteralPath $temporaryRoot) {
        Remove-Item -LiteralPath $temporaryRoot -Recurse -Force
    }
}

Write-Host "Release workflow scripts passed behavioral tests."
