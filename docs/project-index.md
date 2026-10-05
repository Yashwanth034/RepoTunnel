# Project index and large-project reads

RepoTunnel builds a bounded, code-focused view of approved workspaces before broad AI exploration.

The project index is a relevance/performance layer. `src-tauri/src/access.rs` remains the security boundary for every candidate path.

## Filtering

Project discovery/search:

- honors workspace and nested `.gitignore` / `.ignore` rules
- supports ordered negation and common wildcard patterns
- skips symbolic-link traversal
- skips generated/dependency directories such as `.git`, `node_modules`, `dist`, `build`, `target`, framework caches, virtual environments, and Python caches
- passes candidates through the workspace access guard
- excludes protected credential paths
- classifies likely binary/oversized files before broad text scanning

Explicit reads still use the normal access policy. Ignore rules are an AI relevance filter, not authorization.

## Project overview

The bounded project overview can report:

- visible file/directory counts
- text/code, binary, and oversized counts
- total visible bytes
- filtered-entry information
- detected source languages
- common manifests
- whether a returned tree/page is truncated

## Large-project read path

RepoTunnel has a dedicated incremental large-project read layer so a huge repository or directory does not need to be fully pre-buffered before returning a result.

Use:

- `inspect_project_page` for resumable project-tree paging
- `list_directory_page` for very large directories
- `read_file_range` for large text files and high line offsets
- `fast_search_files` for incremental bounded text search with continuation cursors

Cursors are tied to the relevant file/search state and are invalidated when the underlying state changes rather than silently continuing against stale content.

Heavy large-project reads share a bounded gate so overlapping scans do not overwhelm the process.

## Text classification cache

Likely text/binary classification is cached with file metadata and revalidated when file size/mtime changes. This reduces repeated content sniffing without turning cached classification into a security decision.

## Compatibility path

Legacy bounded tools such as `inspect_project`, `list_directory`, `read_file`, and `search_files` remain useful for normal repositories and compatibility.

For a broad or very large repository, prefer the paged/incremental tools above.

## Recommended AI workflow

1. `list_workspaces`
2. `get_workflow_readiness`
3. `inspect_project_page`
4. use `fast_search_files` or a targeted directory page
5. read only the required ranges/files
6. edit only after the relevant current context has been read

This keeps responses bounded and avoids repeatedly scanning or transmitting an entire repository.
