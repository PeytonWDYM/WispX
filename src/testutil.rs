//! Helpers shared by unit tests: scratch directories and fake executables.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new() -> Self {
        let name = format!(
            "wispx-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        );
        // /tmp rather than $TMPDIR: socket paths have a 108-byte limit.
        let path = Path::new("/tmp").join(name);
        fs::create_dir_all(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        TempDir(path)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    /// Writes an executable shell script named `name`, returning its path.
    pub fn script(&self, name: &str, body: &str) -> PathBuf {
        let path = self.0.join(name);
        let temporary = self.0.join(format!(".{name}.tmp"));
        fs::write(&temporary, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&temporary, fs::Permissions::from_mode(0o755)).unwrap();
        fs::rename(&temporary, &path).unwrap();
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
