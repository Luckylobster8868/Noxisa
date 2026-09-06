import sys
TARGET = "drivers/mod.rs"
OLD = """pub mod apic;
pub mod pic;
pub mod amdgpu;
pub mod gdt;
pub mod gpu;       // Generic framebuffer
pub mod intel_xe;  // Intel Iris Xe GPU — i7-1165G7 (your chip)
pub mod ax201;     // Intel Wi-Fi 6 AX201 (your chip)
pub mod idt;
pub mod nvme;
pub mod pci;
pub mod serial;
pub mod wifi;
pub mod font;
pub mod pit;
pub mod keyboard;
pub mod rtl8139;"""
NEW = """pub mod apic;
pub mod pic;
pub mod gdt;
pub mod gpu;       // Generic framebuffer
pub mod idt;
pub mod serial;
pub mod pit;
pub mod keyboard;"""
with open(TARGET) as f:
    content = f.read()
count = content.count(OLD)
print(f"[patch_19] match count in {TARGET}: {count}")
if count != 1:
    print("[patch_19] ABORT: expected exactly 1 match. Nothing written.")
    sys.exit(1)
content = content.replace(OLD, NEW)
with open(TARGET, "w") as f:
    f.write(content)
print("[patch_19] OK: trimmed drivers/mod.rs to 8 live driver declarations")
