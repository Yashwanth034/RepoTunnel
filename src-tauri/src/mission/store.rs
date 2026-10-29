use std::{
    fs,
    path::{Path, PathBuf},
};

use tauri::{AppHandle, Manager};

use super::model::{MissionId, MissionSnapshot, MissionSummary};
use crate::recovery_store;

const MISSION_STORE_DIRECTORY: &str = "agent-missions";
const SNAPSHOT_FILE: &str = "snapshot.json";
const SNAPSHOT_LABEL: &str = "Agent Mission snapshot";

// Windows directory junctions and mount-point reparse records are not always
// reported as Rust symbolic links. Treat all filesystem redirections as
// untrusted Mission state, including before any recursive deletion.
pub(super) fn metadata_is_redirect(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_type().is_symlink() || metadata.file_attributes() & 0x0000_0400 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

const MAX_MISSION_CLEANUP_ENTRIES: usize = 100_000;
const MAX_MISSION_CLEANUP_DEPTH: usize = 64;

fn validate_mission_cleanup_tree(directory: &Path) -> Result<(), String> {
    // Do not remove a private Mission directory that contains links/junctions
    // to data outside the Mission. This is especially important on Windows:
    // a nested junction is a reparse point even when not a Rust symlink.
    let mut pending = vec![(directory.to_path_buf(), 0usize)];
    let mut scanned = 0usize;
    while let Some((path, depth)) = pending.pop() {
        scanned += 1;
        if scanned > MAX_MISSION_CLEANUP_ENTRIES || depth > MAX_MISSION_CLEANUP_DEPTH {
            return Err(
                "Mission private-record cleanup tree exceeds the safe bounded scan.".into(),
            );
        }
        let metadata = fs::symlink_metadata(&path)
            .map_err(|error| format!("Could not inspect Mission cleanup entry: {error}"))?;
        if metadata_is_redirect(&metadata) {
            return Err(
                "Refusing to delete Mission records containing a symbolic link or reparse point."
                    .into(),
            );
        }
        if metadata.is_dir() {
            for entry in fs::read_dir(&path)
                .map_err(|error| format!("Could not inspect Mission cleanup directory: {error}"))?
            {
                let entry = entry.map_err(|error| {
                    format!("Could not enumerate Mission cleanup directory: {error}")
                })?;
                pending.push((entry.path(), depth + 1));
            }
        } else if !metadata.is_file() {
            return Err("Refusing to delete a Mission tree containing a non-regular entry.".into());
        }
    }
    Ok(())
}

pub(super) struct MissionStore {
    root: PathBuf,
}

impl MissionStore {
    pub(super) fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub(super) fn from_app(app: &AppHandle) -> Result<Self, String> {
        app.path()
            .app_data_dir()
            .map(|directory| Self::new(directory.join(MISSION_STORE_DIRECTORY)))
            .map_err(|error| format!("Could not resolve RepoTunnel Agent Mission storage: {error}"))
    }

    pub(super) fn snapshot_path(&self, id: &MissionId) -> PathBuf {
        self.root.join(id.as_str()).join(SNAPSHOT_FILE)
    }

    fn mission_directory(&self, id: &MissionId) -> PathBuf {
        self.root.join(id.as_str())
    }

    pub(super) fn events_directory(&self, id: &MissionId) -> PathBuf {
        self.mission_directory(id).join("events")
    }

    pub(super) fn task_workspace_directory(
        &self,
        id: &MissionId,
        task_id: &str,
    ) -> Result<PathBuf, String> {
        let task_id = task_id.trim();
        if task_id.is_empty()
            || task_id.len() > 160
            || task_id.contains('/')
            || task_id.contains('\\')
            || task_id == "."
            || task_id == ".."
            || task_id.chars().any(char::is_control)
        {
            return Err("Agent Mission task-workspace task ID is invalid.".to_string());
        }
        Ok(self
            .mission_directory(id)
            .join("task-workspaces")
            .join(task_id))
    }

    fn ensure_directory_or_missing(path: &Path, label: &str) -> Result<bool, String> {
        match fs::symlink_metadata(path) {
            Ok(metadata) => {
                if metadata_is_redirect(&metadata) {
                    return Err(format!(
                        "Refusing to use {label} through a symbolic link or reparse point."
                    ));
                }
                if !metadata.file_type().is_dir() {
                    return Err(format!("Refusing to use non-directory {label}."));
                }
                Ok(true)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(format!("Could not inspect {label}: {error}")),
        }
    }

    fn ensure_root_for_write(&self) -> Result<(), String> {
        if !Self::ensure_directory_or_missing(&self.root, "Agent Mission storage")? {
            fs::create_dir_all(&self.root)
                .map_err(|error| format!("Could not create Agent Mission storage: {error}"))?;
            Self::ensure_directory_or_missing(&self.root, "Agent Mission storage")?;
        }
        Ok(())
    }

    fn ensure_mission_directory_for_write(&self, id: &MissionId) -> Result<(), String> {
        self.ensure_root_for_write()?;
        let directory = self.mission_directory(id);
        if !Self::ensure_directory_or_missing(&directory, "Agent Mission directory")? {
            fs::create_dir(&directory)
                .map_err(|error| format!("Could not create Agent Mission directory: {error}"))?;
            Self::ensure_directory_or_missing(&directory, "Agent Mission directory")?;
        }
        Ok(())
    }

    pub(super) fn save(&self, snapshot: &MissionSnapshot) -> Result<(), String> {
        snapshot.validate()?;
        self.ensure_mission_directory_for_write(snapshot.mission().id())?;

        if let Some(existing) = self.load(snapshot.mission().id())? {
            if existing.mission().id() != snapshot.mission().id() {
                return Err("Saved Mission identity does not match its storage path.".to_string());
            }
            if existing.mission().workspace_id() != snapshot.mission().workspace_id() {
                return Err(
                    "Refusing to rewrite an existing Mission workspace identity.".to_string(),
                );
            }
        }

        recovery_store::save_json(
            &self.snapshot_path(snapshot.mission().id()),
            snapshot,
            SNAPSHOT_LABEL,
        )
    }

    pub(super) fn load(&self, id: &MissionId) -> Result<Option<MissionSnapshot>, String> {
        if !Self::ensure_directory_or_missing(&self.root, "Agent Mission storage")? {
            return Ok(None);
        }
        let directory = self.mission_directory(id);
        if !Self::ensure_directory_or_missing(&directory, "Agent Mission directory")? {
            return Ok(None);
        }

        let recovered =
            recovery_store::load_json::<MissionSnapshot>(&self.snapshot_path(id), SNAPSHOT_LABEL)?;
        let Some(snapshot) = recovered.value else {
            return Ok(None);
        };
        snapshot.validate()?;
        if snapshot.mission().id() != id {
            return Err("Saved Mission identity does not match its storage path.".to_string());
        }
        Ok(Some(snapshot))
    }

    // Delete only RepoTunnel-owned Mission state. Never resolve or delete workspace source paths.
    // Rename first so a failed recursive cleanup cannot leave a live Mission behind.
    pub(super) fn delete(&self, id: &MissionId) -> Result<(), String> {
        if !Self::ensure_directory_or_missing(&self.root, "Agent Mission storage")? {
            return Err("Agent Mission was not found.".to_string());
        }
        let source = self.mission_directory(id);
        if !Self::ensure_directory_or_missing(&source, "Agent Mission directory")? {
            return Err("Agent Mission was not found.".to_string());
        }
        validate_mission_cleanup_tree(&source)?;
        let quarantined = self.root.join(format!(".deleted-{}", id.as_str()));
        if fs::symlink_metadata(&quarantined).is_ok() {
            return Err(
                "An old deleted-Mission cleanup requires attention before retrying.".to_string(),
            );
        }
        fs::rename(&source, &quarantined)
            .map_err(|error| format!("Could not detach Mission records for deletion: {error}"))?;
        validate_mission_cleanup_tree(&quarantined).map_err(|error| {
            format!("Mission records were detached, but safe cleanup needs attention: {error}")
        })?;
        fs::remove_dir_all(&quarantined).map_err(|error| {
            format!("Mission was detached but private-record cleanup requires attention: {error}")
        })
    }

    pub(super) fn list(&self, workspace_id: Option<&str>) -> Result<Vec<MissionSummary>, String> {
        if !Self::ensure_directory_or_missing(&self.root, "Agent Mission storage")? {
            return Ok(Vec::new());
        }

        let entries = fs::read_dir(&self.root)
            .map_err(|error| format!("Could not list Agent Mission storage: {error}"))?;
        let mut summaries = Vec::new();
        for entry in entries {
            let entry =
                entry.map_err(|error| format!("Could not read Agent Mission entry: {error}"))?;
            let metadata = fs::symlink_metadata(entry.path())
                .map_err(|error| format!("Could not inspect Agent Mission entry: {error}"))?;
            if metadata_is_redirect(&metadata) {
                return Err(
                    "Refusing to traverse a symbolic-link or reparse-point Agent Mission entry."
                        .to_string(),
                );
            }
            if !metadata.file_type().is_dir() {
                continue;
            }
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            // Reserved quarantine names are not active missions (including after a crash).
            if name.starts_with('.') {
                continue;
            }
            let id = MissionId::parse(name)?;
            let Some(snapshot) = self.load(&id)? else {
                continue;
            };
            if workspace_id.is_some_and(|expected| snapshot.mission().workspace_id() != expected) {
                continue;
            }
            summaries.push(snapshot.summary());
        }
        summaries.sort_by(|left, right| {
            right
                .updated_at
                .cmp(&left.updated_at)
                .then_with(|| left.id.as_str().cmp(right.id.as_str()))
        });
        Ok(summaries)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mission::model::{AgentMission, MissionId, MissionSnapshot};
    use std::{
        fs,
        path::PathBuf,
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
                "repotunnel-agent-mission-{name}-{}-{nonce:x}-{sequence:x}",
                std::process::id()
            ));
            fs::create_dir_all(&path).expect("create mission test directory");
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn deletion_removes_only_the_selected_mission_records() {
        let directory = TestDirectory::new("safe-deletion");
        let store = MissionStore::new(directory.0.clone());
        let id = MissionId::parse("mission-to-delete").unwrap();
        let snapshot = MissionSnapshot::new(
            AgentMission::new(
                id.clone(),
                "workspace-one".into(),
                "Delete private records only".into(),
                1_000,
            )
            .unwrap(),
        );
        store.save(&snapshot).unwrap();
        let unrelated = directory.0.join("human-project-source");
        fs::create_dir(&unrelated).unwrap();
        fs::write(unrelated.join("source.txt"), "do not delete").unwrap();
        store.delete(&id).unwrap();
        assert!(store.load(&id).unwrap().is_none());
        assert_eq!(
            fs::read_to_string(unrelated.join("source.txt")).unwrap(),
            "do not delete"
        );
        assert!(!store.snapshot_path(&id).exists());
    }

    #[test]
    fn existing_mission_workspace_identity_cannot_be_rewritten() {
        let directory = TestDirectory::new("immutable-identity");
        let store = MissionStore::new(directory.0.clone());
        let id = MissionId::parse("mission-immutable").expect("valid mission id");
        let first = MissionSnapshot::new(
            AgentMission::new(
                id.clone(),
                "workspace-original".to_string(),
                "Protect identity".to_string(),
                1_700_000_000_000,
            )
            .expect("create first mission"),
        );
        store.save(&first).expect("save first mission");

        let rewritten = MissionSnapshot::new(
            AgentMission::new(
                id,
                "workspace-other".to_string(),
                "Protect identity".to_string(),
                1_700_000_000_001,
            )
            .expect("create rewritten mission"),
        );
        let error = store
            .save(&rewritten)
            .expect_err("workspace identity rewrite must fail closed");
        assert!(error.contains("workspace identity"));
    }

    #[test]
    fn corrupt_primary_recovers_previous_snapshot_without_widening_permissions() {
        let directory = TestDirectory::new("previous-copy");
        let store = MissionStore::new(directory.0.clone());
        let mission = AgentMission::new(
            MissionId::parse("mission-recovery").expect("valid mission id"),
            "workspace-recovery".to_string(),
            "Keep publication human gated".to_string(),
            1_700_000_000_000,
        )
        .expect("create mission");
        let snapshot = MissionSnapshot::new(mission);

        store.save(&snapshot).expect("first snapshot save");
        store.save(&snapshot).expect("second snapshot save");
        fs::write(store.snapshot_path(snapshot.mission().id()), b"{truncated")
            .expect("corrupt primary snapshot");

        let recovered = store
            .load(snapshot.mission().id())
            .expect("recover previous snapshot")
            .expect("snapshot exists");

        assert_eq!(recovered.mission().id(), snapshot.mission().id());
        assert_eq!(recovered.mission().workspace_id(), "workspace-recovery");
        assert_eq!(recovered.mission().authority_revision(), 1);
        let gates = recovered.mission().human_action_policy();
        assert!(gates.git_stage);
        assert!(gates.git_commit);
        assert!(gates.git_push);
        assert!(gates.deploy_release);
        assert!(gates.external_integration);
        assert!(gates.irreversible_external);
    }

    #[cfg(unix)]
    #[test]
    fn nested_mission_symlink_blocks_recursive_deletion_without_detaching_mission() {
        use std::os::unix::fs::symlink;
        let directory = TestDirectory::new("nested-deletion-link");
        let store = MissionStore::new(directory.0.join("records"));
        let id = MissionId::parse("mission-nested-deletion").unwrap();
        let snapshot = MissionSnapshot::new(
            AgentMission::new(
                id.clone(),
                "workspace".into(),
                "Protect owned records".into(),
                100,
            )
            .unwrap(),
        );
        store.save(&snapshot).unwrap();
        let external = directory.0.join("external");
        fs::create_dir_all(&external).unwrap();
        fs::write(external.join("important.txt"), b"protected").unwrap();
        let task_dirs = store.mission_directory(&id).join("task-workspaces");
        fs::create_dir_all(&task_dirs).unwrap();
        symlink(&external, task_dirs.join("redirect")).unwrap();

        let error = store
            .delete(&id)
            .expect_err("nested link must block cleanup");
        assert!(error.contains("symbolic link"), "{error}");
        assert!(
            store.load(&id).unwrap().is_some(),
            "failed preflight must retain mission"
        );
        assert_eq!(
            fs::read(external.join("important.txt")).unwrap(),
            b"protected"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_nested_junction_blocks_recursive_mission_cleanup() {
        let directory = TestDirectory::new("windows-nested-junction");
        let store = MissionStore::new(directory.0.join("store"));
        let id = MissionId::parse("mission-junction-child").unwrap();
        let snapshot = MissionSnapshot::new(
            AgentMission::new(
                id.clone(),
                "workspace".into(),
                "Retain nested records".into(),
                100,
            )
            .unwrap(),
        );
        store.save(&snapshot).unwrap();
        let external = directory.0.join("important");
        fs::create_dir_all(&external).unwrap();
        fs::write(external.join("secret.txt"), b"never touch").unwrap();
        let task_dir = store.mission_directory(&id).join("task-workspaces");
        fs::create_dir_all(&task_dir).unwrap();
        let junction = task_dir.join("unexpected-link");
        let cmd = crate::windows_test_junction_command(&junction, &external);
        let result = std::process::Command::new("cmd.exe")
            .args(["/D", "/C", &cmd])
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "native junction fixture failed: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        let error = store
            .delete(&id)
            .expect_err("nested junction must block deletion");
        assert!(error.contains("reparse point"), "{error}");
        assert!(store.load(&id).unwrap().is_some());
        assert_eq!(
            fs::read(external.join("secret.txt")).unwrap(),
            b"never touch"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_mission_junction_cannot_redirect_snapshot_read_or_deletion() {
        let directory = TestDirectory::new("windows-junction");
        let store = MissionStore::new(directory.0.join("store"));
        fs::create_dir_all(&store.root).unwrap();
        let external = directory.0.join("outside-records");
        fs::create_dir_all(&external).unwrap();
        fs::write(external.join("important.txt"), b"never delete").unwrap();

        let id = MissionId::parse("mission-junction").unwrap();
        let junction = store.mission_directory(&id);
        let cmd = crate::windows_test_junction_command(&junction, &external);
        let output = std::process::Command::new("cmd.exe")
            .args(["/D", "/C", &cmd])
            .output()
            .expect("start junction fixture");
        assert!(
            output.status.success(),
            "Could not establish native Windows junction fixture: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let denied = store.load(&id).expect_err("junction must not be read");
        assert!(denied.contains("reparse point"), "{denied}");
        let denied = store.delete(&id).expect_err("junction must not be deleted");
        assert!(denied.contains("reparse point"), "{denied}");
        assert_eq!(
            fs::read(external.join("important.txt")).unwrap(),
            b"never delete"
        );
    }

    #[cfg(unix)]
    #[test]
    fn mission_directory_redirect_is_rejected_before_load_or_delete() {
        use std::os::unix::fs::symlink;
        let directory = TestDirectory::new("redirected-mission-root");
        let store = MissionStore::new(directory.0.join("store"));
        fs::create_dir_all(&store.root).unwrap();
        let external = directory.0.join("outside-private-files");
        fs::create_dir_all(&external).unwrap();
        fs::write(external.join("personal.txt"), b"do not traverse").unwrap();
        let id = MissionId::parse("mission-redirected").unwrap();
        symlink(&external, store.mission_directory(&id)).unwrap();

        let error = store
            .load(&id)
            .expect_err("Mission loading must reject redirected root");
        assert!(error.contains("symbolic link"), "{error}");
        let error = store
            .delete(&id)
            .expect_err("Mission deletion must reject redirected root");
        assert!(error.contains("symbolic link"), "{error}");
        assert_eq!(
            fs::read(external.join("personal.txt")).unwrap(),
            b"do not traverse"
        );
    }

    #[cfg(unix)]
    #[test]
    fn mission_snapshot_symlink_is_rejected() {
        use std::os::unix::fs::symlink;

        let directory = TestDirectory::new("symlink");
        let store = MissionStore::new(directory.0.clone());
        let id = MissionId::parse("mission-symlink").expect("valid mission id");
        let path = store.snapshot_path(&id);
        fs::create_dir_all(path.parent().expect("mission directory")).expect("create mission dir");
        let outside = directory.0.join("outside.json");
        fs::write(&outside, b"{}").expect("outside file");
        symlink(&outside, &path).expect("snapshot symlink");

        let error = store
            .load(&id)
            .expect_err("snapshot symlink must fail closed");
        assert!(error.contains("symbolic link"));
    }
}
