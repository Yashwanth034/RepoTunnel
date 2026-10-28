//! Windows human-source installer. No pathname-based last-mile mutation.
//! NtCreateFile with OBJECT_ATTRIBUTES.RootDirectory walks only already-opened
//! directory handles; FileLinkInformation creates the final name atomically
//! without replacement. The receiver saves dispatchUnknown BEFORE calling us.
#![cfg(windows)]

use sha2::{Digest, Sha256};
use std::{
    ffi::c_void,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    mem::{offset_of, size_of, MaybeUninit},
    os::windows::{
        fs::{MetadataExt, OpenOptionsExt},
        io::{AsRawHandle, FromRawHandle},
    },
    ptr,
};
use windows_sys::Win32::Storage::FileSystem::{
    GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
};

use crate::{
    access::{resolve_workspace_path, AccessOperation},
    models::Workspace,
};

type Handle = *mut c_void;
const SYNCHRONIZE: u32 = 0x0010_0000;
const FILE_READ_DATA: u32 = 0x0001;
const FILE_WRITE_DATA: u32 = 0x0002;
const FILE_ADD_SUBDIRECTORY: u32 = 0x0004;
const FILE_TRAVERSE: u32 = 0x0020;
const FILE_READ_ATTRIBUTES: u32 = 0x0080;
const FILE_WRITE_ATTRIBUTES: u32 = 0x0100;
const DELETE_ACCESS: u32 = 0x0001_0000;
// Do not share DELETE on pinned destination directories: a cooperating
// Windows filesystem cannot rename a parent while this handle remains open.
const FILE_SHARE_READ_WRITE: u32 = 0x0003;
const FILE_OPEN: u32 = 1;
const FILE_CREATE: u32 = 2;
const FILE_DIRECTORY_FILE: u32 = 0x0000_0001;
const FILE_NON_DIRECTORY_FILE: u32 = 0x0000_0040;
const FILE_SYNCHRONOUS_IO_NONALERT: u32 = 0x0000_0020;
const FILE_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0010;
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
const OBJ_CASE_INSENSITIVE: u32 = 0x0040;
const OBJ_DONT_REPARSE: u32 = 0x1000;
const STATUS_OBJECT_NAME_NOT_FOUND: u32 = 0xc0000034;
const STATUS_OBJECT_PATH_NOT_FOUND: u32 = 0xc000003a;
const STATUS_OBJECT_NAME_COLLISION: u32 = 0xc0000035;
const FILE_LINK_INFORMATION_CLASS: u32 = 11;
const FILE_DISPOSITION_INFORMATION_CLASS: u32 = 13;

#[repr(C)]
struct UnicodeString {
    length: u16,
    maximum_length: u16,
    buffer: *mut u16,
}
#[repr(C)]
struct ObjectAttributes {
    length: u32,
    root_directory: Handle,
    object_name: *mut UnicodeString,
    attributes: u32,
    security_descriptor: *mut c_void,
    security_quality_of_service: *mut c_void,
}
#[repr(C)]
struct IoStatusBlock {
    status_or_pointer: usize,
    information: usize,
}
#[repr(C)]
struct FileLinkInformation {
    replace_if_exists: u8,
    root_directory: Handle,
    file_name_length: u32,
    file_name: [u16; 1],
}
#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtCreateFile(
        file_handle: *mut Handle,
        desired_access: u32,
        object_attributes: *mut ObjectAttributes,
        io_status_block: *mut IoStatusBlock,
        allocation_size: *mut i64,
        file_attributes: u32,
        share_access: u32,
        create_disposition: u32,
        create_options: u32,
        ea_buffer: *mut c_void,
        ea_length: u32,
    ) -> i32;
    fn NtSetInformationFile(
        file_handle: Handle,
        io_status_block: *mut IoStatusBlock,
        file_information: *mut c_void,
        length: u32,
        file_information_class: u32,
    ) -> i32;
}

fn file_info(file: &File) -> Result<BY_HANDLE_FILE_INFORMATION, String> {
    let mut info = MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::zeroed();
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), info.as_mut_ptr()) } == 0 {
        return Err("Cannot inspect pinned Windows import handle.".into());
    }
    Ok(unsafe { info.assume_init() })
}

