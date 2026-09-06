// tmpfs — in-memory filesystem backed by the kernel heap

extern crate alloc;
use alloc::string::ToString;
use alloc::{string::String, vec::Vec, collections::BTreeMap, boxed::Box};
use spin::RwLock;
use crate::fs::vfs::{DirEntry, FileStat, FsError, Filesystem};

struct TmpfsNode {
    data:   Vec<u8>,
    is_dir: bool,
    mode:   u32,
    uid:    u32,
    gid:    u32,
}

pub struct Tmpfs {
    nodes: RwLock<BTreeMap<String, TmpfsNode>>,
}

impl Tmpfs {
    pub fn new() -> Self {
        let mut m = BTreeMap::new();
        m.insert(String::from("/tmp"), TmpfsNode {
            data: Vec::new(), is_dir: true,
            mode: 0o1777, uid: 0, gid: 0,
        });
        Self { nodes: RwLock::new(m) }
    }
}

impl Filesystem for Tmpfs {
    fn name(&self) -> &'static str { "tmpfs" }

    fn read(&self, path: &str, buf: &mut [u8]) -> Result<usize, FsError> {
        let nodes = self.nodes.read();
        let node  = nodes.get(path).ok_or(FsError::NotFound)?;
        if node.is_dir { return Err(FsError::IsADirectory); }
        let n = buf.len().min(node.data.len());
        buf[..n].copy_from_slice(&node.data[..n]);
        Ok(n)
    }

    fn write(&self, path: &str, buf: &[u8]) -> Result<usize, FsError> {
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
    }

    fn readdir(&self, path: &str) -> Result<Vec<DirEntry>, FsError> {
        let nodes = self.nodes.read();
        if !nodes.get(path).map_or(false, |n| n.is_dir) {
            return Err(FsError::NotADirectory);
        }
        let prefix = if path.ends_with('/') {
            String::from(path)
        } else {
            alloc::format!("{}/", path)
        };
        Ok(nodes.keys()
            .filter(|k| k.starts_with(prefix.as_str()) && !k[prefix.len()..].contains('/'))
            .map(|k| {
                let n = nodes.get(k).unwrap();
                let name = k[prefix.len()..].to_string();
                DirEntry { name, is_dir: n.is_dir, size: n.data.len() as u64 }
            })
            .collect())
    }

    fn stat(&self, path: &str) -> Result<FileStat, FsError> {
        let nodes = self.nodes.read();
        let node  = nodes.get(path).ok_or(FsError::NotFound)?;
        Ok(FileStat {
            size:   node.data.len() as u64,
            is_dir: node.is_dir,
            mode:   node.mode,
            uid:    node.uid,
            gid:    node.gid,
            atime:  0,
            mtime:  0,
        })
    }
}

/// Mount tmpfs at the given path.
pub fn mount(mountpoint: &str) {
    crate::fs::vfs::mount(mountpoint, Box::new(Tmpfs::new()));
}
