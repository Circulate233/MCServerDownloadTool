[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [ValidateNotNullOrEmpty()]
    [string] $Target
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$targetRequirements = @{
    "x86_64-pc-windows-msvc"    = [PSCustomObject]@{
        Os           = [Runtime.InteropServices.OSPlatform]::Windows
        Architecture = [Runtime.InteropServices.Architecture]::X64
    }
    "x86_64-unknown-linux-musl" = [PSCustomObject]@{
        Os           = [Runtime.InteropServices.OSPlatform]::Linux
        Architecture = [Runtime.InteropServices.Architecture]::X64
    }
    "aarch64-apple-darwin"      = [PSCustomObject]@{
        Os           = [Runtime.InteropServices.OSPlatform]::OSX
        Architecture = [Runtime.InteropServices.Architecture]::Arm64
    }
}

if (-not $targetRequirements.ContainsKey($Target)) {
    throw "Runner architecture validation does not support target '$Target'."
}

$requirement = $targetRequirements[$Target]
$actualArchitecture = [Runtime.InteropServices.RuntimeInformation]::OSArchitecture
if (-not [Runtime.InteropServices.RuntimeInformation]::IsOSPlatform($requirement.Os)) {
    throw "Runner operating system does not match native target '$Target'."
}
if ($actualArchitecture -ne $requirement.Architecture) {
    throw "Runner architecture '$actualArchitecture' does not match native target '$Target' ($($requirement.Architecture))."
}

Write-Host "Runner is native for $Target ($actualArchitecture)."
