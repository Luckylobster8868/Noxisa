// Minimal NexusOS userspace program
// sys_write(1, msg, len) then sys_exit(0)
void _start() {
    const char msg[] = "Hello from NexusOS userspace!\n";
    // sys_write
    __asm__ volatile(
        "movq $1, %%rax\n"
        "movq $1, %%rdi\n"
        "movq %0, %%rsi\n"
        "movq $30, %%rdx\n"
        "syscall\n"
        :
        : "r"(msg)
        : "rax", "rdi", "rsi", "rdx"
    );
    // sys_exit(0)
    __asm__ volatile(
        "movq $60, %%rax\n"
        "xorq %%rdi, %%rdi\n"
        "syscall\n"
        ::: "rax", "rdi"
    );
}
