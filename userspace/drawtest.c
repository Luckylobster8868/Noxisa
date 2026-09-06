// NexusOS userspace program for drawtest — calls sys_draw (rax=2), then
// sys_exit(0). Mirrors hello.c's structure exactly (same freestanding
// _start, same inline-asm syscall convention) so it compiles and loads
// the same way, just exercising a different syscall number.
void _start() {
    // sys_draw
    __asm__ volatile(
        "movq $2, %%rax\n"
        "syscall\n"
        :
        :
        : "rax"
    );
    // sys_exit(0)
    __asm__ volatile(
        "movq $60, %%rax\n"
        "xorq %%rdi, %%rdi\n"
        "syscall\n"
        ::: "rax", "rdi"
    );
}
