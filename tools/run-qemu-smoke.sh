#!/usr/bin/env bash
# Bash wrapper for the Feox bounded QEMU smoke boot.
#
# Mirrors tools/run-qemu-smoke.ps1 closely enough to validate locally on
# Linux without pwsh. The PowerShell harness is still the authoritative
# entry point for CI; this script exists so contributors on lx-ws01 (or
# any other Linux host with qemu-system-x86_64 + OVMF + rustup) can run a
# bounded smoke locally without a PowerShell install.

set -euo pipefail

PROFILE="debug"
ARCH="x86_64"
TIMEOUT_SECONDS=20
SUCCESS_MARKER="stage: runtime service idle"
MEMORY_MIB=256

while [[ $# -gt 0 ]]; do
    case "$1" in
        --profile) PROFILE="$2"; shift 2 ;;
        --release) PROFILE="release"; shift ;;
        --timeout) TIMEOUT_SECONDS="$2"; shift 2 ;;
        --memory)  MEMORY_MIB="$2"; shift 2 ;;
        --marker)  SUCCESS_MARKER="$2"; shift 2 ;;
        *) echo "unknown arg: $1" >&2; exit 2 ;;
    esac
done

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

KERNEL_TARGET="x86_64-unknown-none"
LOADER_TARGET="x86_64-unknown-uefi"
LOADER_BOOT_NAME="BOOTX64.EFI"

CARGO_PROFILE_ARGS=()
if [[ "$PROFILE" == "release" ]]; then
    CARGO_PROFILE_ARGS+=("--release")
fi

echo "== building kernel =="
cargo build -p feox-xokernel --bin feox-xokernel --target "$KERNEL_TARGET" "${CARGO_PROFILE_ARGS[@]}"

echo "== building loader =="
cargo build -p feox-loader-uefi --bin feox-loader-uefi --target "$LOADER_TARGET" "${CARGO_PROFILE_ARGS[@]}"

KERNEL_ARTIFACT="$REPO_ROOT/target/$KERNEL_TARGET/$PROFILE/feox-xokernel"
LOADER_ARTIFACT="$REPO_ROOT/target/$LOADER_TARGET/$PROFILE/feox-loader-uefi.efi"

[[ -f "$KERNEL_ARTIFACT" ]] || { echo "missing kernel artifact: $KERNEL_ARTIFACT" >&2; exit 1; }
[[ -f "$LOADER_ARTIFACT" ]] || { echo "missing loader artifact: $LOADER_ARTIFACT" >&2; exit 1; }

STAGE_ROOT="$REPO_ROOT/target/feox-efi"
ESP_ROOT="$STAGE_ROOT/EFI/BOOT"
mkdir -p "$ESP_ROOT"
cp -f "$LOADER_ARTIFACT" "$ESP_ROOT/$LOADER_BOOT_NAME"
cp -f "$KERNEL_ARTIFACT" "$ESP_ROOT/FEOXKERN.ELF"

QEMU_BIN="${FEOX_QEMU:-/usr/bin/qemu-system-x86_64}"
[[ -x "$QEMU_BIN" ]] || { echo "qemu not found at $QEMU_BIN" >&2; exit 1; }

resolve_first() {
    for c in "$@"; do
        [[ -n "$c" && -f "$c" ]] && { echo "$c"; return 0; }
    done
    return 1
}

OVMF_CODE="$(resolve_first "${FEOX_OVMF_CODE:-}" \
    /usr/share/OVMF/OVMF_CODE.fd \
    /usr/share/OVMF/OVMF_CODE_4M.fd \
    /usr/share/edk2/x64/OVMF_CODE.fd \
    /usr/share/edk2-ovmf/x64/OVMF_CODE.fd \
    /usr/share/qemu/OVMF_CODE.fd)" || { echo "OVMF_CODE not found" >&2; exit 1; }

OVMF_VARS="$(resolve_first "${FEOX_OVMF_VARS:-}" \
    /usr/share/OVMF/OVMF_VARS.fd \
    /usr/share/OVMF/OVMF_VARS_4M.fd \
    /usr/share/edk2/x64/OVMF_VARS.fd \
    /usr/share/edk2-ovmf/x64/OVMF_VARS.fd \
    /usr/share/qemu/OVMF_VARS.fd)" || { echo "OVMF_VARS not found" >&2; exit 1; }

