//! Process-unique temporary file names and a drop guard that removes them.
//!
//! Shared by the download and transcode adapters so both stage scratch files
//! against one counter and one cleanup path, even on the error path.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A process- and call-unique stamp for temporary file names.
pub(crate) fn unique_stamp() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{}-{nanos}-{seq}", std::process::id())
}

/// Removes its temporary paths when dropped, even on the error path.
pub(crate) struct Scratch(Vec<PathBuf>);

impl Scratch {
    /// Guard a single temporary path.
    pub(crate) fn new(path: PathBuf) -> Self {
        Scratch(vec![path])
    }

    /// Guard several temporary paths.
    pub(crate) fn all(paths: Vec<PathBuf>) -> Self {
        Scratch(paths)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        for path in &self.0 {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn stamps_are_unique_across_calls() {
        assert_ne!(unique_stamp(), unique_stamp());
    }

    #[test]
    fn drop_removes_a_single_path() {
        let dir = Path::new("target").join(format!("scratch-one-{}", unique_stamp()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("temp.part");
        std::fs::write(&path, b"x").unwrap();
        {
            let _guard = Scratch::new(path.clone());
            assert!(path.exists());
        }
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn drop_removes_all_paths() {
        let dir = Path::new("target").join(format!("scratch-many-{}", unique_stamp()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.part");
        let b = dir.join("b.part");
        std::fs::write(&a, b"a").unwrap();
        std::fs::write(&b, b"b").unwrap();
        drop(Scratch::all(vec![a.clone(), b.clone()]));
        assert!(!a.exists());
        assert!(!b.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
