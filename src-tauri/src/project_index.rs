use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    fs::{self, File},
    io::{ErrorKind, Read},
    path::{Component, Path, PathBuf},
    sync::{Mutex, OnceLock},
};

use crate::{
    access::{resolve_workspace_path, AccessOperation},
    models::{LanguageStat, ProjectEntry, ProjectOverview, ProjectSnapshot, Workspace},
};

const MAX_PROJECT_ENTRIES: usize = 25_000;
const MAX_CLASSIFY_BYTES: u64 = 2 * 1_048_576;
const SNIFF_BYTES: usize = 8_192;
const MAX_IGNORED_ENTRY_DETAILS: usize = 12;
const MAX_TEXT_CLASS_CACHE_ENTRIES: usize = 50_000;

#[derive(Clone)]
struct TextClassCacheEntry {
    size: u64,
    modified_nanos: u128,
    binary: bool,
}

static TEXT_CLASS_CACHE: OnceLock<Mutex<HashMap<String, TextClassCacheEntry>>> = OnceLock::new();

fn modified_nanos(metadata: &fs::Metadata) -> u128 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos())
        .unwrap_or(0)
}

fn is_probably_binary_cached(path: &Path, metadata: &fs::Metadata) -> Result<bool, String> {
    let key = path.to_string_lossy().into_owned();
    let modified_nanos = modified_nanos(metadata);
    let cache = TEXT_CLASS_CACHE.get_or_init(|| Mutex::new(HashMap::new()));

    if let Ok(cache) = cache.lock() {
        if let Some(entry) = cache.get(&key) {
            if entry.size == metadata.len() && entry.modified_nanos == modified_nanos {
                return Ok(entry.binary);
            }
        }
    }

    let binary = is_probably_binary(path, metadata.len())?;
    if let Ok(mut cache) = cache.lock() {
        if cache.len() >= MAX_TEXT_CLASS_CACHE_ENTRIES {
            cache.clear();
        }
        cache.insert(
            key,
            TextClassCacheEntry {
                size: metadata.len(),
                modified_nanos,
                binary,
            },
        );
    }
    Ok(binary)
}

const GENERATED_DIRECTORIES: &[&str] = &[
    ".git",
    "node_modules",
    "dist",
    "build",
    "target",
    "coverage",
    ".next",
    ".nuxt",
    ".svelte-kit",
    ".cache",
    ".repotunnel-tmp",
    ".turbo",
    ".parcel-cache",
    "__pycache__",
    ".pytest_cache",
    ".mypy_cache",
    ".venv",
    "venv",
];

const BINARY_EXTENSIONS: &[&str] = &[
    "7z", "a", "avi", "bin", "bmp", "class", "dll", "dmg", "doc", "docx", "eot", "exe", "gif",
    "gz", "ico", "jar", "jpeg", "jpg", "lockb", "mov", "mp3", "mp4", "o", "otf", "pdf", "png",
    "ppt", "pptx", "pyc", "so", "sqlite", "sqlite3", "tar", "ttf", "wav", "webm", "webp", "woff",
    "woff2", "xls", "xlsx", "zip",
];

const MANIFEST_NAMES: &[&str] = &[
    "package.json",
    "Cargo.toml",
    "pyproject.toml",
    "requirements.txt",
    "go.mod",
    "pom.xml",
    "build.gradle",
    "build.gradle.kts",
    "composer.json",
    "Gemfile",
    "Makefile",
    "Dockerfile",
];

#[derive(Clone, Debug)]
struct IgnoreRule {
    base: PathBuf,
    pattern: String,
    negated: bool,
    directory_only: bool,
    anchored: bool,
}

#[derive(Clone)]
struct WalkItem {
    directory: PathBuf,
    rules: Vec<IgnoreRule>,
}

