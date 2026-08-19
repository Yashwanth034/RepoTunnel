use std::{
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Read, Write},
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

use crate::{
    access::{resolve_workspace_path, AccessOperation},
    models::{DirectoryEntry, FileContent, FileInfo, SearchMatch, Workspace},
    project_index,
};

const MAX_DIRECTORY_ENTRIES: usize = 1_000;
const MAX_READ_BYTES: u64 = 1_048_576;
const MAX_WRITE_BYTES: usize = 2_097_152;
const MAX_SEARCH_FILE_BYTES: u64 = 1_048_576;
const MAX_SEARCH_FILES: usize = 10_000;
const MAX_SEARCH_RESULTS: usize = 200;
const MAX_SEARCH_LINE_BYTES: usize = 16_384;
const SEARCH_PREVIEW_CHARS: usize = 240;

fn workspace_relative_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn modified_millis(metadata: &fs::Metadata) -> Option<u64> {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
}

fn file_kind(metadata: &fs::Metadata) -> &'static str {
    let kind = metadata.file_type();
    if kind.is_symlink() {
        "symlink"
    } else if kind.is_dir() {
        "directory"
    } else if kind.is_file() {
        "file"
    } else {
        "other"
    }
}

fn ensure_not_workspace_root(path: &str) -> Result<(), String> {
    if path.trim().is_empty() || path == "." {
        return Err("The workspace root cannot be modified or deleted.".to_string());
    }
    Ok(())
}

fn ensure_leaf_is_not_symlink(path: &Path) -> Result<fs::Metadata, String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("Could not inspect the requested path: {error}"))?;

    if metadata.file_type().is_symlink() {
        return Err("Write and destructive operations do not follow symbolic links.".to_string());
    }

    Ok(metadata)
}

fn ensure_parent_directory(path: &Path) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "Could not resolve the destination folder.".to_string())?;

    let metadata = fs::metadata(parent)
        .map_err(|error| format!("Could not inspect the destination folder: {error}"))?;

    if !metadata.is_dir() {
        return Err("The destination parent is not a folder.".to_string());
    }

    Ok(())
}

fn ensure_content_size(content: &str) -> Result<(), String> {
    if content.len() > MAX_WRITE_BYTES {
        return Err(format!(
            "File content exceeds the {} MiB write limit.",
            MAX_WRITE_BYTES / 1_048_576
        ));
    }
    Ok(())
}

fn validate_text_content(bytes: &[u8]) -> Result<String, String> {
    if bytes.contains(&0) {
        return Err("Binary files are not available through the text file tool.".to_string());
    }

    String::from_utf8(bytes.to_vec()).map_err(|_| "This file is not valid UTF-8 text.".to_string())
}

fn atomic_write(path: &Path, content: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "Could not resolve the file's parent folder.".to_string())?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| "The file name is not valid UTF-8.".to_string())?;

    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "System time is unavailable.".to_string())?
        .as_nanos();
    let temporary = parent.join(format!(".{file_name}.repotunnel-{nonce:x}.tmp"));

    let existing_permissions = fs::metadata(path)
        .ok()
        .map(|metadata| metadata.permissions());

    let result = (|| -> Result<(), String> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|error| format!("Could not create a temporary file: {error}"))?;

        file.write_all(content)
            .map_err(|error| format!("Could not write the file: {error}"))?;
        file.sync_all()
            .map_err(|error| format!("Could not flush the file safely: {error}"))?;

        if let Some(permissions) = existing_permissions.clone() {
            fs::set_permissions(&temporary, permissions)
                .map_err(|error| format!("Could not preserve file permissions: {error}"))?;
        }

        fs::rename(&temporary, path)
            .map_err(|error| format!("Could not replace the file: {error}"))?;
        Ok(())
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }

    result
}

