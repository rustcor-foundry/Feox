# Booting Feox on real hardware (Orange Pi RV / RV2)

The riscv64 kernel boots on hardware through the standard RISC-V Linux boot
protocol: U-Boot's `booti` loads a flat `Image` (our ELF objcopy'd to binary,
carrying the 64-byte Linux boot header), and enters S-mode at the image start
with `a0 = hartid`, `a1 = dtb`. OpenSBI (resident from the vendor firmware)
provides the console, timer, and HSM — exactly the environment the QEMU CI
boot exercises on every commit.

## Boards

| Board | SoC | DRAM base | Image | Link address |
|---|---|---|---|---|
| Orange Pi RV  | StarFive JH7110  | `0x4000_0000` | `feox-jh7110-Image` | `0x4020_0000` |
| Orange Pi RV2 | SpacemiT Ky X1   | `0x0`         | `feox-ky-x1-Image`  | `0x0020_0000` |

The link address must equal the board's **DRAM base + 0x200000** (the
header's `text_offset`). `booti` relocates the Image there automatically.

## Get the images

Every CI run on `main` uploads a `feox-boot-images` artifact containing
`feox-jh7110-Image`, `feox-ky-x1-Image`, `feox-qemu.elf`, this guide, and
`SHA256SUMS`. To build locally instead:

```text
tools/build-board.sh jh7110 --release        # or: tools\build-board.ps1 -Board jh7110 -Release
llvm-objcopy -O binary \
  target/jh7110/riscv64gc-unknown-none-elf/release/feox-xokernel feox-jh7110-Image
```

(`llvm-objcopy` ships with the `llvm-tools` rustup component.)

## Boot procedure

1. **Serial console first.** Connect the board's debug UART (115200 8N1).
   All Feox output goes through the SBI console — without serial you see
   nothing.
2. **Interrupt U-Boot** (any key during the countdown).
3. **Verify the DRAM base** before the first boot:

   ```text
   bdinfo
   ```

   Find the first DRAM bank's `start`. It must match the table above; if it
   differs, fix `BASE_ADDRESS` in the board's linker script to
   `start + 0x200000` and rebuild. (This is the one assumption worth checking
   per board revision.)
4. **Load and boot.** From a FAT partition on SD (adjust dev:part), or TFTP:

   ```text
   fatload mmc 0:1 ${kernel_addr_r} feox-jh7110-Image
   booti ${kernel_addr_r} - ${fdtcontroladdr}
   ```

   or

   ```text
   dhcp; tftpboot ${kernel_addr_r} feox-jh7110-Image
   booti ${kernel_addr_r} - ${fdtcontroladdr}
   ```

   `${fdtcontroladdr}` is U-Boot's own control DTB for the board — exactly
   what the kernel's FDT parser wants. (Use the RV2 image name on the RV2.)

## What to expect on first boot

The hardware path runs every milestone that does not depend on QEMU's fixed
device windows:

- banner, `arch: riscv64 (S-mode)`, boot hart + DTB address (via SBI console)
- trap vector + `breakpoint trap handled` (M2)
- `sv39 paging enabled` (M3), DTB/RAM discovery, `frame allocator online` (M4)
- `console: native UART @ ...` — from this line on, output goes straight to
  the hardware UART (as U-Boot configured it); SBI is only the fallback. If
  the *early* lines are missing but output starts here, the vendor OpenSBI
  lacks the legacy console — harmless.
- feox-async executor (M5), `kernel heap online` (M10), VM self-test (M11)
- `pcie/net: skipped (non-QEMU; DT-derived drivers TODO)` — deliberate
- SMP: secondary harts come up DT-driven — only cpu nodes with an
  `mmu-type` are started (the JH7110's S7 monitor hart is skipped by
  design). Expect `hart N alive` per application core.
- timer interrupts at the DT timebase (M9)
- U-mode execution (M12), ASI syscalls + capability table (M13), preemptive
  scheduler (M14), ELF processes (M15), the embedded apps: feox-hello
  (M16/17) and feox-pingpong IPC (M18)
- net/TCP/RFS demos skip cleanly (their devices are QEMU-window-gated)
- `riscv64 bring-up alive; parking boot hart.`

## Troubleshooting

- **`Bad Linux RISCV Image magic!`** — the file is the ELF, not the
  objcopy'd Image; or the download was truncated (check `SHA256SUMS`).
- **booti runs but no output at all** — (a) wrong DRAM base / link address
  (see `bdinfo` above); (b) the vendor OpenSBI lacks the legacy console
  putchar extension — check the OpenSBI banner earlier in the boot for the
  console driver; a native UART driver in Feox is the fallback (TODO).
- **Output stops after the banner** — the DTB parse likely failed; confirm
  `booti` was given `${fdtcontroladdr}` as the third argument (the `-` in
  the middle is required: no initrd).
- **Panic with a trap dump** — that's the kernel's own diagnostics working;
  the cause/sepc/stval line localizes the fault. File it with the serial log.

## Known gaps on hardware (the next arc)

- DT-derived PLIC routing (interrupt-driven I/O on the boards)
- JH7110 dwmac / Ky X1 ethernet behind the existing `NetDevice` capability
  lane, NVMe/SD storage behind the storage lane — then the full QEMU
  milestone ladder (TCP, RFS) runs on silicon unchanged.
