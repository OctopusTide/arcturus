//! Sizes on disk: a file's length, or a folder's total walked without following links or
//! junctions (whatever they point at belongs to someone else). Logical sizes, not space on
//! disk; a walk stops after a set number of entries and says so.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
    /// A symbolic link or junction: not followed, counted as 0.
    Link,
    Missing,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::File => "file",
            Kind::Dir => "dir",
            Kind::Link => "link",
            Kind::Missing => "missing",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Size {
    pub kind: Kind,
    pub bytes: u64,
    pub files: u64,
    /// Creation time, Unix ms, when the file system keeps one.
    pub created_ms: Option<i64>,
    /// The walk hit the entry limit (or was stopped): `bytes` is a lower bound.
    pub truncated: bool,
}

fn ms(t: std::io::Result<std::time::SystemTime>) -> Option<i64> {
    t.ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_millis() as i64)
}

pub fn measure(path: &Path, max_entries: u64, stop: &AtomicBool) -> Size {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return Size { kind: Kind::Missing, bytes: 0, files: 0, created_ms: None, truncated: false };
    };
    let created_ms = ms(meta.created());
    let ft = meta.file_type();
    if ft.is_symlink() {
        return Size { kind: Kind::Link, bytes: 0, files: 0, created_ms, truncated: false };
    }
    if !ft.is_dir() {
        return Size { kind: Kind::File, bytes: meta.len(), files: 1, created_ms, truncated: false };
    }
    let (mut bytes, mut files, mut entries, mut truncated) = (0u64, 0u64, 0u64, false);
    let mut stack = vec![path.to_path_buf()];
    'walk: while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            entries += 1;
            if entries > max_entries || (entries % 4096 == 0 && stop.load(Ordering::Relaxed)) {
                truncated = true;
                break 'walk;
            }
            // From the directory listing itself: links are not followed.
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_symlink() {
                continue;
            }
            if ft.is_dir() {
                stack.push(e.path());
            } else if let Ok(m) = e.metadata() {
                bytes += m.len();
                files += 1;
            }
        }
    }
    Size { kind: Kind::Dir, bytes, files, created_ms, truncated }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_folders_and_missing() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("r");
        std::fs::create_dir_all(root.join("a/b")).unwrap();
        std::fs::write(root.join("x.bin"), [0u8; 10]).unwrap();
        std::fs::write(root.join("a/b/y.bin"), [0u8; 5]).unwrap();
        let stop = AtomicBool::new(false);
        let s = measure(&root, 1000, &stop);
        assert_eq!((s.kind, s.bytes, s.files, s.truncated), (Kind::Dir, 15, 2, false));
        assert!(s.created_ms.is_some());
        assert_eq!(measure(&root.join("x.bin"), 1000, &stop).bytes, 10);
        assert_eq!(measure(&root.join("none"), 1000, &stop).kind, Kind::Missing);
        assert!(measure(&root, 2, &stop).truncated);
    }
}
