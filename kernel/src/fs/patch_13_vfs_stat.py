#!/usr/bin/env python3
"""
patch_13_vfs_stat.py -- adds a public vfs::stat() wrapper. Filesystem::stat()
already exists per-backend and vfs::write() already calls it internally for
the permission check, but nothing exposes it to callers (kshell tests,
future syscalls) that just want to read back a file's metadata -- e.g. to
verify patch_10's uid/gid-on-creation fix actually took effect.

Run from ~/nexus-os/kernel/src/fs.
"""

with open('vfs.rs') as f:
    full = f.read()

old = '''/// Write a file by absolute path.
pub fn write(path: &str, buf: &[u8]) -> Result<usize, FsError> {'''

new = '''/// Stat a file by absolute path -- no permission check of its own (stat
/// itself isn't gated in this model, same as real Unix stat() not
/// requiring read access to the file's contents, only to the containing
/// directory, which this simplified VFS doesn't model yet).
pub fn stat(path: &str) -> Result<FileStat, FsError> {
    let mounts = MOUNTS.read();
    let entry = mounts.iter().rev()
        .find(|m| path.starts_with(m.mountpoint.as_str()))
        .ok_or(FsError::NotMounted)?;
    entry.fs.stat(path)
}

/// Write a file by absolute path.
pub fn write(path: &str, buf: &[u8]) -> Result<usize, FsError> {'''

n = full.count(old)
print("Match count:", n)

if n == 1:
    full = full.replace(old, new, 1)
    with open('vfs.rs', 'w') as f:
        f.write(full)
    print("APPLIED - vfs::stat() is now public")
else:
    print(f"NOT WRITTEN - expected 1 match, found {n}")
