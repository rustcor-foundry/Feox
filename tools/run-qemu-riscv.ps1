param(
    [ValidateSet('debug', 'release')]
    [string]$Profile = 'debug',

    [string]$QemuPath = $env:FEOX_QEMU_RISCV,

    [int]$MemoryMiB = 256,

    [int]$Cores = 4,

    [switch]$SkipBuild,

    [int]$TimeoutSeconds = 0,

    [string]$SuccessMarker = 'riscv64 bring-up alive',

    # When set, build the ELF locally, ship it to this RustyKey host, and run
    # QEMU there instead of on the local machine. The canonical loop on Paul's
    # setup: dev/build here (Windows), boot on a Linux host with qemu-system-riscv64.
    [string]$Remote,

    [string]$RemotePath = '/home/rustykey/feox-riscv64.elf',

    [string]$RuskPath = $env:FEOX_RUSK,

    [string[]]$ExtraQemuArgs = @()
)

# riscv64 milestone-1 QEMU launcher.
#
# Unlike the x86_64 / aarch64 path (tools\run-qemu.ps1), riscv64 does NOT use a
# UEFI loader or an EFI system partition. QEMU's built-in OpenSBI (`-bios
# default`) enters the kernel directly in S-mode with a0=hartid and a1=dtb,
# exactly mirroring how the real Orange Pi RV boots (OpenSBI -> U-Boot -> S-mode).
# So we build the bare kernel ELF and hand it to `-kernel`; no staging, no OVMF.
#
# Local mode runs QEMU on this machine. -Remote <host> builds here, ships the
# ELF over RustyKey, and runs QEMU on that host (which needs qemu-system-riscv64
# but no Rust/repo). -Remote requires an active RustyKey session on this machine
# (an enrolled `rusk` identity); if `rusk cp` reports a missing node-cert, run
# `rusk up` first.

$ErrorActionPreference = 'Stop'

function Resolve-FirstExistingPath {
    param(
        [Parameter(Mandatory = $true)]
        [string[]]$Candidates
    )

    foreach ($candidate in $Candidates) {
        if ($candidate -and (Test-Path $candidate)) {
            return (Resolve-Path $candidate).Path
        }
    }

    return $null
}

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$target = 'riscv64gc-unknown-none-elf'
$kernelElf = Join-Path $repoRoot "target\$target\$Profile\feox-xokernel"

if (-not $SkipBuild) {
    Push-Location $repoRoot
    try {
        $cargoArgs = @('kernel-riscv')
        if ($Profile -eq 'release') {
            $cargoArgs += '--release'
        }
        & cargo @cargoArgs
        if ($LASTEXITCODE -ne 0) {
            throw "cargo kernel-riscv failed with status $LASTEXITCODE."
        }
    }
    finally {
        Pop-Location
    }
}

if (-not (Test-Path $kernelElf)) {
    throw "Kernel ELF not found at $kernelElf. Build it with 'cargo kernel-riscv' first or drop -SkipBuild."
}

if ($Remote) {
    # Remote runner: ship the freshly built ELF to a RustyKey host and boot it
    # there under QEMU. The ELF is a self-contained, arch-independent artifact,
    # so no Rust toolchain or repo checkout is needed on the remote host.
    $rusk = Resolve-FirstExistingPath -Candidates (@(
        $RuskPath,
        'C:\Program Files\RustyKey\rusk.exe',
        'C:\Program Files (x86)\RustyKey\rusk.exe'
    ) | Where-Object { $_ })
    if (-not $rusk) {
        $cmd = Get-Command rusk -ErrorAction SilentlyContinue
        if ($cmd) { $rusk = $cmd.Source }
    }
    if (-not $rusk) {
        throw "rusk.exe not found. Set FEOX_RUSK or pass -RuskPath."
    }

    # The boot hart parks in `wfi` forever, so a remote run must be time-bounded.
    $effTimeout = if ($TimeoutSeconds -gt 0) { $TimeoutSeconds } else { 10 }

    Write-Host "Launching Feox (riscv64) on remote host '$Remote' via RustyKey:"
    Write-Host "  rusk:    $rusk"
    Write-Host "  Kernel:  $kernelElf"
    Write-Host "  Remote:  ${Remote}:${RemotePath}"
    Write-Host "  Machine: virt (rv64, $Cores core(s), ${MemoryMiB}MiB), OpenSBI -bios default"
    Write-Host "  Timeout: ${effTimeout}s (success marker: '$SuccessMarker')"

    Write-Host "Copying ELF to ${Remote}:${RemotePath} ..."
    & $rusk cp $kernelElf "${Remote}:${RemotePath}"
    if ($LASTEXITCODE -ne 0) {
        throw "rusk cp failed with status $LASTEXITCODE. -Remote needs an active RustyKey session; run 'rusk up' (or open an enrolled shell) first, then retry."
    }

    $remoteScript = "timeout -k 2 $effTimeout qemu-system-riscv64 -machine virt -cpu rv64 -smp $Cores -m ${MemoryMiB}M -display none -monitor none -serial stdio -bios default -kernel '$RemotePath' -no-reboot"

    Write-Host "Booting under QEMU on $Remote ..."
    Write-Host ('-' * 60)
    $output = & $rusk exec --timeout ($effTimeout + 5) $Remote env bash -c $remoteScript 2>&1 | Out-String
    Write-Host $output
    Write-Host ('-' * 60)

    if ($SuccessMarker -and ($output -notmatch [regex]::Escape($SuccessMarker))) {
        throw "Remote QEMU run did not reach success marker '$SuccessMarker'."
    }
    Write-Host "Success marker '$SuccessMarker' observed on $Remote."
    return
}