fn exact_directory(file: &File) -> Result<(), String> {
    let info = file_info(file)?;
    if info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0
        || info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
    {
        return Err("Windows import parent is not an actual, non-reparse directory.".into());
    }
    Ok(())
}

fn open_relative(
    directory: &File,
    name: &str,
    disposition: u32,
    is_dir: bool,
) -> Result<File, u32> {
    // Input was already reviewed by the shared path validation, but never
    // allow NT device paths, stream syntax, or separators in a component.
    if name.is_empty()
        || matches!(name, "." | "..")
        || name
            .chars()
            .any(|ch| ch == '/' || ch == '\\' || ch == ':' || ch == '\0')
    {
        return Err(0xc000000d);
    }
    let wide: Vec<u16> = name.encode_utf16().collect();
    let Ok(length) = u16::try_from(wide.len().saturating_mul(2)) else {
        return Err(0xc000000d);
    };
    let mut wide = wide;
    let mut name = UnicodeString {
        length,
        maximum_length: length,
        buffer: wide.as_mut_ptr(),
    };
    let mut obj = ObjectAttributes {
        length: size_of::<ObjectAttributes>() as u32,
        root_directory: directory.as_raw_handle(),
        object_name: &mut name,
        attributes: OBJ_CASE_INSENSITIVE | OBJ_DONT_REPARSE,
        security_descriptor: ptr::null_mut(),
        security_quality_of_service: ptr::null_mut(),
    };
    let mut io = IoStatusBlock {
        status_or_pointer: 0,
        information: 0,
    };
    let mut handle = ptr::null_mut();
    let access = if is_dir {
        FILE_READ_DATA
            | FILE_WRITE_DATA
            | FILE_ADD_SUBDIRECTORY
            | FILE_TRAVERSE
            | FILE_READ_ATTRIBUTES
            | SYNCHRONIZE
    } else {
        FILE_READ_DATA
            | FILE_WRITE_DATA
            | FILE_READ_ATTRIBUTES
            | FILE_WRITE_ATTRIBUTES
            | DELETE_ACCESS
            | SYNCHRONIZE
    };
    let status = unsafe {
        NtCreateFile(
            &mut handle,
            access,
            &mut obj,
            &mut io,
            ptr::null_mut(),
            if is_dir && disposition == FILE_CREATE {
                FILE_ATTRIBUTE_DIRECTORY
            } else {
                0
            },
            FILE_SHARE_READ_WRITE,
            disposition,
            FILE_SYNCHRONOUS_IO_NONALERT
                | FILE_OPEN_REPARSE_POINT
                | if is_dir {
                    FILE_DIRECTORY_FILE
                } else {
                    FILE_NON_DIRECTORY_FILE
                },
            ptr::null_mut(),
            0,
        )
    };
    if status < 0 {
        return Err(status as u32);
    }
    if handle.is_null() {
        return Err(0xc0000001);
    }
    Ok(unsafe { File::from_raw_handle(handle) })
}

fn next_directory(parent: &File, component: &str) -> Result<File, String> {
    let opened = match open_relative(parent, component, FILE_OPEN, true) {
        Ok(child) => child,
        Err(STATUS_OBJECT_NAME_NOT_FOUND | STATUS_OBJECT_PATH_NOT_FOUND) => {
            match open_relative(parent, component, FILE_CREATE, true) {
                Ok(child) => child,
                Err(STATUS_OBJECT_NAME_COLLISION) => {
                    open_relative(parent, component, FILE_OPEN, true).map_err(|code| {
                        format!("Source parent was concurrently changed ({code:#x}).")
                    })?
                }
                Err(code) => {
                    return Err(format!(
                        "Cannot create source parent relative to pinned directory ({code:#x})."
                    ))
                }
            }
        }
        Err(code) => {
            return Err(format!(
                "Unsafe source destination directory or access denied ({code:#x})."
            ))
        }
    };
    exact_directory(&opened)?;
    Ok(opened)
}

