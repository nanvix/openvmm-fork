OUTPUT_FORMAT(elf64-x86-64)
OUTPUT_ARCH(i386:x86-64)
ENTRY(_pvh_start32)

PHDRS
{
    note PT_NOTE FLAGS(4);
    text PT_LOAD FLAGS(5);
    data PT_LOAD FLAGS(6);
}

SECTIONS
{
    .note.xen 0 : ALIGN(4)
    {
        KEEP(*(.note.xen))
    } :note

    . = 0x100000;
    .text : ALIGN(0x1000)
    {
        KEEP(*(.text.boot))
        *(.text .text.*)
    } :text

    .rodata : ALIGN(16)
    {
        *(.rodata.boot)
        *(.rodata .rodata.*)
    } :text

    . = ALIGN(0x1000);
    .data : ALIGN(0x1000)
    {
        *(.data .data.*)
    } :data

    .bss (NOLOAD) : ALIGN(0x1000)
    {
        *(.bss.boot)
        *(.bss .bss.* COMMON)
    } :data

    /DISCARD/ :
    {
        *(.eh_frame .eh_frame_hdr)
        *(.comment)
    }

    ASSERT(_pvh_start32 >= 0x100000, "PVH entry must be at or above 1 MiB")
    ASSERT(_pvh_start32 < 0x100000000, "PVH entry must fit in 32 bits")
}
