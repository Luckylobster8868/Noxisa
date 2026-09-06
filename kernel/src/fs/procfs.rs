//! procfs — virtual /proc filesystem exposing kernel state

extern crate alloc;
use alloc::{string::String, vec::Vec, format, boxed::Box};
use crate::fs::vfs::{DirEntry, FileStat, FsError, Filesystem};

pub struct Procfs;

impl Filesystem for Procfs {
    fn name(&self) -> &'static str { "procfs" }

    fn read(&self, path: &str, buf: &mut [u8]) -> Result<usize, FsError> {
        let content: String = match path {
            "/proc/meminfo" => {
                let free = crate::memory::pmm::free_frames();
                format!("MemFree: {} kB\n", free * 4)
            }
            "/proc/version" => {
                String::from("Noxisa version 0.1.0 (Rust no_std kernel)\n")
            }
            "/proc/uptime" => {
                String::from("0.00 0.00\n") // TODO: real timer
            }
            _ if path.starts_with("/proc/") => {
                return Err(FsError::NotFound);
            }
            _ => return Err(FsError::InvalidPath),
        };
        let bytes = content.as_bytes();
        let n = buf.len().min(bytes.len());
        buf[..n].copy_from_slice(&bytes[..n]);
        Ok(n)
    }

    fn write(&self, _path: &str, _buf: &[u8]) -> Result<usize, FsError> {
        Err(FsError::PermissionDenied)
    }

    fn readdir(&self, path: &str) -> Result<Vec<DirEntry>, FsError> {
        match path {
            "/proc" => Ok(alloc::vec![
                DirEntry { name: String::from("meminfo"), is_dir: false, size: 64 },
                DirEntry { name: String::from("version"), is_dir: false, size: 64 },
                DirEntry { name: String::from("uptime"),  is_dir: false, size: 16 },
            ]),
            _ => Err(FsError::NotFound),
        }
    }

    fn stat(&self, path: &str) -> Result<FileStat, FsError> {
        if path == "/proc" || path.starts_with("/proc/") {
            Ok(FileStat {
                size: 0, is_dir: path == "/proc",
                mode: 0o444, uid: 0, gid: 0, atime: 0, mtime: 0,
            })
        } else {
            Err(FsError::NotFound)
        }
    }
}

/// Mount procfs at /proc.
pub fn mount(mountpoint: &str) {
    crate::fs::vfs::mount(mountpoint, Box::new(Procfs));
}
