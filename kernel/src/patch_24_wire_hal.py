import sys

TARGET = "main.rs"

OLD1 = """pub mod kshell;     // kernel AI shell (early-boot / emergency)"""
NEW1 = """pub mod kshell;     // kernel AI shell (early-boot / emergency)
pub mod hal;        // Console/InputDevice/DisplayDevice wrappers over drivers/"""

OLD2 = """    ipc::init();

    kprintln!("[boot] Handing off to scheduler - kshell starts now (native)...");"""
NEW2 = """    ipc::init();

    hal::init();

    kprintln!("[boot] Handing off to scheduler - kshell starts now (native)...");"""

with open(TARGET) as f:
    content = f.read()

count1 = content.count(OLD1)
count2 = content.count(OLD2)
print(f"[patch_24] match count for change 1 (mod decl) in {TARGET}: {count1}")
print(f"[patch_24] match count for change 2 (init call) in {TARGET}: {count2}")

if count1 != 1 or count2 != 1:
    print("[patch_24] ABORT: expected exactly 1 match for each change. Nothing written.")
    sys.exit(1)

content = content.replace(OLD1, NEW1)
content = content.replace(OLD2, NEW2)

with open(TARGET, "w") as f:
    f.write(content)

print("[patch_24] OK: pub mod hal; added, hal::init() wired in after ipc::init()")
