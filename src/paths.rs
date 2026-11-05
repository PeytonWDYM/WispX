//! XDG locations and the private runtime directory.

use std::env;
use std::fs::{self, DirBuilder, File, OpenOptions, Permissions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

pub fn home() -> Option<PathBuf> {
    env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
}

fn xdg_dir(variable: &str, fallback: &str) -> Result<PathBuf, String> {
    // The XDG spec requires ignoring relative values.
    if let Some(value) = env::var_os(variable).map(PathBuf::from)
        && value.is_absolute()
    {
        return Ok(value);
    }
    home()
        .map(|home| home.join(fallback))
        .ok_or_else(|| format!("neither {variable} nor HOME is set"))
}

pub fn default_config_file() -> Result<PathBuf, String> {
    Ok(xdg_dir("XDG_CONFIG_HOME", ".config")?
        .join("wispx")
        .join("config.json"))
}

pub fn data_dir() -> Result<PathBuf, String> {
    Ok(xdg_dir("XDG_DATA_HOME", ".local/share")?.join("wispx"))
}

/// Codex keeps its login under `$CODEX_HOME`, defaulting to `~/.codex`.
pub fn default_codex_auth_file() -> Result<PathBuf, String> {
    if let Some(value) = env::var_os("CODEX_HOME").map(PathBuf::from)
        && value.is_absolute()
    {
        return Ok(value.join("auth.json"));
    }
    home()
        .map(|home| home.join(".codex").join("auth.json"))
        .ok_or_else(|| "neither CODEX_HOME nor HOME is set".to_string())
}

/// Expands a leading `~/` and requires the result to be absolute, so a daemon
/// started from any directory resolves a configured path the same way.
pub fn expand_absolute(value: &str) -> Result<PathBuf, String> {
    let path = if value == "~" {
        home().ok_or("HOME is not set")?
    } else if let Some(rest) = value.strip_prefix("~/") {
        home().ok_or("HOME is not set")?.join(rest)
    } else {
        PathBuf::from(value)
    };
    if path.is_absolute() {
        Ok(path)
    } else {
        Err(format!(
            "`{value}` must be an absolute path or start with ~/"
        ))
    }
}

/// The directory holding the socket, lock and in-flight audio, created if
/// needed. It lives on `$XDG_RUNTIME_DIR` (a per-user tmpfs) whenever possible, so
/// private audio never reaches a disk.
pub fn runtime_dir() -> Result<PathBuf, String> {
    let dir = runtime_location()?;
    ensure_private_dir(&dir)?;
    Ok(dir)
}

fn runtime_location() -> Result<PathBuf, String> {
    match env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from) {
        Some(base) if base.is_absolute() && base.is_dir() => Ok(base.join("wispx")),
        _ => {
            let uid = current_uid().map_err(|e| format!("cannot determine user id: {e}"))?;
            Ok(env::temp_dir().join(format!("wispx-{uid}")))
        }
    }
}

pub fn socket_path(runtime: &Path) -> PathBuf {
    runtime.join("wispx.sock")
}

/// Where a client should look for the daemon. Only the daemon creates anything,
/// so asking about it changes nothing. When there is no directory this user can
/// trust the path leads nowhere, which clients report as "not running".
pub fn client_socket_path() -> PathBuf {
    runtime_location()
        .and_then(|dir| check_own_dir(&dir).map(|_| socket_path(&dir)))
        .unwrap_or_else(|_| PathBuf::from("/nonexistent/wispx.sock"))
}

pub fn applications_dir() -> Result<PathBuf, String> {
    Ok(xdg_dir("XDG_DATA_HOME", ".local/share")?.join("applications"))
}

pub fn autostart_dir() -> Result<PathBuf, String> {
    Ok(xdg_dir("XDG_CONFIG_HOME", ".config")?.join("autostart"))
}

pub fn lock_path(runtime: &Path) -> PathBuf {
    runtime.join("wispx.lock")
}

/// Creates `dir` as 0700, or accepts an existing directory only if this user
/// owns it. The ownership check is what makes the `/tmp` fallback safe against
/// another user pre-creating the path.
pub fn ensure_private_dir(dir: &Path) -> Result<(), String> {
    match DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(format!("cannot create {}: {e}", dir.display())),
    }
    let meta = check_own_dir(dir)?;
    if meta.mode() & 0o077 != 0 {
        fs::set_permissions(dir, Permissions::from_mode(0o700))
            .map_err(|e| format!("cannot restrict {}: {e}", dir.display()))?;
    }
    Ok(())
}

