import sys

CHANGES = [
    ("hal/console.rs",
     "pub trait Console {",
     "pub trait Console: Sync {"),
    ("hal/input.rs",
     "pub trait InputDevice {",
     "pub trait InputDevice: Sync {"),
    ("hal/display.rs",
     "pub trait DisplayDevice {",
     "pub trait DisplayDevice: Sync {"),
]

for path, old, new in CHANGES:
    with open(path) as f:
        content = f.read()
    count = content.count(old)
    print(f"[patch_25] match count in {path}: {count}")
    if count != 1:
        print(f"[patch_25] ABORT: expected exactly 1 match in {path}. Nothing written.")
        sys.exit(1)
    content = content.replace(old, new)
    with open(path, "w") as f:
        f.write(content)
    print(f"[patch_25] wrote: {path}")

print("[patch_25] OK: Console, InputDevice, DisplayDevice now require Sync, allowing &'static dyn Trait to live inside a Mutex")
