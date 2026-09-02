use std::{
    collections::{BTreeSet, HashSet},
    fs::{self, File},
    io::Read,
    path::Path,
};

use serde::{Deserialize, Serialize};

use crate::{
    access::{resolve_workspace_path, AccessOperation},
    models::{ProjectEntry, ProjectSnapshot, Workspace},
    project_index, project_setup,
};

const SNAPSHOT_LIMIT: usize = 1_200;
const MAX_RETRIEVAL_FILES: usize = 160;
const MAX_TOTAL_RETRIEVAL_BYTES: usize = 1_500_000;
const MAX_RETRIEVAL_FILE_BYTES: usize = 96_000;
const MAX_SNIPPETS: usize = 6;
const MAX_SNIPPET_CHARS: usize = 4_500;
const MAX_ATTACHMENTS: usize = 4;
const MAX_ATTACHMENT_BYTES: u64 = 512 * 1024;
const MAX_ATTACHMENT_CHARS: usize = 48_000;
const MAX_CURRENT_FILE_CHARS: usize = 36_000;
const MAX_SELECTION_CHARS: usize = 20_000;
const MAX_ERROR_CHARS: usize = 6_000;
const MAX_ERRORS: usize = 8;
const MAX_HISTORY_MESSAGES: usize = 12;
const MAX_HISTORY_MESSAGE_CHARS: usize = 8_000;
const DEFAULT_CONTEXT_CHARS: usize = 48_000;
const MIN_CONTEXT_CHARS: usize = 24_000;
const MAX_CONTEXT_CHARS: usize = 120_000;