pub(crate) fn list_directory(
    workspace: &Workspace,
    relative_path: &str,
) -> Result<Vec<DirectoryEntry>, String> {
    let path = resolve_workspace_path(workspace, relative_path, AccessOperation::Read, true)?;
    let root = Path::new(&workspace.path)
        .canonicalize()
        .map_err(|error| format!("Could not resolve the approved workspace: {error}"))?;

    if !path.is_dir() {
        return Err("The requested path is not a folder.".to_string());
    }
    if path != root {
        let parent = path.parent().unwrap_or(&root);
        if !project_index::should_include_entry(workspace, parent, &path, true)? {
            return Err("That folder is excluded from the smart project view.".to_string());
        }
    }

    let mut entries = Vec::new();
    let read_dir = fs::read_dir(&path)
        .map_err(|error| format!("Could not list the requested folder: {error}"))?;

    for entry in read_dir {
        let entry = entry.map_err(|error| format!("Could not read a folder entry: {error}"))?;
        let entry_path = entry.path();
        let entry_metadata = fs::symlink_metadata(&entry_path)
            .map_err(|error| format!("Could not inspect a folder entry: {error}"))?;
        if !project_index::should_include_entry(
            workspace,
            &path,
            &entry_path,
            entry_metadata.is_dir(),
        )? {
            continue;
        }
        let relative = workspace_relative_path(&root, &entry_path);

        if resolve_workspace_path(workspace, &relative, AccessOperation::Read, true).is_err() {
            continue;
        }

        if entries.len() >= MAX_DIRECTORY_ENTRIES {
            return Err(format!(
                "This folder contains more than {MAX_DIRECTORY_ENTRIES} accessible entries. Narrow the request to a subfolder."
            ));
        }

        let name = entry.file_name().to_string_lossy().into_owned();

        entries.push(DirectoryEntry {
            name,
            path: relative,
            kind: file_kind(&entry_metadata).to_string(),
            size: entry_metadata.is_file().then_some(entry_metadata.len()),
            modified_at: modified_millis(&entry_metadata),
        });
    }

    entries.sort_by(|left, right| {
        let left_dir = left.kind == "directory";
        let right_dir = right.kind == "directory";
        right_dir
            .cmp(&left_dir)
            .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
    });

    Ok(entries)
}

pub(crate) fn read_file(workspace: &Workspace, relative_path: &str) -> Result<FileContent, String> {
    let path = resolve_workspace_path(workspace, relative_path, AccessOperation::Read, true)?;
    let metadata = fs::metadata(&path)
        .map_err(|error| format!("Could not inspect the requested file: {error}"))?;

    if !metadata.is_file() {
        return Err("The requested path is not a file.".to_string());
    }

    if metadata.len() > MAX_READ_BYTES {
        return Err(format!(
            "The file is larger than the {} MiB text-read limit.",
            MAX_READ_BYTES / 1_048_576
        ));
    }

    if project_index::is_probably_binary(&path, metadata.len())? {
        return Err("Binary files are not available through the text file tool.".to_string());
    }

    let file = File::open(&path).map_err(|error| format!("Could not open the file: {error}"))?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_READ_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("Could not read the file: {error}"))?;
    if bytes.len() as u64 > MAX_READ_BYTES {
        return Err(format!(
            "The file is larger than the {} MiB text-read limit.",
            MAX_READ_BYTES / 1_048_576
        ));
    }
    let content = validate_text_content(&bytes)?;

    Ok(FileContent {
        path: relative_path.replace('\\', "/"),
        content,
        size: metadata.len(),
        modified_at: modified_millis(&metadata),
    })
}

