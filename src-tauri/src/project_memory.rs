use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use tauri::{path::BaseDirectory, AppHandle, Manager};

use crate::{
    activity, continuity, git,
    models::{
        ProjectKnowledgeEntry, ProjectKnowledgeKind, ProjectKnowledgeState, ProjectMemory,
        Workspace,
    },
    secret_guard,
};

const MEMORY_FILE: &str = "project-memory.json";
const MAX_CONTEXT_CHARS: usize = 12_000;
const MAX_ITEM_CHARS: usize = 1_000;
const MAX_ITEMS: usize = 40;
const MAX_KNOWLEDGE: usize = 64;
const MAX_KNOWLEDGE_TITLE_CHARS: usize = 120;
const MAX_KNOWLEDGE_GUIDANCE_CHARS: usize = 3_000;
const MAX_KNOWLEDGE_EVIDENCE_CHARS: usize = 512;
static MEMORY_MUTATION_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[derive(Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct MemoryStore {
    projects: BTreeMap<String, ProjectMemory>,
}

fn path(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .resolve(MEMORY_FILE, BaseDirectory::AppData)
        .map_err(|error| format!("Could not resolve project memory storage: {error}"))
}

fn backup_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or(MEMORY_FILE);
    path.with_file_name(format!(".{file_name}.previous"))
}

fn readable_store_path(path: &Path) -> Result<Option<PathBuf>, String> {
    if fs::symlink_metadata(path)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Err("Refusing to read project memory through a symbolic link.".to_string());
    }
    if path.exists() {
        return Ok(Some(path.to_path_buf()));
    }
    let backup = backup_path(path);
    if fs::symlink_metadata(&backup)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Err("Refusing to read project memory backup through a symbolic link.".to_string());
    }
    Ok(backup.exists().then_some(backup))
}

fn load(app: &AppHandle) -> Result<MemoryStore, String> {
    let path = path(app)?;
    let Some(read_path) = readable_store_path(&path)? else {
        return Ok(MemoryStore::default());
    };
    let text = fs::read_to_string(read_path)
        .map_err(|error| format!("Could not read project memory: {error}"))?;
    if text.trim().is_empty() {
        return Ok(MemoryStore::default());
    }
    serde_json::from_str(&text).map_err(|error| format!("Saved project memory is invalid: {error}"))
}

#[cfg(not(windows))]
fn install_staged_file(temporary: &Path, path: &Path) -> Result<(), String> {
    fs::rename(temporary, path).map_err(|error| format!("Could not save project memory: {error}"))
}

#[cfg(windows)]
fn install_staged_file(temporary: &Path, path: &Path) -> Result<(), String> {
    let backup = backup_path(path);
    if backup.exists() {
        fs::remove_file(&backup)
            .map_err(|error| format!("Could not clear old project memory backup: {error}"))?;
    }
    if path.exists() {
        fs::rename(path, &backup)
            .map_err(|error| format!("Could not stage existing project memory: {error}"))?;
    }
    if let Err(error) = fs::rename(temporary, path) {
        if backup.exists() {
            let _ = fs::rename(&backup, path);
        }
        return Err(format!("Could not save project memory: {error}"));
    }
    if backup.exists() {
        let _ = fs::remove_file(backup);
    }
    Ok(())
}

fn private_write(path: &Path, contents: &[u8]) -> Result<(), String> {
    if fs::symlink_metadata(path)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Err("Refusing to write project memory through a symbolic link.".to_string());
    }

    let parent = path
        .parent()
        .ok_or_else(|| "Could not resolve RepoTunnel data directory.".to_string())?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("Could not create project memory directory: {error}"))?;

    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or(MEMORY_FILE);
    let temporary = parent.join(format!(".{file_name}.{nonce:x}.tmp"));
    if fs::symlink_metadata(&temporary)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Err("Refusing to stage project memory through a symbolic link.".to_string());
    }

    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temporary)
        .map_err(|error| format!("Could not stage project memory: {error}"))?;
    if let Err(error) = file.write_all(contents).and_then(|_| file.sync_all()) {
        let _ = fs::remove_file(&temporary);
        return Err(format!("Could not stage project memory: {error}"));
    }
    drop(file);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(error) = fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600)) {
            let _ = fs::remove_file(&temporary);
            return Err(format!("Could not protect project memory: {error}"));
        }
    }

    if let Err(error) = install_staged_file(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    Ok(())
}

