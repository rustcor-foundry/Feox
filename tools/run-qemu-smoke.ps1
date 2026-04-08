param(
    [ValidateSet('debug', 'release')]
    [string]$Profile = 'debug',

    [ValidateSet('x86_64', 'aarch64')]
    [string]$Architecture = 'x86_64',

    [int]$TimeoutSeconds = 20,

    [string]$SuccessMarker = 'stage: runtime service idle',

    [int]$MemoryMiB = 256
)

$ErrorActionPreference = 'Stop'

$toolRoot = $PSScriptRoot

& (Join-Path $toolRoot 'check-host.ps1') -Architecture $Architecture
if ($LASTEXITCODE -ne 0) {
    throw "Host prerequisites for Feox $Architecture smoke boot are not satisfied."
}

& (Join-Path $toolRoot 'run-qemu.ps1') `
    -Profile $Profile `
    -Architecture $Architecture `
    -MemoryMiB $MemoryMiB `
    -TimeoutSeconds $TimeoutSeconds `
    -SuccessMarker $SuccessMarker