#[derive(Default)]
struct ScanStats {
    file_count: usize,
    directory_count: usize,
    text_file_count: usize,
    binary_file_count: usize,
    large_file_count: usize,
    ignored_entry_count: usize,
    ignored_entries: Vec<String>,
    total_bytes: u64,
    languages: BTreeMap<String, usize>,
    manifests: Vec<String>,
    truncated: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ProjectFileMetadata {
    pub(crate) path: String,
    pub(crate) size: u64,
    pub(crate) modified_nanos: u128,
}

fn workspace_root(workspace: &Workspace) -> Result<PathBuf, String> {
    Path::new(&workspace.path)
        .canonicalize()
        .map_err(|error| format!("Could not resolve the approved workspace: {error}"))
}

fn relative_string(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn normalize_pattern(pattern: &str) -> String {
    pattern.replace('\\', "/")
}

fn generated_directory(name: &str) -> bool {
    GENERATED_DIRECTORIES
        .iter()
        .any(|candidate| name.eq_ignore_ascii_case(candidate))
}

fn has_generated_component(root: &Path, path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(root) else {
        return false;
    };
    relative.components().any(|component| {
        matches!(component, Component::Normal(part) if generated_directory(part.to_string_lossy().as_ref()))
    })
}

fn parse_ignore_file(root: &Path, directory: &Path, file_name: &str) -> Vec<IgnoreRule> {
    let path = directory.join(file_name);
    let Ok(content) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let base = directory
        .strip_prefix(root)
        .unwrap_or(directory)
        .to_path_buf();

    content
        .lines()
        .filter_map(|line| {
            let mut line = line.trim_end_matches('\r').trim().to_string();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            if line.starts_with("\\#") || line.starts_with("\\!") {
                line.remove(0);
            }

            let negated = line.starts_with('!');
            if negated {
                line.remove(0);
            }
            if line.is_empty() {
                return None;
            }

            let anchored = line.starts_with('/');
            if anchored {
                line.remove(0);
            }
            let directory_only = line.ends_with('/');
            while line.ends_with('/') {
                line.pop();
            }
            if line.is_empty() {
                return None;
            }

            Some(IgnoreRule {
                base: base.clone(),
                pattern: normalize_pattern(&line),
                negated,
                directory_only,
                anchored,
            })
        })
        .collect()
}

fn rules_for_directory(root: &Path, directory: &Path) -> Vec<IgnoreRule> {
    let mut rules = Vec::new();
    let Ok(relative) = directory.strip_prefix(root) else {
        return rules;
    };
    let mut current = root.to_path_buf();

    rules.extend(parse_ignore_file(root, &current, ".gitignore"));
    rules.extend(parse_ignore_file(root, &current, ".ignore"));

    for component in relative.components() {
        if let Component::Normal(part) = component {
            current.push(part);
            rules.extend(parse_ignore_file(root, &current, ".gitignore"));
            rules.extend(parse_ignore_file(root, &current, ".ignore"));
        }
    }
    rules
}

fn wildcard_matches(
    pattern: &[char],
    text: &[char],
    pi: usize,
    ti: usize,
    memo: &mut BTreeMap<(usize, usize), bool>,
) -> bool {
    if let Some(value) = memo.get(&(pi, ti)) {
        return *value;
    }

    let result = if pi == pattern.len() {
        ti == text.len()
    } else if pattern[pi] == '*' {
        if pi + 1 < pattern.len() && pattern[pi + 1] == '*' {
            let zero_directories = pi + 2 < pattern.len()
                && pattern[pi + 2] == '/'
                && wildcard_matches(pattern, text, pi + 3, ti, memo);
            zero_directories
                || wildcard_matches(pattern, text, pi + 2, ti, memo)
                || (ti < text.len() && wildcard_matches(pattern, text, pi, ti + 1, memo))
        } else {
            wildcard_matches(pattern, text, pi + 1, ti, memo)
                || (ti < text.len()
                    && text[ti] != '/'
                    && wildcard_matches(pattern, text, pi, ti + 1, memo))
        }
    } else if ti < text.len() && (pattern[pi] == '?' && text[ti] != '/' || pattern[pi] == text[ti])
    {
        wildcard_matches(pattern, text, pi + 1, ti + 1, memo)
    } else {
        false
    };

    memo.insert((pi, ti), result);
    result
}

fn glob_matches(pattern: &str, text: &str) -> bool {
    let pattern = pattern.chars().collect::<Vec<_>>();
    let text = text.chars().collect::<Vec<_>>();
    wildcard_matches(&pattern, &text, 0, 0, &mut BTreeMap::new())
}

fn rule_matches(rule: &IgnoreRule, root: &Path, path: &Path, is_directory: bool) -> bool {
    if rule.directory_only && !is_directory {
        return false;
    }

    let base_abs = root.join(&rule.base);
    let Ok(relative) = path.strip_prefix(&base_abs) else {
        return false;
    };
    let relative = relative.to_string_lossy().replace('\\', "/");
    if relative.is_empty() {
        return false;
    }

    if rule.anchored || rule.pattern.contains('/') {
        glob_matches(&rule.pattern, &relative)
    } else {
        relative
            .split('/')
            .any(|component| glob_matches(&rule.pattern, component))
    }
}

fn ignored_by_rules(rules: &[IgnoreRule], root: &Path, path: &Path, is_directory: bool) -> bool {
    let mut ignored = false;
    for rule in rules {
        if rule_matches(rule, root, path, is_directory) {
            ignored = !rule.negated;
        }
    }
    ignored
}

fn extension(path: &Path) -> Option<String> {
    path.extension()
        .and_then(|value| value.to_str())
        .map(|value| value.to_ascii_lowercase())
}

fn language_for_path(path: &Path) -> Option<&'static str> {
    let name = path.file_name()?.to_str()?;
    if name == "Dockerfile" {
        return Some("Dockerfile");
    }
    if name == "Makefile" {
        return Some("Makefile");
    }

    match extension(path).as_deref()? {
        "c" | "h" => Some("C"),
        "cc" | "cpp" | "cxx" | "hpp" => Some("C++"),
        "cs" => Some("C#"),
        "css" => Some("CSS"),
        "dart" => Some("Dart"),
        "go" => Some("Go"),
        "html" | "htm" => Some("HTML"),
        "java" => Some("Java"),
        "js" | "mjs" | "cjs" | "jsx" => Some("JavaScript"),
        "json" | "jsonc" => Some("JSON"),
        "kt" | "kts" => Some("Kotlin"),
        "lua" => Some("Lua"),
        "md" | "mdx" => Some("Markdown"),
        "php" => Some("PHP"),
        "py" | "pyi" => Some("Python"),
        "rb" => Some("Ruby"),
        "rs" => Some("Rust"),
        "sh" | "bash" | "zsh" => Some("Shell"),
        "sql" => Some("SQL"),
        "swift" => Some("Swift"),
        "toml" => Some("TOML"),
        "ts" | "mts" | "cts" | "tsx" => Some("TypeScript"),
        "vue" => Some("Vue"),
        "xml" => Some("XML"),
        "yaml" | "yml" => Some("YAML"),
        _ => None,
    }
}

pub(crate) fn should_include_entry(
    workspace: &Workspace,
    parent: &Path,
    path: &Path,
    is_directory: bool,
) -> Result<bool, String> {
    let root = workspace_root(workspace)?;
    if has_generated_component(&root, path) {
        return Ok(false);
    }
    let rules = rules_for_directory(&root, parent);
    Ok(!ignored_by_rules(&rules, &root, path, is_directory))
}

pub(crate) fn is_probably_binary(path: &Path, size: u64) -> Result<bool, String> {
    if extension(path)
        .as_deref()
        .is_some_and(|ext| BINARY_EXTENSIONS.contains(&ext))
    {
        return Ok(true);
    }
    if size == 0 {
        return Ok(false);
    }

    let mut file =
        File::open(path).map_err(|error| format!("Could not inspect file content: {error}"))?;
    let mut buffer = vec![0u8; SNIFF_BYTES.min(size as usize)];
    let read = file
        .read(&mut buffer)
        .map_err(|error| format!("Could not inspect file content: {error}"))?;
    buffer.truncate(read);

    if buffer.contains(&0) {
        return Ok(true);
    }

    let suspicious = buffer
        .iter()
        .filter(|byte| matches!(**byte, 0..=8 | 11 | 12 | 14..=31))
        .count();
    Ok(!buffer.is_empty() && suspicious * 100 / buffer.len() > 15)
}

fn inspect_file(
    path: &Path,
    relative: &str,
    metadata: &fs::Metadata,
    stats: &mut ScanStats,
) -> Result<ProjectEntry, String> {
    stats.file_count += 1;
    stats.total_bytes = stats.total_bytes.saturating_add(metadata.len());

    let large = metadata.len() > MAX_CLASSIFY_BYTES;
    let binary = if large {
        stats.large_file_count += 1;
        false
    } else {
        is_probably_binary(path, metadata.len())?
    };

    if binary {
        stats.binary_file_count += 1;
    } else if !large {
        stats.text_file_count += 1;
        if let Some(language) = language_for_path(path) {
            *stats.languages.entry(language.to_string()).or_default() += 1;
        }
    }

    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    if MANIFEST_NAMES.contains(&name) {
        stats.manifests.push(relative.to_string());
    }

    Ok(ProjectEntry {
        path: relative.to_string(),
        kind: "file".to_string(),
        size: Some(metadata.len()),
        binary,
        large,
        language: language_for_path(path).map(str::to_string),
    })
}

pub(crate) fn project_file_metadata(
    workspace: &Workspace,
    entry_limit: usize,
) -> Result<(Vec<ProjectFileMetadata>, bool), String> {
    let entry_limit = entry_limit.clamp(100, MAX_PROJECT_ENTRIES);
    let root = workspace_root(workspace)?;
    let root_rules = rules_for_directory(&root, &root);
    let mut queue = VecDeque::from([WalkItem {
        directory: root.clone(),
        rules: root_rules,
    }]);
    let mut files = Vec::new();
    let mut seen_entries = 0usize;
    let mut truncated = false;

    while let Some(item) = queue.pop_front() {
        let read_dir = fs::read_dir(&item.directory)
            .map_err(|error| format!("Could not inspect project structure: {error}"))?;

        for entry in read_dir {
            let entry =
                entry.map_err(|error| format!("Could not inspect project entry: {error}"))?;
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path)
                .map_err(|error| format!("Could not inspect project entry: {error}"))?;

            if metadata.file_type().is_symlink() {
                continue;
            }

            let name = entry.file_name().to_string_lossy().into_owned();
            let is_directory = metadata.is_dir();
            if (is_directory && generated_directory(&name))
                || ignored_by_rules(&item.rules, &root, &path, is_directory)
            {
                continue;
            }

            let relative = relative_string(&root, &path);
            if resolve_workspace_path(workspace, &relative, AccessOperation::Read, true).is_err() {
                continue;
            }

            if seen_entries >= entry_limit {
                truncated = true;
                break;
            }
            seen_entries += 1;

            if is_directory {
                let mut child_rules = item.rules.clone();
                child_rules.extend(parse_ignore_file(&root, &path, ".gitignore"));
                child_rules.extend(parse_ignore_file(&root, &path, ".ignore"));
                queue.push_back(WalkItem {
                    directory: path,
                    rules: child_rules,
                });
            } else if metadata.is_file() {
                let modified_nanos = metadata
                    .modified()
                    .ok()
                    .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|duration| duration.as_nanos())
                    .unwrap_or(0);
                files.push(ProjectFileMetadata {
                    path: relative,
                    size: metadata.len(),
                    modified_nanos,
                });
            }
        }