fn save(app: &AppHandle, store: &MemoryStore) -> Result<(), String> {
    let path = path(app)?;
    let text = serde_json::to_vec_pretty(store)
        .map_err(|error| format!("Could not serialize project memory: {error}"))?;
    private_write(&path, &text)
}

fn clip(value: String, limit: usize) -> String {
    let value = value.trim();
    if value.chars().count() <= limit {
        return value.to_string();
    }
    value.chars().take(limit).collect()
}

fn clean_items(items: Vec<String>) -> Vec<String> {
    items
        .into_iter()
        .map(|item| clip(item, MAX_ITEM_CHARS))
        .filter(|item| !item.is_empty())
        .take(MAX_ITEMS)
        .collect()
}

pub(crate) fn get(app: &AppHandle, workspace: &Workspace) -> Result<ProjectMemory, String> {
    Ok(load(app)?
        .projects
        .remove(&workspace.id)
        .unwrap_or_else(|| ProjectMemory {
            workspace_id: workspace.id.clone(),
            workspace_name: workspace.name.clone(),
            summary: String::new(),
            goals: Vec::new(),
            decisions: Vec::new(),
            preferences: Vec::new(),
            next_steps: Vec::new(),
            knowledge: Vec::new(),
            updated_at: 0,
            git_head_at_update: None,
            activity_updated_at: 0,
        }))
}

pub(crate) fn update(
    app: &AppHandle,
    workspace: &Workspace,
    summary: String,
    goals: Vec<String>,
    decisions: Vec<String>,
    preferences: Vec<String>,
    next_steps: Vec<String>,
) -> Result<ProjectMemory, String> {
    let _guard = MEMORY_MUTATION_LOCK
        .lock()
        .map_err(|_| "Project memory mutation is temporarily unavailable.")?;
    let mut store = load(app)?;
    let knowledge = store
        .projects
        .get(&workspace.id)
        .map(|existing| existing.knowledge.clone())
        .unwrap_or_default();
    let summary = clip(summary, MAX_CONTEXT_CHARS);
    let goals = clean_items(goals);
    let decisions = clean_items(decisions);
    let preferences = clean_items(preferences);
    let next_steps = clean_items(next_steps);
    let combined = format!(
        "{summary}\n{}\n{}\n{}\n{}",
        goals.join("\n"),
        decisions.join("\n"),
        preferences.join("\n"),
        next_steps.join("\n")
    );
    if let Some(kind) = secret_guard::detect_secret(combined.as_bytes()) {
        return Err(format!(
            "Project memory appears to contain {kind}. Remove credentials/secrets before saving memory that connected AIs can read."
        ));
    }
    let git_head_at_update = git::repository_status(workspace).head;
    let activity_updated_at = activity::timeline(app, Some(&workspace.id))
        .map(|timeline| continuity::latest_meaningful_activity_at(&timeline.groups))
        .unwrap_or(0);
    let memory = ProjectMemory {
        workspace_id: workspace.id.clone(),
        workspace_name: workspace.name.clone(),
        summary,
        goals,
        decisions,
        preferences,
        next_steps,
        knowledge,
        updated_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or(0),
        git_head_at_update,
        activity_updated_at,
    };
    store.projects.insert(workspace.id.clone(), memory.clone());
    save(app, &store)?;
    Ok(memory)
}

/// A reusable project fact belongs to the existing durable project memory.
/// No new database, framework or capability registry is created.
#[derive(Clone, Debug)]
pub(crate) struct ProjectKnowledgeDraft {
    pub(crate) key: String,
    pub(crate) kind: ProjectKnowledgeKind,
    pub(crate) state: ProjectKnowledgeState,
    pub(crate) title: String,
    pub(crate) guidance: String,
    pub(crate) evidence: Option<String>,
}

