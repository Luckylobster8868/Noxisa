//! VFS — Virtual Filesystem Switch
//!
//! Every filesystem (ext4, tmpfs, procfs) registers itself here.
//! All path operations go through this layer, which dispatches
//! to the correct backend based on the mount table.

extern crate alloc;
use alloc::{string::String, vec::Vec, boxed::Box};
use spin::RwLock;

// ─── Per-process open files (Part 6 item 1: per-process resources) ────────────
//
// An OpenFile lives on the owning Tcb (scheduler::Tcb::open_files), not in a
// global table - each process's fd numbers are its own, exactly like real
// Unix fd tables. This module only defines the shape and the open/read/
// write/close entry points; the actual storage and fd-slot allocation is on
// the scheduler, same "wrapping over forking" split as current_uid()/
// current_gid() already use (identity lives on the Tcb, vfs.rs just wraps
// the accessor).
#[derive(Debug, Clone)]
pub struct OpenFile {
    pub path:   String,
    pub offset: usize,
    pub flags:  u8,   // bit0 = read requested, bit1 = write requested
}

pub const O_RDONLY: u8 = 0b01;
pub const O_WRONLY: u8 = 0b10;
pub const O_RDWR:   u8 = O_RDONLY | O_WRONLY;

// ─── Filesystem trait ─────────────────────────────────────────────────────────

