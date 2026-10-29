use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{de::DeserializeOwned, Serialize};

const MAX_RECOVERY_ERROR_CHARS: usize = 512;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum JsonRecoverySource {
    Missing,
    Primary,
    Previous,
}

#[derive(Debug)]
pub(crate) struct JsonRecovery<T> {
    pub(crate) value: Option<T>,
    pub(crate) source: JsonRecoverySource,
    pub(crate) primary_error: Option<String>,
}

fn bounded_error(value: impl AsRef<str>) -> String {
    value
        .as_ref()
        .chars()
        .take(MAX_RECOVERY_ERROR_CHARS)
        .collect()
}

fn previous_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("store.json");
    path.with_file_name(format!(".{file_name}.previous"))
}

fn ensure_regular_or_missing(path: &Path, label: &str, role: &str) -> Result<(), String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(format!("Could not inspect {label} {role}: {error}"));
        }
    };
    #[cfg(windows)]
    let redirected = {
        use std::os::windows::fs::MetadataExt;
        // Reparse point tags include junctions and aliases that Rust does not
        // always report as FileType::is_symlink().
        metadata.file_type().is_symlink() || metadata.file_attributes() & 0x0000_0400 != 0
    };
    #[cfg(not(windows))]
    let redirected = metadata.file_type().is_symlink();
    if redirected {
        return Err(format!(
            "Refusing to use {label} {role} through a symbolic link or reparse point."
        ));
    }
    if !metadata.file_type().is_file() {
        return Err(format!("Refusing to use non-regular {label} {role}."));
    }
    Ok(())
}

fn read_json_file<T: DeserializeOwned>(
    path: &Path,
    label: &str,
    role: &str,
) -> Result<Option<T>, String> {
    ensure_regular_or_missing(path, label, role)?;
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("Could not read {label} {role}: {error}")),
    };
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(None);
    }
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|error| format!("Saved {label} {role} is invalid: {error}"))
}

pub(crate) fn load_json<T: DeserializeOwned>(
    path: &Path,
    label: &str,
) -> Result<JsonRecovery<T>, String> {
    let previous = previous_path(path);
    ensure_regular_or_missing(path, label, "primary")?;
    ensure_regular_or_missing(&previous, label, "previous copy")?;

    match read_json_file(path, label, "primary") {
        Ok(Some(value)) => Ok(JsonRecovery {
            value: Some(value),
            source: JsonRecoverySource::Primary,
            primary_error: None,
        }),
        Ok(None) => match read_json_file(&previous, label, "previous copy")? {
            Some(value) => Ok(JsonRecovery {
                value: Some(value),
                source: JsonRecoverySource::Previous,
                primary_error: None,
            }),
            None => Ok(JsonRecovery {
                value: None,
                source: JsonRecoverySource::Missing,
                primary_error: None,
            }),
        },
        Err(primary_error) => {
            let bounded_primary = bounded_error(&primary_error);
            match read_json_file(&previous, label, "previous copy") {
                Ok(Some(value)) => Ok(JsonRecovery {
                    value: Some(value),
                    source: JsonRecoverySource::Previous,
                    primary_error: Some(bounded_primary),
                }),
                Ok(None) => Err(format!(
                    "{bounded_primary}; no valid previous {label} copy is available."
                )),
                Err(previous_error) => Err(format!(
                    "{bounded_primary}; previous {label} copy is also invalid: {}",
                    bounded_error(previous_error)
                )),
            }
        }
    }
}

fn unique_temporary_path(path: &Path, purpose: &str) -> Result<PathBuf, String> {
    let parent = path
        .parent()
        .ok_or_else(|| "Could not resolve recovery-store directory.".to_string())?;
    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("store.json");
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    Ok(parent.join(format!(
        ".{file_name}.{}-{nonce:x}-{purpose}.tmp",
        std::process::id()
    )))
}

fn write_private_staged(path: &Path, bytes: &[u8], label: &str) -> Result<(), String> {
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|error| format!("Could not stage {label}: {error}"))?;
    if let Err(error) = file.write_all(bytes).and_then(|_| file.sync_all()) {
        let _ = fs::remove_file(path);
        return Err(format!("Could not stage {label}: {error}"));
    }
    drop(file);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(error) = fs::set_permissions(path, fs::Permissions::from_mode(0o600)) {
            let _ = fs::remove_file(path);
            return Err(format!("Could not protect staged {label}: {error}"));
        }
    }
    Ok(())
}