fn validate_knowledge_draft(draft: &ProjectKnowledgeDraft) -> Result<(), String> {
    if draft.key.is_empty()
        || draft.key.len() > 80
        || !draft.key.bytes().all(|c| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'-' | b'_' | b'.')
        })
    {
        return Err("Knowledge key must be 1..80 lowercase ASCII letters, digits, dots, dashes or underscores.".into());
    }
    let valid = |value: &str, max: usize| !value.trim().is_empty() && value.chars().count() <= max;
    if !valid(&draft.title, MAX_KNOWLEDGE_TITLE_CHARS)
        || !valid(&draft.guidance, MAX_KNOWLEDGE_GUIDANCE_CHARS)
    {
        return Err("Knowledge title must be 1..120 chars and guidance 1..3000 chars, with no silent truncation.".into());
    }
    if draft
        .evidence
        .as_deref()
        .is_some_and(|text| text.chars().count() > MAX_KNOWLEDGE_EVIDENCE_CHARS)
    {
        return Err("Knowledge evidence reference is limited to 512 characters.".into());
    }
    // Resource references may mention a previously configured session, never
    // contain the credential/session data itself. Do not weaken this guard.
    let combined = format!(
        "{}\n{}\n{}\n{}",
        draft.key,
        draft.title,
        draft.guidance,
        draft.evidence.as_deref().unwrap_or("")
    );
    if let Some(kind) = secret_guard::detect_secret(combined.as_bytes()) {
        return Err(format!(
            "Project knowledge contains {kind}; save only non-secret references and steps."
        ));
    }
    Ok(())
}

fn apply_knowledge(
    memory: &mut ProjectMemory,
    draft: ProjectKnowledgeDraft,
    now: u64,
) -> Result<bool, String> {
    validate_knowledge_draft(&draft)?;
    let evidence = draft
        .evidence
        .map(|x| x.trim().to_string())
        .filter(|x| !x.is_empty());
    let title = draft.title.trim().to_string();
    let guidance = draft.guidance.trim().to_string();
    if let Some(existing) = memory
        .knowledge
        .iter_mut()
        .find(|entry| entry.key == draft.key)
    {
        if existing.kind == draft.kind
            && existing.state == draft.state
            && existing.title == title
            && existing.guidance == guidance
            && existing.evidence == evidence
        {
            return Ok(false);
        }
        existing.kind = draft.kind;
        existing.state = draft.state;
        existing.title = title;
        existing.guidance = guidance;
        existing.evidence = evidence;
        existing.updated_at = now;
    } else {
        if memory.knowledge.len() >= MAX_KNOWLEDGE {
            return Err("Project knowledge is full (64 records); explicitly retire/consolidate an existing item instead of silently discarding it.".into());
        }
        memory.knowledge.push(ProjectKnowledgeEntry {
            key: draft.key,
            kind: draft.kind,
            state: draft.state,
            title,
            guidance,
            evidence,
            updated_at: now,
        });
    }
    Ok(true)
}

/// Upsert a durable, bounded reusable note in the EXISTING project-memory.json.
pub(crate) fn upsert_knowledge(
    app: &AppHandle,
    workspace: &Workspace,
    draft: ProjectKnowledgeDraft,
) -> Result<ProjectKnowledgeEntry, String> {
    validate_knowledge_draft(&draft)?;
    let _guard = MEMORY_MUTATION_LOCK
        .lock()
        .map_err(|_| "Project memory mutation is temporarily unavailable.")?;
    let mut store = load(app)?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0);
    let key = draft.key.clone();
    let memory = store
        .projects
        .entry(workspace.id.clone())
        .or_insert_with(|| ProjectMemory {
            workspace_id: workspace.id.clone(),
            workspace_name: workspace.name.clone(),
            summary: String::new(),
            goals: Vec::new(),
            decisions: Vec::new(),
            preferences: Vec::new(),
            next_steps: Vec::new(),
            knowledge: Vec::new(),
            updated_at: 0,
            git_head_at_update: None,
            activity_updated_at: 0,
        });
    let changed = apply_knowledge(memory, draft, now)?;
    let result = memory
        .knowledge
        .iter()
        .find(|entry| entry.key == key)
        .ok_or("Project knowledge entry missing after upsert.")?
        .clone();
    if changed {
        // Keep saved semantic intent intact; knowledge is recorded, not
        // a claim that code or runtime resources were reverified.
        // Knowledge has its own per-entry timestamp; changing a test note
        // must not make an older semantic summary appear freshly verified.
        save(app, &store)?;
    }
    Ok(result)
}

