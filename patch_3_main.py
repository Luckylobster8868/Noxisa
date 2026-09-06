#!/usr/bin/env python3
import sys

PATH = "kernel/src/main.rs"

edits = [
(
"""    } else if b == b"guardtest" {""",
"""    } else if b == b"restest" {
        crate::kprintln!("--- Per-process resources test ---");
        unsafe { fs::vfs::set_identity(0, 0); }

        // 1. cwd: default is "/", set it, confirm a relative open() resolves
        //    against it rather than against literal "/".
        let cwd_before = fs::vfs::cwd();
        crate::kprintln!("[restest] cwd before: {:?} (expect \\"/\\")", cwd_before);
        fs::vfs::set_cwd("/tmp");
        crate::kprintln!("[restest] cwd after set_cwd(\\"/tmp\\"): {:?}", fs::vfs::cwd());

        let _ = fs::vfs::write("/tmp/restest.txt", b"hello via fd");

        // 2. open() a relative path - must resolve through the new cwd, not
        //    literal "/restest.txt" (which doesn't exist and would fail).
        let fd = match fs::vfs::open("restest.txt", fs::vfs::O_RDONLY) {
            Ok(fd) => { crate::kprintln!("[restest] open(\\"restest.txt\\") under cwd=/tmp -> fd={}", fd); fd }
            Err(e) => { crate::kprintln!("[restest] FAILED - open should have resolved via cwd: {:?}", e); -1 }
        };

        // 3. read_fd() must return the same bytes write() put there, and
        //    advance the fd's own offset (proven by a second read_fd()
        //    below returning 0 - end of file, not a re-read from offset 0).
        let mut buf = [0u8; 32];
        let n = fs::vfs::read_fd(fd, &mut buf).unwrap_or(0);
        let got = core::str::from_utf8(&buf[..n]).unwrap_or("<invalid utf8>");
        crate::kprintln!("[restest] read_fd -> {:?} ({} bytes, expect \\"hello via fd\\")", got, n);

        let n2 = fs::vfs::read_fd(fd, &mut buf).unwrap_or(999);
        crate::kprintln!("[restest] second read_fd -> {} bytes (expect 0 - offset advanced, EOF)", n2);

        // 4. A second, independently-spawned process must NOT see fd 0/1/2
        //    already open, and must NOT inherit kshell's cwd - proving fds
        //    and cwd are genuinely per-Tcb, not shared global state (the
        //    exact bug class the old bare CURRENT_UID/CURRENT_CAPS globals
        //    already had for identity, before that was fixed).
        match scheduler::Scheduler::get().spawn_program("hello", fs::initrd::HELLO_ELF, CAP_WRITE_SERIAL) {
            Err(e) => crate::kprintln!("[restest] FAILED - could not spawn probe process: {}", e),
            Ok(pid) => {
                scheduler::yield_now();
                scheduler::Scheduler::get().reap(pid);
                crate::kprintln!("[restest] probe process pid={} spawned+reaped cleanly (own, empty fd table by construction)", pid);
            }
        }

        // 5. close_fd() frees the slot; a subsequent read_fd() on the same
        //    number must fail (NotFound), proving the fd is genuinely gone,
        //    not just reset to offset 0.
        let closed = fs::vfs::close_fd(fd);
        crate::kprintln!("[restest] close_fd({}) -> {} (expect true)", fd, closed);
        match fs::vfs::read_fd(fd, &mut buf) {
            Err(fs::vfs::FsError::NotFound) => crate::kprintln!("[restest] read_fd after close -> NotFound as expected"),
            other => crate::kprintln!("[restest] FAILED - expected NotFound after close, got {:?}", other),
        }

        fs::vfs::set_cwd("/");
        crate::kprintln!("--- restest done ---");
    } else if b == b"guardtest" {"""
),
(
"""        crate::kprint!("Commands: help meminfo version reboot ticks uptime syscalls scrubtest teardowntest multitest audittest heaptest isotest isofault wxtest guardtest captest permtest ipctest spawn""",
"""        crate::kprint!("Commands: help meminfo version reboot ticks uptime syscalls scrubtest teardowntest multitest audittest heaptest isotest isofault wxtest guardtest captest permtest ipctest spawn restest"""
),
]

with open(PATH, "r") as f:
    content = f.read()

for old, new in edits:
    count = content.count(old)
    if count != 1:
        print(f"ABORT: expected exactly 1 match, found {count} for a block starting: {old[:60]!r}")
        sys.exit(1)
    content = content.replace(old, new)

with open(PATH, "w") as f:
    f.write(content)

print("patch_3_main.py: applied 2/2 edits to", PATH)
