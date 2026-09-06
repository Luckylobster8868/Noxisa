#!/usr/bin/env python3
"""
patch_10_tmpfs_ownership.py -- closes the "new-file creation in tmpfs
doesn't respect current_uid()/current_gid()" weakness from
NOXISA_COMPLETE.pdf Part 2 / Part 3.5's table.

Run from ~/nexus-os/kernel/src/fs.
"""

edits = []  # (path, old, new, description)

old1 = '''    fn write(&self, path: &str, buf: &[u8])      -> Result<usize, FsError>;'''

new1 = '''    fn write(&self, path: &str, buf: &[u8])      -> Result<usize, FsError>;
    /// Like write(), but tells the backend which identity is doing the
    /// writing, so a backend that distinguishes creation from overwrite
    /// (e.g. Tmpfs) can stamp real ownership on a brand-new file instead
    /// of a hardcoded default. Default impl ignores uid/gid and forwards
    /// to write() unchanged, so existing backends (procfs's read-only
    /// stub) need no edits to keep compiling.
    fn write_owned(&self, path: &str, buf: &[u8], _uid: u32, _gid: u32) -> Result<usize, FsError> {
        self.write(path, buf)
    }'''

edits.append(('vfs.rs', old1, new1, "add write_owned() default to Filesystem trait"))

old2 = '''    crate::audit::audit_log("vfs", "write", uid, true, String::from(path));
    entry.fs.write(path, buf)
}'''

new2 = '''    crate::audit::audit_log("vfs", "write", uid, true, String::from(path));
    let gid = current_gid();
    entry.fs.write_owned(path, buf, uid, gid)
}'''

edits.append(('vfs.rs', old2, new2, "vfs::write() forwards real uid/gid via write_owned()"))

old3 = '''    fn write(&self, path: &str, buf: &[u8]) -> Result<usize, FsError> {
        let mut nodes = self.nodes.write();
        let node = nodes.entry(String::from(path)).or_insert(TmpfsNode {
            data: Vec::new(), is_dir: false,
            mode: 0o644, uid: 0, gid: 0,
        });
        if node.is_dir { return Err(FsError::IsADirectory); }
        node.data.clear();
        node.data.extend_from_slice(buf);
        Ok(buf.len())
    }'''

new3 = '''    fn write(&self, path: &str, buf: &[u8]) -> Result<usize, FsError> {
        // Fallback path (no identity given) - keeps the old uid=0/gid=0
        // creation behaviour for any caller that still goes through the
        // trait's default write_owned(), e.g. kernel-internal writes
        // before a real caller identity exists.
        self.write_owned(path, buf, 0, 0)
    }

    fn write_owned(&self, path: &str, buf: &[u8], uid: u32, gid: u32) -> Result<usize, FsError> {
        let mut nodes = self.nodes.write();
        // entry().or_insert() only runs the closure on genuine creation -
        // an existing file's uid/gid must NOT change on overwrite, same
        // as real Unix write() semantics.
        let node = nodes.entry(String::from(path)).or_insert(TmpfsNode {
            data: Vec::new(), is_dir: false,
            mode: 0o644, uid, gid,
        });
        if node.is_dir { return Err(FsError::IsADirectory); }
        node.data.clear();
        node.data.extend_from_slice(buf);
        Ok(buf.len())
    }'''

edits.append(('tmpfs.rs', old3, new3, "Tmpfs::write_owned() stamps real identity on creation only"))

sources = {}
for fname, old, new, desc in edits:
    if fname not in sources:
        with open(fname) as f:
            sources[fname] = f.read()

counts = [sources[fname].count(old) for fname, old, new, desc in edits]
print("Match counts:", counts)

if all(c == 1 for c in counts):
    for fname, old, new, desc in edits:
        sources[fname] = sources[fname].replace(old, new, 1)
    for fname, content in sources.items():
        with open(fname, 'w') as f:
            f.write(content)
    print("APPLIED - tmpfs now stamps real uid/gid on file creation")
else:
    for (fname, old, new, desc), c in zip(edits, counts):
        status = "ok" if c == 1 else f"NOT WRITTEN (found {c})"
        print(f"  [{status}] {fname}: {desc}")
    print("NOT WRITTEN - one or more edits didn't match exactly once.")