/// Multi-word, ranked retrieval from the EXISTING project-memory store.
/// This works when a new chat describes a previously used procedure in
/// different words; it does not invent or verify resources that are absent.
fn rank_knowledge(
    entries: &[ProjectKnowledgeEntry],
    query: Option<&str>,
    kind: Option<ProjectKnowledgeKind>,
    include_retired: bool,
    limit: usize,
) -> Vec<ProjectKnowledgeEntry> {
    let query = query
        .unwrap_or("")
        .chars()
        .take(240)
        .collect::<String>()
        .to_ascii_lowercase();
    let terms = query
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|term| term.len() >= 2)
        .take(12)
        .collect::<Vec<_>>();
    let mut results = entries
        .iter()
        .filter(|item| kind.is_none_or(|selected| selected == item.kind))
        .filter(|item| include_retired || item.state != ProjectKnowledgeState::Retired)
        .filter_map(|item| {
            let key = item.key.to_ascii_lowercase();
            let title = item.title.to_ascii_lowercase();
            let guidance = item.guidance.to_ascii_lowercase();
            let mut score = 0;
            for term in &terms {
                if key.contains(term) {
                    score += 6;
                }
                if title.contains(term) {
                    score += 4;
                }
                if guidance.contains(term) {
                    score += 1;
                }
            }
            if !terms.is_empty() && score == 0 {
                None
            } else {
                Some((score, item))
            }
        })
        .collect::<Vec<_>>();
    results.sort_by(|(score_a, a), (score_b, b)| {
        score_b
            .cmp(score_a)
            .then_with(|| b.updated_at.cmp(&a.updated_at))
            .then_with(|| a.key.cmp(&b.key))
    });
    results
        .into_iter()
        .take(limit.clamp(1, 15))
        .map(|(_, item)| item.clone())
        .collect()
}

/// Bounded, intent-based retrieval: avoid dumping the entire memory record
/// when a returning agent needs a specific test procedure or resource.
pub(crate) fn find_knowledge(
    app: &AppHandle,
    workspace: &Workspace,
    query: Option<&str>,
    kind: Option<ProjectKnowledgeKind>,
    include_retired: bool,
    limit: usize,
) -> Result<Vec<ProjectKnowledgeEntry>, String> {
    let memory = get(app, workspace)?;
    Ok(rank_knowledge(
        &memory.knowledge,
        query,
        kind,
        include_retired,
        limit,
    ))
}

