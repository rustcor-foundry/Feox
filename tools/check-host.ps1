param(
    [ValidateSet('x86_64', 'aarch64')]
    [string]$Architecture = 'x86_64'
)

$ErrorActionPreference = 'Stop'

function Test-InstalledRustTarget {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Target
    )

    $installed = rustup target list --installed 2>$null
    if (-not $installed) {
        return $false
    }

    return $installed -contains $Target
}

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

switch ($Architecture) {
    'x86_64' {
        $kernelTarget = 'x86_64-unknown-none'
        $loaderTarget = 'x86_64-unknown-uefi'
        $qemuCandidates = @(
            $env:FEOX_QEMU,
            'C:\Program Files\qemu\qemu-system-x86_64.exe',
            'C:\Program Files (x86)\qemu\qemu-system-x86_64.exe',
            'C:\msys64\mingw64\bin\qemu-system-x86_64.exe',
            'C:\msys64\ucrt64\bin\qemu-system-x86_64.exe',
            'C:\msys64\clang64\bin\qemu-system-x86_64.exe',
            'D:\Paul\Software Projects\Feox\tools\host\qemu\qemu-system-x86_64.exe'
        )
        $firmwareCodeCandidates = @(
            $env:FEOX_OVMF_CODE,
            'C:\Program Files\qemu\share\edk2-x86_64-code.fd',
            'C:\Program Files\qemu\share\OVMF_CODE.fd',
            'C:\Program Files\qemu\OVMF_CODE.fd',
            'C:\Program Files (x86)\qemu\share\edk2-x86_64-code.fd',
            'C:\Program Files (x86)\qemu\share\OVMF_CODE.fd',
            'C:\Program Files (x86)\qemu\OVMF_CODE.fd',
            'C:\msys64\mingw64\share\edk2-ovmf\x64\OVMF_CODE.fd',
            'C:\msys64\ucrt64\share\edk2-ovmf\x64\OVMF_CODE.fd',
            'C:\msys64\clang64\share\edk2-ovmf\x64\OVMF_CODE.fd'
        )
        $firmwareVarsCandidates = @(
            $env:FEOX_OVMF_VARS,
            'C:\Program Files\qemu\share\edk2-i386-vars.fd',
            'C:\Program Files\qemu\share\OVMF_VARS.fd',
            'C:\Program Files\qemu\OVMF_VARS.fd',
            'C:\Program Files (x86)\qemu\share\edk2-i386-vars.fd',
            'C:\Program Files (x86)\qemu\share\OVMF_VARS.fd',
            'C:\Program Files (x86)\qemu\OVMF_VARS.fd',
            'C:\msys64\mingw64\share\edk2-ovmf\x64\OVMF_VARS.fd',
            'C:\msys64\ucrt64\share\edk2-ovmf\x64\OVMF_VARS.fd',
            'C:\msys64\clang64\share\edk2-ovmf\x64\OVMF_VARS.fd'
        )
    }
    'aarch64' {
        $kernelTarget = 'aarch64-unknown-none-softfloat'
        $loaderTarget = 'aarch64-unknown-uefi'
        $qemuCandidates = @(
            $env:FEOX_QEMU,
            'C:\Program Files\qemu\qemu-system-aarch64.exe',
            'C:\Program Files (x86)\qemu\qemu-system-aarch64.exe',
            'C:\msys64\mingw64\bin\qemu-system-aarch64.exe',
            'C:\msys64\ucrt64\bin\qemu-system-aarch64.exe',
            'C:\msys64\clang64\bin\qemu-system-aarch64.exe',
            'D:\Paul\Software Projects\Feox\tools\host\qemu\qemu-system-aarch64.exe'
        )
        $firmwareCodeCandidates = @(
            $env:FEOX_OVMF_CODE,
            $env:FEOX_ARMVIRT_CODE,
            'C:\Program Files\qemu\share\edk2-aarch64-code.fd',
            'C:\Program Files\qemu\share\QEMU_EFI.fd',
            'C:\Program Files (x86)\qemu\share\edk2-aarch64-code.fd',
            'C:\Program Files (x86)\qemu\share\QEMU_EFI.fd',
            'C:\msys64\mingw64\share\edk2-armvirt\aarch64\QEMU_EFI.fd',
            'C:\msys64\ucrt64\share\edk2-armvirt\aarch64\QEMU_EFI.fd',
            'C:\msys64\clang64\share\edk2-armvirt\aarch64\QEMU_EFI.fd'
        )
        $firmwareVarsCandidates = @(
            $env:FEOX_OVMF_VARS,
            $env:FEOX_ARMVIRT_VARS,
            'C:\Program Files\qemu\share\vars-template-pflash.raw',
            'C:\Program Files (x86)\qemu\share\vars-template-pflash.raw',
            'C:\msys64\mingw64\share\edk2-armvirt\aarch64\vars-template-pflash.raw',
            'C:\msys64\ucrt64\share\edk2-armvirt\aarch64\vars-template-pflash.raw',
            'C:\msys64\clang64\share\edk2-armvirt\aarch64\vars-template-pflash.raw'
        )
    }
}

$qemuPath = Resolve-FirstExistingPath -Candidates ($qemuCandidates | Where-Object { $_ })
$firmwareCode = Resolve-FirstExistingPath -Candidates ($firmwareCodeCandidates | Where-Object { $_ })
$firmwareVars = Resolve-FirstExistingPath -Candidates ($firmwareVarsCandidates | Where-Object { $_ })

$checks = @(
    [PSCustomObject]@{
        Check = 'Rust kernel target'
        Target = $kernelTarget
        Ready = Test-InstalledRustTarget -Target $kernelTarget
        Detail = $kernelTarget
    }
    [PSCustomObject]@{
        Check = 'Rust loader target'
        Target = $loaderTarget
        Ready = Test-InstalledRustTarget -Target $loaderTarget
        Detail = $loaderTarget
    }
    [PSCustomObject]@{
        Check = 'QEMU binary'
        Target = $Architecture
        Ready = [bool]$qemuPath
        Detail = if ($qemuPath) { $qemuPath } else { 'Not found' }
    }
    [PSCustomObject]@{
        Check = 'Firmware code'
        Target = $Architecture
        Ready = [bool]$firmwareCode
        Detail = if ($firmwareCode) { $firmwareCode } else { 'Not found' }
    }
    [PSCustomObject]@{
        Check = 'Firmware vars'
        Target = $Architecture
        Ready = [bool]$firmwareVars
        Detail = if ($firmwareVars) { $firmwareVars } else { 'Not found' }
    }
)

$checks | Format-Table -AutoSize | Out-Host

if ($checks.Ready -notcontains $false) {
    Write-Host ""
    Write-Host "Host ready for Feox $Architecture boot flow."
    exit 0
}

Write-Host ""
Write-Host "Host is not ready for Feox $Architecture boot flow."
exit 1
