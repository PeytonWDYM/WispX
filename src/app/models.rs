//! Choosing and downloading whisper.cpp models. A download goes to a private
//! partial file and is only ever linked into place, so it can never overwrite a
//! model that is already there.

use crate::process::{self, RunError, Spec, Toolbox};
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

const BASE_URL: &str = "https://huggingface.co/ggerganov/whisper.cpp/resolve/main";
/// A slow connection fetching the largest model listed here.
const DOWNLOAD_LIMIT: Duration = Duration::from_secs(6 * 60 * 60);
const PROGRESS_EVERY: Duration = Duration::from_millis(250);
/// Every ggml model starts with these bytes, and an error page does not.
const GGML_MAGIC: &[u8; 4] = b"lmgg";
const MIN_MODEL_BYTES: u64 = 1024 * 1024;

pub struct Choice {
    pub id: &'static str,
    pub file: &'static str,
    pub label: &'static str,
    /// Only for the progress bar: it is the size of the file when this was
    /// written, and nothing depends on it matching.
    pub bytes: u64,
}

pub const CATALOG: [Choice; 5] = [
    Choice {
        id: "base",
        file: "ggml-base.bin",
        label: "Base, all languages (148 MB)",
        bytes: 147_951_465,
    },
    Choice {
        id: "base.en",
        file: "ggml-base.en.bin",
        label: "Base, English only (148 MB)",
        bytes: 147_964_211,
    },
    Choice {
        id: "tiny.en",
        file: "ggml-tiny.en.bin",
        label: "Tiny, English only, fastest (78 MB)",
        bytes: 77_704_715,
    },
    Choice {
        id: "small",
        file: "ggml-small.bin",
        label: "Small, all languages, slower (488 MB)",
        bytes: 487_601_967,
    },
    Choice {
        id: "small.en",
        file: "ggml-small.en.bin",
        label: "Small, English only, slower (488 MB)",
        bytes: 487_614_201,
    },
];

pub fn choice(id: &str) -> Option<&'static Choice> {
    CATALOG.iter().find(|choice| choice.id == id)
}

/// Whether a model only understands English, so needs `language` set to match.
pub fn is_english_only(path: &Path) -> bool {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .is_some_and(|name| name.ends_with(".en.bin") || name.contains(".en-"))
}

/// The models in `dir`, for choosing between, in name order.
pub fn scan(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension().is_some_and(|ext| ext == "bin")
                && path.metadata().is_ok_and(|meta| meta.is_file())
        })
        .collect();
    found.sort();
    found
}

pub fn looks_like_a_model(path: &Path) -> bool {
    let Ok(meta) = path.metadata() else {
        return false;
    };
    let mut magic = [0u8; 4];
    meta.len() >= MIN_MODEL_BYTES
        && File::open(path)
            .and_then(|mut file| file.read_exact(&mut magic))
            .is_ok()
        && &magic == GGML_MAGIC
}

fn part_path(dir: &Path, file: &str) -> PathBuf {
    dir.join(format!(".{file}.part-{}", std::process::id()))
}

/// Fetches a model into `dir`, returning where it ended up. If that file exists
/// by the time this finishes, it is kept and the download thrown away.
pub fn download(
    choice: &Choice,
    dir: &Path,
    tools: &Toolbox,
    cancel: &AtomicBool,
    progress: impl Fn(u64) + Send + 'static,
) -> Result<PathBuf, String> {
    let destination = dir.join(choice.file);
    if destination.metadata().is_ok_and(|meta| meta.len() > 0) {
        return Ok(destination);
    }
    let curl = tools.find("curl").ok_or(
        "curl is needed to download a model. Install curl, or choose a model file you already \
         have",
    )?;
    fs::create_dir_all(dir).map_err(|error| format!("cannot create {}: {error}", dir.display()))?;

    let part = part_path(dir, choice.file);
    crate::paths::create_private_file(&part)
        .map_err(|error| format!("cannot create {}: {error}", part.display()))?;
    let result = fetch(choice, &curl, &part, cancel, progress)
        .and_then(|()| {
            if looks_like_a_model(&part) {
                Ok(())
            } else {
                Err("the download did not produce a whisper.cpp model".to_string())
            }
        })
        .and_then(|()| link_into_place(&part, &destination));
    let _ = fs::remove_file(&part);
    result.map(|()| destination)
}

fn link_into_place(part: &Path, destination: &Path) -> Result<(), String> {
    match fs::hard_link(part, destination) {
        Ok(()) => Ok(()),
        // Someone put a model there in the meantime, and it stays.
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(format!("cannot save {}: {error}", destination.display())),
    }
}