pub(crate) fn forget(app: &AppHandle, workspace_id: &str) {
    let Ok(_guard) = MEMORY_MUTATION_LOCK.lock() else {
        return;
    };
    if let Ok(mut store) = load(app) {
        if store.projects.remove(workspace_id).is_some() {
            let _ = save(app, &store);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::{
        apply_knowledge, backup_path, private_write, rank_knowledge, readable_store_path,
        validate_knowledge_draft, ProjectKnowledgeDraft,
    };
    use crate::models::{ProjectKnowledgeKind, ProjectKnowledgeState, ProjectMemory};

    fn example_memory() -> ProjectMemory {
        ProjectMemory {
            workspace_id: "workspace-1".into(),
            workspace_name: "Example".into(),
            summary: "Preserve our original working project".into(),
            goals: vec!["Keep existing tests working".into()],
            decisions: Vec::new(),
            preferences: vec!["Do not ask for login again until checked".into()],
            next_steps: vec!["Read current Git status".into()],
            knowledge: Vec::new(),
            updated_at: 100,
            git_head_at_update: Some("abc".into()),
            activity_updated_at: 100,
        }
    }

    fn test_procedure() -> ProjectKnowledgeDraft {
        ProjectKnowledgeDraft {
            key: "integration-smoke-test".into(),
            kind: ProjectKnowledgeKind::TestProcedure,
            state: ProjectKnowledgeState::NeedsRecheck,
            title: "Re-use previously configured test session".into(),
            guidance: "Inspect the existing authorized test session and verify validity before requesting new login".into(),
            evidence: Some("Prior workspace test receipt reference".into()),
        }
    }

    #[test]
    fn legacy_project_memory_is_compatible_with_new_knowledge_field() {
        let old = serde_json::json!({
            "workspaceId":"workspace-old","workspaceName":"Original",
            "summary":"Old user instructions","goals":[],"decisions":[],
            "preferences":[],"nextSteps":[],"updatedAt":22
        });
        let parsed: ProjectMemory =
            serde_json::from_value(old).expect("read saved pre-knowledge memory");
        assert!(parsed.knowledge.is_empty());
        assert_eq!(parsed.summary, "Old user instructions");
    }

    #[test]
    fn adding_and_replaying_knowledge_preserves_existing_project_intent() {
        let mut memory = example_memory();
        let old_summary = memory.summary.clone();
        let old_next = memory.next_steps.clone();
        assert!(apply_knowledge(&mut memory, test_procedure(), 200).unwrap());
        assert_eq!(memory.knowledge.len(), 1);
        assert_eq!(memory.knowledge[0].updated_at, 200);
        assert!(!apply_knowledge(&mut memory, test_procedure(), 300).unwrap());
        assert_eq!(memory.knowledge.len(), 1);
        assert_eq!(memory.knowledge[0].updated_at, 200);
        assert_eq!(memory.summary, old_summary);
        assert_eq!(memory.next_steps, old_next);
        assert_eq!(memory.updated_at, 100);
        let roundtrip: ProjectMemory =
            serde_json::from_value(serde_json::to_value(memory).unwrap()).unwrap();
        assert_eq!(roundtrip.knowledge[0].key, "integration-smoke-test");
    }

    #[test]
    fn project_knowledge_input_validation_rejects_invalid_keys_or_truncation() {
        let mut candidate = test_procedure();
        candidate.key = "Different/Workspace".into();
        assert!(validate_knowledge_draft(&candidate).is_err());
        candidate = test_procedure();
        candidate.guidance = "x".repeat(3001);
        assert!(validate_knowledge_draft(&candidate).is_err());
        candidate = test_procedure();
        candidate.title = "   ".into();
        assert!(validate_knowledge_draft(&candidate).is_err());
    }

    #[test]
    fn intent_retrieval_matches_multiple_words_without_exact_phrase() {
        let mut memory = example_memory();
        apply_knowledge(&mut memory, test_procedure(), 100).unwrap();
        let found = rank_knowledge(
            &memory.knowledge,
            Some("test session login"),
            Some(ProjectKnowledgeKind::TestProcedure),
            false,
            10,
        );
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].key, "integration-smoke-test");
        let wrong_kind = rank_knowledge(
            &memory.knowledge,
            Some("session"),
            Some(ProjectKnowledgeKind::KnownPitfall),
            false,
            10,
        );
        assert!(wrong_kind.is_empty());
        let zero = rank_knowledge(
            &memory.knowledge,
            Some("completely unrelated topic"),
            None,
            false,
            10,
        );
        assert!(zero.is_empty());
        let mut retired = test_procedure();
        retired.key = "retired-test".into();
        retired.state = ProjectKnowledgeState::Retired;
        apply_knowledge(&mut memory, retired, 120).unwrap();
        assert_eq!(
            rank_knowledge(&memory.knowledge, Some("session"), None, false, 10).len(),
            1
        );
        assert_eq!(
            rank_knowledge(&memory.knowledge, Some("session"), None, true, 10).len(),
            2
        );
    }

    #[test]
    fn project_knowledge_stable_key_updates_without_adding_duplicate() {
        let mut memory = example_memory();
        apply_knowledge(&mut memory, test_procedure(), 100).unwrap();
        let mut updated = test_procedure();
        updated.guidance =
            "Check live authorized mount before using the stored test session".into();
        assert!(apply_knowledge(&mut memory, updated, 200).unwrap());
        assert_eq!(memory.knowledge.len(), 1);
        assert_eq!(memory.knowledge[0].updated_at, 200);
        assert!(memory.knowledge[0].guidance.contains("authorized mount"));
    }

    fn temp_file(label: &str) -> (PathBuf, PathBuf) {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "repotunnel-project-memory-{label}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&directory).expect("temp directory");
        let file = directory.join("project-memory.json");
        (directory, file)
    }

    #[test]
    fn private_write_replaces_memory_without_partial_destination() {
        let (directory, file) = temp_file("replace");
        fs::write(&file, b"old").expect("old memory");

        private_write(&file, b"new memory").expect("atomic memory save");

        assert_eq!(fs::read(&file).expect("new memory"), b"new memory");
        let leftovers = fs::read_dir(&directory)
            .expect("directory")
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .count();
        assert_eq!(leftovers, 0);
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn missing_primary_memory_recovers_from_previous_backup() {
        let (directory, file) = temp_file("backup");
        let backup = backup_path(&file);
        fs::write(&backup, b"backup memory").expect("backup memory");

        assert_eq!(
            readable_store_path(&file).expect("readable path"),
            Some(backup)
        );
        let _ = fs::remove_dir_all(directory);
    }
}
