import { invoke } from "@tauri-apps/api/core";
import type { ChangeOutcome, DirectoryEntry, FileContent, FileInfo, ImagePreview, LaunchActionOutcome, SearchMatch } from "../types";

export async function listDirectory(
  workspaceId: string,
  relativePath = "",
): Promise<DirectoryEntry[]> {
  return invoke<DirectoryEntry[]>("list_directory", { workspaceId, relativePath });
}

export async function readFile(
  workspaceId: string,
  relativePath: string,
): Promise<FileContent> {
  return invoke<FileContent>("read_file", { workspaceId, relativePath });
}

export async function searchFiles(
  workspaceId: string,
  query: string,
  relativePath = "",
): Promise<SearchMatch[]> {
  return invoke<SearchMatch[]>("search_files", { workspaceId, relativePath, query });
}

export async function createFile(
  workspaceId: string,
  relativePath: string,
  content: string,
): Promise<ChangeOutcome> {
  return invoke<ChangeOutcome>("create_file", { workspaceId, relativePath, content });
}

export async function writeFile(
  workspaceId: string,
  relativePath: string,
  content: string,
): Promise<ChangeOutcome> {
  return invoke<ChangeOutcome>("write_file", { workspaceId, relativePath, content });
}

export async function patchFile(
  workspaceId: string,
  relativePath: string,
  expected: string,
  replacement: string,
): Promise<ChangeOutcome> {
  return invoke<ChangeOutcome>("patch_file", {
    workspaceId,
    relativePath,
    expected,
    replacement,
  });
}

export async function createDirectory(
  workspaceId: string,
  relativePath: string,
  recursive = false,
): Promise<ChangeOutcome> {
  return invoke<ChangeOutcome>("create_directory", { workspaceId, relativePath, recursive });
}

export async function renameEntry(
  workspaceId: string,
  relativePath: string,
  newName: string,
): Promise<ChangeOutcome> {
  return invoke<ChangeOutcome>("rename_entry", { workspaceId, relativePath, newName });
}

export async function moveEntry(
  workspaceId: string,
  sourcePath: string,
  destinationPath: string,
): Promise<ChangeOutcome> {
  return invoke<ChangeOutcome>("move_entry", { workspaceId, sourcePath, destinationPath });
}

export async function deleteEntry(
  workspaceId: string,
  relativePath: string,
  recursive = false,
): Promise<ChangeOutcome> {
  return invoke<ChangeOutcome>("delete_entry", { workspaceId, relativePath, recursive });
}

export async function getFileInfo(
  workspaceId: string,
  relativePath: string,
): Promise<FileInfo> {
  return invoke<FileInfo>("get_file_info", { workspaceId, relativePath });
}

export async function saveEditorFile(
  workspaceId: string,
  relativePath: string,
  content: string,
  expectedContent: string,
): Promise<ChangeOutcome> {
  return invoke<ChangeOutcome>("editor_save_file", { workspaceId, relativePath, content, expectedContent });
}

export async function createEditorFile(
  workspaceId: string,
  relativePath: string,
  content = "",
): Promise<ChangeOutcome> {
  return invoke<ChangeOutcome>("editor_create_file", { workspaceId, relativePath, content });
}

export async function createEditorDirectory(
  workspaceId: string,
  relativePath: string,
): Promise<ChangeOutcome> {
  return invoke<ChangeOutcome>("editor_create_directory", { workspaceId, relativePath });
}

export async function renameEditorEntry(
  workspaceId: string,
  relativePath: string,
  newName: string,
): Promise<ChangeOutcome> {
  return invoke<ChangeOutcome>("editor_rename_entry", { workspaceId, relativePath, newName });
}

export async function deleteEditorEntry(
  workspaceId: string,
  relativePath: string,
  recursive = false,
): Promise<ChangeOutcome> {
  return invoke<ChangeOutcome>("editor_delete_entry", { workspaceId, relativePath, recursive });
}

export async function previewWorkspaceImage(
  workspaceId: string,
  relativePath: string,
): Promise<ImagePreview> {
  return invoke<ImagePreview>("preview_workspace_image", { workspaceId, relativePath });
}

export async function openWorkspacePathLocal(
  workspaceId: string,
  relativePath: string,
): Promise<LaunchActionOutcome> {
  return invoke<LaunchActionOutcome>("open_workspace_path_local", { workspaceId, relativePath });
}
