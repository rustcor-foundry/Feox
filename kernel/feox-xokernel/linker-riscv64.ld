OUTPUT_FORMAT(elf64-littleriscv)
OUTPUT_ARCH(riscv)
ENTRY(_start)

/*
 * QEMU `virt` (and the real Orange Pi RV via U-Boot) hands control to S-mode
 * with the firmware (OpenSBI) resident at 0x8000_0000. A `-kernel` payload is
 * loaded at 0x8020_0000, which is where we base the image so the ELF load
 * address matches the entry PC OpenSBI jumps to. No relocation is performed;
 * the kernel runs from its physical link address until sv39 paging is wired up.
 */

BASE_ADDRESS = 0x80200000;

SECTIONS
{
    . = BASE_ADDRESS;
    __kernel_start = .;

    .text : ALIGN(4K)
    {
        KEEP(*(.text.boot))
        *(.text .text.*)
    }

    .rodata : ALIGN(4K)
    {
        *(.rodata .rodata.*)
        *(.srodata .srodata.*)
    }

    .data : ALIGN(4K)
    {
        *(.data .data.*)
        *(.sdata .sdata.*)
    }

    .bss (NOLOAD) : ALIGN(4K)
    {
        *(.bss.boot_stack)
        *(.sbss .sbss.*)
        *(.bss .bss.*)
        *(COMMON)
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
