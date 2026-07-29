[CmdletBinding()]
param(
    [Parameter()]
    [string] $RepositoryRoot = (Resolve-Path (Join-Path $PSScriptRoot "../..")).Path
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$workflows = @(
    Get-ChildItem -LiteralPath (Join-Path $RepositoryRoot ".github/workflows") -File |
        Where-Object { $_.Extension -in @(".yml", ".yaml") }
)
if ($workflows.Count -eq 0) {
    throw "No GitHub Actions workflows were found."
}

$usesPattern = [regex]'(?m)^\s*-?\s*uses:\s*([^@\s]+)@([^\s#]+)'
$shaPattern = [regex]'^[0-9a-fA-F]{40}$'
$checked = 0
foreach ($workflow in $workflows) {
    $content = [IO.File]::ReadAllText($workflow.FullName)
    foreach ($match in $usesPattern.Matches($content)) {
        $action = $match.Groups[1].Value
        $reference = $match.Groups[2].Value
        if ($action.StartsWith("./", [StringComparison]::Ordinal)) {
            continue
        }
        $checked++
        if (-not $shaPattern.IsMatch($reference)) {
            throw "Workflow '$($workflow.Name)' action '$action' is not pinned to a full 40-character commit SHA: '$reference'."
        }
    }
}
if ($checked -eq 0) {
    throw "No external GitHub Actions references were checked."
}

Write-Host "Validated $checked external GitHub Actions commit pins."