/// Link an already-flushed sibling file into the final name with
/// ReplaceIfExists=FALSE and RootDirectory set to the pinned destination
/// parent, never a re-resolved absolute path.
fn link_create_only(staged: &File, parent: &File, leaf: &str) -> Result<(), String> {
    if leaf.is_empty()
        || matches!(leaf, "." | "..")
        || leaf.chars().any(|ch| matches!(ch, '/' | '\\' | ':' | '\0'))
    {
        return Err("Unsafe Windows source filename.".into());
    }
    let wide: Vec<u16> = leaf.encode_utf16().collect();
    let name_bytes = wide
        .len()
        .checked_mul(2)
        .and_then(|n| u32::try_from(n).ok())
        .ok_or_else(|| "Windows import filename is too long.".to_string())?;
    let len = offset_of!(FileLinkInformation, file_name)
        .checked_add(name_bytes as usize)
        .ok_or_else(|| "Windows import link metadata overflow.".to_string())?
        .max(size_of::<FileLinkInformation>());
    // An aligned, zeroed buffer holds the variable-length NT structure.
    let mut buffer = vec![0usize; len.div_ceil(size_of::<usize>())];
    let link = buffer.as_mut_ptr().cast::<FileLinkInformation>();
    unsafe {
        (*link).replace_if_exists = 0;
        (*link).root_directory = parent.as_raw_handle();
        (*link).file_name_length = name_bytes;
        ptr::copy_nonoverlapping(wide.as_ptr(), (*link).file_name.as_mut_ptr(), wide.len());
    }
    let mut io = IoStatusBlock {
        status_or_pointer: 0,
        information: 0,
    };
    let status = unsafe {
        NtSetInformationFile(
            staged.as_raw_handle(),
            &mut io,
            buffer.as_mut_ptr().cast(),
            len as u32,
            FILE_LINK_INFORMATION_CLASS,
        )
    };
    if status < 0 {
        return Err(format!(
            "Atomic no-replace Windows source link refused ({:#x}).",
            status as u32
        ));
    }
    Ok(())
}

fn unlink_private_stage(stage: &File) -> Result<(), String> {
    let mut delete: u8 = 1;
    let mut io = IoStatusBlock {
        status_or_pointer: 0,
        information: 0,
    };
    let status = unsafe {
        NtSetInformationFile(
            stage.as_raw_handle(),
            &mut io,
            (&mut delete as *mut u8).cast(),
            1,
            FILE_DISPOSITION_INFORMATION_CLASS,
        )
    };
    if status < 0 {
        return Err(format!(
            "Cannot remove private Windows source stage ({:#x}).",
            status as u32
        ));
    }
    Ok(())
}

fn opened_source(
    workspace: &Workspace,
    task_id: &str,
    source: &str,
    size: u64,
) -> Result<File, String> {
    let path = super::temp_file_path(workspace, task_id, source, false, true)?;
    let metadata = fs::symlink_metadata(&path)
        .map_err(|_| "Approved Windows staged source disappeared.".to_string())?;
    if !metadata.is_file()
        || metadata.len() != size
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    {
        return Err("Approved Windows staged source is missing, aliased or wrong size.".into());
    }
    let mut opts = OpenOptions::new();
    let file = opts
        .read(true)
        .custom_flags(FILE_OPEN_REPARSE_POINT)
        .open(&path)
        .map_err(|_| "Cannot open approved source without following reparse points.".to_string())?;
    let info = file_info(&file)?;
    if info.dwFileAttributes & (FILE_ATTRIBUTE_REPARSE_POINT | FILE_ATTRIBUTE_DIRECTORY) != 0
        || info.nNumberOfLinks != 1
        || file
            .metadata()
            .map_err(|_| "Cannot inspect opened staged bytes.".to_string())?
            .len()
            != size
    {
        return Err(
            "Opened Windows source is aliased, hardlinked or differs from approved size.".into(),
        );
    }
    // Do not use unstable Windows MetadataExt by-handle identity methods.
    // The SHA-256 below independently proves ALL opened source bytes even
    // if another local process replaces the staging pathname mid-transfer.
    Ok(file)
}

