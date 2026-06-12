#!/usr/bin/env bash
# Builds a hardware kernel image for one of the Orange Pi boards: the same
# source as the QEMU build, linked at the board's DRAM base + 2 MiB via a
# per-board linker script.
#
# RUSTFLAGS is passed via env because env REPLACES config-level rustflags
# while config files MERGE — the repo-level riscv64 rustflags carry the QEMU
# linker script, which must not be joined with the board one (same lesson as
# the app builds in kernel/feox-xokernel/build.rs).
#
# Usage: tools/build-board.sh <jh7110|ky-x1> [--release]
#   jh7110  Orange Pi RV   (StarFive JH7110, DRAM @ 0x4000_0000)
#   ky-x1   Orange Pi RV2  (SpacemiT Ky X1,  DRAM @ 0x0)
# Output: target/<board>/riscv64gc-unknown-none-elf/{debug|release}/feox-xokernel
set -euo pipefail
cd "$(dirname "$0")/.."

board="${1:?usage: tools/build-board.sh <jh7110|ky-x1> [--release]}"
case "$board" in
  jh7110|ky-x1) ;;
  *) echo "unknown board '$board' (expected jh7110 or ky-x1)" >&2; exit 1 ;;
esac
shift

RUSTFLAGS="-Clink-arg=-Tkernel/feox-xokernel/linker-riscv64-${board}.ld -Crelocation-model=static" \
exec cargo build -p feox-xokernel --bin feox-xokernel \
    --target riscv64gc-unknown-none-elf \
    --no-default-features --features runtime,storage,rfs \
    --target-dir "target/${board}" "$@"
