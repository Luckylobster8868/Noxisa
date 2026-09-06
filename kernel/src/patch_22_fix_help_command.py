import sys
TARGET = "main.rs"
OLD = '''crate::kprint!("Commands: help meminfo version reboot ticks uptime syscalls scrubtest teardowntest multitest audittest heaptest isotest isofault wxtest guardtest captest permtest ipctest spawn restest conctest preempttest'''
NEW = '''crate::kprint!("Commands: help meminfo version reboot ticks uptime syscalls scrubtest teardowntest multitest audittest heaptest isotest isofault wxtest guardtest captest permtest ipctest spawn restest conctest preempttest drawtest ownertest'''
with open(TARGET) as f:
    content = f.read()
count = content.count(OLD)
print(f"[patch_22] match count in {TARGET}: {count}")
if count != 1:
    print("[patch_22] ABORT: expected exactly 1 match. Nothing written.")
    sys.exit(1)
content = content.replace(OLD, NEW)
with open(TARGET, "w") as f:
    f.write(content)
print("[patch_22] OK: added drawtest and ownertest to the help command's printed list")