#[cfg(unix)]
fn sync_parent_directory(path: &Path, label: &str) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("Could not resolve {label} directory."))?;
    std::fs::File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| format!("Could not sync {label} directory: {error}"))
}

#[cfg(not(unix))]
fn sync_parent_directory(_path: &Path, _label: &str) -> Result<(), String> {
    Ok(())
}

fn install_replacement(temporary: &Path, path: &Path, label: &str) -> Result<(), String> {
    #[cfg(not(windows))]
    {
        fs::rename(temporary, path)
            .map_err(|error| format!("Could not install {label}: {error}"))?;
        sync_parent_directory(path, label)
    }

    #[cfg(windows)]
    {
        let displaced = unique_temporary_path(path, "displaced")?;
        let had_primary = path.exists();
        if had_primary {
            fs::rename(path, &displaced)
                .map_err(|error| format!("Could not stage existing {label}: {error}"))?;
        }
        if let Err(error) = fs::rename(temporary, path) {
            if had_primary {
                let _ = fs::rename(&displaced, path);
            }
            return Err(format!("Could not install {label}: {error}"));
        }
        if had_primary {
            let _ = fs::remove_file(displaced);
        }
        sync_parent_directory(path, label)
    }
}

fn preserve_valid_primary<T: DeserializeOwned>(
    path: &Path,
    previous: &Path,
    label: &str,
) -> Result<(), String> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("Could not read current {label}: {error}")),
    };
    if bytes.iter().all(u8::is_ascii_whitespace) || serde_json::from_slice::<T>(&bytes).is_err() {
        return Ok(());
    }

    ensure_regular_or_missing(previous, label, "previous copy")?;
    let staged_previous = unique_temporary_path(previous, "previous")?;
    write_private_staged(&staged_previous, &bytes, label)?;
    if let Err(error) = install_replacement(&staged_previous, previous, label) {
        let _ = fs::remove_file(&staged_previous);
        return Err(error);
    }
    Ok(())
}

pub(crate) fn save_json<T>(path: &Path, value: &T, label: &str) -> Result<(), String>
where
    T: Serialize + DeserializeOwned,
{
    save_json_with_install(path, value, label, |temporary, primary, _previous| {
        install_replacement(temporary, primary, label)
    })
}

fn save_json_with_install<T, F>(
    path: &Path,
    value: &T,
    label: &str,
    install: F,
) -> Result<(), String>
where
    T: Serialize + DeserializeOwned,
    F: FnOnce(&Path, &Path, &Path) -> Result<(), String>,
{
    ensure_regular_or_missing(path, label, "primary")?;
    let previous = previous_path(path);
    ensure_regular_or_missing(&previous, label, "previous copy")?;

    let bytes = serde_json::to_vec_pretty(value)
        .map_err(|error| format!("Could not serialize {label}: {error}"))?;
    let parent = path
        .parent()
        .ok_or_else(|| format!("Could not resolve {label} directory."))?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("Could not create {label} directory: {error}"))?;

    let temporary = unique_temporary_path(path, "stage")?;
    write_private_staged(&temporary, &bytes, label)?;

    if let Err(error) = preserve_valid_primary::<T>(path, &previous, label) {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }

    if let Err(error) = install(&temporary, path, &previous) {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    sync_parent_directory(path, label)
}