        if truncated {
            break;
        }
    }

    Ok((files, truncated))
}

pub(crate) fn project_snapshot(
    workspace: &Workspace,
    entry_limit: usize,
) -> Result<ProjectSnapshot, String> {
    let entry_limit = entry_limit.clamp(100, MAX_PROJECT_ENTRIES);
    let root = workspace_root(workspace)?;
    let root_rules = rules_for_directory(&root, &root);
    let mut queue = VecDeque::from([WalkItem {
        directory: root.clone(),
        rules: root_rules,
    }]);
    let mut entries = Vec::new();
    let mut stats = ScanStats::default();

    while let Some(item) = queue.pop_front() {
        let read_dir = fs::read_dir(&item.directory)
            .map_err(|error| format!("Could not inspect project structure: {error}"))?;

        for entry in read_dir {
            let entry =
                entry.map_err(|error| format!("Could not inspect project entry: {error}"))?;
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path)
                .map_err(|error| format!("Could not inspect project entry: {error}"))?;

            if metadata.file_type().is_symlink() {
                continue;
            }

            let name = entry.file_name().to_string_lossy().into_owned();
            let is_directory = metadata.is_dir();
            let generated = is_directory && generated_directory(&name);
            let ignored_by_rule = ignored_by_rules(&item.rules, &root, &path, is_directory);
            if generated || ignored_by_rule {
                stats.ignored_entry_count += 1;

                if stats.ignored_entries.len() < MAX_IGNORED_ENTRY_DETAILS {
                    let relative = relative_string(&root, &path);
                    if generated {
                        stats
                            .ignored_entries
                            .push(format!("{relative}/ — generated folder"));
                    } else if resolve_workspace_path(
                        workspace,
                        &relative,
                        AccessOperation::Read,
                        true,
                    )
                    .is_ok()
                    {
                        stats
                            .ignored_entries
                            .push(format!("{relative} — ignored by project rules"));
                    }
                }

                continue;
            }

            let relative = relative_string(&root, &path);
            if resolve_workspace_path(workspace, &relative, AccessOperation::Read, true).is_err() {
                continue;
            }

            if entries.len() >= entry_limit {
                stats.truncated = true;
                break;
            }

            if is_directory {
                stats.directory_count += 1;
                entries.push(ProjectEntry {
                    path: relative,
                    kind: "directory".to_string(),
                    size: None,
                    binary: false,
                    large: false,
                    language: None,
                });

                let mut child_rules = item.rules.clone();
                child_rules.extend(parse_ignore_file(&root, &path, ".gitignore"));
                child_rules.extend(parse_ignore_file(&root, &path, ".ignore"));
                queue.push_back(WalkItem {
                    directory: path,
                    rules: child_rules,
                });
            } else if metadata.is_file() {
                entries.push(inspect_file(&path, &relative, &metadata, &mut stats)?);
            }
        }

        if stats.truncated {
            break;
        }
    }

    stats.manifests.sort();
    stats.manifests.dedup();
    let mut languages = stats
        .languages
        .into_iter()
        .map(|(name, files)| LanguageStat { name, files })
        .collect::<Vec<_>>();
    languages.sort_by(|left, right| {
        right
            .files
            .cmp(&left.files)
            .then_with(|| left.name.cmp(&right.name))
    });

    Ok(ProjectSnapshot {
        overview: ProjectOverview {
            file_count: stats.file_count,
            directory_count: stats.directory_count,
            text_file_count: stats.text_file_count,
            binary_file_count: stats.binary_file_count,
            large_file_count: stats.large_file_count,
            ignored_entry_count: stats.ignored_entry_count,
            ignored_entries: stats.ignored_entries,
            total_bytes: stats.total_bytes,
            languages,
            manifests: stats.manifests,
            truncated: stats.truncated,
        },
        entries,
    })
}

