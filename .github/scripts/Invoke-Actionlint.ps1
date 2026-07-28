[CmdletBinding()]
param(
    [Parameter()]
    [string] $RepositoryRoot = (Resolve-Path (Join-Path $PSScriptRoot "../..")).Path
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$version = "1.7.7"
$archiveName = "actionlint_${version}_linux_amd64.tar.gz"
$expectedSha256 = "023070a287cd8cccd71515fedc843f1985bf96c436b7effaecce67290e7e0757"
$downloadUrl = "https://github.com/rhysd/actionlint/releases/download/v$version/$archiveName"

if (-not [Runtime.InteropServices.RuntimeInformation]::IsOSPlatform([Runtime.InteropServices.OSPlatform]::Linux)) {
    throw "The pinned actionlint runner supports only Linux quality jobs."
}
if ([Runtime.InteropServices.RuntimeInformation]::OSArchitecture -ne [Runtime.InteropServices.Architecture]::X64) {
    throw "The pinned actionlint asset requires an x86_64 Linux runner."
}

$temporaryRoot = Join-Path ([IO.Path]::GetTempPath()) ("mcsdt-actionlint-" + [Guid]::NewGuid().ToString("N"))
$archivePath = Join-Path $temporaryRoot $archiveName
$executablePath = Join-Path $temporaryRoot "actionlint"

try {
    New-Item -ItemType Directory -Path $temporaryRoot -Force | Out-Null
    Invoke-WebRequest -UseBasicParsing -Uri $downloadUrl -OutFile $archivePath

    $actualSha256 = (Get-FileHash -LiteralPath $archivePath -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($actualSha256 -cne $expectedSha256) {
        throw "actionlint archive SHA-256 mismatch: expected $expectedSha256, received $actualSha256."
    }

    & tar -xzf $archivePath -C $temporaryRoot actionlint
    if ($LASTEXITCODE -ne 0) {
        throw "Unable to extract actionlint; tar exited with code $LASTEXITCODE."
    }
    if (-not (Test-Path -LiteralPath $executablePath -PathType Leaf)) {
        throw "The verified actionlint archive did not contain the expected executable."
    }

    & chmod 755 $executablePath
    if ($LASTEXITCODE -ne 0) {
        throw "Unable to mark actionlint executable; chmod exited with code $LASTEXITCODE."
    }

    & $executablePath (Join-Path $RepositoryRoot ".github/workflows/ci.yml") (Join-Path $RepositoryRoot ".github/workflows/release.yml")
    if ($LASTEXITCODE -ne 0) {
        throw "actionlint v$version failed with exit code $LASTEXITCODE."
    }
}
finally {
    if (Test-Path -LiteralPath $temporaryRoot) {
        Remove-Item -LiteralPath $temporaryRoot -Recurse -Force
    }
}

Write-Host "actionlint v$version passed after SHA-256 verification."