fn search_file(
    path: &Path,
    relative_path: &str,
    query_lower: &str,
    matches: &mut Vec<SearchMatch>,
) -> Result<(), String> {
    let metadata = fs::metadata(path)
        .map_err(|error| format!("Could not inspect a file during search: {error}"))?;

    if !metadata.is_file() || metadata.len() > MAX_SEARCH_FILE_BYTES {
        return Ok(());
    }

    let file = File::open(path)
        .map_err(|error| format!("Could not open a file during search: {error}"))?;
    let reader = BufReader::new(file);

    for (index, line_result) in reader.split(b'\n').enumerate() {
        let mut bytes =
            line_result.map_err(|error| format!("Could not read a file during search: {error}"))?;
        if bytes.len() > MAX_SEARCH_LINE_BYTES || bytes.contains(&0) {
            continue;
        }
        if bytes.last() == Some(&b'\r') {
            bytes.pop();
        }
        let Ok(line) = String::from_utf8(bytes) else {
            continue;
        };
        let lower = line.to_lowercase();
        let Some(byte_index) = lower.find(query_lower) else {
            continue;
        };

        let column = lower[..byte_index].chars().count() + 1;
        let mut preview: String = line.chars().take(SEARCH_PREVIEW_CHARS).collect();
        if line.chars().count() > SEARCH_PREVIEW_CHARS {
            preview.push('…');
        }

        matches.push(SearchMatch {
            path: relative_path.to_string(),
            line: index + 1,
            column,
            preview,
        });

        if matches.len() >= MAX_SEARCH_RESULTS {
            break;
        }
    }

    Ok(())
}

pub(crate) fn search_files(
    workspace: &Workspace,
    relative_path: &str,
    query: &str,
) -> Result<Vec<SearchMatch>, String> {
    let query = query.trim();
    if query.is_empty() {
        return Err("Search query cannot be empty.".to_string());
    }
    if query.chars().count() > 256 {
        return Err("Search query is too long.".to_string());
    }

    let root = Path::new(&workspace.path)
        .canonicalize()
        .map_err(|error| format!("Could not resolve the approved workspace: {error}"))?;
    let query_lower = query.to_lowercase();
    let mut matches = Vec::new();
    let files = project_index::smart_text_files(workspace, relative_path, MAX_SEARCH_FILES + 1)?;

    if files.len() > MAX_SEARCH_FILES {
        return Err(format!(
            "Search found more than {MAX_SEARCH_FILES} relevant text files. Narrow the search folder."
        ));
    }

    for path in files {
        let relative = workspace_relative_path(&root, &path);
        search_file(&path, &relative, &query_lower, &mut matches)?;
        if matches.len() >= MAX_SEARCH_RESULTS {
            break;
        }
    }

    Ok(matches)
}

pub(crate) fn create_file(
    workspace: &Workspace,
    relative_path: &str,
    content: &str,
) -> Result<FileInfo, String> {
    ensure_not_workspace_root(relative_path)?;
    ensure_content_size(content)?;
    let path = resolve_workspace_path(workspace, relative_path, AccessOperation::Write, false)?;

    if path.exists() {
        return Err("A file or folder already exists at the destination path.".to_string());
    }

    ensure_parent_directory(&path)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|error| format!("Could not create the file: {error}"))?;
    file.write_all(content.as_bytes())
        .map_err(|error| format!("Could not write the new file: {error}"))?;
    file.sync_all()
        .map_err(|error| format!("Could not flush the new file safely: {error}"))?;

    file_info(workspace, relative_path)
}

pub(crate) fn write_file(
    workspace: &Workspace,
    relative_path: &str,
    content: &str,
) -> Result<FileInfo, String> {
    ensure_not_workspace_root(relative_path)?;
    ensure_content_size(content)?;
    let path = resolve_workspace_path(workspace, relative_path, AccessOperation::Write, true)?;
    let metadata = ensure_leaf_is_not_symlink(&path)?;

    if !metadata.is_file() {
        return Err("The requested path is not a file.".to_string());
    }

    atomic_write(&path, content.as_bytes())?;
    file_info(workspace, relative_path)
}