$qemuCandidates = @(
    $QemuPath,
    '/usr/bin/qemu-system-riscv64',
    '/usr/local/bin/qemu-system-riscv64',
    'C:\Program Files\qemu\qemu-system-riscv64.exe',
    'C:\Program Files (x86)\qemu\qemu-system-riscv64.exe',
    'C:\msys64\mingw64\bin\qemu-system-riscv64.exe',
    'C:\msys64\ucrt64\bin\qemu-system-riscv64.exe',
    'C:\msys64\clang64\bin\qemu-system-riscv64.exe',
    (Join-Path $repoRoot 'tools\host\msys64\msys64\mingw64\bin\qemu-system-riscv64.exe')
)

$QemuPath = Resolve-FirstExistingPath -Candidates ($qemuCandidates | Where-Object { $_ })

if (-not $QemuPath) {
    throw "qemu-system-riscv64 was not found. Set FEOX_QEMU_RISCV or pass -QemuPath."
}

$qemuArgs = @(
    '-machine', 'virt',
    '-cpu', 'rv64',
    '-smp', $Cores.ToString(),
    '-m', $MemoryMiB.ToString(),
    '-nographic',
    '-bios', 'default',
    '-kernel', $kernelElf,
    '-no-reboot'
)

if ($ExtraQemuArgs.Count -gt 0) {
    $qemuArgs += $ExtraQemuArgs
}

Write-Host "Launching Feox (riscv64) with QEMU:"
Write-Host "  QEMU:    $QemuPath"
Write-Host "  Kernel:  $kernelElf"
Write-Host "  Machine: virt (rv64, $Cores core(s), ${MemoryMiB}MiB), OpenSBI -bios default"
if ($TimeoutSeconds -gt 0) {
    Write-Host "  Timeout: ${TimeoutSeconds}s (success marker: '$SuccessMarker')"
}

$runRoot = Join-Path $repoRoot 'target\feox-qemu'
New-Item -ItemType Directory -Path $runRoot -Force | Out-Null

Push-Location $repoRoot
try {
    if ($TimeoutSeconds -gt 0) {
        $stdoutLog = Join-Path $runRoot "riscv64-stdout.$Profile.log"
        $stderrLog = Join-Path $runRoot "riscv64-stderr.$Profile.log"
        Remove-Item $stdoutLog, $stderrLog -Force -ErrorAction SilentlyContinue

        $process = Start-Process `
            -FilePath $QemuPath `
            -ArgumentList $qemuArgs `
            -WorkingDirectory $repoRoot `
            -RedirectStandardOutput $stdoutLog `
            -RedirectStandardError $stderrLog `
            -PassThru
        $timedOut = $false
        if (-not $process.WaitForExit($TimeoutSeconds * 1000)) {
            $timedOut = $true
            Stop-Process -Id $process.Id -Force
            $process.WaitForExit()
        }

        Start-Sleep -Milliseconds 750

        $output = ''
        if (Test-Path $stdoutLog) {
            $output = Get-Content $stdoutLog -Raw
        }
        Write-Host $output

        if ($SuccessMarker -and ($output -notmatch [regex]::Escape($SuccessMarker))) {
            throw "QEMU riscv64 smoke run did not reach success marker '$SuccessMarker'."
        }
        Write-Host "Success marker '$SuccessMarker' observed."
    }
    else {
        & $QemuPath @qemuArgs
        if ($LASTEXITCODE -ne 0) {
            throw "QEMU exited with status $LASTEXITCODE."
        }
    }
}
finally {
    Pop-Location
}