fn skippable_search_io(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        ErrorKind::PermissionDenied | ErrorKind::NotFound
    )
}

#[derive(Debug, Default)]
pub(crate) struct SmartTextFileScan {
    pub(crate) files: Vec<PathBuf>,
    pub(crate) skipped_io_count: usize,
    pub(crate) skipped_policy_count: usize,
    pub(crate) truncated: bool,
}

pub(crate) fn smart_text_files_detailed(
    workspace: &Workspace,
    relative_path: &str,
    max_files: usize,
) -> Result<SmartTextFileScan, String> {
    let start = resolve_workspace_path(workspace, relative_path, AccessOperation::Read, true)?;
    let root = workspace_root(workspace)?;
    if start != root {
        let parent = start.parent().unwrap_or(&root);
        if !should_include_entry(workspace, parent, &start, start.is_dir())? {
            return Ok(SmartTextFileScan::default());
        }
    }
    if start.is_file() {
        let metadata = match fs::metadata(&start) {
            Ok(metadata) => metadata,
            Err(error) if skippable_search_io(&error) => {
                return Ok(SmartTextFileScan {
                    skipped_io_count: 1,
                    ..SmartTextFileScan::default()
                });
            }
            Err(error) => {
                return Err(format!("Could not inspect the search file: {error}"));
            }
        };
        if metadata.len() > MAX_CLASSIFY_BYTES {
            return Ok(SmartTextFileScan::default());
        }
        match is_probably_binary(&start, metadata.len()) {
            Ok(false) => {
                return Ok(SmartTextFileScan {
                    files: vec![start],
                    ..SmartTextFileScan::default()
                });
            }
            Ok(true) => return Ok(SmartTextFileScan::default()),
            Err(_) => {
                return Ok(SmartTextFileScan {
                    skipped_io_count: 1,
                    ..SmartTextFileScan::default()
                });
            }
        }
    }
    if !start.is_dir() {
        return Err("Search can only start from a file or folder.".to_string());
    }

    let initial_rules = rules_for_directory(&root, &start);
    let mut queue = VecDeque::from([WalkItem {
        directory: start,
        rules: initial_rules,
    }]);
    let mut scan = SmartTextFileScan::default();

    while let Some(item) = queue.pop_front() {
        let read_dir = match fs::read_dir(&item.directory) {
            Ok(read_dir) => read_dir,
            Err(error) if skippable_search_io(&error) => {
                scan.skipped_io_count = scan.skipped_io_count.saturating_add(1);
                continue;
            }
            Err(error) => {
                return Err(format!("Could not search a project folder: {error}"));
            }
        };
        for entry in read_dir {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) if skippable_search_io(&error) => {
                    scan.skipped_io_count = scan.skipped_io_count.saturating_add(1);
                    continue;
                }
                Err(error) => {
                    return Err(format!("Could not inspect a project entry: {error}"));
                }
            };
            let path = entry.path();
            let metadata = match fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) if skippable_search_io(&error) => {
                    scan.skipped_io_count = scan.skipped_io_count.saturating_add(1);
                    continue;
                }
                Err(error) => {
                    return Err(format!("Could not inspect a project entry: {error}"));
                }
            };
            if metadata.file_type().is_symlink() {
                continue;
            }

            let name = entry.file_name().to_string_lossy().into_owned();
            let is_directory = metadata.is_dir();
            if (is_directory && generated_directory(&name))
                || ignored_by_rules(&item.rules, &root, &path, is_directory)
            {
                continue;
            }

            let relative = relative_string(&root, &path);
            if resolve_workspace_path(workspace, &relative, AccessOperation::Read, true).is_err() {
                scan.skipped_policy_count = scan.skipped_policy_count.saturating_add(1);
                continue;
            }

            if is_directory {
                let mut child_rules = item.rules.clone();
                child_rules.extend(parse_ignore_file(&root, &path, ".gitignore"));
                child_rules.extend(parse_ignore_file(&root, &path, ".ignore"));
                queue.push_back(WalkItem {
                    directory: path,
                    rules: child_rules,
                });
            } else if metadata.is_file() && metadata.len() <= MAX_CLASSIFY_BYTES {
                match is_probably_binary(&path, metadata.len()) {
                    Ok(false) => {
                        scan.files.push(path);
                        if scan.files.len() >= max_files {
                            scan.truncated = true;
                            return Ok(scan);
                        }
                    }
                    Ok(true) => {}
                    Err(_) => {
                        scan.skipped_io_count = scan.skipped_io_count.saturating_add(1);
                    }
                }
            }
        }
    }

    Ok(scan)
}