pub(crate) fn patch_file(
    workspace: &Workspace,
    relative_path: &str,
    expected: &str,
    replacement: &str,
) -> Result<FileInfo, String> {
    if expected.is_empty() {
        return Err("Patch expected text cannot be empty.".to_string());
    }

    let current = read_file(workspace, relative_path)?;
    let occurrences = current.content.matches(expected).count();

    match occurrences {
        0 => return Err("The expected text was not found, so no patch was applied.".to_string()),
        1 => {}
        count => {
            return Err(format!(
                "The expected text appears {count} times. Provide a more specific patch context."
            ))
        }
    }

    let updated = current.content.replacen(expected, replacement, 1);
    ensure_content_size(&updated)?;
    write_file(workspace, relative_path, &updated)
}

pub(crate) fn create_directory(
    workspace: &Workspace,
    relative_path: &str,
    recursive: bool,
) -> Result<FileInfo, String> {
    ensure_not_workspace_root(relative_path)?;
    let path = resolve_workspace_path(workspace, relative_path, AccessOperation::Write, false)?;

    if path.exists() {
        return Err("A file or folder already exists at the destination path.".to_string());
    }

    if recursive {
        fs::create_dir_all(&path)
            .map_err(|error| format!("Could not create the folder: {error}"))?;
    } else {
        ensure_parent_directory(&path)?;
        fs::create_dir(&path).map_err(|error| format!("Could not create the folder: {error}"))?;
    }

    file_info(workspace, relative_path)
}

fn validate_new_name(new_name: &str) -> Result<(), String> {
    let path = Path::new(new_name);
    if new_name.trim().is_empty()
        || path.is_absolute()
        || path.components().count() != 1
        || new_name == "."
        || new_name == ".."
    {
        return Err("The new name must be a single file or folder name.".to_string());
    }
    Ok(())
}

pub(crate) fn rename_entry(
    workspace: &Workspace,
    relative_path: &str,
    new_name: &str,
) -> Result<FileInfo, String> {
    ensure_not_workspace_root(relative_path)?;
    validate_new_name(new_name)?;

    let source = resolve_workspace_path(workspace, relative_path, AccessOperation::Write, true)?;
    ensure_leaf_is_not_symlink(&source)?;
    let parent = source
        .parent()
        .ok_or_else(|| "Could not resolve the source parent folder.".to_string())?;
    let destination = parent.join(new_name);

    if destination.exists() {
        return Err("A file or folder already exists with that name.".to_string());
    }

    let root = Path::new(&workspace.path)
        .canonicalize()
        .map_err(|error| format!("Could not resolve the approved workspace: {error}"))?;
    let destination_relative = workspace_relative_path(&root, &destination);
    resolve_workspace_path(
        workspace,
        &destination_relative,
        AccessOperation::Write,
        false,
    )?;

    fs::rename(&source, &destination)
        .map_err(|error| format!("Could not rename the requested path: {error}"))?;

    file_info(workspace, &destination_relative)
}

pub(crate) fn move_entry(
    workspace: &Workspace,
    source_path: &str,
    destination_path: &str,
) -> Result<FileInfo, String> {
    ensure_not_workspace_root(source_path)?;
    ensure_not_workspace_root(destination_path)?;

    let source = resolve_workspace_path(workspace, source_path, AccessOperation::Write, true)?;
    ensure_leaf_is_not_symlink(&source)?;
    let destination =
        resolve_workspace_path(workspace, destination_path, AccessOperation::Write, false)?;

    if destination.exists() {
        return Err("A file or folder already exists at the destination path.".to_string());
    }
    ensure_parent_directory(&destination)?;

    fs::rename(&source, &destination)
        .map_err(|error| format!("Could not move the requested path: {error}"))?;

    file_info(workspace, destination_path)
}

