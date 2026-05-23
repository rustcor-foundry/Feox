# SMP bring-up notes

Last updated: 2026-05-23

Working notes for AP boot v2 (real → protected → long mode trampoline +
Rust `ap_entry`). v1 is shipped (`crate::smp::bring_up_first_ap` with
a 14-byte halt-after-write trampoline). v2 was attempted, hit a hang
that needs a fresh session to debug, and was reverted. This document
captures the design + the lessons learned so the next attempt doesn't
repeat the dead ends.

## v2 design

Trampoline layout (one 4 KiB page, loader-allocated below 1 MiB):

| Offset | Purpose |
|--------|---------|
| 0x000 | 16-bit real-mode prologue (loads GDT, enters PE, far jumps to pmode32) |
| 0x044 | 32-bit pmode32 entry (loads CR3, enables PAE+LME+PG, far jumps to lmode64) |
| 0x09e | 64-bit lmode64 entry (loads RSP + ap_entry from data area, jumps to Rust) |
| 0x0c0 | Data area: `cr3` / `stack_top` / `ap_entry` (3 × u64) |
| 0x0d8 | GDT (null / code32 / data32 / code64 / data64, all base=0) |
| 0x100 | GDT pointer (limit=0x27, base patched at runtime) |

Far-jump targets need absolute linear addresses, so:
- BSP patches the 16→32 jump target (offset 0x040) with
  `trampoline_phys + 0x044` before SIPI.
- The 16-bit code itself patches `gdt_ptr.base` (offset 0x102) and
  the 32→64 jump target (offset 0x098) at boot time.

## What's already known to work

- Loader allocates the trampoline frame via
  `uefi::boot::allocate_pages(AllocateType::MaxAddress(0x100000), ...)`.
- LAPIC driver (`crate::lapic`) maps the MMIO, reads ID/version, and
  successfully dispatches INIT-SIPI-SIPI to AP 1.
- v1 trampoline (14 bytes of 16-bit asm: write magic + halt) wakes
  the AP and the BSP observes the magic. So the IPI plumbing,
  trampoline placement, and SIPI vector decoding are all sound.
- The v2 trampoline assembled bytes (verified via `objdump -s`) match
  the intent: data offsets resolve correctly, RIP-relative addresses
  in the 64-bit section point at the data fields, the far-jump
  encodings are right.

## LLVM MC quirks (Rust's `global_asm!`)

These are the syntax rules I learned the hard way; ignoring any of
them produces silent miscompiles or compile errors. Document them
here so future asm work in this repo doesn't relitigate them.

- **Memory operands can't contain two symbols.**
  `[reg + (label_a - label_b)]` fails with "cannot use more than one
  symbol in memory operand". Workaround: `.equ NAME, label_a -
  label_b` before the use, then `[reg + NAME]`. The equate resolves
  to an absolute immediate that the memory-operand parser accepts.
- **Bare symbols default to memory operands.** `sub ebx, NAME` is
  assembled as `sub ebx, [NAME]` (i.e. a load) rather than
  `sub ebx, imm32`. Use `OFFSET NAME` to force the immediate form.
- **No `movzx r32, sreg`.** Use `mov ax, cs ; movzx eax, ax`.

## Known bug in v2 attempt

`call 1f ; pop ebx` to recompute the trampoline base inside the 32-bit
section faults because `SS:ESP` is whatever the AP held at SIPI time
(undefined). The push to the stack hits invalid memory.

Fix: don't recompute. The 16-bit prologue already sets `ebx =
trampoline_base_linear`, and general-purpose registers survive across
mode transitions. The 32-bit code should just use the inherited `ebx`.
(This fix was made but did not resolve the remaining hang — see below.)

## Open hang

With the `call/pop` removed, the BSP still wedges after sending SIPI.
`v1 trampoline + same SIPI path` works fine; only the v2 trampoline
triggers the wedge. Hypothesis: the AP triple-faults somewhere in the
mode transitions and (under QEMU `-smp 4` TCG) starves or hangs the
BSP polling loop.

The next session needs to find which transition fails. Suggested
debugging steps:

1. **Build a "stage 1 only" trampoline.** Strip everything after
   entering protected mode — the AP should just halt in 32-bit mode.
   If the BSP doesn't wedge, the 16→32 transition is fine.
2. **Build a "stage 2 only" trampoline.** Add the CR3/CR4/EFER/CR0
   toggles but stop short of the 32→64 far jump (halt in 32-bit
   compatibility mode). Tests the paging-enable path.
3. **Build a "stage 3 only" trampoline.** Add the 32→64 far jump and
   halt in 64-bit mode immediately. Tests the long-mode entry.
4. Only then add the data-area loads + jump to Rust.

Use QEMU monitor (`-monitor stdio` via a side channel) and run
`info registers -a` after the hang to see exactly where each vCPU
stopped. `info mem` shows the AP's CR3. `x /16i 0xXXXXX` disassembles
arbitrary memory.

## Build-system thought

If the asm gets too fiddly to maintain inline, the cleanest path is a
`build.rs` that invokes `nasm -f bin` on a separate `.asm` file and
includes the resulting flat binary via `include_bytes!`. Adds nasm as
a build dependency (CI runner `lx-ws01` would need it installed) but
sidesteps every LLVM MC quirk. Decide once the inline approach proves
genuinely unworkable, not pre-emptively.
