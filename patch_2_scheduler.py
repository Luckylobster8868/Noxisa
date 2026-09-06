#!/usr/bin/env python3
import sys

PATH = "kernel/src/scheduler/mod.rs"

edits = [
(
"use alloc::{boxed::Box, collections::VecDeque, vec::Vec};",
"use alloc::{boxed::Box, collections::VecDeque, string::String, vec::Vec};"
),
(
"""    pub isolated_table_frames:    Option<Vec<u64>>,
    pub isolated_user_stack_phys: u64,

    // Name for debugging (fixed-size, no heap)
    name: [u8; 32],
}""",
"""    pub isolated_table_frames:    Option<Vec<u64>>,
    pub isolated_user_stack_phys: u64,

    // Name for debugging (fixed-size, no heap)
    name: [u8; 32],

    // ── Per-process resources (Part 6 item 1) ──────────────────────────────
    // Now that spawn_program() is generic (Part 6 item "Generic process
    // creation", DONE), a spawned process needs its own working directory
    // and its own fd table rather than sharing kshell's identity globals'
    // former shape (a single bare state). Each field lives per-Tcb, same
    // "storage moves to the Tcb, vfs.rs/callers just wrap an accessor"
    // pattern already used for caps/uid/gid above - not a second,
    // divergent per-process state mechanism.
    pub cwd:         String,
    pub open_files:  Vec<Option<crate::fs::vfs::OpenFile>>,
    pub args:        Vec<String>,
}"""
),
(
"""            isolated_table_frames: None,
            isolated_user_stack_phys: 0,
            name:       name_arr,
        });""",
"""            isolated_table_frames: None,
            isolated_user_stack_phys: 0,
            name:       name_arr,
            cwd:        String::from("/"),
            open_files: Vec::new(),
            args:       Vec::new(),
        });"""
),
(
"""    pub fn set_identity(&self, uid: u32, gid: u32) {
        let pid = self.current_pid();
        if let Some(Some(t)) = self.tasks.write().get_mut(pid as usize) {
            t.uid = uid;
            t.gid = gid;
        }
    }
}""",
"""    pub fn set_identity(&self, uid: u32, gid: u32) {
        let pid = self.current_pid();
        if let Some(Some(t)) = self.tasks.write().get_mut(pid as usize) {
            t.uid = uid;
            t.gid = gid;
        }
    }

    // ── Per-process resources accessors (Part 6 item 1) ─────────────────────
    // Same shape throughout as caps()/uid()/gid() above: look up the
    // current Tcb by current_pid(), read or write the one field, fall back
    // to a sane default if there's no current task yet (mirrors every
    // existing accessor's fallback behaviour).

    pub fn cwd(&self) -> String {
        let pid = self.current_pid();
        self.tasks.read().get(pid as usize).and_then(|s| s.as_deref())
            .map(|t| t.cwd.clone()).unwrap_or_else(|| String::from("/"))
    }

    pub fn set_cwd(&self, path: &str) {
        let pid = self.current_pid();
        if let Some(Some(t)) = self.tasks.write().get_mut(pid as usize) {
            t.cwd = String::from(path);
        }
    }

    pub fn args(&self) -> Vec<String> {
        let pid = self.current_pid();
        self.tasks.read().get(pid as usize).and_then(|s| s.as_deref())
            .map(|t| t.args.clone()).unwrap_or_default()
    }

    pub fn set_args(&self, args: Vec<String>) {
        let pid = self.current_pid();
        if let Some(Some(t)) = self.tasks.write().get_mut(pid as usize) {
            t.args = args;
        }
    }

    /// Allocate a fd for `of` on the current Tcb: reuses the first `None`
    /// slot (a closed fd) if one exists, otherwise appends a new slot.
    /// Real Unix fd-reuse behaviour (lowest-available-number), not just
    /// monotonically growing - matters once open/close/open cycles happen
    /// repeatedly on a long-lived process rather than once per test run.
    pub fn alloc_fd(&self, of: crate::fs::vfs::OpenFile) -> i32 {
        let pid = self.current_pid();
        let mut tasks = self.tasks.write();
        let t = match tasks.get_mut(pid as usize).and_then(|s| s.as_deref_mut()) {
            Some(t) => t,
            None => return -1,   // no current task - degenerate case, same
                                  // "no-op fallback" shape as the other
                                  // accessors above rather than a panic.
        };
        if let Some(slot) = t.open_files.iter().position(|s| s.is_none()) {
            t.open_files[slot] = Some(of);
            slot as i32
        } else {
            t.open_files.push(Some(of));
            (t.open_files.len() - 1) as i32
        }
    }

    /// Snapshot of an open fd's (path, offset, flags), or None if `fd`
    /// isn't currently open on the caller's Tcb.
    pub fn fd_info(&self, fd: i32) -> Option<(String, usize, u8)> {
        if fd < 0 { return None; }
        let pid = self.current_pid();
        self.tasks.read().get(pid as usize).and_then(|s| s.as_deref())
            .and_then(|t| t.open_files.get(fd as usize))
            .and_then(|slot| slot.as_ref())
            .map(|of| (of.path.clone(), of.offset, of.flags))
    }

    pub fn set_fd_offset(&self, fd: i32, offset: usize) {
        if fd < 0 { return; }
        let pid = self.current_pid();
        if let Some(Some(t)) = self.tasks.write().get_mut(pid as usize) {
            if let Some(Some(of)) = t.open_files.get_mut(fd as usize) {
                of.offset = offset;
            }
        }
    }

    /// Close `fd`, freeing its slot for reuse by a later alloc_fd(). Returns
    /// false for an already-closed or out-of-range fd (double-close is not
    /// treated as an error at this layer - callers that care can check the
    /// return value).
    pub fn close_fd(&self, fd: i32) -> bool {
        if fd < 0 { return false; }
        let pid = self.current_pid();
        if let Some(Some(t)) = self.tasks.write().get_mut(pid as usize) {
            if let Some(slot) = t.open_files.get_mut(fd as usize) {
                if slot.is_some() {
                    *slot = None;
                    return true;
                }
            }
        }
        false
    }

    /// Number of currently-open fds on the caller's Tcb - used by restest
    /// to prove a freshly spawned process's fd table starts genuinely
    /// empty, not inherited from whichever process spawned it.
    pub fn open_fd_count(&self) -> usize {
        let pid = self.current_pid();
        self.tasks.read().get(pid as usize).and_then(|s| s.as_deref())
            .map(|t| t.open_files.iter().filter(|s| s.is_some()).count())
            .unwrap_or(0)
    }
}"""
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

print("patch_2_scheduler.py: applied 4/4 edits to", PATH)
