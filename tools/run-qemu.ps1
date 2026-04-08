param(
    [ValidateSet('debug', 'release')]
    [string]$Profile = 'debug',

    [ValidateSet('x86_64', 'aarch64')]
    [string]$Architecture = 'x86_64',

    [string]$StageRoot = 'target\feox-efi',

    [string]$RunRoot = 'target\feox-qemu',

    [string]$QemuPath = $env:FEOX_QEMU,

    [string]$OvmfCode = $env:FEOX_OVMF_CODE,

    [string]$OvmfVars = $env:FEOX_OVMF_VARS,

    [int]$MemoryMiB = 256,

    [switch]$SkipBuild,

    [switch]$Graphic,

    [int]$TimeoutSeconds = 0,

    [string]$SuccessMarker = '',

    [string[]]$ExtraQemuArgs = @()
)

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

function Convert-ToQemuPath {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Path
    )

    [System.IO.Path]::GetFullPath($Path) -replace '\\', '/'
}

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$localMsysRoot = Join-Path $repoRoot 'tools\host\msys64\msys64'

$stageInfo = & (Join-Path $PSScriptRoot 'stage-efi.ps1') -Profile $Profile -Architecture $Architecture -StageRoot $StageRoot -SkipBuild:$SkipBuild

switch ($Architecture) {
    'x86_64' {
        $machine = 'q35'
        $cpu = 'qemu64'
        $qemuCandidates = @(
            $QemuPath,
            '/usr/bin/qemu-system-x86_64',
            '/usr/local/bin/qemu-system-x86_64',
            'C:\Program Files\qemu\qemu-system-x86_64.exe',
            'C:\Program Files (x86)\qemu\qemu-system-x86_64.exe',
            'C:\msys64\mingw64\bin\qemu-system-x86_64.exe',
            'C:\msys64\ucrt64\bin\qemu-system-x86_64.exe',
            'C:\msys64\clang64\bin\qemu-system-x86_64.exe',
            (Join-Path $localMsysRoot 'mingw64\bin\qemu-system-x86_64.exe')
        )
        $firmwareCodeCandidates = @(
            $OvmfCode,
            '/usr/share/OVMF/OVMF_CODE.fd',
            '/usr/share/OVMF/OVMF_CODE_4M.fd',
            '/usr/share/edk2/x64/OVMF_CODE.fd',
            '/usr/share/edk2-ovmf/x64/OVMF_CODE.fd',
            '/usr/share/qemu/OVMF_CODE.fd',
            'C:\Program Files\qemu\share\edk2-x86_64-code.fd',
            'C:\Program Files\qemu\share\OVMF_CODE.fd',
            'C:\Program Files\qemu\OVMF_CODE.fd',
            'C:\Program Files (x86)\qemu\share\edk2-x86_64-code.fd',
            'C:\Program Files (x86)\qemu\share\OVMF_CODE.fd',
            'C:\Program Files (x86)\qemu\OVMF_CODE.fd',
            'C:\msys64\mingw64\share\edk2-ovmf\x64\OVMF_CODE.fd',
            'C:\msys64\ucrt64\share\edk2-ovmf\x64\OVMF_CODE.fd',
            'C:\msys64\clang64\share\edk2-ovmf\x64\OVMF_CODE.fd',
            (Join-Path $localMsysRoot 'mingw64\share\qemu\edk2-x86_64-code.fd')
        )
        $firmwareVarsCandidates = @(
            $OvmfVars,
            '/usr/share/OVMF/OVMF_VARS.fd',
            '/usr/share/OVMF/OVMF_VARS_4M.fd',
            '/usr/share/edk2/x64/OVMF_VARS.fd',
            '/usr/share/edk2-ovmf/x64/OVMF_VARS.fd',
            '/usr/share/qemu/OVMF_VARS.fd',
            'C:\Program Files\qemu\share\edk2-i386-vars.fd',
            'C:\Program Files\qemu\share\OVMF_VARS.fd',
            'C:\Program Files\qemu\OVMF_VARS.fd',
            'C:\Program Files (x86)\qemu\share\edk2-i386-vars.fd',
            'C:\Program Files (x86)\qemu\share\OVMF_VARS.fd',
            'C:\Program Files (x86)\qemu\OVMF_VARS.fd',
            'C:\msys64\mingw64\share\edk2-ovmf\x64\OVMF_VARS.fd',
            'C:\msys64\ucrt64\share\edk2-ovmf\x64\OVMF_VARS.fd',
            'C:\msys64\clang64\share\edk2-ovmf\x64\OVMF_VARS.fd',
            (Join-Path $localMsysRoot 'mingw64\share\qemu\edk2-i386-vars.fd')
        )
    }
    'aarch64' {
        $machine = 'virt'
        $cpu = 'cortex-a72'
        $qemuCandidates = @(
            $QemuPath,
            '/usr/bin/qemu-system-aarch64',
            '/usr/local/bin/qemu-system-aarch64',
            'C:\Program Files\qemu\qemu-system-aarch64.exe',
            'C:\Program Files (x86)\qemu\qemu-system-aarch64.exe',
            'C:\msys64\mingw64\bin\qemu-system-aarch64.exe',
            'C:\msys64\ucrt64\bin\qemu-system-aarch64.exe',
            'C:\msys64\clang64\bin\qemu-system-aarch64.exe'
        )
        $firmwareCodeCandidates = @(
            $OvmfCode,
            $env:FEOX_ARMVIRT_CODE,
            '/usr/share/AAVMF/AAVMF_CODE.fd',
            '/usr/share/AAVMF/AAVMF_CODE.ms.fd',
            '/usr/share/edk2/aarch64/QEMU_EFI.fd',
            '/usr/share/qemu-efi-aarch64/QEMU_EFI.fd',
            'C:\Program Files\qemu\share\edk2-aarch64-code.fd',
            'C:\Program Files\qemu\share\QEMU_EFI.fd',
            'C:\Program Files (x86)\qemu\share\edk2-aarch64-code.fd',
            'C:\Program Files (x86)\qemu\share\QEMU_EFI.fd',
            'C:\msys64\mingw64\share\edk2-armvirt\aarch64\QEMU_EFI.fd',
            'C:\msys64\ucrt64\share\edk2-armvirt\aarch64\QEMU_EFI.fd',
            'C:\msys64\clang64\share\edk2-armvirt\aarch64\QEMU_EFI.fd'
        )
        $firmwareVarsCandidates = @(
            $OvmfVars,
            $env:FEOX_ARMVIRT_VARS,
            '/usr/share/AAVMF/AAVMF_VARS.fd',
            '/usr/share/AAVMF/AAVMF_VARS.ms.fd',
            '/usr/share/edk2/aarch64/vars-template-pflash.raw',
            '/usr/share/qemu-efi-aarch64/vars-template-pflash.raw',
            'C:\Program Files\qemu\share\vars-template-pflash.raw',
            'C:\Program Files (x86)\qemu\share\vars-template-pflash.raw',
            'C:\msys64\mingw64\share\edk2-armvirt\aarch64\vars-template-pflash.raw',
            'C:\msys64\ucrt64\share\edk2-armvirt\aarch64\vars-template-pflash.raw',
            'C:\msys64\clang64\share\edk2-armvirt\aarch64\vars-template-pflash.raw'
        )
    }
    default {
        throw "Unsupported architecture: $Architecture"
    }
}