struct IncrementalDirectoryBatch {
    rules: Vec<IgnoreRule>,
    entries: fs::ReadDir,
}

pub(crate) struct IncrementalTextWalker {
    root: PathBuf,
    queue: VecDeque<WalkItem>,
    current: Option<IncrementalDirectoryBatch>,
    single_file: Option<PathBuf>,
    skipped_io_count: usize,
    skipped_policy_count: usize,
    visited_entry_count: usize,
    exhausted: bool,
}

#[derive(Debug)]
pub(crate) struct IncrementalTextFile {
    pub(crate) path: PathBuf,
    pub(crate) relative_path: String,
}

impl IncrementalTextWalker {
    pub(crate) fn new(workspace: &Workspace, relative_path: &str) -> Result<Self, String> {
        let start = resolve_workspace_path(workspace, relative_path, AccessOperation::Read, true)?;
        let root = workspace_root(workspace)?;
        if start != root {
            let parent = start.parent().unwrap_or(&root);
            if !should_include_entry(workspace, parent, &start, start.is_dir())? {
                return Ok(Self {
                    root,
                    queue: VecDeque::new(),
                    current: None,
                    single_file: None,
                    skipped_io_count: 0,
                    skipped_policy_count: 1,
                    visited_entry_count: 0,
                    exhausted: true,
                });
            }
        }

        if start.is_file() {
            return Ok(Self {
                root,
                queue: VecDeque::new(),
                current: None,
                single_file: Some(start),
                skipped_io_count: 0,
                skipped_policy_count: 0,
                visited_entry_count: 0,
                exhausted: false,
            });
        }
        if !start.is_dir() {
            return Err("Search can only start from a file or folder.".to_string());
        }

        let initial_rules = rules_for_directory(&root, &start);
        Ok(Self {
            root,
            queue: VecDeque::from([WalkItem {
                directory: start,
                rules: initial_rules,
            }]),
            current: None,
            single_file: None,
            skipped_io_count: 0,
            skipped_policy_count: 0,
            visited_entry_count: 0,
            exhausted: false,
        })
    }

