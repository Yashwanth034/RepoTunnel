use std::{
    collections::{HashMap, VecDeque},
    fs::{self, File},
    io::{BufRead, BufReader, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, OnceLock,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde::Serialize;

use crate::{
    access::{resolve_workspace_path, AccessOperation},
    models::Workspace,
    project_index::{self, IncrementalTextWalker},
};

const MAX_FAST_SEARCH_FILE_BYTES: u64 = 4 * 1024 * 1024;
const MAX_FAST_SEARCH_RESULTS: usize = 100;
const DEFAULT_FAST_SEARCH_RESULTS: usize = 40;
const MIN_SEARCH_BUDGET_MS: u64 = 100;
const DEFAULT_SEARCH_BUDGET_MS: u64 = 900;
const MAX_SEARCH_BUDGET_MS: u64 = 1_500;
const SEARCH_PREVIEW_CHARS: usize = 240;
const MAX_SEARCH_LINE_BYTES: usize = 16 * 1024;
const SEARCH_SESSION_TTL: Duration = Duration::from_secs(180);
const RECENT_SEARCH_TTL: Duration = Duration::from_secs(1);
const MAX_SEARCH_SESSIONS: usize = 64;

const DEFAULT_RANGE_LINES: usize = 240;
const MAX_RANGE_LINES: usize = 1_000;
const DEFAULT_RANGE_BYTES: usize = 192 * 1024;
const MAX_RANGE_BYTES: usize = 256 * 1024;
const MAX_RANGE_LINE_CAPTURE_BYTES: usize = 64 * 1024;
const RANGE_BUDGET: Duration = Duration::from_millis(900);

const DEFAULT_DIRECTORY_PAGE_SIZE: usize = 200;
const MAX_DIRECTORY_PAGE_SIZE: usize = 500;
const DIRECTORY_SESSION_TTL: Duration = Duration::from_secs(180);
const MAX_DIRECTORY_SESSIONS: usize = 32;
const DIRECTORY_PAGE_BUDGET: Duration = Duration::from_millis(700);

static SESSION_SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn next_session_id(prefix: &str) -> String {
    let seq = SESSION_SEQUENCE.fetch_add(1, Ordering::Relaxed) + 1;
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0);
    format!("{prefix}-{}-{millis:x}-{seq:x}", std::process::id())
}

fn modified_millis(metadata: &fs::Metadata) -> Option<u64> {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
}

fn workspace_relative_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FastSearchMatch {
    pub(crate) path: String,
    pub(crate) line: usize,
    pub(crate) column: usize,
    pub(crate) preview: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FastSearchPage {
    pub(crate) matches: Vec<FastSearchMatch>,
    pub(crate) files_searched_this_page: usize,
    pub(crate) total_files_searched: usize,
    pub(crate) visited_entries: usize,
    pub(crate) skipped_io_count: usize,
    pub(crate) skipped_policy_count: usize,
    pub(crate) done: bool,
    pub(crate) truncated: bool,
    pub(crate) next_cursor: Option<String>,
    pub(crate) elapsed_ms: u64,
    pub(crate) busy: bool,
    pub(crate) retry_after_ms: Option<u64>,
    pub(crate) reused_recent_result: bool,
}

struct PendingSearchFile {
    path: PathBuf,
    relative_path: String,
    byte_offset: u64,
    next_line: usize,
}

struct SearchSession {
    workspace_id: String,
    relative_path: String,
    query: String,
    query_lower: String,
    walker: IncrementalTextWalker,
    pending_file: Option<PendingSearchFile>,
    total_files_searched: usize,
    last_access: Instant,
}

struct RecentSearch {
    at: Instant,
    page: FastSearchPage,
}

#[derive(Default)]
struct SearchStore {
    sessions: HashMap<String, SearchSession>,
    recent: HashMap<String, RecentSearch>,
}

static SEARCH_STORE: OnceLock<Mutex<SearchStore>> = OnceLock::new();
static WORKSPACE_HEAVY_READ_GATES: OnceLock<Mutex<HashMap<String, Arc<Mutex<()>>>>> =
    OnceLock::new();

fn search_store() -> &'static Mutex<SearchStore> {
    SEARCH_STORE.get_or_init(|| Mutex::new(SearchStore::default()))
}

fn workspace_heavy_read_gate(workspace_id: &str) -> Arc<Mutex<()>> {
    let gates = WORKSPACE_HEAVY_READ_GATES.get_or_init(|| Mutex::new(HashMap::new()));
    let mut gates = gates
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    gates
        .entry(workspace_id.to_string())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

fn cleanup_search_store(store: &mut SearchStore) {
    let now = Instant::now();
    store
        .sessions
        .retain(|_, session| now.duration_since(session.last_access) <= SEARCH_SESSION_TTL);
    store
        .recent
        .retain(|_, recent| now.duration_since(recent.at) <= RECENT_SEARCH_TTL);

    if store.sessions.len() > MAX_SEARCH_SESSIONS {
        let mut ages = store
            .sessions
            .iter()
            .map(|(id, session)| (id.clone(), session.last_access))
            .collect::<Vec<_>>();
        ages.sort_by_key(|(_, at)| *at);
        let remove_count = store.sessions.len().saturating_sub(MAX_SEARCH_SESSIONS);
        for (id, _) in ages.into_iter().take(remove_count) {
            store.sessions.remove(&id);
        }
    }
}

fn search_request_key(
    workspace: &Workspace,
    relative_path: &str,
    query: &str,
    max_results: usize,
    budget_ms: u64,
) -> String {
    format!(
        "{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}",
        workspace.id,
        relative_path,
        query.to_lowercase(),
        max_results,
        budget_ms
    )
}

struct BoundedLine {
    captured: Vec<u8>,
    consumed: usize,
    truncated: bool,
    had_data: bool,
}

fn read_bounded_line<R: BufRead>(
    reader: &mut R,
    max_capture_bytes: usize,
) -> std::io::Result<BoundedLine> {
    let mut captured = Vec::with_capacity(max_capture_bytes.min(8 * 1024));
    let mut consumed = 0usize;
    let mut truncated = false;
    let mut had_data = false;

    loop {
        let buffer = reader.fill_buf()?;
        if buffer.is_empty() {
            return Ok(BoundedLine {
                captured,
                consumed,
                truncated,
                had_data,
            });
        }
        had_data = true;
        let newline = buffer.iter().position(|byte| *byte == b'\n');
        let take_from_buffer = newline.map(|index| index + 1).unwrap_or(buffer.len());
        let remaining = max_capture_bytes.saturating_sub(captured.len());
        let capture = take_from_buffer.min(remaining);
        if capture > 0 {
            captured.extend_from_slice(&buffer[..capture]);
        }
        if take_from_buffer > remaining {
            truncated = true;
        }
        consumed = consumed.saturating_add(take_from_buffer);
        reader.consume(take_from_buffer);
        if newline.is_some() {
            return Ok(BoundedLine {
                captured,
                consumed,
                truncated,
                had_data,
            });
        }
    }
}

struct SearchFileOutcome {
    matches: Vec<FastSearchMatch>,
    next_byte_offset: Option<u64>,
    next_line: usize,
    elapsed_out: bool,
}

fn search_file_page(
    pending: &PendingSearchFile,
    query_lower: &str,
    result_limit: usize,
    deadline: Instant,
) -> Result<SearchFileOutcome, String> {
    let mut file = File::open(&pending.path)
        .map_err(|error| format!("Could not open a file during fast search: {error}"))?;
    file.seek(SeekFrom::Start(pending.byte_offset))
        .map_err(|error| format!("Could not resume a fast-search file: {error}"))?;
    let mut reader = BufReader::new(file);
    let mut byte_offset = pending.byte_offset;
    let mut line_number = pending.next_line;
    let mut matches = Vec::new();

    loop {
        if Instant::now() >= deadline {
            return Ok(SearchFileOutcome {
                matches,
                next_byte_offset: Some(byte_offset),
                next_line: line_number,
                elapsed_out: true,
            });
        }

        let line = read_bounded_line(&mut reader, MAX_SEARCH_LINE_BYTES)
            .map_err(|error| format!("Could not read a file during fast search: {error}"))?;
        if !line.had_data {
            return Ok(SearchFileOutcome {
                matches,
                next_byte_offset: None,
                next_line: line_number,
                elapsed_out: false,
            });
        }
        byte_offset = byte_offset.saturating_add(line.consumed as u64);

        if !line.truncated && !line.captured.contains(&0) {
            let mut bytes = line.captured;
            while matches!(bytes.last(), Some(b'\n' | b'\r')) {
                bytes.pop();
            }
            if let Ok(text) = String::from_utf8(bytes) {
                let lower = text.to_lowercase();
                if let Some(byte_index) = lower.find(query_lower) {
                    let column = lower[..byte_index].chars().count() + 1;
                    let mut preview = text.chars().take(SEARCH_PREVIEW_CHARS).collect::<String>();
                    if text.chars().count() > SEARCH_PREVIEW_CHARS {
                        preview.push('…');
                    }
                    matches.push(FastSearchMatch {
                        path: pending.relative_path.clone(),
                        line: line_number,
                        column,
                        preview,
                    });
                    if matches.len() >= result_limit {
                        return Ok(SearchFileOutcome {
                            matches,
                            next_byte_offset: Some(byte_offset),
                            next_line: line_number.saturating_add(1),
                            elapsed_out: false,
                        });
                    }
                }
            }
        }
        line_number = line_number.saturating_add(1);
    }
}

pub(crate) fn fast_search_page(
    workspace: &Workspace,
    relative_path: &str,
    query: &str,
    cursor: Option<&str>,
    max_results: Option<usize>,
    budget_ms: Option<u64>,
) -> Result<FastSearchPage, String> {
    let query = query.trim();
    if query.is_empty() {
        return Err("Fast search query cannot be empty.".to_string());
    }
    if query.chars().count() > 256 {
        return Err("Fast search query is too long.".to_string());
    }

    let max_results = max_results
        .unwrap_or(DEFAULT_FAST_SEARCH_RESULTS)
        .clamp(1, MAX_FAST_SEARCH_RESULTS);
    let budget_ms = budget_ms
        .unwrap_or(DEFAULT_SEARCH_BUDGET_MS)
        .clamp(MIN_SEARCH_BUDGET_MS, MAX_SEARCH_BUDGET_MS);

    let gate = workspace_heavy_read_gate(&workspace.id);
    let Ok(_guard) = gate.try_lock() else {
        return Ok(FastSearchPage {
            matches: Vec::new(),
            files_searched_this_page: 0,
            total_files_searched: 0,
            visited_entries: 0,
            skipped_io_count: 0,
            skipped_policy_count: 0,
            done: false,
            truncated: true,
            next_cursor: cursor.map(str::to_string),
            elapsed_ms: 0,
            busy: true,
            retry_after_ms: Some(100),
            reused_recent_result: false,
        });
    };

    let request_key = search_request_key(workspace, relative_path, query, max_results, budget_ms);
    if cursor.is_none() {
        let mut store = search_store()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        cleanup_search_store(&mut store);
        if let Some(recent) = store.recent.get(&request_key) {
            if recent.at.elapsed() <= RECENT_SEARCH_TTL {
                let mut page = recent.page.clone();
                page.reused_recent_result = true;
                return Ok(page);
            }
        }
    }

    let (session_id, mut session) = if let Some(cursor) = cursor {
        let mut store = search_store()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        cleanup_search_store(&mut store);
        store
            .recent
            .retain(|_, recent| recent.page.next_cursor.as_deref() != Some(cursor));
        let session = store.sessions.remove(cursor).ok_or_else(|| {
            "Fast-search cursor is unknown or expired; start a new fast search.".to_string()
        })?;
        if session.workspace_id != workspace.id
            || session.relative_path != relative_path
            || session.query != query
        {
            store.sessions.insert(cursor.to_string(), session);
            return Err(
                "Fast-search cursor does not belong to this workspace/path/query.".to_string(),
            );
        }
        (cursor.to_string(), session)
    } else {
        (
            next_session_id("fast-search"),
            SearchSession {
                workspace_id: workspace.id.clone(),
                relative_path: relative_path.to_string(),
                query: query.to_string(),
                query_lower: query.to_lowercase(),
                walker: IncrementalTextWalker::new(workspace, relative_path)?,
                pending_file: None,
                total_files_searched: 0,
                last_access: Instant::now(),
            },
        )
    };

    let started = Instant::now();
    let deadline = started + Duration::from_millis(budget_ms);
    let page_files_before = session.total_files_searched;
    let mut matches = Vec::new();

    while matches.len() < max_results && Instant::now() < deadline {
        let pending = if let Some(pending) = session.pending_file.take() {
            pending
        } else {
            let Some(file) = session
                .walker
                .next_text_file(workspace, MAX_FAST_SEARCH_FILE_BYTES)?
            else {
                break;
            };
            session.total_files_searched = session.total_files_searched.saturating_add(1);
            PendingSearchFile {
                path: file.path,
                relative_path: file.relative_path,
                byte_offset: 0,
                next_line: 1,
            }
        };

        let remaining = max_results.saturating_sub(matches.len()).max(1);
        let outcome = search_file_page(&pending, &session.query_lower, remaining, deadline)?;
        matches.extend(outcome.matches);
        if let Some(next_byte_offset) = outcome.next_byte_offset {
            session.pending_file = Some(PendingSearchFile {
                path: pending.path,
                relative_path: pending.relative_path,
                byte_offset: next_byte_offset,
                next_line: outcome.next_line,
            });
        }
        if outcome.elapsed_out || matches.len() >= max_results {
            break;
        }
    }

    session.last_access = Instant::now();
    let done = session.pending_file.is_none() && session.walker.is_exhausted();
    let next_cursor = (!done).then(|| session_id.clone());
    let page = FastSearchPage {
        matches,
        files_searched_this_page: session
            .total_files_searched
            .saturating_sub(page_files_before),
        total_files_searched: session.total_files_searched,
        visited_entries: session.walker.visited_entry_count(),
        skipped_io_count: session.walker.skipped_io_count(),
        skipped_policy_count: session.walker.skipped_policy_count(),
        done,
        truncated: !done,
        next_cursor: next_cursor.clone(),
        elapsed_ms: started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
        busy: false,
        retry_after_ms: None,
        reused_recent_result: false,
    };

    let mut store = search_store()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    cleanup_search_store(&mut store);
    if !done {
        store.sessions.insert(session_id, session);
    }
    if cursor.is_none() {
        store.recent.insert(
            request_key,
            RecentSearch {
                at: Instant::now(),
                page: page.clone(),
            },
        );
    }
    Ok(page)
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FileRangeResult {
    pub(crate) path: String,
    pub(crate) content: String,
    pub(crate) requested_start_line: usize,
    pub(crate) start_line: usize,
    pub(crate) end_line: usize,
    pub(crate) next_cursor: Option<String>,
    pub(crate) eof: bool,
    pub(crate) reached_requested_start: bool,
    pub(crate) size: u64,
    pub(crate) modified_at: Option<u64>,
    pub(crate) truncated_line_count: usize,
    pub(crate) elapsed_ms: u64,
}

fn encode_range_cursor(
    size: u64,
    modified_at: Option<u64>,
    line: usize,
    byte_offset: u64,
    requested_start_line: usize,
) -> String {
    format!(
        "r1:{size}:{}:{line}:{byte_offset}:{requested_start_line}",
        modified_at.unwrap_or(0)
    )
}

fn decode_range_cursor(cursor: &str) -> Result<(u64, u64, usize, u64, usize), String> {
    let parts = cursor.split(':').collect::<Vec<_>>();
    if parts.len() != 6 || parts[0] != "r1" {
        return Err("File-range cursor is invalid.".to_string());
    }
    let parse = |value: &str| {
        value
            .parse::<u64>()
            .map_err(|_| "File-range cursor is invalid.".to_string())
    };
    let size = parse(parts[1])?;
    let modified = parse(parts[2])?;
    let line = usize::try_from(parse(parts[3])?)
        .map_err(|_| "File-range cursor is invalid.".to_string())?;
    let offset = parse(parts[4])?;
    let requested = usize::try_from(parse(parts[5])?)
        .map_err(|_| "File-range cursor is invalid.".to_string())?;
    Ok((size, modified, line, offset, requested))
}

pub(crate) fn read_file_range(
    workspace: &Workspace,
    relative_path: &str,
    start_line: Option<usize>,
    max_lines: Option<usize>,
    cursor: Option<&str>,
) -> Result<FileRangeResult, String> {
    let path = resolve_workspace_path(workspace, relative_path, AccessOperation::Read, true)?;
    let metadata = fs::metadata(&path)
        .map_err(|error| format!("Could not inspect the requested file: {error}"))?;
    if !metadata.is_file() {
        return Err("The requested path is not a file.".to_string());
    }
    if project_index::is_probably_binary(&path, metadata.len())? {
        return Err("Binary files are not available through the text range reader.".to_string());
    }

    let modified_at = modified_millis(&metadata);
    let requested_start_line = start_line.unwrap_or(1).max(1);
    let max_lines = max_lines
        .unwrap_or(DEFAULT_RANGE_LINES)
        .clamp(1, MAX_RANGE_LINES);
    let started = Instant::now();
    let deadline = started + RANGE_BUDGET;

    let (mut line_number, mut byte_offset, target_start_line) = if let Some(cursor) = cursor {
        let (cursor_size, cursor_modified, cursor_line, cursor_offset, cursor_target) =
            decode_range_cursor(cursor)?;
        if cursor_size != metadata.len() || cursor_modified != modified_at.unwrap_or(0) {
            return Err(
                "File changed since this range cursor was created; restart the range read."
                    .to_string(),
            );
        }
        (cursor_line, cursor_offset, cursor_target)
    } else {
        (1usize, 0u64, requested_start_line)
    };

    let mut file =
        File::open(&path).map_err(|error| format!("Could not open the requested file: {error}"))?;
    file.seek(SeekFrom::Start(byte_offset))
        .map_err(|error| format!("Could not resume the requested file range: {error}"))?;
    let mut reader = BufReader::new(file);
    let mut output = String::new();
    let mut first_returned_line = None;
    let mut last_returned_line = 0usize;
    let mut returned_lines = 0usize;
    let mut returned_bytes = 0usize;
    let mut truncated_line_count = 0usize;
    let mut eof = false;

    loop {
        if Instant::now() >= deadline {
            break;
        }
        let line_start_offset = byte_offset;
        let line = read_bounded_line(&mut reader, MAX_RANGE_LINE_CAPTURE_BYTES)
            .map_err(|error| format!("Could not read the requested file range: {error}"))?;
        if !line.had_data {
            eof = true;
            break;
        }
        byte_offset = byte_offset.saturating_add(line.consumed as u64);

        if line_number >= target_start_line
            && returned_lines < max_lines
            && returned_bytes < DEFAULT_RANGE_BYTES.min(MAX_RANGE_BYTES)
        {
            if line.captured.contains(&0) {
                return Err("Binary data was encountered while reading text lines.".to_string());
            }
            let text = String::from_utf8_lossy(&line.captured);
            let remaining = MAX_RANGE_BYTES.saturating_sub(returned_bytes);
            if text.len() > remaining {
                byte_offset = line_start_offset;
                break;
            }
            first_returned_line.get_or_insert(line_number);
            output.push_str(&text);
            returned_bytes = returned_bytes.saturating_add(text.len());
            returned_lines = returned_lines.saturating_add(1);
            last_returned_line = line_number;
            if line.truncated {
                truncated_line_count = truncated_line_count.saturating_add(1);
                output.push_str("\n[RepoTunnel: line truncated for bounded read]\n");
            }
        }

        line_number = line_number.saturating_add(1);
        if returned_lines >= max_lines || returned_bytes >= DEFAULT_RANGE_BYTES {
            break;
        }
    }

    let reached_requested_start =
        first_returned_line.is_some() || eof && line_number >= target_start_line;
    let next_cursor = (!eof).then(|| {
        encode_range_cursor(
            metadata.len(),
            modified_at,
            line_number,
            byte_offset,
            target_start_line,
        )
    });

    Ok(FileRangeResult {
        path: relative_path.replace('\\', "/"),
        content: output,
        requested_start_line: target_start_line,
        start_line: first_returned_line.unwrap_or(line_number),
        end_line: last_returned_line,
        next_cursor,
        eof,
        reached_requested_start,
        size: metadata.len(),
        modified_at,
        truncated_line_count,
        elapsed_ms: started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
    })
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DirectoryPageEntry {
    pub(crate) name: String,
    pub(crate) path: String,
    pub(crate) kind: String,
    pub(crate) size: Option<u64>,
    pub(crate) modified_at: Option<u64>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DirectoryPage {
    pub(crate) entries: Vec<DirectoryPageEntry>,
    pub(crate) next_cursor: Option<String>,
    pub(crate) done: bool,
    pub(crate) truncated: bool,
    pub(crate) scanned_entries: usize,
    pub(crate) skipped_entries: usize,
    pub(crate) elapsed_ms: u64,
    pub(crate) busy: bool,
    pub(crate) retry_after_ms: Option<u64>,
}

struct DirectorySession {
    workspace_id: String,
    relative_path: String,
    root: PathBuf,
    directory: PathBuf,
    read_dir: fs::ReadDir,
    exhausted: bool,
    scanned_entries: usize,
    skipped_entries: usize,
    last_access: Instant,
}

static DIRECTORY_SESSIONS: OnceLock<Mutex<HashMap<String, DirectorySession>>> = OnceLock::new();

fn directory_sessions() -> &'static Mutex<HashMap<String, DirectorySession>> {
    DIRECTORY_SESSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn cleanup_directory_sessions(sessions: &mut HashMap<String, DirectorySession>) {
    let now = Instant::now();
    sessions.retain(|_, session| now.duration_since(session.last_access) <= DIRECTORY_SESSION_TTL);
    if sessions.len() > MAX_DIRECTORY_SESSIONS {
        let mut ages = sessions
            .iter()
            .map(|(id, session)| (id.clone(), session.last_access))
            .collect::<Vec<_>>();
        ages.sort_by_key(|(_, at)| *at);
        let remove_count = sessions.len().saturating_sub(MAX_DIRECTORY_SESSIONS);
        for (id, _) in ages.into_iter().take(remove_count) {
            sessions.remove(&id);
        }
    }
}

fn new_directory_session(
    workspace: &Workspace,
    relative_path: &str,
) -> Result<DirectorySession, String> {
    let directory = resolve_workspace_path(workspace, relative_path, AccessOperation::Read, true)?;
    if !directory.is_dir() {
        return Err("The requested path is not a folder.".to_string());
    }
    let root = Path::new(&workspace.path)
        .canonicalize()
        .map_err(|error| format!("Could not resolve the approved workspace: {error}"))?;

    let read_dir = fs::read_dir(&directory)
        .map_err(|error| format!("Could not list the requested folder: {error}"))?;

    Ok(DirectorySession {
        workspace_id: workspace.id.clone(),
        relative_path: relative_path.to_string(),
        root,
        directory,
        read_dir,
        exhausted: false,
        scanned_entries: 0,
        skipped_entries: 0,
        last_access: Instant::now(),
    })
}

pub(crate) fn list_directory_page(
    workspace: &Workspace,
    relative_path: &str,
    cursor: Option<&str>,
    page_size: Option<usize>,
) -> Result<DirectoryPage, String> {
    let page_size = page_size
        .unwrap_or(DEFAULT_DIRECTORY_PAGE_SIZE)
        .clamp(1, MAX_DIRECTORY_PAGE_SIZE);
    let gate = workspace_heavy_read_gate(&workspace.id);
    let Ok(_guard) = gate.try_lock() else {
        return Ok(DirectoryPage {
            entries: Vec::new(),
            next_cursor: cursor.map(str::to_string),
            done: false,
            truncated: true,
            scanned_entries: 0,
            skipped_entries: 0,
            elapsed_ms: 0,
            busy: true,
            retry_after_ms: Some(100),
        });
    };
    let session_id;
    let mut session = if let Some(cursor) = cursor {
        let mut sessions = directory_sessions()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        cleanup_directory_sessions(&mut sessions);
        let session = sessions.remove(cursor).ok_or_else(|| {
            "Directory-page cursor is unknown or expired; start a new directory page.".to_string()
        })?;
        if session.workspace_id != workspace.id || session.relative_path != relative_path {
            sessions.insert(cursor.to_string(), session);
            return Err(
                "Directory-page cursor does not belong to this workspace/path.".to_string(),
            );
        }
        session_id = cursor.to_string();
        session
    } else {
        session_id = next_session_id("dir-page");
        new_directory_session(workspace, relative_path)?
    };

    let started = Instant::now();
    let deadline = started + DIRECTORY_PAGE_BUDGET;
    let mut entries = Vec::new();

    while entries.len() < page_size && Instant::now() < deadline {
        let path = match session.read_dir.next() {
            Some(Ok(entry)) => entry.path(),
            Some(Err(_)) => {
                session.skipped_entries = session.skipped_entries.saturating_add(1);
                continue;
            }
            None => {
                session.exhausted = true;
                break;
            }
        };
        session.scanned_entries = session.scanned_entries.saturating_add(1);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(_) => {
                session.skipped_entries = session.skipped_entries.saturating_add(1);
                continue;
            }
        };
        if metadata.file_type().is_symlink() {
            session.skipped_entries = session.skipped_entries.saturating_add(1);
            continue;
        }
        if !project_index::should_include_entry(
            workspace,
            &session.directory,
            &path,
            metadata.is_dir(),
        )? {
            session.skipped_entries = session.skipped_entries.saturating_add(1);
            continue;
        }
        let relative = workspace_relative_path(&session.root, &path);
        if resolve_workspace_path(workspace, &relative, AccessOperation::Read, true).is_err() {
            session.skipped_entries = session.skipped_entries.saturating_add(1);
            continue;
        }

        let kind = if metadata.is_dir() {
            "directory"
        } else if metadata.is_file() {
            "file"
        } else {
            "other"
        };
        entries.push(DirectoryPageEntry {
            name: path
                .file_name()
                .map(|value| value.to_string_lossy().into_owned())
                .unwrap_or_default(),
            path: relative,
            kind: kind.to_string(),
            size: metadata.is_file().then_some(metadata.len()),
            modified_at: modified_millis(&metadata),
        });
    }

    entries.sort_by(|left, right| {
        let left_dir = left.kind == "directory";
        let right_dir = right.kind == "directory";
        right_dir
            .cmp(&left_dir)
            .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
    });

    let done = session.exhausted;
    let next_cursor = (!done).then(|| session_id.clone());
    session.last_access = Instant::now();
    let result = DirectoryPage {
        entries,
        next_cursor: next_cursor.clone(),
        done,
        truncated: !done,
        scanned_entries: session.scanned_entries,
        skipped_entries: session.skipped_entries,
        elapsed_ms: started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
        busy: false,
        retry_after_ms: None,
    };

    if !done {
        let mut sessions = directory_sessions()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        cleanup_directory_sessions(&mut sessions);
        sessions.insert(session_id, session);
    }
    Ok(result)
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProjectTreePageEntry {
    pub(crate) path: String,
    pub(crate) kind: String,
    pub(crate) size: Option<u64>,
    pub(crate) modified_at: Option<u64>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProjectTreePage {
    pub(crate) entries: Vec<ProjectTreePageEntry>,
    pub(crate) next_cursor: Option<String>,
    pub(crate) done: bool,
    pub(crate) truncated: bool,
    pub(crate) scanned_entries: usize,
    pub(crate) skipped_entries: usize,
    pub(crate) elapsed_ms: u64,
    pub(crate) busy: bool,
    pub(crate) retry_after_ms: Option<u64>,
}

struct TreeDirectoryBatch {
    directory: PathBuf,
    entries: fs::ReadDir,
}

struct ProjectTreeSession {
    workspace_id: String,
    relative_path: String,
    root: PathBuf,
    queue: VecDeque<PathBuf>,
    current: Option<TreeDirectoryBatch>,
    scanned_entries: usize,
    skipped_entries: usize,
    last_access: Instant,
}

const DEFAULT_TREE_PAGE_SIZE: usize = 300;
const MAX_TREE_PAGE_SIZE: usize = 800;
const TREE_PAGE_BUDGET: Duration = Duration::from_millis(900);
const TREE_SESSION_TTL: Duration = Duration::from_secs(180);
const MAX_TREE_SESSIONS: usize = 32;

static TREE_SESSIONS: OnceLock<Mutex<HashMap<String, ProjectTreeSession>>> = OnceLock::new();

fn tree_sessions() -> &'static Mutex<HashMap<String, ProjectTreeSession>> {
    TREE_SESSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn cleanup_tree_sessions(sessions: &mut HashMap<String, ProjectTreeSession>) {
    let now = Instant::now();
    sessions.retain(|_, session| now.duration_since(session.last_access) <= TREE_SESSION_TTL);
    if sessions.len() > MAX_TREE_SESSIONS {
        let mut ages = sessions
            .iter()
            .map(|(id, session)| (id.clone(), session.last_access))
            .collect::<Vec<_>>();
        ages.sort_by_key(|(_, at)| *at);
        let remove_count = sessions.len().saturating_sub(MAX_TREE_SESSIONS);
        for (id, _) in ages.into_iter().take(remove_count) {
            sessions.remove(&id);
        }
    }
}

fn new_tree_session(
    workspace: &Workspace,
    relative_path: &str,
) -> Result<ProjectTreeSession, String> {
    let start = resolve_workspace_path(workspace, relative_path, AccessOperation::Read, true)?;
    if !start.is_dir() {
        return Err("Paged project inspection must start from a folder.".to_string());
    }
    let root = Path::new(&workspace.path)
        .canonicalize()
        .map_err(|error| format!("Could not resolve the approved workspace: {error}"))?;
    Ok(ProjectTreeSession {
        workspace_id: workspace.id.clone(),
        relative_path: relative_path.to_string(),
        root,
        queue: VecDeque::from([start]),
        current: None,
        scanned_entries: 0,
        skipped_entries: 0,
        last_access: Instant::now(),
    })
}

fn load_tree_directory(session: &mut ProjectTreeSession) -> Result<bool, String> {
    loop {
        let Some(directory) = session.queue.pop_front() else {
            return Ok(false);
        };
        let read_dir = match fs::read_dir(&directory) {
            Ok(read_dir) => read_dir,
            Err(_) => {
                session.skipped_entries = session.skipped_entries.saturating_add(1);
                continue;
            }
        };

        session.current = Some(TreeDirectoryBatch {
            directory,
            entries: read_dir,
        });
        return Ok(true);
    }
}

pub(crate) fn inspect_project_page(
    workspace: &Workspace,
    relative_path: &str,
    cursor: Option<&str>,
    page_size: Option<usize>,
) -> Result<ProjectTreePage, String> {
    let page_size = page_size
        .unwrap_or(DEFAULT_TREE_PAGE_SIZE)
        .clamp(1, MAX_TREE_PAGE_SIZE);
    let gate = workspace_heavy_read_gate(&workspace.id);
    let Ok(_guard) = gate.try_lock() else {
        return Ok(ProjectTreePage {
            entries: Vec::new(),
            next_cursor: cursor.map(str::to_string),
            done: false,
            truncated: true,
            scanned_entries: 0,
            skipped_entries: 0,
            elapsed_ms: 0,
            busy: true,
            retry_after_ms: Some(100),
        });
    };
    let session_id;
    let mut session = if let Some(cursor) = cursor {
        let mut sessions = tree_sessions()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        cleanup_tree_sessions(&mut sessions);
        let session = sessions.remove(cursor).ok_or_else(|| {
            "Project-tree cursor is unknown or expired; start a new paged inspection.".to_string()
        })?;
        if session.workspace_id != workspace.id || session.relative_path != relative_path {
            sessions.insert(cursor.to_string(), session);
            return Err("Project-tree cursor does not belong to this workspace/path.".to_string());
        }
        session_id = cursor.to_string();
        session
    } else {
        session_id = next_session_id("project-tree");
        new_tree_session(workspace, relative_path)?
    };

    let started = Instant::now();
    let deadline = started + TREE_PAGE_BUDGET;
    let mut entries = Vec::new();

    while entries.len() < page_size && Instant::now() < deadline {
        if session.current.is_none() && !load_tree_directory(&mut session)? {
            break;
        }

        let (parent_directory, path) = {
            let Some(batch) = &mut session.current else {
                break;
            };
            match batch.entries.next() {
                Some(Ok(entry)) => (batch.directory.clone(), entry.path()),
                Some(Err(_)) => {
                    session.skipped_entries = session.skipped_entries.saturating_add(1);
                    continue;
                }
                None => {
                    session.current = None;
                    continue;
                }
            }
        };
        session.scanned_entries = session.scanned_entries.saturating_add(1);

        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(_) => {
                session.skipped_entries = session.skipped_entries.saturating_add(1);
                continue;
            }
        };
        if metadata.file_type().is_symlink() {
            session.skipped_entries = session.skipped_entries.saturating_add(1);
            continue;
        }
        if !project_index::should_include_entry(
            workspace,
            &parent_directory,
            &path,
            metadata.is_dir(),
        )? {
            session.skipped_entries = session.skipped_entries.saturating_add(1);
            continue;
        }

        let relative = workspace_relative_path(&session.root, &path);
        if resolve_workspace_path(workspace, &relative, AccessOperation::Read, true).is_err() {
            session.skipped_entries = session.skipped_entries.saturating_add(1);
            continue;
        }

        if metadata.is_dir() {
            session.queue.push_back(path.clone());
        }
        let kind = if metadata.is_dir() {
            "directory"
        } else if metadata.is_file() {
            "file"
        } else {
            "other"
        };
        entries.push(ProjectTreePageEntry {
            path: relative,
            kind: kind.to_string(),
            size: metadata.is_file().then_some(metadata.len()),
            modified_at: modified_millis(&metadata),
        });
    }

    let done = session.current.is_none() && session.queue.is_empty();
    let next_cursor = (!done).then(|| session_id.clone());
    session.last_access = Instant::now();
    let result = ProjectTreePage {
        entries,
        next_cursor: next_cursor.clone(),
        done,
        truncated: !done,
        scanned_entries: session.scanned_entries,
        skipped_entries: session.skipped_entries,
        elapsed_ms: started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
        busy: false,
        retry_after_ms: None,
    };

    if !done {
        let mut sessions = tree_sessions()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        cleanup_tree_sessions(&mut sessions);
        sessions.insert(session_id, session);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::{fast_search_page, inspect_project_page, list_directory_page, read_file_range};
    use crate::models::{CommandPolicy, Workspace, WorkspaceAccessMode, WorkspaceChangePolicy};

    fn temp_workspace() -> (PathBuf, Workspace) {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("repotunnel-large-read-{nonce}"));
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("src/app.rs"),
            (1..=500)
                .map(|line| format!("line {line} needle\n"))
                .collect::<String>(),
        )
        .unwrap();
        fs::write(root.join("src/other.rs"), "hello\nneedle here\n").unwrap();
        fs::write(root.join(".gitignore"), "ignored/\n").unwrap();
        fs::create_dir_all(root.join("ignored")).unwrap();
        fs::write(root.join("ignored/hidden.rs"), "needle hidden\n").unwrap();

        let workspace = Workspace {
            id: format!("test-{nonce}"),
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
    fn range_reads_large_text_in_pages() {
        let (root, workspace) = temp_workspace();
        let first = read_file_range(&workspace, "src/app.rs", Some(250), Some(20), None).unwrap();
        assert!(first.content.contains("line 250 needle"));
        assert!(first.end_line >= 269);
        assert!(first.next_cursor.is_some());

        let second = read_file_range(
            &workspace,
            "src/app.rs",
            Some(250),
            Some(20),
            first.next_cursor.as_deref(),
        )
        .unwrap();
        assert!(second.start_line > first.end_line);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn fast_search_continues_with_cursor_without_restarting() {
        let (root, workspace) = temp_workspace();
        let first = fast_search_page(&workspace, "", "needle", None, Some(5), Some(500)).unwrap();
        assert_eq!(first.matches.len(), 5);
        assert!(first.next_cursor.is_some());
        let first_total = first.total_files_searched;

        let second = fast_search_page(
            &workspace,
            "",
            "needle",
            first.next_cursor.as_deref(),
            Some(5),
            Some(500),
        )
        .unwrap();
        assert_eq!(second.matches.len(), 5);
        assert!(second.total_files_searched >= first_total);
        assert!(second
            .matches
            .iter()
            .all(|item| !item.path.contains("ignored/")));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn immediate_duplicate_search_reuses_recent_first_page() {
        let (root, workspace) = temp_workspace();
        let first = fast_search_page(&workspace, "", "needle", None, Some(4), Some(500)).unwrap();
        let retry = fast_search_page(&workspace, "", "needle", None, Some(4), Some(500)).unwrap();
        assert_eq!(first.matches.len(), retry.matches.len());
        assert_eq!(first.next_cursor, retry.next_cursor);
        assert!(retry.reused_recent_result);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn changed_file_invalidates_range_cursor() {
        let (root, workspace) = temp_workspace();
        let first = read_file_range(&workspace, "src/app.rs", Some(1), Some(5), None).unwrap();
        let cursor = first.next_cursor.expect("range cursor");
        fs::write(root.join("src/app.rs"), "changed size and content\n").unwrap();
        let error =
            read_file_range(&workspace, "src/app.rs", Some(1), Some(5), Some(&cursor)).unwrap_err();
        assert!(error.contains("File changed"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn project_tree_is_paginated_and_resumable() {
        let (root, workspace) = temp_workspace();
        for directory in 0..8 {
            let path = root.join("src").join(format!("dir-{directory}"));
            fs::create_dir_all(&path).unwrap();
            for file in 0..8 {
                fs::write(path.join(format!("{file}.txt")), "x").unwrap();
            }
        }
        let first = inspect_project_page(&workspace, "", None, Some(10)).unwrap();
        assert_eq!(first.entries.len(), 10);
        assert!(first.next_cursor.is_some());

        let second =
            inspect_project_page(&workspace, "", first.next_cursor.as_deref(), Some(10)).unwrap();
        assert_eq!(second.entries.len(), 10);
        assert!(second.scanned_entries > first.scanned_entries);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    #[ignore = "manual synthetic large-project stress test"]
    fn synthetic_large_project_returns_bounded_search_page() {
        let (root, workspace) = temp_workspace();
        let bulk = root.join("bulk");
        fs::create_dir_all(&bulk).unwrap();
        for directory in 0..50 {
            let folder = bulk.join(format!("part-{directory:02}"));
            fs::create_dir_all(&folder).unwrap();
            for file in 0..100 {
                let body = if directory == 49 && file == 99 {
                    "needle-at-the-end\n"
                } else {
                    "ordinary project text\n"
                };
                fs::write(folder.join(format!("{file:03}.txt")), body).unwrap();
            }
        }

        let page = fast_search_page(
            &workspace,
            "",
            "needle-at-the-end",
            None,
            Some(20),
            Some(100),
        )
        .unwrap();
        assert!(page.done || page.next_cursor.is_some());
        assert!(page.elapsed_ms <= 1_500);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn directory_listing_is_paginated_instead_of_hard_failing() {
        let (root, workspace) = temp_workspace();
        for index in 0..30 {
            fs::write(root.join("src").join(format!("{index:03}.txt")), "x").unwrap();
        }
        let first = list_directory_page(&workspace, "src", None, Some(10)).unwrap();
        assert_eq!(first.entries.len(), 10);
        assert!(first.next_cursor.is_some());
        let second =
            list_directory_page(&workspace, "src", first.next_cursor.as_deref(), Some(10)).unwrap();
        assert_eq!(second.entries.len(), 10);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn shared_heavy_read_gate_prevents_overlapping_scans() {
        let (root, workspace) = temp_workspace();
        let gate = super::workspace_heavy_read_gate(&workspace.id);
        let guard = gate.lock().unwrap();

        let search = fast_search_page(&workspace, "", "needle", None, Some(5), Some(500)).unwrap();
        let directory = list_directory_page(&workspace, "src", None, Some(10)).unwrap();
        let tree = inspect_project_page(&workspace, "", None, Some(10)).unwrap();

        assert!(search.busy);
        assert!(directory.busy);
        assert!(tree.busy);
        assert_eq!(search.retry_after_ms, Some(100));
        assert_eq!(directory.retry_after_ms, Some(100));
        assert_eq!(tree.retry_after_ms, Some(100));

        drop(guard);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    #[ignore = "manual huge single-directory paging stress test"]
    fn huge_single_directory_is_paged_without_prebuffering() {
        let (root, workspace) = temp_workspace();
        let bulk = root.join("bulk-flat");
        fs::create_dir_all(&bulk).unwrap();
        for index in 0..20_000 {
            fs::write(bulk.join(format!("{index:05}.txt")), "x").unwrap();
        }

        let directory = list_directory_page(&workspace, "bulk-flat", None, Some(25)).unwrap();
        assert_eq!(directory.entries.len(), 25);
        assert!(directory.next_cursor.is_some());
        assert!(directory.elapsed_ms <= 1_000);

        let tree = inspect_project_page(&workspace, "bulk-flat", None, Some(25)).unwrap();
        assert_eq!(tree.entries.len(), 25);
        assert!(tree.next_cursor.is_some());
        assert!(tree.elapsed_ms <= 1_500);

        let _ = fs::remove_dir_all(root);
    }
}