pub(crate) fn delete_entry(
    workspace: &Workspace,
    relative_path: &str,
    recursive: bool,
) -> Result<(), String> {
    ensure_not_workspace_root(relative_path)?;
    let path = resolve_workspace_path(workspace, relative_path, AccessOperation::Write, true)?;
    let metadata = ensure_leaf_is_not_symlink(&path)?;

    if metadata.is_file() {
        fs::remove_file(&path).map_err(|error| format!("Could not delete the file: {error}"))?;
        return Ok(());
    }

    if metadata.is_dir() {
        if recursive {
            fs::remove_dir_all(&path)
                .map_err(|error| format!("Could not delete the folder: {error}"))?;
        } else {
            fs::remove_dir(&path).map_err(|error| {
                format!("Could not delete the folder. It may not be empty: {error}")
            })?;
        }
        return Ok(());
    }

    Err("The requested path is not a regular file or folder.".to_string())
}

pub(crate) fn file_info(workspace: &Workspace, relative_path: &str) -> Result<FileInfo, String> {
    let path = resolve_workspace_path(workspace, relative_path, AccessOperation::Read, true)?;
    let metadata = fs::symlink_metadata(&path)
        .map_err(|error| format!("Could not inspect the requested path: {error}"))?;

    Ok(FileInfo {
        path: relative_path.replace('\\', "/"),
        kind: file_kind(&metadata).to_string(),
        size: metadata.is_file().then_some(metadata.len()),
        modified_at: modified_millis(&metadata),
        readonly: metadata.permissions().readonly(),
    })
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::{
        create_directory, create_file, delete_entry, list_directory, patch_file, read_file,
        search_files, write_file,
    };
    use crate::models::{CommandPolicy, Workspace, WorkspaceAccessMode, WorkspaceChangePolicy};

    fn temp_workspace() -> (PathBuf, Workspace) {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("repotunnel-files-{nonce}"));
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/app.ts"), "const greeting = 'hello';\n").unwrap();

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
    fn reads_and_patches_text_files() {
        let (root, workspace) = temp_workspace();
        let before = read_file(&workspace, "src/app.ts").unwrap();
        assert!(before.content.contains("hello"));

        patch_file(&workspace, "src/app.ts", "hello", "world").unwrap();
        let after = read_file(&workspace, "src/app.ts").unwrap();
        assert!(after.content.contains("world"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn creates_writes_searches_and_deletes_files() {
        let (root, workspace) = temp_workspace();
        create_file(&workspace, "src/new.ts", "export const value = 1;\n").unwrap();
        write_file(&workspace, "src/new.ts", "export const value = 2;\n").unwrap();

        let matches = search_files(&workspace, "src", "value = 2").unwrap();
        assert_eq!(matches.len(), 1);

        delete_entry(&workspace, "src/new.ts", false).unwrap();
        assert!(!root.join("src/new.ts").exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn creates_directories_and_hides_protected_files() {
        let (root, workspace) = temp_workspace();
        fs::write(root.join(".env"), "TOKEN=secret\n").unwrap();
        create_directory(&workspace, "src/generated", false).unwrap();

        let entries = list_directory(&workspace, "").unwrap();
        assert!(!entries.iter().any(|entry| entry.name == ".env"));
        assert!(root.join("src/generated").is_dir());

        let matches = search_files(&workspace, "", "TOKEN=secret").unwrap();
        assert!(matches.is_empty());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn ambiguous_patch_is_rejected() {
        let (root, workspace) = temp_workspace();
        fs::write(root.join("src/repeated.txt"), "same same").unwrap();
        let result = patch_file(&workspace, "src/repeated.txt", "same", "changed");

        assert!(result.is_err());
        assert_eq!(
            fs::read_to_string(root.join("src/repeated.txt")).unwrap(),
            "same same"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn destructive_operations_reject_symlink_targets() {
        use std::os::unix::fs::symlink;

        let (root, workspace) = temp_workspace();
        symlink(root.join("src/app.ts"), root.join("src/link.ts")).unwrap();
        let result = delete_entry(&workspace, "src/link.ts", false);

        assert!(result.is_err());
        assert!(root.join("src/app.ts").exists());
        let _ = fs::remove_dir_all(root);
    }
}
