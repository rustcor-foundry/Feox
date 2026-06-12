/* Feox U-mode app layout: loaded by the kernel's ELF loader (M15) into a
 * per-process address space. The image lives in sv39 root slot 8
 * (0x2_0000_0000+), the process VA window; the kernel maps the stack
 * separately, above the image. Sections are page-aligned so each PT_LOAD
 * carries one permission set (text R+X, rodata R, data/bss R+W). */
ENTRY(_start)

SECTIONS
{
  . = 0x200000000;

  .text : {
    *(.text .text.*)
  }

  . = ALIGN(4096);
  .rodata : {
    *(.rodata .rodata.* .srodata .srodata.*)
  }

  . = ALIGN(4096);
  .data : {
    *(.data .data.* .sdata .sdata.*)
  }

  .bss : {
    *(.bss .bss.* .sbss .sbss.* COMMON)
  }

  /DISCARD/ : {
    *(.eh_frame .eh_frame_hdr .riscv.attributes .comment)
  }
}
