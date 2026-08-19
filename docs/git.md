# Git integration

RepoTunnel exposes focused Git operations for approved repositories without providing a generic Git command surface.

## Repository boundary

Git integration is enabled only when the approved workspace root contains its own `.git` directory. RepoTunnel intentionally rejects parent-repository and linked-worktree layouts because their Git metadata can live outside the folder the user approved.

## Read operations

RepoTunnel can report:

- branch and HEAD state
- ahead/behind counts when an upstream exists
- staged, unstaged, untracked, and conflicted paths
- bounded staged or unstaged diffs
- recent commit summaries without author email addresses

Diff execution disables external diff drivers and text conversion.

## Staging flow

AI clients may request staging only for explicit workspace-relative files. RepoTunnel rejects protected paths, symlinks, directories, files with credential-like content, and files that use Git clean filters because clean filters can execute external programs. A staging request fingerprints the selected files and index state. In AI Auto, a validated request applies immediately; in AI Review it waits for local desktop approval and is revalidated before approval.

## Commit flow

RepoTunnel never stages files as part of a commit request. A commit request captures the currently staged diff and current HEAD and revalidates both before execution. In AI Auto, a validated commit applies immediately; in AI Review it waits for desktop approval and fails if either HEAD or the staged diff changed after the request was prepared.

Commit execution disables repository hooks and GPG signing. MCP can request and inspect Git actions but cannot approve or reject pending AI Review actions.

## Restore flow

RepoTunnel does not expose `git reset`, `git clean`, or unrestricted `git restore` to AI clients. A restore request is limited to one tracked UTF-8 text file and reads the file's HEAD version through Git. RepoTunnel then converts that content into a normal safe-editing request, preserving the existing diff/backup/undo protections and respecting the workspace change policy: immediate in AI Auto, local review in AI Review.

Restore-to-HEAD is offered only for unstaged, non-conflicted tracked files in the desktop UI. Staged changes are never silently discarded.

## Push flow

AI Auto is not standing permission to publish repository history. A normal push is accepted only when the current human instruction explicitly asks the AI to push. RepoTunnel performs a committed-tree secret preflight, blocks force/delete/mirror/all/tags-style broad pushes and arbitrary remote URLs, and disables local Git hooks for the controlled push. Once that explicit intent exists, AI Auto does not add another approval popup. AI Review retains its normal local command approval boundary.