    fn next_candidate_path(&mut self) -> Result<Option<(PathBuf, Vec<IgnoreRule>)>, String> {
        loop {
            if let Some(batch) = &mut self.current {
                loop {
                    match batch.entries.next() {
                        Some(Ok(entry)) => {
                            return Ok(Some((entry.path(), batch.rules.clone())));
                        }
                        Some(Err(error)) if skippable_search_io(&error) => {
                            self.skipped_io_count = self.skipped_io_count.saturating_add(1);
                        }
                        Some(Err(error)) => {
                            return Err(format!("Could not inspect a project entry: {error}"));
                        }
                        None => {
                            self.current = None;
                            break;
                        }
                    }
                }
            }

            let Some(item) = self.queue.pop_front() else {
                self.exhausted = true;
                return Ok(None);
            };
            let read_dir = match fs::read_dir(&item.directory) {
                Ok(read_dir) => read_dir,
                Err(error) if skippable_search_io(&error) => {
                    self.skipped_io_count = self.skipped_io_count.saturating_add(1);
                    continue;
                }
                Err(error) => {
                    return Err(format!("Could not search a project folder: {error}"));
                }
            };

            self.current = Some(IncrementalDirectoryBatch {
                rules: item.rules,
                entries: read_dir,
            });
        }
    }