/// Every filesystem driver implements this trait.
pub trait Filesystem: Send + Sync {
    fn name(&self)  -> &'static str;
    fn read(&self,  path: &str, buf: &mut [u8]) -> Result<usize, FsError>;
    fn write(&self, path: &str, buf: &[u8])      -> Result<usize, FsError>;
    /// Like write(), but tells the backend which identity is doing the
    /// writing, so a backend that distinguishes creation from overwrite
    /// (e.g. Tmpfs) can stamp real ownership on a brand-new file instead
    /// of a hardcoded default. Default impl ignores uid/gid and forwards
    /// to write() unchanged, so existing backends (procfs's read-only
    /// stub) need no edits to keep compiling.
    fn write_owned(&self, path: &str, buf: &[u8], _uid: u32, _gid: u32) -> Result<usize, FsError> {
        self.write(path, buf)
    }
    fn readdir(&self, path: &str)                -> Result<Vec<DirEntry>, FsError>;
    fn stat(&self, path: &str)                   -> Result<FileStat, FsError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsError {
    NotFound,
    PermissionDenied,
    NotADirectory,
    IsADirectory,
    NoSpace,
    InvalidPath,
    IoError,
    NotMounted,
}

#[derive(Debug, Clone)]
pub struct DirEntry {
    pub name:    String,
    pub is_dir:  bool,
    pub size:    u64,
}

#[derive(Debug, Clone, Copy)]
pub struct FileStat {
    pub size:    u64,
    pub is_dir:  bool,
    pub mode:    u32,   // Unix permission bits
    pub uid:     u32,
    pub gid:     u32,
    pub atime:   u64,
    pub mtime:   u64,
}

// ─── Mount table ──────────────────────────────────────────────────────────────

struct MountEntry {
    mountpoint: String,
    fs:         Box<dyn Filesystem>,
}

static MOUNTS: RwLock<Vec<MountEntry>> = RwLock::new(Vec::new());

/// Initialise the VFS layer. Call once during kernel boot.
pub fn init() {
    crate::kprintln!("[vfs] Initialised");
}

/// Mount a filesystem at a path.
pub fn mount(mountpoint: &str, fs: Box<dyn Filesystem>) {
    crate::kprintln!("[vfs] Mounting {} at {}", fs.name(), mountpoint);
    MOUNTS.write().push(MountEntry {
        mountpoint: String::from(mountpoint),
        fs,
    });
}

/// Mount the root filesystem from the NVMe drive.
pub fn mount_root() {
    crate::kprintln!("[vfs] Mounting root filesystem");
    // TODO: probe /dev/nvme0n1p1, detect ext4 magic, mount it
}

// Part 6 item 2: real per-Tcb identity via the scheduler, replacing the
// former bare `static mut` globals. Kept as thin wrapper functions (not a
// second, divergent identity mechanism) so ipc/mod.rs and main.rs keep the
// same call shape, per this project's "prefer wrapping over forking"
// principle.
pub fn current_uid() -> u32 { crate::scheduler::Scheduler::get().uid() }
pub fn current_gid() -> u32 { crate::scheduler::Scheduler::get().gid() }
pub fn set_identity(uid: u32, gid: u32) { crate::scheduler::Scheduler::get().set_identity(uid, gid); }

// ─── Per-process cwd (Part 6 item 1) ───────────────────────────────────────────
// Same thin-wrapper shape as current_uid()/current_gid() above - storage
// lives on the current Tcb, this just forwards to it.
pub fn cwd() -> String { crate::scheduler::Scheduler::get().cwd() }
pub fn set_cwd(path: &str) { crate::scheduler::Scheduler::get().set_cwd(path); }

/// Resolve a possibly-relative path against the caller's cwd. Absolute
/// paths (starting with '/') pass through unchanged. Only used by open() -
/// the existing path-based read()/write() keep taking absolute paths
/// unchanged, so no existing caller (permtest, ipctest, spawn_program's
/// boot-time /tmp/test.txt write, etc.) is affected by this addition.
fn resolve_path(path: &str) -> String {
    if path.starts_with('/') {
        String::from(path)
    } else {
        let base = cwd();
        if base.ends_with('/') {
            alloc::format!("{}{}", base, path)
        } else {
            alloc::format!("{}/{}", base, path)
        }
    }
}

const R_BIT: u32 = 0o4;
const W_BIT: u32 = 0o2;

fn check_permission(stat: &FileStat, want: u32) -> Result<(), FsError> {
    let (uid, gid) = (current_uid(), current_gid());
    let shift = if uid == stat.uid { 6 } else if gid == stat.gid { 3 } else { 0 };
    let bits = (stat.mode >> shift) & 0o7;
    if bits & want == want { Ok(()) } else { Err(FsError::PermissionDenied) }
}

/// Read a file by absolute path.
pub fn read(path: &str, buf: &mut [u8]) -> Result<usize, FsError> {
    let mounts = MOUNTS.read();
    let entry = mounts.iter().rev()
        .find(|m| path.starts_with(m.mountpoint.as_str()))
        .ok_or(FsError::NotMounted)?;
    let uid = current_uid();
    match entry.fs.stat(path) {
        Ok(stat) => {
            if let Err(e) = check_permission(&stat, R_BIT) {
                crate::audit::audit_log("vfs", "read", uid, false, String::from(path));
                return Err(e);
            }
        }
        Err(FsError::NotFound) => return Err(FsError::NotFound),
        Err(e) => return Err(e),
    }
    crate::audit::audit_log("vfs", "read", uid, true, String::from(path));
    entry.fs.read(path, buf)
}

/// Stat a file by absolute path -- no permission check of its own (stat
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
pub fn write(path: &str, buf: &[u8]) -> Result<usize, FsError> {
    let mounts = MOUNTS.read();
    let entry = mounts.iter().rev()
        .find(|m| path.starts_with(m.mountpoint.as_str()))
        .ok_or(FsError::NotMounted)?;
    let uid = current_uid();
    match entry.fs.stat(path) {
        Ok(stat) => {
            if let Err(e) = check_permission(&stat, W_BIT) {
                crate::audit::audit_log("vfs", "write", uid, false, String::from(path));
                return Err(e);
            }
        }
        // KNOWN GAP: Filesystem::write has no uid/gid param, so new files
        // still get stamped with Tmpfs's own hardcoded owner, not
        // current_uid()/current_gid(). Creation is allowed through unconditionally.
        Err(FsError::NotFound) => {}
        Err(e) => return Err(e),
    }
    crate::audit::audit_log("vfs", "write", uid, true, String::from(path));
    let gid = current_gid();
    entry.fs.write_owned(path, buf, uid, gid)
}

// ─── Per-process file descriptors (Part 6 item 1) ──────────────────────────────
//
// open() does the one real permission check up front (same check_permission()
// chokepoint read()/write() already use - stat() the resolved path against
// the requested access bits) and then just remembers path+offset+flags on
// the caller's own Tcb. read_fd()/write_fd() do NOT re-check permission on
// every call - re-checking per-call would only re-derive the same answer
// open() already got (identity doesn't change mid-fd-lifetime in any
// caller today), so the single check-at-open() point stays the actual
// chokepoint, same "one place gets checked" principle as vfs::read/write
// already follow one layer up. If identity ever becomes revocable
// mid-lifetime, that's a deliberate future change to this comment, not an
// oversight.

/// Open a file, returning a per-process fd (>= 0) on success. Denied if the
/// caller lacks the requested access bits (own audit_log call, same shape
/// as read()/write()'s). NotFound if opened write-only/read-write on a
/// nonexistent path - unlike write(), open() does not implicitly create,
/// since "does this path exist" and "should writing to it create it" are
/// different questions and open()'s caller may only want O_RDONLY.
pub fn open(path: &str, flags: u8) -> Result<i32, FsError> {
    let resolved = resolve_path(path);
    let mounts = MOUNTS.read();
    let entry = mounts.iter().rev()
        .find(|m| resolved.starts_with(m.mountpoint.as_str()))
        .ok_or(FsError::NotMounted)?;
    let uid = current_uid();
    let stat = entry.fs.stat(&resolved)?;

    let want = ((flags & O_RDONLY != 0) as u32) * R_BIT
             | ((flags & O_WRONLY != 0) as u32) * W_BIT;
    if let Err(e) = check_permission(&stat, want) {
        crate::audit::audit_log("vfs", "open", uid, false, resolved.clone());
        return Err(e);
    }
    drop(mounts);

    crate::audit::audit_log("vfs", "open", uid, true, resolved.clone());
    let fd = crate::scheduler::Scheduler::get().alloc_fd(OpenFile {
        path: resolved, offset: 0, flags,
    });
    Ok(fd)
}

/// Read from an open fd at its current offset, advancing the offset by the
/// number of bytes actually read. BadFd-equivalent (NotFound) if the fd
/// isn't open on the caller's Tcb, or wasn't opened for reading.
pub fn read_fd(fd: i32, buf: &mut [u8]) -> Result<usize, FsError> {
    let sched = crate::scheduler::Scheduler::get();
    let (path, offset, flags) = sched.fd_info(fd).ok_or(FsError::NotFound)?;
    if flags & O_RDONLY == 0 { return Err(FsError::PermissionDenied); }
    let n = read(&path, buf)?;   // re-uses the existing whole-file read() -
                                  // tmpfs has no partial-read primitive yet,
                                  // so this is "read whole file, slice from
                                  // the tracked offset" rather than a true
                                  // seek+read; documented, not hidden.
    let start = offset.min(n);
    let avail = n - start;
    let take = avail.min(buf.len());
    if start > 0 {
        buf.copy_within(start..start + take, 0);
    }
    sched.set_fd_offset(fd, offset + take);
    Ok(take)
}

/// Write to an open fd. Appends at the fd's tracked offset - since Tmpfs
/// has no partial-write/splice primitive yet either, this reads the whole
/// existing file, splices `buf` in at the offset, and writes the whole
/// result back. Correct for the common "open, write once, close" case this
/// milestone's own test exercises; a real seek+splice-write belongs to a
/// later Tmpfs upgrade, not silently pretended here.
pub fn write_fd(fd: i32, buf: &[u8]) -> Result<usize, FsError> {
    let sched = crate::scheduler::Scheduler::get();
    let (path, offset, flags) = sched.fd_info(fd).ok_or(FsError::NotFound)?;
    if flags & O_WRONLY == 0 { return Err(FsError::PermissionDenied); }

    let mut existing = alloc::vec![0u8; offset + buf.len()];
    let existing_len = read(&path, &mut existing).unwrap_or(0);
    let mut merged = alloc::vec::Vec::with_capacity(offset.max(existing_len) + buf.len());
    merged.extend_from_slice(&existing[..existing_len.min(offset)]);
    while merged.len() < offset { merged.push(0); }
    merged.extend_from_slice(buf);
    if existing_len > merged.len() {
        merged.extend_from_slice(&existing[merged.len()..existing_len]);
    }

    write(&path, &merged)?;
    sched.set_fd_offset(fd, offset + buf.len());
    Ok(buf.len())
}

/// Close a fd, freeing its slot on the caller's Tcb for reuse. Returns
/// false if the fd wasn't open (double-close, or a stale/foreign fd
/// number) - callers that need a hard error can map that themselves.
pub fn close_fd(fd: i32) -> bool {
    crate::scheduler::Scheduler::get().close_fd(fd)
}