/// Accepts only a real directory that this user owns, not a symlink to one.
fn check_own_dir(dir: &Path) -> Result<fs::Metadata, String> {
    let meta = fs::symlink_metadata(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    if !meta.file_type().is_dir() {
        return Err(format!(
            "{} exists but is not a directory; refusing to use it",
            dir.display()
        ));
    }
    let uid = current_uid().map_err(|e| format!("cannot determine user id: {e}"))?;
    if meta.uid() != uid {
        return Err(format!(
            "{} is owned by another user; refusing to use it",
            dir.display()
        ));
    }
    Ok(meta)
}

/// Creates a new file readable only by this user, failing if the path exists.
pub fn create_private_file(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

pub fn current_uid() -> io::Result<u32> {
    // /proc/self is owned by the effective user; this avoids an FFI call.
    fs::metadata("/proc/self").map(|meta| meta.uid())
}

/// Whether two paths name the same file, which need not exist yet. Symlinks count
/// as the file they point to, the same as for [`atomic_write`].
pub fn same_file(a: &Path, b: &Path) -> bool {
    let resolve = |path: &Path| {
        fs::canonicalize(path).unwrap_or_else(|_| {
            match (path.parent().map(fs::canonicalize), path.file_name()) {
                (Some(Ok(dir)), Some(name)) => dir.join(name),
                _ => path.to_path_buf(),
            }
        })
    };
    resolve(a) == resolve(b)
}

/// Atomically replaces `path`. A symlinked target stays a symlink so dotfile
/// managers keep working.
pub fn atomic_write(path: &Path, contents: &[u8]) -> Result<(), String> {
    use std::io::Write;

    let target = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let dir = target
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", target.display()))?;
    fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let name = target
        .file_name()
        .ok_or_else(|| format!("{} has no file name", target.display()))?
        .to_string_lossy();
    let temporary = dir.join(format!(".{name}.tmp.{}", std::process::id()));

    let result = (|| -> io::Result<()> {
        let mut file = create_private_file(&temporary)?;
        file.write_all(contents)?;
        file.sync_all()?;
        fs::rename(&temporary, &target)
    })();
    if let Err(error) = result {
        let _ = fs::remove_file(&temporary);
        return Err(format!("cannot write {}: {error}", target.display()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    #[test]
    fn relative_paths_are_rejected() {
        assert!(expand_absolute("models/ggml.bin").is_err());
        assert_eq!(
            expand_absolute("/opt/model.bin").unwrap(),
            PathBuf::from("/opt/model.bin")
        );
    }

    #[test]
    fn tilde_expands_against_home() {
        // HOME is always set under cargo test on Linux runners.
        if let Some(home) = home() {
            assert_eq!(expand_absolute("~/m.bin").unwrap(), home.join("m.bin"));
            assert_eq!(expand_absolute("~").unwrap(), home);
        }
        assert!(expand_absolute("~other/m.bin").is_err());
    }

    #[test]
    fn private_dir_is_created_0700_and_tightened() {
        let temp = TempDir::new();
        let dir = temp.path().join("runtime");
        ensure_private_dir(&dir).unwrap();
        let mode = |p: &Path| fs::metadata(p).unwrap().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);

        fs::set_permissions(&dir, Permissions::from_mode(0o755)).unwrap();
        ensure_private_dir(&dir).unwrap();
        assert_eq!(mode(&dir), 0o700);
    }

    #[test]
    fn private_dir_refuses_symlinks_and_files() {
        let temp = TempDir::new();
        let real = temp.path().join("real");
        fs::create_dir(&real).unwrap();
        let link = temp.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(ensure_private_dir(&link).is_err());

        let file = temp.path().join("file");
        fs::write(&file, b"x").unwrap();
        assert!(ensure_private_dir(&file).is_err());
    }

    #[test]
    fn private_file_is_0600_and_never_reuses_a_path() {
        let temp = TempDir::new();
        let path = temp.path().join("audio.wav");
        drop(create_private_file(&path).unwrap());
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        assert!(create_private_file(&path).is_err());
    }

    #[test]
    fn same_file_sees_through_links_and_needs_no_file() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new();
        let real = temp.path().join("real");
        fs::create_dir(&real).unwrap();
        let linked = temp.path().join("linked");
        symlink(&real, &linked).unwrap();

        let file = real.join("a.json");
        assert!(same_file(&file, &linked.join("a.json")));
        assert!(same_file(&file, &real.join(".").join("a.json")));
        assert!(!same_file(&file, &real.join("b.json")));
        assert!(!same_file(&file, Path::new("/nonexistent/dir/a.json")));

        fs::write(&file, "{}").unwrap();
        let link = temp.path().join("link.json");
        symlink(&file, &link).unwrap();
        assert!(same_file(&file, &link));
        assert!(same_file(&link, &linked.join("a.json")));
        assert!(!same_file(&link, &real.join("b.json")));
    }

    #[test]
    fn atomic_write_replaces_through_a_symlink_and_leaves_no_temp() {
        let temp = TempDir::new();
        let real = temp.path().join("real.json");
        fs::write(&real, b"old").unwrap();
        let link = temp.path().join("config.json");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        atomic_write(&link, b"new").unwrap();

        assert_eq!(fs::read(&real).unwrap(), b"new");
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        let entries = fs::read_dir(temp.path()).unwrap().count();
        assert_eq!(entries, 2);
    }
}
