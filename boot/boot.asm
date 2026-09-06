; Native Limine protocol entry stub.
;
; Under Multiboot2, the bootloader left us in 32-bit protected mode and we
; had to build our own GDT, page tables, and long-mode transition by hand
; (see git history — the old boot.asm). Under Limine's native protocol, all
; of that is already done for us before this code ever runs: we're called
; directly in 64-bit long mode, with paging enabled and a valid stack.
; This is also why the framebuffer bug we chased for so long only shows up
; under Multiboot2 on real hardware — native protocol guarantees the
; framebuffer stays backed by real VRAM after ExitBootServices, which
; Multiboot2 never promised.

bits 64
section .text.entry

global _start
extern kernel_main_native

_start:
    cli
    cld
    xor rbp, rbp
    ; Limine guarantees a valid stack already, so we don't need to set one
    ; up ourselves the way the old Multiboot2 trampoline did.
    call kernel_main_native
.hang:
    cli
    hlt
    jmp .hang