#[cfg(test)]
mod tests {
    use super::{load_json, save_json, save_json_with_install, JsonRecoverySource};
    use serde_json::json;
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    static TEST_SEQUENCE: AtomicU64 = AtomicU64::new(1);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new(name: &str) -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0);
            let sequence = TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "repotunnel-recovery-store-{name}-{}-{nonce:x}-{sequence:x}",
                std::process::id()
            ));
            fs::create_dir_all(&path).expect("create recovery-store test directory");
            Self(path)
        }

        fn join(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn previous_path(path: &Path) -> PathBuf {
        let file_name = path
            .file_name()
            .and_then(|value| value.to_str())
            .expect("test filename");
        path.with_file_name(format!(".{file_name}.previous"))
    }

    #[test]
    fn atomic_save_round_trips_json_and_keeps_private_regular_files() {
        let directory = TestDirectory::new("round-trip");
        let path = directory.join("state.json");
        let value = json!({"status": "running", "count": 7});

        save_json(&path, &value, "test recovery store").expect("save JSON");
        let recovered =
            load_json::<serde_json::Value>(&path, "test recovery store").expect("load JSON");

        assert_eq!(recovered.source, JsonRecoverySource::Primary);
        assert_eq!(recovered.value, Some(value));
        assert!(recovered.primary_error.is_none());

        let metadata = fs::symlink_metadata(&path).expect("primary metadata");
        assert!(metadata.file_type().is_file());
        assert!(!metadata.file_type().is_symlink());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        }
    }

    #[test]
    fn corrupt_primary_recovers_previous_good_json() {
        let directory = TestDirectory::new("previous");
        let path = directory.join("state.json");
        let first = json!({"generation": 1});
        let second = json!({"generation": 2});

        save_json(&path, &first, "test recovery store").expect("first save");
        save_json(&path, &second, "test recovery store").expect("second save");
        fs::write(&path, b"{truncated").expect("corrupt primary");

        let recovered = load_json::<serde_json::Value>(&path, "test recovery store")
            .expect("recover previous JSON");
        assert_eq!(recovered.source, JsonRecoverySource::Previous);
        assert_eq!(recovered.value, Some(first));
        assert!(recovered.primary_error.is_some());
    }

    #[test]
    fn corrupt_primary_and_previous_fail_closed() {
        let directory = TestDirectory::new("double-corrupt");
        let path = directory.join("state.json");
        fs::write(&path, b"{bad-primary").expect("write bad primary");
        fs::write(previous_path(&path), b"{bad-previous").expect("write bad previous");

        let error = load_json::<serde_json::Value>(&path, "test recovery store")
            .expect_err("both corrupt copies must fail");
        assert!(error.contains("test recovery store"));
        assert!(error.contains("previous"));
    }

    #[test]
    fn failed_install_leaves_prior_primary_readable() {
        let directory = TestDirectory::new("failed-install");
        let path = directory.join("state.json");
        let original = json!({"generation": 1});
        let replacement = json!({"generation": 2});
        save_json(&path, &original, "test recovery store").expect("initial save");

        let error = save_json_with_install(
            &path,
            &replacement,
            "test recovery store",
            |_temporary, _primary, _previous| Err("injected install failure".to_string()),
        )
        .expect_err("injected install failure should escape");
        assert!(error.contains("injected install failure"));

        let recovered = load_json::<serde_json::Value>(&path, "test recovery store")
            .expect("original remains readable");
        assert_eq!(recovered.source, JsonRecoverySource::Primary);
        assert_eq!(recovered.value, Some(original));
    }

    #[cfg(windows)]
    #[test]
    fn windows_reparse_point_in_recovery_path_is_rejected() {
        let directory = TestDirectory::new("windows-reparse");
        let outside = directory.join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("secret.json"), b"{\"protected\":true}").unwrap();
        let junction = directory.join("redirect");
        let output = crate::windows_test_create_junction(&junction, &outside)
            .expect("create Windows reparse fixture");
        assert!(
            output.status.success(),
            "Windows junction fixture could not be created: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let denied = super::ensure_regular_or_missing(&junction, "test recovery store", "primary")
            .expect_err("Windows reparse directory must be recognized");
        assert!(denied.contains("reparse point"), "{denied}");
        assert_eq!(
            fs::read(outside.join("secret.json")).unwrap(),
            b"{\"protected\":true}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_primary_or_previous_is_rejected() {
        use std::os::unix::fs::symlink;

        let directory = TestDirectory::new("symlink");
        let outside = directory.join("outside.json");
        fs::write(&outside, b"{}").expect("outside file");

        let primary = directory.join("primary.json");
        symlink(&outside, &primary).expect("primary symlink");
        let primary_error = save_json(&primary, &json!({"safe": true}), "test recovery store")
            .expect_err("primary symlink must be rejected");
        assert!(primary_error.contains("symbolic link"));

        let path = directory.join("with-previous.json");
        save_json(&path, &json!({"generation": 1}), "test recovery store")
            .expect("initial primary");
        let previous = previous_path(&path);
        symlink(&outside, &previous).expect("previous symlink");
        let previous_error = save_json(&path, &json!({"generation": 2}), "test recovery store")
            .expect_err("previous symlink must be rejected");
        assert!(previous_error.contains("symbolic link"));
    }
}
