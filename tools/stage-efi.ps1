param(
    [ValidateSet('debug', 'release')]
    [string]$Profile = 'debug',

    [ValidateSet('x86_64', 'aarch64')]
    [string]$Architecture = 'x86_64',

    [string]$StageRoot = 'target\feox-efi',

    [switch]$SkipBuild
)

$ErrorActionPreference = 'Stop'

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$resolvedStageRoot = Resolve-Path $StageRoot -ErrorAction SilentlyContinue
if ($resolvedStageRoot) {
    $stageRoot = $resolvedStageRoot.Path
}
else {
    if ([System.IO.Path]::IsPathRooted($StageRoot)) {
        $stageRoot = [System.IO.Path]::GetFullPath($StageRoot)
    }
    else {
        $stageRoot = [System.IO.Path]::GetFullPath((Join-Path $repoRoot $StageRoot))
    }
}

$profileArgs = @()
if ($Profile -eq 'release') {
    $profileArgs += '--release'
}

switch ($Architecture) {
    'x86_64' {
        $kernelTarget = 'x86_64-unknown-none'
        $loaderTarget = 'x86_64-unknown-uefi'
        $loaderBootName = 'BOOTX64.EFI'
    }
    'aarch64' {
        $kernelTarget = 'aarch64-unknown-none-softfloat'
        $loaderTarget = 'aarch64-unknown-uefi'
        $loaderBootName = 'BOOTAA64.EFI'
    }
    default {
        throw "Unsupported architecture: $Architecture"
    }
}

Push-Location $repoRoot
try {
    if (-not $SkipBuild) {
        & cargo build -p feox-xokernel --bin feox-xokernel --target $kernelTarget @profileArgs | Out-Host
        if ($LASTEXITCODE -ne 0) {
            throw "Kernel build failed."
        }

        & cargo build -p feox-loader-uefi --bin feox-loader-uefi --target $loaderTarget @profileArgs | Out-Host
        if ($LASTEXITCODE -ne 0) {
            throw "Loader build failed."
        }
    }

    $targetRoot = Join-Path $repoRoot 'target'
    $loaderArtifact = Join-Path $targetRoot "$loaderTarget\$Profile\feox-loader-uefi.efi"
    $kernelArtifact = Join-Path $targetRoot "$kernelTarget\$Profile\feox-xokernel"

    if (-not (Test-Path $loaderArtifact)) {
        throw "Loader artifact not found at $loaderArtifact"
    }
    if (-not (Test-Path $kernelArtifact)) {
        throw "Kernel artifact not found at $kernelArtifact"
    }

    $espRoot = Join-Path $stageRoot 'EFI\BOOT'
    New-Item -ItemType Directory -Path $espRoot -Force | Out-Null

    $loaderDestination = Join-Path $espRoot $loaderBootName
    $kernelDestination = Join-Path $espRoot 'FEOXKERN.ELF'

    Copy-Item $loaderArtifact $loaderDestination -Force
    Copy-Item $kernelArtifact $kernelDestination -Force

    [PSCustomObject]@{
        RepoRoot = $repoRoot
        StageRoot = $stageRoot
        EspRoot = $espRoot
        Loader = $loaderDestination
        Kernel = $kernelDestination
        Profile = $Profile
        Architecture = $Architecture
        KernelTarget = $kernelTarget
        LoaderTarget = $loaderTarget
    }
}
finally {
    Pop-Location
}
