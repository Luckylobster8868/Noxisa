OUTPUT_FORMAT(elf64-x86-64)
OUTPUT_ARCH(i386:x86-64)
ENTRY(_start)

/* Explicit program headers, one per permission group. Without this, the
   linker can merge our writable .requests section into the same page as
   executable .text regardless of ALIGN() — that was the actual cause of
   the repeated "PHDRs with different permissions sharing the same memory
   page" panic, not an alignment/rebuild issue. */
PHDRS
{
    limine_requests PT_LOAD;
    text            PT_LOAD;
    rodata          PT_LOAD;
    data            PT_LOAD;
}

SECTIONS
{
    . = 0xffffffff80000000;

    .requests_start_marker : {
        KEEP(*(.requests_start_marker))
    } :limine_requests
    .requests : {
        KEEP(*(.requests))
    } :limine_requests
    .requests_end_marker : {
        KEEP(*(.requests_end_marker))
    } :limine_requests

    . = ALIGN(4K);
    .text : {
        KEEP(*(.text.entry))
        *(.text .text.*)
    } :text

    . = ALIGN(4K);
    .rodata : {
        *(.rodata .rodata.*)
    } :rodata

    . = ALIGN(4K);
    .data : {
        *(.data .data.*)
    } :data

    .bss : {
        _bss_start = .;
        *(.bss .bss.*)
        *(COMMON)
        _bss_end = .;
    } :data

    _kernel_end = .;

    /DISCARD/ :
    {
        *(.comment)
        *(.note*)
        *(.eh_frame*)
    }
}
