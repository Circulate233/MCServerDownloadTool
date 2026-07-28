Set-StrictMode -Version Latest

function Get-CargoProject {
    [CmdletBinding()]
    param(
        [Parameter()]
        [string] $RepositoryRoot = (Resolve-Path (Join-Path $PSScriptRoot "../..")).Path
    )

    $manifestPath = Join-Path $RepositoryRoot "Cargo.toml"
    if (-not (Test-Path -LiteralPath $manifestPath -PathType Leaf)) {
        throw "Cargo.toml was not found at repository root: $RepositoryRoot"
    }

    if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
        throw "cargo is required but was not found on PATH."
    }

    $metadataJson = & cargo metadata --manifest-path $manifestPath --no-deps --format-version 1
    if ($LASTEXITCODE -ne 0) {
        throw "cargo metadata failed with exit code $LASTEXITCODE."
    }

    try {
        $metadata = $metadataJson | ConvertFrom-Json -Depth 100
    }
    catch {
        throw "cargo metadata returned invalid JSON: $($_.Exception.Message)"
    }

    $rootManifest = [IO.Path]::GetFullPath($manifestPath)
    $rootPackage = @($metadata.packages | Where-Object {
            [IO.Path]::GetFullPath([string] $_.manifest_path) -eq $rootManifest
        })

    if ($rootPackage.Count -eq 1) {
        $candidatePackages = $rootPackage
    }
    else {
        $workspaceIds = @($metadata.workspace_members)
        $candidatePackages = @($metadata.packages | Where-Object { $workspaceIds -contains $_.id })
    }

    $binaryCandidates = @(
        foreach ($package in $candidatePackages) {
            foreach ($target in $package.targets) {
                if (@($target.kind) -contains "bin") {
                    [PSCustomObject]@{
                        Package = $package
                        Target  = $target
                    }
                }
            }
        }
    )

    if ($binaryCandidates.Count -ne 1) {
        $names = @($binaryCandidates | ForEach-Object { "$($_.Package.name):$($_.Target.name)" }) -join ", "
        throw "Expected exactly one workspace binary target, found $($binaryCandidates.Count). Candidates: $names"
    }

    $candidate = $binaryCandidates[0]
    [PSCustomObject]@{
        PackageName  = [string] $candidate.Package.name
        BinaryName   = [string] $candidate.Target.name
        ManifestPath = [string] $candidate.Package.manifest_path
    }
}

function Test-StrictReleaseVersion {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)]
        [AllowEmptyString()]
        [string] $Version
    )

    return $Version -cmatch '^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$'
}

function Test-BuildVersion {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)]
        [AllowEmptyString()]
        [string] $Version
    )

    return $Version -cmatch '^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(\+[0-9a-f]{7})?$'
}

function Get-BuiltBinaryVersion {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)]
        [ValidateNotNullOrEmpty()]
        [string] $BinaryPath,

        [Parameter(Mandatory)]
        [ValidateNotNullOrEmpty()]
        [string] $BinaryName
    )

    if (-not (Test-Path -LiteralPath $BinaryPath -PathType Leaf)) {
        throw "Built binary was not found at expected path: $BinaryPath"
    }
    $versionOutput = (& $BinaryPath --version 2>&1 | Out-String).Trim()
    if ($LASTEXITCODE -ne 0) {
        throw "Version probe failed with exit code $LASTEXITCODE for '$BinaryPath': $versionOutput"
    }
    $prefix = "$BinaryName "
    if (-not $versionOutput.StartsWith($prefix, [StringComparison]::Ordinal)) {
        throw "Version probe returned unexpected command identity '$versionOutput'; expected prefix '$prefix'."
    }
    $version = $versionOutput.Substring($prefix.Length)
    if (-not (Test-BuildVersion -Version $version)) {
        throw "Version probe returned invalid build version '$version'."
    }
    return $version
}