const STOP_WORDS: &[&str] = &[
    "about", "after", "again", "also", "and", "are", "because", "before", "build", "can", "code",
    "does", "explain", "feature", "file", "files", "find", "for", "from", "have", "how", "into",
    "is", "it", "make", "project", "should", "that", "the", "this", "to", "what", "where", "which",
    "why", "with", "would", "you", "your",
];

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ContextTextInput {
    pub(crate) path: String,
    pub(crate) content: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ContextErrorInput {
    pub(crate) path: Option<String>,
    pub(crate) line: Option<usize>,
    pub(crate) column: Option<usize>,
    pub(crate) message: String,
    pub(crate) source: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ContextHistoryInput {
    pub(crate) role: String,
    pub(crate) content: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", default)]
pub(crate) struct ProjectContextRequest {
    pub(crate) include_project: Option<bool>,
    pub(crate) attachments: Vec<String>,
    pub(crate) current_file: Option<ContextTextInput>,
    pub(crate) selection: Option<ContextTextInput>,
    pub(crate) error: Option<ContextErrorInput>,
    #[serde(default)]
    pub(crate) errors: Vec<ContextErrorInput>,
    pub(crate) history: Vec<ContextHistoryInput>,
    pub(crate) context_window: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ContextSource {
    pub(crate) kind: String,
    pub(crate) path: Option<String>,
    pub(crate) label: String,
    pub(crate) line_start: Option<usize>,
    pub(crate) line_end: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BuiltProjectContext {
    pub(crate) text: String,
    pub(crate) history: Vec<ContextHistoryInput>,
    pub(crate) sources: Vec<ContextSource>,
    pub(crate) warnings: Vec<String>,
    pub(crate) reduced: bool,
    pub(crate) budget_chars: usize,
}

#[derive(Clone, Debug)]
struct ContextChunk {
    priority: u8,
    label: String,
    text: String,
    source: Option<ContextSource>,
    preserve_prefix: bool,
}

#[derive(Clone, Debug)]
struct RetrievalHit {
    score: i64,
    path: String,
    line_start: usize,
    line_end: usize,
    snippet: String,
}

fn truncate_chars(value: &str, limit: usize) -> (String, bool) {
    if value.chars().count() <= limit {
        return (value.to_string(), false);
    }
    let mut result = value
        .chars()
        .take(limit.saturating_sub(1))
        .collect::<String>();
    result.push('…');
    (result, true)
}

fn context_budget(context_window: Option<u64>, question_chars: usize) -> usize {
    let base = context_window
        .and_then(|tokens| usize::try_from(tokens).ok())
        .map(|tokens| tokens.saturating_mul(3).saturating_mul(55) / 100)
        .unwrap_or(DEFAULT_CONTEXT_CHARS)
        .clamp(MIN_CONTEXT_CHARS, MAX_CONTEXT_CHARS);
    base.saturating_sub(question_chars.min(8_000)).max(16_000)
}

fn query_terms(question: &str) -> Vec<String> {
    let stop = STOP_WORDS.iter().copied().collect::<HashSet<_>>();
    let mut terms = BTreeSet::new();
    let mut token = String::new();
    for ch in question.chars() {
        if ch.is_alphanumeric() || ch == '_' || ch == '-' {
            token.push(ch.to_ascii_lowercase());
        } else if !token.is_empty() {
            if token.len() >= 3 && !stop.contains(token.as_str()) {
                terms.insert(token.clone());
            }
            token.clear();
        }
    }
    if token.len() >= 3 && !stop.contains(token.as_str()) {
        terms.insert(token);
    }
    terms.into_iter().take(12).collect()
}

fn source_extension_bonus(path: &str) -> i64 {
    match Path::new(path)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "rs" | "ts" | "tsx" | "js" | "jsx" | "py" | "go" | "java" | "cs" | "cpp" | "c" | "h" => 8,
        "toml" | "json" | "yaml" | "yml" | "md" => 4,
        _ => 0,
    }
}

fn path_score(path: &str, terms: &[String]) -> i64 {
    let lower = path.to_ascii_lowercase();
    let name = lower.rsplit('/').next().unwrap_or(&lower);
    let mut score = source_extension_bonus(path);
    for term in terms {
        if name.contains(term) {
            score += 30;
        } else if lower.contains(term) {
            score += 16;
        }
    }
    score
}

fn content_score(content_lower: &str, terms: &[String]) -> i64 {
    let mut score = 0i64;
    for term in terms {
        let occurrences = content_lower.match_indices(term).take(8).count() as i64;
        score += occurrences * 6;
    }
    score
}

fn safe_input_path(
    workspace: &Workspace,
    relative_path: &str,
) -> Result<std::path::PathBuf, String> {
    let path = resolve_workspace_path(workspace, relative_path, AccessOperation::Read, true)?;
    let metadata = fs::symlink_metadata(&path)
        .map_err(|error| format!("Could not inspect project context file: {error}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err("Only regular approved project files can be used as AI context.".to_string());
    }
    let root = Path::new(&workspace.path)
        .canonicalize()
        .map_err(|error| format!("Could not resolve approved project context: {error}"))?;
    let parent = path.parent().unwrap_or(&root);
    if !project_index::should_include_entry(workspace, parent, &path, false)? {
        return Err(
            "That file is ignored or excluded from RepoTunnel project context.".to_string(),
        );
    }
    Ok(path)
}

fn read_attachment(workspace: &Workspace, relative_path: &str) -> Result<String, String> {
    let path = safe_input_path(workspace, relative_path)?;
    let metadata = fs::metadata(&path)
        .map_err(|error| format!("Could not inspect attached project file: {error}"))?;
    if metadata.len() > MAX_ATTACHMENT_BYTES {
        return Err(format!(
            "{} is larger than the {} KB Home context attachment limit.",
            relative_path,
            MAX_ATTACHMENT_BYTES / 1024
        ));
    }
    if project_index::is_probably_binary(&path, metadata.len())? {
        return Err(format!(
            "{relative_path} is binary and was not added to model context."
        ));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    File::open(&path)
        .map_err(|error| format!("Could not open attached project file: {error}"))?
        .take(MAX_ATTACHMENT_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("Could not read attached project file: {error}"))?;
    if bytes.len() as u64 > MAX_ATTACHMENT_BYTES {
        return Err(format!(
            "{relative_path} exceeded the Home context attachment limit."
        ));
    }
    String::from_utf8(bytes).map_err(|_| format!("{relative_path} is not UTF-8 text."))
}

fn validate_supplied_text(
    workspace: &Workspace,
    supplied: &ContextTextInput,
    max_chars: usize,
) -> Result<(String, bool), String> {
    let path = safe_input_path(workspace, &supplied.path)?;
    let metadata = fs::metadata(path)
        .map_err(|error| format!("Could not inspect current editor context: {error}"))?;
    if project_index::is_probably_binary(
        &resolve_workspace_path(workspace, &supplied.path, AccessOperation::Read, true)?,
        metadata.len(),
    )? {
        return Err("Binary editor content cannot be added to Home AI context.".to_string());
    }
    Ok(truncate_chars(&supplied.content, max_chars))
}

fn read_retrieval_file(path: &Path, byte_limit: usize) -> Result<String, String> {
    let metadata = fs::metadata(path)
        .map_err(|error| format!("Could not inspect a project retrieval candidate: {error}"))?;
    if !metadata.is_file() || metadata.len() == 0 {
        return Ok(String::new());
    }
    let limit = byte_limit.min(MAX_RETRIEVAL_FILE_BYTES);
    let mut bytes = Vec::with_capacity((metadata.len() as usize).min(limit));
    File::open(path)
        .map_err(|error| format!("Could not open a project retrieval candidate: {error}"))?
        .take(limit as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("Could not read a project retrieval candidate: {error}"))?;
    if bytes.contains(&0) {
        return Ok(String::new());
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn snippet_for_content(content: &str, terms: &[String]) -> (usize, usize, String) {
    let lines = content.lines().collect::<Vec<_>>();
    if lines.is_empty() {
        return (1, 1, String::new());
    }
    let mut best_line = 0usize;
    let mut best_score = -1i64;
    for (index, line) in lines.iter().enumerate() {
        let lower = line.to_ascii_lowercase();
        let score = terms
            .iter()
            .map(|term| lower.match_indices(term).count() as i64)
            .sum::<i64>();
        if score > best_score {
            best_score = score;
            best_line = index;
        }
    }
    let start = best_line.saturating_sub(6);
    let end = (best_line + 8).min(lines.len());
    let numbered = lines[start..end]
        .iter()
        .enumerate()
        .map(|(offset, line)| format!("{:>5} | {}", start + offset + 1, line))
        .collect::<Vec<_>>()
        .join("\n");
    let (snippet, _) = truncate_chars(&numbered, MAX_SNIPPET_CHARS);
    (start + 1, end, snippet)
}

fn retrieve(
    workspace: &Workspace,
    snapshot: &ProjectSnapshot,
    question: &str,
    excluded_paths: &HashSet<String>,
) -> Result<Vec<RetrievalHit>, String> {
    let terms = query_terms(question);
    if terms.is_empty() {
        return Ok(Vec::new());
    }
    let root = Path::new(&workspace.path)
        .canonicalize()
        .map_err(|error| format!("Could not resolve approved project for retrieval: {error}"))?;
    let mut candidates = snapshot
        .entries
        .iter()
        .filter(|entry| entry.kind == "file" && !entry.binary && !entry.large)
        .filter(|entry| !excluded_paths.contains(&entry.path))
        .map(|entry| (path_score(&entry.path, &terms), entry))
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| {
        right
            .0
            .cmp(&left.0)
            .then_with(|| left.1.path.cmp(&right.1.path))
    });

    let mut total_bytes = 0usize;
    let mut hits = Vec::new();
    for (base_score, entry) in candidates.into_iter().take(MAX_RETRIEVAL_FILES) {
        if total_bytes >= MAX_TOTAL_RETRIEVAL_BYTES {
            break;
        }
        let path = root.join(&entry.path);
        let remaining = MAX_TOTAL_RETRIEVAL_BYTES - total_bytes;
        let content = read_retrieval_file(&path, remaining)?;
        total_bytes = total_bytes.saturating_add(content.len());
        if content.is_empty() {
            continue;
        }
        let lower = content.to_ascii_lowercase();
        let score = base_score + content_score(&lower, &terms);
        if score <= source_extension_bonus(&entry.path) {
            continue;
        }
        let (line_start, line_end, snippet) = snippet_for_content(&content, &terms);
        hits.push(RetrievalHit {
            score,
            path: entry.path.clone(),
            line_start,
            line_end,
            snippet,
        });
    }
    hits.sort_by(|left, right| {
        right
            .score
            .cmp(&left.score)
            .then_with(|| left.path.cmp(&right.path))
    });
    hits.truncate(MAX_SNIPPETS);
    Ok(hits)
}

fn project_tree_text(snapshot: &ProjectSnapshot) -> String {
    let mut entries = snapshot
        .entries
        .iter()
        .filter(|entry| entry.kind == "directory" || entry.kind == "file")
        .take(140)
        .map(|entry| {
            if entry.kind == "directory" {
                format!("{}/", entry.path)
            } else {
                entry.path.clone()
            }
        })
        .collect::<Vec<_>>();
    if snapshot.overview.truncated || snapshot.entries.len() > entries.len() {
        entries.push("… project tree abbreviated by RepoTunnel".to_string());
    }
    entries.join("\n")
}

fn setup_text(workspace: &Workspace, snapshot: &ProjectSnapshot) -> String {
    let mut lines = vec![format!("Project: {}", workspace.name)];
    if let Ok(setup) = project_setup::detect(workspace) {
        lines.push(format!("Project kind: {}", setup.project_kind));
        lines.push(format!("Framework: {}", setup.framework));
        if let Some(manager) = setup.package_manager {
            lines.push(format!("Package manager: {manager}"));
        }
    }
    if !snapshot.overview.languages.is_empty() {
        lines.push(format!(
            "Languages: {}",
            snapshot
                .overview
                .languages
                .iter()
                .take(8)
                .map(|language| format!("{} ({})", language.name, language.files))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !snapshot.overview.manifests.is_empty() {
        lines.push(format!(
            "Manifests: {}",
            snapshot
                .overview
                .manifests
                .iter()
                .take(12)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    lines.join("\n")
}

fn add_chunk(
    output: &mut String,
    sources: &mut Vec<ContextSource>,
    chunk: ContextChunk,
    remaining: &mut usize,
    reduced: &mut bool,
) {
    if *remaining == 0 {
        *reduced = true;
        return;
    }
    let header = format!("\n\n## {}\n", chunk.label);
    if header.len() >= *remaining {
        *reduced = true;
        return;
    }
    let available = remaining.saturating_sub(header.len());
    let char_count = chunk.text.chars().count();
    let text = if char_count <= available {
        chunk.text
    } else {
        *reduced = true;
        let keep = available.saturating_sub(32);
        if keep < 96 {
            return;
        }
        if chunk.preserve_prefix {
            format!(
                "{}\n… context truncated by RepoTunnel",
                chunk.text.chars().take(keep).collect::<String>()
            )
        } else {
            chunk.text.chars().take(keep).collect::<String>()
        }
    };
    output.push_str(&header);
    output.push_str(&text);
    *remaining = remaining.saturating_sub(header.len() + text.len());
    if let Some(source) = chunk.source {
        sources.push(source);
    }
}

fn history_for_budget(
    history: &[ContextHistoryInput],
    remaining: &mut usize,
    reduced: &mut bool,
) -> Vec<ContextHistoryInput> {
    let mut result = Vec::new();
    for message in history.iter().rev().take(MAX_HISTORY_MESSAGES).rev() {
        if !matches!(message.role.as_str(), "user" | "assistant") {
            continue;
        }
        let (content, was_truncated) = truncate_chars(&message.content, MAX_HISTORY_MESSAGE_CHARS);
        let cost = content.len().saturating_add(64);
        if cost > *remaining {
            *reduced = true;
            continue;
        }
        *remaining -= cost;
        *reduced |= was_truncated;
        result.push(ContextHistoryInput {
            role: message.role.clone(),
            content,
        });
    }
    result
}

pub(crate) fn build_general(
    question: &str,
    request: &ProjectContextRequest,
) -> BuiltProjectContext {
    let budget = context_budget(request.context_window, question.chars().count());
    let mut remaining = budget;
    let mut reduced = false;
    let history = history_for_budget(&request.history, &mut remaining, &mut reduced);
    let has_project_only_context = request.include_project.unwrap_or(false)
        || !request.attachments.is_empty()
        || request.current_file.is_some()
        || request.selection.is_some()
        || request.error.is_some()
        || !request.errors.is_empty();
    let warnings = if has_project_only_context {
        vec!["No project is selected, so project-specific context was not included.".to_string()]
    } else {
        Vec::new()
    };

    BuiltProjectContext {
        text: String::new(),
        history,
        sources: Vec::new(),
        warnings,
        reduced,
        budget_chars: budget,
    }
}

pub(crate) fn build(
    workspace: &Workspace,
    question: &str,
    request: &ProjectContextRequest,
) -> Result<BuiltProjectContext, String> {
    let budget = context_budget(request.context_window, question.chars().count());
    let snapshot = project_index::project_snapshot(workspace, SNAPSHOT_LIMIT)?;
    let mut chunks = Vec::new();
    let mut warnings = Vec::new();
    let mut excluded_paths = HashSet::new();
    let mut reduced = snapshot.overview.truncated;

    for path in request.attachments.iter().take(MAX_ATTACHMENTS) {
        match read_attachment(workspace, path) {
            Ok(content) => {
                let (content, truncated) = truncate_chars(&content, MAX_ATTACHMENT_CHARS);
                reduced |= truncated;
                excluded_paths.insert(path.clone());
                chunks.push(ContextChunk {
                    priority: 100,
                    label: format!("Explicit project file: {path}"),
                    text: content,
                    source: Some(ContextSource {
                        kind: "file".to_string(),
                        path: Some(path.clone()),
                        label: path.clone(),
                        line_start: None,
                        line_end: None,
                    }),
                    preserve_prefix: true,
                });
            }
            Err(error) => warnings.push(error),
        }
    }
    if request.attachments.len() > MAX_ATTACHMENTS {
        warnings.push(format!(
            "RepoTunnel limited explicit attachments to {MAX_ATTACHMENTS} files for this question."
        ));
        reduced = true;
    }

    if let Some(selection) = &request.selection {
        match validate_supplied_text(workspace, selection, MAX_SELECTION_CHARS) {
            Ok((content, truncated)) if !content.trim().is_empty() => {
                reduced |= truncated;
                excluded_paths.insert(selection.path.clone());
                chunks.push(ContextChunk {
                    priority: 99,
                    label: format!("Selected code from {}", selection.path),
                    text: content,
                    source: Some(ContextSource {
                        kind: "selection".to_string(),
                        path: Some(selection.path.clone()),
                        label: format!("Selected code · {}", selection.path),
                        line_start: None,
                        line_end: None,
                    }),
                    preserve_prefix: true,
                });
            }
            Ok(_) => {}
            Err(error) => warnings.push(error),
        }
    }

    if let Some(current) = &request.current_file {
        match validate_supplied_text(workspace, current, MAX_CURRENT_FILE_CHARS) {
            Ok((content, truncated)) if !content.trim().is_empty() => {
                reduced |= truncated;
                excluded_paths.insert(current.path.clone());
                chunks.push(ContextChunk {
                    priority: 90,
                    label: format!("Current editor file: {}", current.path),
                    text: content,
                    source: Some(ContextSource {
                        kind: "currentFile".to_string(),
                        path: Some(current.path.clone()),
                        label: current.path.clone(),
                        line_start: None,
                        line_end: None,
                    }),
                    preserve_prefix: true,
                });
            }
            Ok(_) => {}
            Err(error) => warnings.push(error),
        }
    }

    let mut diagnostics = Vec::new();
    if let Some(error) = &request.error {
        diagnostics.push(error);
    }
    diagnostics.extend(request.errors.iter());
    if diagnostics.len() > MAX_ERRORS {
        warnings.push(format!(
            "RepoTunnel limited explicit diagnostics to {MAX_ERRORS} entries for this request."
        ));
        reduced = true;
    }
    for (index, error) in diagnostics.into_iter().take(MAX_ERRORS).enumerate() {
        let mut text = format!("Source: {}\nError: {}", error.source, error.message);
        let mut safe_path = None;
        if let Some(path) = &error.path {
            if safe_input_path(workspace, path).is_ok() {
                safe_path = Some(path.clone());
                text.push_str(&format!("\nFile: {path}"));
                if let Some(line) = error.line {
                    text.push_str(&format!("\nLine: {line}"));
                }
                if let Some(column) = error.column {
                    text.push_str(&format!("\nColumn: {column}"));
                }
            }
        }
        let (text, truncated) = truncate_chars(&text, MAX_ERROR_CHARS);
        reduced |= truncated;
        chunks.push(ContextChunk {
            priority: 86u8.saturating_sub(index as u8),
            label: if index == 0 {
                "Attached diagnostic".to_string()
            } else {
                format!("Attached diagnostic {}", index + 1)
            },
            text,
            source: Some(ContextSource {
                kind: "error".to_string(),
                path: safe_path,
                label: format!("Error · {}", error.source),
                line_start: error.line,
                line_end: error.line,
            }),
            preserve_prefix: true,
        });
    }

    if request.include_project.unwrap_or(true) {
        for hit in retrieve(workspace, &snapshot, question, &excluded_paths)? {
            chunks.push(ContextChunk {
                priority: 70,
                label: format!(
                    "Retrieved source: {} (lines {}–{})",
                    hit.path, hit.line_start, hit.line_end
                ),
                text: hit.snippet,
                source: Some(ContextSource {
                    kind: "retrieved".to_string(),
                    path: Some(hit.path.clone()),
                    label: hit.path,
                    line_start: Some(hit.line_start),
                    line_end: Some(hit.line_end),
                }),
                preserve_prefix: false,
            });
        }

        chunks.push(ContextChunk {
            priority: 50,
            label: "Project setup".to_string(),
            text: setup_text(workspace, &snapshot),
            source: None,
            preserve_prefix: false,
        });
        chunks.push(ContextChunk {
            priority: 45,
            label: "Relevant project tree".to_string(),
            text: project_tree_text(&snapshot),
            source: None,
            preserve_prefix: false,
        });
    }

    chunks.sort_by_key(|entry| std::cmp::Reverse(entry.priority));
    let (primary_chunks, extra_chunks): (Vec<_>, Vec<_>) =
        chunks.into_iter().partition(|chunk| chunk.priority >= 70);
    let mut text = String::new();
    let mut sources = Vec::new();
    let mut remaining = budget;
    for chunk in primary_chunks {
        add_chunk(&mut text, &mut sources, chunk, &mut remaining, &mut reduced);
    }
    let history = history_for_budget(&request.history, &mut remaining, &mut reduced);
    for chunk in extra_chunks {
        add_chunk(&mut text, &mut sources, chunk, &mut remaining, &mut reduced);
    }

    Ok(BuiltProjectContext {
        text,
        history,
        sources,
        warnings,
        reduced,
        budget_chars: budget,
    })
}

pub(crate) fn safe_attachment_candidates(
    workspace: &Workspace,
    limit: usize,
) -> Result<Vec<ProjectEntry>, String> {
    let snapshot = project_index::project_snapshot(workspace, limit.clamp(100, 800))?;
    Ok(snapshot
        .entries
        .into_iter()
        .filter(|entry| entry.kind == "file" && !entry.binary && !entry.large)
        .filter(|entry| entry.size.unwrap_or(0) <= MAX_ATTACHMENT_BYTES)
        .take(limit.min(400))
        .collect())
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::{
        build, build_general, ContextHistoryInput, ContextTextInput, ProjectContextRequest,
    };
    use crate::models::{CommandPolicy, Workspace, WorkspaceAccessMode, WorkspaceChangePolicy};

    fn temp_workspace() -> (PathBuf, Workspace) {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("repotunnel-context-{nonce}"));
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
        fs::write(
            root.join("src/direct_https.rs"),
            "pub fn provision_certificate() {\n    let challenge = \"letsencrypt\";\n}\n",
        )
        .unwrap();
        fs::write(
            root.join("src/server.rs"),
            "pub fn auth_handler() { verify_token(); }\nfn verify_token() {}\n",
        )
        .unwrap();
        fs::write(root.join("src/ignored.rs"), "SECRET_IMPLEMENTATION").unwrap();
        fs::write(root.join(".gitignore"), "src/ignored.rs\nnode_modules/\n").unwrap();
        fs::write(root.join(".env"), "TOKEN=should-never-appear").unwrap();
        fs::write(root.join("node_modules/pkg/index.js"), "dependency secret").unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname='fixture'\nversion='0.1.0'\n",
        )
        .unwrap();
        let workspace = Workspace {
            id: "fixture".to_string(),
            name: "fixture".to_string(),
            path: root.to_string_lossy().into_owned(),
            added_at: 0,
            access_mode: WorkspaceAccessMode::ReadWrite,
            change_policy: WorkspaceChangePolicy::Review,
            command_policy: CommandPolicy::Review,
        };
        (root, workspace)
    }

    #[test]
    fn general_context_keeps_history_without_project_access() {
        let request = ProjectContextRequest {
            include_project: Some(false),
            history: vec![ContextHistoryInput {
                role: "user".to_string(),
                content: "Earlier general question".to_string(),
            }],
            ..ProjectContextRequest::default()
        };
        let result = build_general("What is Rust?", &request);
        assert!(result.text.is_empty());
        assert!(result.sources.is_empty());
        assert_eq!(result.history.len(), 1);
        assert!(result.warnings.is_empty());
    }

    #[test]
    fn retrieval_selects_relevant_source() {
        let (root, workspace) = temp_workspace();
        let result = build(
            &workspace,
            "Where is Direct HTTPS certificate provisioning implemented?",
            &ProjectContextRequest::default(),
        )
        .unwrap();
        assert!(result
            .sources
            .iter()
            .any(|source| source.path.as_deref() == Some("src/direct_https.rs")));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn ignored_and_protected_content_never_enters_context() {
        let (root, workspace) = temp_workspace();
        let request = ProjectContextRequest {
            attachments: vec!["src/ignored.rs".to_string(), ".env".to_string()],
            ..ProjectContextRequest::default()
        };
        let result = build(&workspace, "Explain secret implementation", &request).unwrap();
        assert!(!result.text.contains("SECRET_IMPLEMENTATION"));
        assert!(!result.text.contains("should-never-appear"));
        assert!(result.warnings.len() >= 2);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn oversized_attachment_is_excluded_safely() {
        let (root, workspace) = temp_workspace();
        fs::write(root.join("src/huge.txt"), vec![b'a'; 600 * 1024]).unwrap();
        let request = ProjectContextRequest {
            attachments: vec!["src/huge.txt".to_string()],
            ..ProjectContextRequest::default()
        };
        let result = build(&workspace, "Explain attached file", &request).unwrap();
        assert!(!result.text.contains(&"a".repeat(10_000)));
        assert!(result
            .warnings
            .iter()
            .any(|warning| warning.contains("larger")));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn recent_history_is_budgeted_before_extra_project_tree() {
        let (root, workspace) = temp_workspace();
        for index in 0..220 {
            fs::write(
                root.join("src").join(format!("extra_{index:03}.rs")),
                format!("pub fn extra_{index:03}() {{}}\n"),
            )
            .unwrap();
        }
        let request = ProjectContextRequest {
            selection: Some(ContextTextInput {
                path: "src/server.rs".to_string(),
                content: "selected context\n".repeat(900),
            }),
            history: vec![super::ContextHistoryInput {
                role: "assistant".to_string(),
                content: "RECENT_HISTORY_PRIORITY ".repeat(180),
            }],
            context_window: Some(8_000),
            ..ProjectContextRequest::default()
        };
        let result = build(&workspace, "Why does this fail?", &request).unwrap();
        assert!(result
            .history
            .iter()
            .any(|message| message.content.contains("RECENT_HISTORY_PRIORITY")));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn explicit_selection_has_priority_under_tight_budget() {
        let (root, workspace) = temp_workspace();
        let selection_text = "important selected code\n".repeat(500);
        let request = ProjectContextRequest {
            selection: Some(ContextTextInput {
                path: "src/server.rs".to_string(),
                content: selection_text,
            }),
            context_window: Some(8_000),
            ..ProjectContextRequest::default()
        };
        let result = build(&workspace, "Why does this fail?", &request).unwrap();
        assert!(result.text.contains("important selected code"));
        assert!(result.text.chars().count() <= result.budget_chars + 256);
        let _ = fs::remove_dir_all(root);
    }
}