$QemuPath = Resolve-FirstExistingPath -Candidates ($qemuCandidates | Where-Object { $_ })

if (-not $QemuPath) {
    throw "QEMU was not found for $Architecture. Set FEOX_QEMU or pass -QemuPath."
}

$firmwareCode = Resolve-FirstExistingPath -Candidates ($firmwareCodeCandidates | Where-Object { $_ })

if (-not $firmwareCode) {
    throw "Firmware code image was not found for $Architecture. Set the firmware env vars or pass -OvmfCode."
}

$firmwareVars = Resolve-FirstExistingPath -Candidates ($firmwareVarsCandidates | Where-Object { $_ })

if (-not $firmwareVars) {
    throw "Firmware vars image was not found for $Architecture. Set the firmware env vars or pass -OvmfVars."
}

if ([System.IO.Path]::IsPathRooted($RunRoot)) {
    $runRoot = [System.IO.Path]::GetFullPath($RunRoot)
}
else {
$runRoot = [System.IO.Path]::GetFullPath((Join-Path $repoRoot $RunRoot))
}
New-Item -ItemType Directory -Path $runRoot -Force | Out-Null

$varsCopy = Join-Path $runRoot "$Architecture-vars.$Profile.fd"
Copy-Item $firmwareVars $varsCopy -Force
$debugLog = Join-Path $runRoot "$Architecture-debug.$Profile.log"
Remove-Item $debugLog -Force -ErrorAction SilentlyContinue
$stdoutLog = Join-Path $runRoot "$Architecture-stdout.$Profile.log"
$stderrLog = Join-Path $runRoot "$Architecture-stderr.$Profile.log"
if ($TimeoutSeconds -gt 0) {
    Remove-Item $stdoutLog, $stderrLog -Force -ErrorAction SilentlyContinue
}

$qemuArgs = @(
    '-machine', $machine,
    '-cpu', $cpu,
    '-m', $MemoryMiB.ToString(),
    '-drive', "if=pflash,format=raw,readonly=on,file=""$(Convert-ToQemuPath $firmwareCode)""",
    '-drive', "if=pflash,format=raw,file=""$(Convert-ToQemuPath $varsCopy)""",
    '-drive', "format=raw,file=fat:rw:""$(Convert-ToQemuPath $stageInfo.StageRoot)""",
    '-global', 'isa-debugcon.iobase=0x402',
    '-debugcon', "file:""$(Convert-ToQemuPath $debugLog)""",
    '-serial', 'stdio',
    '-monitor', 'none',
    '-no-reboot',
    '-no-shutdown'
)

if (-not $Graphic) {
    $qemuArgs += @('-display', 'none')
}

if ($ExtraQemuArgs.Count -gt 0) {
    $qemuArgs += $ExtraQemuArgs
}

Write-Host "Launching Feox with QEMU:"
Write-Host "  Arch:      $Architecture"
Write-Host "  QEMU:      $QemuPath"
Write-Host "  Firmware:  $firmwareCode"
Write-Host "  Vars:      $varsCopy"
Write-Host "  ESP root:  $($stageInfo.StageRoot)"
Write-Host "  Debug log: $debugLog"
if ($TimeoutSeconds -gt 0) {
    Write-Host "  Timeout:   ${TimeoutSeconds}s"
}

Push-Location $repoRoot
try {
    if ($TimeoutSeconds -gt 0) {
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

        if (-not $timedOut -and $process.ExitCode -ne 0) {
            throw "QEMU exited with status $($process.ExitCode)."
        }

        $logSources = @()
        if (Test-Path $debugLog) {
            $logSources += Get-Content $debugLog -Raw
        }
        if (Test-Path $stdoutLog) {
            $logSources += Get-Content $stdoutLog -Raw
        }

        if ($SuccessMarker -and (($logSources -join "`n") -notmatch [regex]::Escape($SuccessMarker))) {
            throw "QEMU smoke run did not reach success marker '$SuccessMarker'."
        }
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
