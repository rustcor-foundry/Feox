# Builds a hardware kernel image for one of the Orange Pi boards on Windows.
# See build-board.sh for the rationale (env RUSTFLAGS replaces the repo's
# QEMU linker rustflags instead of merging with them).
#
# Usage: tools\build-board.ps1 -Board jh7110|ky-x1 [-Release]
param(
    [Parameter(Mandatory = $true)][ValidateSet("jh7110", "ky-x1")][string]$Board,
    [switch]$Release
)
Set-Location (Join-Path $PSScriptRoot "..")
$env:RUSTFLAGS = "-Clink-arg=-Tkernel/feox-xokernel/linker-riscv64-$Board.ld -Crelocation-model=static"
$buildArgs = @(
    "build", "-p", "feox-xokernel", "--bin", "feox-xokernel",
    "--target", "riscv64gc-unknown-none-elf",
    "--no-default-features", "--features", "runtime,storage,rfs",
    "--target-dir", "target/$Board"
)
if ($Release) { $buildArgs += "--release" }
& cargo @buildArgs
$code = $LASTEXITCODE
Remove-Item Env:\RUSTFLAGS
exit $code