    fn classify_file(
        &mut self,
        workspace: &Workspace,
        path: PathBuf,
        rules: &[IgnoreRule],
        max_file_bytes: u64,
    ) -> Result<Option<IncrementalTextFile>, String> {
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if skippable_search_io(&error) => {
                self.skipped_io_count = self.skipped_io_count.saturating_add(1);
                return Ok(None);
            }
            Err(error) => {
                return Err(format!("Could not inspect a project entry: {error}"));
            }
        };
        self.visited_entry_count = self.visited_entry_count.saturating_add(1);

        if metadata.file_type().is_symlink() {
            return Ok(None);
        }
        let name = path
            .file_name()
            .map(|value| value.to_string_lossy().into_owned())
            .unwrap_or_default();
        let is_directory = metadata.is_dir();
        if (is_directory && generated_directory(&name))
            || ignored_by_rules(rules, &self.root, &path, is_directory)
        {
            return Ok(None);
        }

        let relative = relative_string(&self.root, &path);
        if resolve_workspace_path(workspace, &relative, AccessOperation::Read, true).is_err() {
            self.skipped_policy_count = self.skipped_policy_count.saturating_add(1);
            return Ok(None);
        }

        if is_directory {
            let mut child_rules = rules.to_vec();
            child_rules.extend(parse_ignore_file(&self.root, &path, ".gitignore"));
            child_rules.extend(parse_ignore_file(&self.root, &path, ".ignore"));
            self.queue.push_back(WalkItem {
                directory: path,
                rules: child_rules,
            });
            return Ok(None);
        }