fn fetch(
    choice: &Choice,
    curl: &Path,
    part: &Path,
    cancel: &AtomicBool,
    progress: impl Fn(u64) + Send + 'static,
) -> Result<(), String> {
    let args: Vec<OsString> = vec![
        "--fail".into(),
        "--location".into(),
        "--silent".into(),
        "--show-error".into(),
        "--proto".into(),
        "=https".into(),
        "--output".into(),
        part.into(),
        format!("{BASE_URL}/{}", choice.file).into(),
    ];
    let (finished, done) = mpsc::channel::<()>();
    let watched = part.to_path_buf();
    let watcher = thread::spawn(move || {
        while matches!(
            done.recv_timeout(PROGRESS_EVERY),
            Err(mpsc::RecvTimeoutError::Timeout)
        ) {
            if let Ok(meta) = watched.metadata() {
                progress(meta.len());
            }
        }
    });
    let mut spec = Spec::new(curl, &args, DOWNLOAD_LIMIT);
    spec.cancel = Some(cancel);
    let outcome = process::run(spec);
    drop(finished);
    let _ = watcher.join();
    match outcome {
        Ok(output) if output.status.success() => Ok(()),
        Ok(output) => Err(format!(
            "the download failed: {}",
            process::last_error_line(&output.stderr).unwrap_or_else(|| output.status.to_string())
        )),
        Err(RunError::Cancelled) => Err("cancelled".to_string()),
        Err(error) => Err(format!("the download failed: {error}")),
    }
}

/// Whether a cancelled flag is set; lets the caller word its message.
pub fn was_cancelled(cancel: &AtomicBool) -> bool {
    cancel.load(Ordering::SeqCst)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;
    use std::os::unix::fs::PermissionsExt;

    fn fake_curl(temp: &TempDir, body: &str) -> Toolbox {
        let bin = temp.path().join("bin");
        fs::create_dir_all(&bin).unwrap();
        let script = bin.join("curl");
        fs::write(&script, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        Toolbox::with_path(bin.into_os_string())
    }

    /// Writes a plausible model to curl's `--output`, which comes second to last.
    const WRITES_A_MODEL: &str = r#"
while [ "$#" -gt 2 ]; do shift; done
{ printf lmgg; head -c 1100000 /dev/zero; } > "$1"
"#;

    #[test]
    fn a_download_lands_in_place_and_leaves_nothing_behind() {
        let temp = TempDir::new();
        let dir = temp.path().join("models");
        let tools = fake_curl(&temp, WRITES_A_MODEL);
        let cancel = AtomicBool::new(false);
        let path = download(choice("tiny.en").unwrap(), &dir, &tools, &cancel, |_| {}).unwrap();
        assert_eq!(path, dir.join("ggml-tiny.en.bin"));
        assert!(looks_like_a_model(&path));
        assert_eq!(scan(&dir), vec![path]);
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
    }

    #[test]
    fn an_existing_model_is_never_replaced_or_fetched_again() {
        let temp = TempDir::new();
        let dir = temp.path().join("models");
        fs::create_dir_all(&dir).unwrap();
        let existing = dir.join("ggml-base.bin");
        fs::write(&existing, b"mine").unwrap();
        let ran = temp.path().join("curl-ran");
        let tools = fake_curl(&temp, &format!("touch {}", ran.display()));
        let cancel = AtomicBool::new(false);
        let path = download(choice("base").unwrap(), &dir, &tools, &cancel, |_| {}).unwrap();
        assert_eq!(path, existing);
        assert_eq!(fs::read(&existing).unwrap(), b"mine");
        assert!(!ran.exists());
    }

    #[test]
    fn a_model_appearing_during_the_download_is_kept() {
        let temp = TempDir::new();
        let dir = temp.path().join("models");
        fs::create_dir_all(&dir).unwrap();
        let existing = dir.join("ggml-base.bin");
        let body = format!("echo mine > {}\n{WRITES_A_MODEL}", existing.display());
        let tools = fake_curl(&temp, &body);
        let cancel = AtomicBool::new(false);
        download(choice("base").unwrap(), &dir, &tools, &cancel, |_| {}).unwrap();
        assert_eq!(fs::read_to_string(&existing).unwrap().trim(), "mine");
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
    }

    #[test]
    fn a_failed_or_bogus_download_says_so_and_leaves_nothing() {
        let temp = TempDir::new();
        let dir = temp.path().join("models");
        let cancel = AtomicBool::new(false);
        let tools = fake_curl(&temp, "echo 'curl: (22) 404' >&2\nexit 22");
        let error = download(choice("base").unwrap(), &dir, &tools, &cancel, |_| {}).unwrap_err();
        assert!(error.contains("404"), "{error}");
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);

        let tools = fake_curl(
            &temp,
            "while [ \"$#\" -gt 2 ]; do shift; done\nhead -c 2000000 /dev/zero > \"$1\"",
        );
        let error = download(choice("base").unwrap(), &dir, &tools, &cancel, |_| {}).unwrap_err();
        assert!(error.contains("did not produce"), "{error}");
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
    }

    #[test]
    fn a_cancelled_download_stops_and_cleans_up() {
        let temp = TempDir::new();
        let dir = temp.path().join("models");
        let tools = fake_curl(&temp, "sleep 30");
        let cancel = AtomicBool::new(true);
        let error = download(choice("base").unwrap(), &dir, &tools, &cancel, |_| {}).unwrap_err();
        assert_eq!(error, "cancelled");
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
    }

    #[test]
    fn english_only_models_are_recognized_by_name() {
        assert!(is_english_only(Path::new("/m/ggml-tiny.en.bin")));
        assert!(is_english_only(Path::new("/m/ggml-small.en-q5_1.bin")));
        assert!(!is_english_only(Path::new("/m/ggml-base.bin")));
    }
}
