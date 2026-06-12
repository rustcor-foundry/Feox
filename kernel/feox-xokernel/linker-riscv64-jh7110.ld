OUTPUT_FORMAT(elf64-littleriscv)
OUTPUT_ARCH(riscv)
ENTRY(_start)

/*
 * Orange Pi RV (StarFive JH7110): DRAM starts at 0x4000_0000, so the image
 * links at DRAM base + the 2 MiB text_offset declared in the boot image
 * header. U-Boot's `booti` relocates the Image there if needed and enters
 * in S-mode with a0=hartid, a1=dtb (the RISC-V Linux boot protocol). No
 * self-relocation is performed; the kernel runs from its physical link
 * address until sv39 paging is wired up.
 *
 * Verify on the board before first boot: `bdinfo` in U-Boot shows the DRAM
 * bank start; this BASE_ADDRESS must equal that start + 0x200000.
 */

BASE_ADDRESS = 0x40200000;

SECTIONS
{
    . = BASE_ADDRESS;
    __kernel_start = .;

    .text : ALIGN(4K)
    {
        __stext = .;
        KEEP(*(.text.boot))
        *(.text .text.*)
    }

    /* Each section is 4 KiB-aligned so the kernel can be mapped with
       per-section sv39 permissions (text R-X, rodata R--, data/bss RW-)
       without two permission classes sharing a page. */
    .rodata : ALIGN(4K)
    {
        __srodata = .;
        *(.rodata .rodata.*)
        *(.srodata .srodata.*)
    }

    .data : ALIGN(4K)
    {
        __sdata = .;
        *(.data .data.*)
        *(.sdata .sdata.*)
    }

    .bss (NOLOAD) : ALIGN(4K)
    {
        /* _start zeroes [__sbss, __ebss) itself: U-Boot's booti copies a raw
           image and does NOT zero bss the way an ELF loader does. */
        __sbss = .;
        *(.bss.boot_stack)
        *(.sbss .sbss.*)
        *(.bss .bss.*)
        *(COMMON)
        . = ALIGN(8);
        __ebss = .;
    }

    __kernel_end = .;

    /DISCARD/ :
    {
        *(.comment .comment.*)
        *(.eh_frame .eh_frame.*)
        *(.note .note.*)
        *(.riscv.attributes)
    }
}
