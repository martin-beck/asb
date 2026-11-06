//! Project-local, independently owned fixture directories for viewer checks.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

pub(crate) struct Fixture {
    pub(crate) path: PathBuf,
}

impl Fixture {
    pub(crate) fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let parent =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/replay-debug-tests");
        std::fs::create_dir_all(&parent).unwrap();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = parent.join(format!(
            "{}-{nanos}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self { path }
    }

    pub(crate) fn write(&self, relative: &str, contents: &str) -> PathBuf {
        let path = self.path.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, contents).unwrap();
        path
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}
