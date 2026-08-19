# Project index

RepoTunnel builds a bounded, code-focused view of approved workspaces before broad AI exploration.

## Filtering

The index and smart search:

- honor `.gitignore` and `.ignore` rules from the workspace root and nested directories
- support ordered negation rules and common `*`, `?`, and `**` wildcard patterns
- skip symbolic links during recursive discovery
- skip generated/dependency directories such as `.git`, `node_modules`, `dist`, `build`, `target`, coverage output, framework caches, virtual environments, and Python caches
- pass every candidate path through the workspace access guard, so protected credential paths remain unavailable

Explicit file reads still use the normal workspace security policy. Ignore rules are an AI relevance filter, not a security boundary.

## File classification

Files are classified with extension checks plus a bounded content sniff. RepoTunnel avoids treating likely binary content as source text and excludes oversized files from broad text search. Direct text reads retain their stricter size limit.

The project overview reports:

- visible file and directory counts
- text/code, binary, and oversized file counts
- total visible bytes
- filtered-entry count
- detected source languages
- common project manifests
- whether the returned tree hit its configured entry limit

## AI workflow

The MCP `inspect_project` tool should normally be used after `list_workspaces` and before a broad search. It gives the model a bounded map of the repository without exposing the canonical absolute Linux path.

The existing `search_files` tool now uses the same smart traversal rules, so dependency trees, generated output, ignored files, likely binary files, and oversized files are not scanned by default.