        if !metadata.is_file() || metadata.len() > max_file_bytes {
            return Ok(None);
        }
        match is_probably_binary_cached(&path, &metadata) {
            Ok(false) => Ok(Some(IncrementalTextFile {
                path,
                relative_path: relative,
            })),
            Ok(true) => Ok(None),
            Err(_) => {
                self.skipped_io_count = self.skipped_io_count.saturating_add(1);
                Ok(None)
            }
        }
    }

    pub(crate) fn next_text_file(
        &mut self,
        workspace: &Workspace,
        max_file_bytes: u64,
    ) -> Result<Option<IncrementalTextFile>, String> {
        if self.exhausted {
            return Ok(None);
        }

        if let Some(path) = self.single_file.take() {
            let rules = path
                .parent()
                .map(|parent| rules_for_directory(&self.root, parent))
                .unwrap_or_default();
            let result = self.classify_file(workspace, path, &rules, max_file_bytes)?;
            self.exhausted = true;
            return Ok(result);
        }

        loop {
            let Some((path, rules)) = self.next_candidate_path()? else {
                return Ok(None);
            };
            if let Some(file) = self.classify_file(workspace, path, &rules, max_file_bytes)? {
                return Ok(Some(file));
            }
        }
    }

    pub(crate) fn skipped_io_count(&self) -> usize {
        self.skipped_io_count
    }

    pub(crate) fn skipped_policy_count(&self) -> usize {
        self.skipped_policy_count
    }

    pub(crate) fn visited_entry_count(&self) -> usize {
        self.visited_entry_count
    }

    pub(crate) fn is_exhausted(&self) -> bool {
        self.exhausted
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        io::ErrorKind,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::{
        glob_matches, project_file_metadata, project_snapshot, skippable_search_io,
        smart_text_files_detailed,
    };
    use crate::models::{CommandPolicy, Workspace, WorkspaceAccessMode, WorkspaceChangePolicy};

    fn temp_workspace() -> (PathBuf, Workspace) {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("repotunnel-index-{nonce}"));
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
        fs::write(root.join("src/main.ts"), "console.log('hello');").unwrap();
        fs::write(root.join("src/ignored.ts"), "secret-ish generated data").unwrap();
        fs::write(root.join("node_modules/pkg/index.js"), "ignored dependency").unwrap();
        fs::write(root.join("package.json"), "{}").unwrap();
        fs::write(root.join(".gitignore"), "src/ignored.ts\n").unwrap();
        fs::write(root.join("image.png"), [0u8, 1, 2, 0, 4]).unwrap();

        let workspace = Workspace {
            id: "test".to_string(),
            name: "test".to_string(),
            path: root.to_string_lossy().into_owned(),
            added_at: 0,
            access_mode: WorkspaceAccessMode::ReadWrite,
            change_policy: WorkspaceChangePolicy::Review,
            command_policy: CommandPolicy::Review,
        };
        (root, workspace)
    }

    #[test]
    fn wildcard_supports_double_star() {
        assert!(glob_matches("**/generated/*.js", "src/generated/app.js"));
        assert!(glob_matches("**/*.rs", "main.rs"));
        assert!(glob_matches("**/*.rs", "src/main.rs"));
        assert!(glob_matches("*.log", "server.log"));
        assert!(!glob_matches("*.log", "logs/server.log"));
    }

    #[test]
    fn snapshot_filters_ignored_and_generated_content() {
        let (root, workspace) = temp_workspace();
        let snapshot = project_snapshot(&workspace, 500).unwrap();
        let paths = snapshot
            .entries
            .iter()
            .map(|entry| entry.path.as_str())
            .collect::<Vec<_>>();

        assert!(paths.contains(&"src/main.ts"));
        assert!(paths.contains(&"package.json"));
        assert!(!paths.contains(&"src/ignored.ts"));
        assert!(!paths.iter().any(|path| path.starts_with("node_modules/")));
        assert!(snapshot
            .overview
            .ignored_entries
            .iter()
            .any(|item| item.contains("src/ignored.ts")));
        assert!(snapshot
            .overview
            .ignored_entries
            .iter()
            .any(|item| item.contains("node_modules/")));
        assert!(snapshot.overview.binary_file_count >= 1);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn ignored_detail_list_does_not_expose_protected_paths() {
        let (root, workspace) = temp_workspace();
        fs::write(root.join(".env"), "SECRET=value").unwrap();
        fs::write(root.join(".gitignore"), "src/ignored.ts\n.env\n").unwrap();

        let snapshot = project_snapshot(&workspace, 500).unwrap();
        assert!(snapshot.overview.ignored_entry_count >= 2);
        assert!(!snapshot
            .overview
            .ignored_entries
            .iter()
            .any(|item| item.contains(".env")));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn metadata_scan_preserves_project_boundaries_without_content_classification() {
        let (root, workspace) = temp_workspace();
        fs::write(root.join(".env"), "SECRET=value").unwrap();
        fs::write(root.join(".gitignore"), "src/ignored.ts\n.env\n").unwrap();

        let (files, truncated) = project_file_metadata(&workspace, 500).unwrap();
        let paths = files
            .iter()
            .map(|entry| entry.path.as_str())
            .collect::<Vec<_>>();

        assert!(!truncated);
        assert!(paths.contains(&"src/main.ts"));
        assert!(paths.contains(&"package.json"));
        assert!(paths.contains(&"image.png"));
        assert!(!paths.contains(&"src/ignored.ts"));
        assert!(!paths.contains(&".env"));
        assert!(!paths.iter().any(|path| path.starts_with("node_modules/")));
        assert!(files.iter().all(|entry| entry.modified_nanos > 0));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn search_skips_only_expected_partial_io_failures() {
        assert!(skippable_search_io(&std::io::Error::from(
            ErrorKind::PermissionDenied
        )));
        assert!(skippable_search_io(&std::io::Error::from(
            ErrorKind::NotFound
        )));
        assert!(!skippable_search_io(&std::io::Error::from(
            ErrorKind::InvalidData
        )));
        assert!(!skippable_search_io(&std::io::Error::from(
            ErrorKind::Other
        )));
    }

    #[test]
    fn smart_search_candidates_skip_binary_and_ignored_files() {
        let (root, workspace) = temp_workspace();
        let files = smart_text_files_detailed(&workspace, "", 100)
            .unwrap()
            .files;
        let names = files
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        assert!(names.contains(&"main.ts".to_string()));
        assert!(!names.contains(&"ignored.ts".to_string()));
        assert!(!names.contains(&"image.png".to_string()));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn detailed_search_reports_protected_skips_and_candidate_truncation() {
        let (root, workspace) = temp_workspace();
        fs::write(root.join(".env"), "TOKEN=hidden\n").unwrap();
        fs::write(root.join("src/second.ts"), "export const second = true;\n").unwrap();

        let scan = smart_text_files_detailed(&workspace, "", 1).unwrap();
        assert_eq!(scan.files.len(), 1);
        assert!(scan.truncated);

        let full_scan = smart_text_files_detailed(&workspace, "", 100).unwrap();
        assert!(full_scan.skipped_policy_count >= 1);
        assert!(!full_scan
            .files
            .iter()
            .any(|path| path.file_name().is_some_and(|name| name == ".env")));

        let _ = fs::remove_dir_all(root);
    }
}
