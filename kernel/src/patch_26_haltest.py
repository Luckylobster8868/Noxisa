import sys

TARGET = "main.rs"

OLD = '''    } else if b == b"drawtest" {'''

NEW = '''    } else if b == b"haltest" {
        crate::kprintln!("[haltest] Proving hal::registry wrappers are genuine pass-throughs, not just structure");

        // Console: write through the HAL registry, then compare against
        // calling drivers::serial directly. If the HAL path is a real
        // pass-through, both produce byte-identical serial output.
        hal::registry::console_write_fmt(format_args!("[haltest] via hal::registry::console_write_fmt\\r\\n"));
        drivers::serial::write_fmt(format_args!("[haltest] via drivers::serial::write_fmt directly\\r\\n"));

        // Display: dimensions from the HAL registry must equal the
        // dimensions from calling drivers::gpu directly.
        let hal_dims = hal::registry::display_dimensions();
        let direct_dims = drivers::gpu::dimensions();
        crate::kprintln!("[haltest] hal::registry::display_dimensions() = {:?}", hal_dims);
        crate::kprintln!("[haltest] drivers::gpu::dimensions() direct  = {:?}", direct_dims);
        let dims_match = hal_dims == Some(direct_dims);

        // Display: fill_rect through the HAL registry must return true
        // (meaning a display was actually registered and reached).
        let fill_ok = hal::registry::display_fill_rect(4, 4, 8, 8, 0x0000FF00);
        crate::kprintln!("[haltest] hal::registry::display_fill_rect(...) returned {}", fill_ok);

        // Input: poll once through the HAL registry. Doesn't assert a
        // specific keypress (none is guaranteed at test time) -- only
        // proves the call reaches drivers::keyboard without panicking.
        let hal_key = hal::registry::input_poll_char();
        crate::kprintln!("[haltest] hal::registry::input_poll_char() = {:?} (no keypress expected; None is a PASS here)", hal_key);

        if dims_match && fill_ok {
            crate::kprintln!("[haltest] PASS - console, display, and input all reached the real drivers through the HAL registry, dimensions matched exactly");
        } else {
            crate::kprintln!("[haltest] FAIL - dims_match={} fill_ok={}", dims_match, fill_ok);
        }
    } else if b == b"drawtest" {'''

with open(TARGET) as f:
    content = f.read()

count = content.count(OLD)
print(f"[patch_26] match count in {TARGET}: {count}")
if count != 1:
    print("[patch_26] ABORT: expected exactly 1 match. Nothing written.")
    sys.exit(1)

content = content.replace(OLD, NEW)
with open(TARGET, "w") as f:
    f.write(content)

print("[patch_26] OK: haltest kshell command added")