pub(super) fn install(
    workspace: &Workspace,
    task_id: &str,
    source_relative: &str,
    destination_relative: &str,
    expected_bytes: u64,
    expected_sha256: &str,
) -> Result<(), String> {
    crate::remote_source_receiver::require_unaliased_destination(workspace, destination_relative)?;
    let _ = resolve_workspace_path(
        workspace,
        destination_relative,
        AccessOperation::Write,
        false,
    )?;
    let root = crate::access::canonical_workspace_root(workspace)?;
    let metadata = fs::symlink_metadata(&root)
        .map_err(|_| "Cannot inspect authorized Windows workspace root.".to_string())?;
    if !metadata.is_dir() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(
            "Authorized Windows import root is a reparse point or is not a directory.".into(),
        );
    }
    let open_root = || -> Result<File, String> {
        let mut opts = OpenOptions::new();
        let opened = opts
            .read(true)
            .share_mode(FILE_SHARE_READ_WRITE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_OPEN_REPARSE_POINT)
            .open(&root)
            .map_err(|_| "Cannot pin approved Windows import root.".to_string())?;
        exact_directory(&opened)?;
        Ok(opened)
    };
    // Compare TWO independently opened Win32 directory handles using the
    // kernel identity, not the unstable MetadataExt by-handle methods.
    let first = open_root()?;
    let mut parent = open_root()?;
    if crate::platform_sandbox::windows_private_opened_object_identity(&first)?
        != crate::platform_sandbox::windows_private_opened_object_identity(&parent)?
    {
        return Err("Approved Windows import root was swapped during opening.".into());
    }
    let components: Vec<&str> = destination_relative.split('/').collect();
    let (leaf, parents) = components
        .split_last()
        .ok_or_else(|| "Source import destination has no filename.".to_string())?;
    // Keep ALL ancestor handles alive without FILE_SHARE_DELETE until the
    // atomic link finishes. A local rename cannot relocate a formerly
    // pinned ancestor outside the approved workspace during installation.
    let mut pinned_ancestors: Vec<File> = Vec::with_capacity(parents.len());
    for component in parents {
        let child = next_directory(&parent, component)?;
        pinned_ancestors.push(parent);
        parent = child;
    }
    exact_directory(&parent)?;

    let mut input = opened_source(workspace, task_id, source_relative, expected_bytes)?;
    let mut random = [0u8; 16];
    getrandom::fill(&mut random)
        .map_err(|_| "Cannot generate private Windows staging filename.".to_string())?;
    let staging_name = format!(
        ".repotunnel-import-{}",
        random
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    );
    let mut stage = open_relative(&parent, &staging_name, FILE_CREATE, false).map_err(|code| {
        format!("Cannot create private stage under pinned destination ({code:#x}).")
    })?;
    let result = (|| -> Result<(), String> {
        let mut digest = Sha256::new();
        let mut total = 0u64;
        let mut buffer = [0u8; 65536];
        loop {
            let n = input
                .read(&mut buffer)
                .map_err(|_| "Cannot read approved source bytes.".to_string())?;
            if n == 0 {
                break;
            }
            total = total
                .checked_add(n as u64)
                .ok_or("Windows source length overflow.")?;
            if total > expected_bytes {
                return Err("Approved staged source grew during installation.".into());
            }
            stage
                .write_all(&buffer[..n])
                .map_err(|_| "Cannot write pinned Windows sibling source stage.".to_string())?;
            digest.update(&buffer[..n]);
        }
        if total != expected_bytes
            || digest
                .finalize()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
                != expected_sha256
        {
            return Err(
                "Windows imported source changed from approved SHA-256 or byte length.".into(),
            );
        }
        stage
            .sync_all()
            .map_err(|_| "Cannot flush source stage before Windows atomic link.".to_string())?;
        // Check path policy again. Destination creation below still uses ONLY
        // the pinned parent handle, so an alias swap cannot redirect the link.
        crate::remote_source_receiver::require_unaliased_destination(
            workspace,
            destination_relative,
        )?;
        link_create_only(&stage, &parent, leaf)?;
        Ok(())
    })();
    // On either failure or success remove only our random sibling stage.
    // If the final link succeeded but cleanup failed, caller keeps UNKNOWN.
    let cleanup = unlink_private_stage(&stage);
    drop(stage);
    match (result, cleanup) {
        (Err(error), _) => Err(error),
        (Ok(()), Err(error)) => Err(format!(
            "Windows source installed, but staging cleanup uncertain: {error}; NEVER replay."
        )),
        (Ok(()), Ok(())) => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{CommandPolicy, WorkspaceAccessMode, WorkspaceChangePolicy};
    use base64::Engine as _;

    #[test]
    fn pinned_windows_parent_blocks_rename_while_handle_is_live() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pinned");
        fs::create_dir(&path).unwrap();
        let root = OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ_WRITE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_OPEN_REPARSE_POINT)
            .open(dir.path())
            .unwrap();
        let child = next_directory(&root, "pinned").unwrap();
        assert!(
            fs::rename(&path, dir.path().join("moved")).is_err(),
            "A pinned NT directory must refuse concurrent rename-away"
        );
        drop(child);
        assert!(fs::rename(&path, dir.path().join("moved")).is_ok());
    }

    #[test]
    fn windows_human_source_uses_anchored_no_replace_link_and_rejects_alias() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace {
            id: "windows-human-import-test".into(),
            name: "Windows human import".into(),
            path: dir.path().to_string_lossy().into_owned(),
            added_at: 0,
            access_mode: WorkspaceAccessMode::ReadWrite,
            change_policy: WorkspaceChangePolicy::Review,
            command_policy: CommandPolicy::Review,
        };
        let task = "remote-sync-0123456789abcdef0123456789abcdef";
        let owner =
            "human-source-v1:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        crate::temp_workspace::create_owned_source_task(&ws, task, "Verified source import", owner)
            .unwrap();
        let bytes = b"NT handle relative create-only import";
        let digest = Sha256::digest(bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let id = "0123456789abcdef0123456789abcdef";
        crate::remote_source_stage::write_chunk(
            &ws,
            owner,
            crate::remote_source_stage::ChunkRequest {
                task_id: task.into(),
                file_id: id.into(),
                total_bytes: bytes.len() as u64,
                sha256: digest.clone(),
                offset: 0,
                chunk_sha256: digest.clone(),
                data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
            },
        )
        .unwrap();

        let _lock = crate::temp_workspace::source_transfer_writer_lock(&ws).unwrap();
        let source = format!("payload/{id}.blob");
        let final_path = dir.path().join("new").join("nested").join("approved.bin");
        // A bad digest must not expose an incomplete final destination.
        assert!(install(
            &ws,
            task,
            &source,
            "new/nested/approved.bin",
            bytes.len() as u64,
            &"0".repeat(64)
        )
        .is_err());
        assert!(!final_path.exists());

        install(
            &ws,
            task,
            &source,
            "new/nested/approved.bin",
            bytes.len() as u64,
            &digest,
        )
        .unwrap();
        assert_eq!(fs::read(&final_path).unwrap(), bytes);
        assert!(
            install(
                &ws,
                task,
                &source,
                "new/nested/approved.bin",
                bytes.len() as u64,
                &digest
            )
            .is_err(),
            "Never overwrite an existing final file"
        );
        assert_eq!(fs::read(&final_path).unwrap(), bytes);
        assert!(
            fs::read_dir(final_path.parent().unwrap())
                .unwrap()
                .all(|entry| !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".repotunnel-import-")),
            "Private staging names must not remain after successful install"
        );

        let outside = tempfile::tempdir().unwrap();
        // Creating test symlinks requires Developer Mode or elevation on
        // some Windows versions. If unavailable, exercise the earlier proof
        // and let the user run the symlink case with Developer Mode enabled.
        if std::os::windows::fs::symlink_dir(outside.path(), dir.path().join("alias")).is_ok() {
            assert!(install(
                &ws,
                task,
                &source,
                "alias/escaped.bin",
                bytes.len() as u64,
                &digest
            )
            .is_err());
            assert!(!outside.path().join("escaped.bin").exists());
        }
    }
}