RUN_ROOT="$REPO_ROOT/target/feox-qemu"
mkdir -p "$RUN_ROOT"
VARS_COPY="$RUN_ROOT/$ARCH-vars.$PROFILE.fd"
cp -f "$OVMF_VARS" "$VARS_COPY"
DEBUG_LOG="$RUN_ROOT/$ARCH-debug.$PROFILE.log"
STDOUT_LOG="$RUN_ROOT/$ARCH-stdout.$PROFILE.log"
STDERR_LOG="$RUN_ROOT/$ARCH-stderr.$PROFILE.log"
rm -f "$DEBUG_LOG" "$STDOUT_LOG" "$STDERR_LOG"

# Lazily create a 16 MiB sparse backing file for the smoke NVMe controller.
# The self-test reads LBA 0 over the live I/O queue; we stamp a known ASCII
# pattern there each run so the trace shows recognizable bytes instead of
# the all-zeros a fresh image would return.
NVME_DISK="$RUN_ROOT/$ARCH-nvme-disk.$PROFILE.img"
if [[ ! -f "$NVME_DISK" ]]; then
    truncate -s 16M "$NVME_DISK"
fi
printf 'FEOX-NVME-SMOKE-LBA0' | dd of="$NVME_DISK" conv=notrunc bs=1 count=20 status=none 2>/dev/null || true

echo "== launching QEMU =="
echo "  arch:      $ARCH"
echo "  qemu:      $QEMU_BIN"
echo "  code fw:   $OVMF_CODE"
echo "  vars copy: $VARS_COPY"
echo "  esp root:  $STAGE_ROOT"
echo "  debug log: $DEBUG_LOG"
echo "  timeout:   ${TIMEOUT_SECONDS}s"
echo "  marker:    $SUCCESS_MARKER"

set +e
timeout --kill-after=2 "${TIMEOUT_SECONDS}" \
    "$QEMU_BIN" \
        -machine q35 \
        -cpu qemu64 \
        -smp 4 \
        -m "$MEMORY_MIB" \
        -drive "if=pflash,format=raw,readonly=on,file=$OVMF_CODE" \
        -drive "if=pflash,format=raw,file=$VARS_COPY" \
        -drive "format=raw,file=fat:rw:$STAGE_ROOT" \
        -drive "id=feox-nvme-disk,file=$NVME_DISK,format=raw,if=none" \
        -device "nvme,drive=feox-nvme-disk,serial=feox-smoke" \
        -global isa-debugcon.iobase=0x402 \
        -debugcon "file:$DEBUG_LOG" \
        -serial stdio \
        -monitor none \
        -no-reboot \
        -no-shutdown \
        -display none \
        > "$STDOUT_LOG" 2> "$STDERR_LOG"
QEMU_EXIT=$?
set -e
# `timeout` exits 124 on TERM, 137 on KILL after the kill-after window.
TIMED_OUT=0
if [[ "$QEMU_EXIT" -eq 124 || "$QEMU_EXIT" -eq 137 ]]; then
    TIMED_OUT=1
fi

# Give the debug-console and stdio redirects a brief moment to flush after
# a bounded stop so success-marker checks do not race the final log write.
sleep 1

if [[ "$TIMED_OUT" -eq 0 && "$QEMU_EXIT" -ne 0 ]]; then
    echo "QEMU exited with status $QEMU_EXIT" >&2
    exit "$QEMU_EXIT"
fi

if [[ -n "$SUCCESS_MARKER" ]]; then
    if grep -F -q -- "$SUCCESS_MARKER" "$DEBUG_LOG" "$STDOUT_LOG" 2>/dev/null; then
        echo "== success: marker reached =="
        exit 0
    else
        echo "== FAIL: marker '$SUCCESS_MARKER' not found =="
        echo "  see $DEBUG_LOG and $STDOUT_LOG"
        exit 1
    fi
fi

exit 0
