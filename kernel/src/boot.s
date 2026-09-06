/* NexusOS — boot.s
 * Multiboot2 header + _start entry stub
 * Assembled with: nasm -f elf64 boot.s -o boot.o   (on Linux/WSL)
 * OR: gcc -c boot.s -o boot.o                       (GNU as syntax)
 *
 * This file does three things:
 *   1. Embeds a Multiboot2 header so QEMU/GRUB can load our ELF
 *   2. Sets up a temporary 16 KiB stack
 *   3. Zeroes the BSS segment
 *   4. Jumps to kernel_main with a null BootInfo* (QEMU direct boot mode)
 */

/* ── Multiboot2 constants ───────────────────────────────────────────────── */
.set MB2_MAGIC,  0xE85250D6
.set MB2_ARCH,   0           /* i386 protected / 64-bit long mode */
.set STACK_SIZE, 16384       /* 16 KiB temporary boot stack       */

/* ── Multiboot2 header — MUST be in first 32 KiB of the ELF image ─────── */
.section .multiboot2, "a"
.align 8
mb2_header_start:
    .long  MB2_MAGIC
    .long  MB2_ARCH
    .long  mb2_header_end - mb2_header_start          /* total length  */
    .long  -(MB2_MAGIC + MB2_ARCH + (mb2_header_end - mb2_header_start))  /* checksum */
    /* end tag — required */
    .short 0
    .short 0
    .long  8
mb2_header_end:

/* ── Kernel entry ─────────────────────────────────────────────────────── */
.section .text
.global _start
.type   _start, @function
_start:
    cli                         /* disable interrupts until IDT is up       */

    /* Set up the temporary stack.
     * In QEMU -kernel mode, the processor is in 32-bit protected mode when
     * it reaches _start.  We switch to long mode here if needed, but since
     * we're building an ELF64 the QEMU firmware/Multiboot2 shim does this
     * for us on modern QEMU versions (≥7.0).
     * For now: trust QEMU put us in 64-bit mode already.
     */
    movq $stack_top, %rsp       /* point RSP at top of our boot stack       */
    movq $0,         %rbp       /* clear frame pointer (aids backtraces)     */

    /* Zero the BSS segment.
     *   rdi = start of BSS
     *   rcx = byte count
     *   al  = 0
     */
    lea  _bss_start(%rip), %rdi
    lea  _bss_end(%rip),   %rcx
    subq %rdi, %rcx
    xorb %al,  %al
    rep  stosb

    /* Call kernel_main(boot_info = NULL).
     * When booted via QEMU -kernel, we don't have a real BootInfo struct.
     * The kernel handles boot_info == NULL gracefully (minimal mode).
     *
     * When booted via our UEFI bootloader, %rdi already contains the
     * BootInfo* pointer the bootloader placed there — we don't touch it.
     * So just call straight through.
     */
    xorq %rdi, %rdi             /* boot_info = NULL for QEMU direct boot    */
    call kernel_main

    /* kernel_main is  -> !  (never returns).
     * If it somehow does, halt the CPU forever.
     */
.hang:
    cli
    hlt
    jmp .hang

.size _start, . - _start

/* ── Boot stack (BSS so it doesn't bloat the binary) ─────────────────── */
.section .bss
.align 16
stack_bottom:
    .skip STACK_SIZE
stack_top:
