# Safe Editing

RepoTunnel separates permission to write from the policy for applying a write.

## Workspace write policies

Normal AI mutation tools keep the compatibility-safe MCP schema: ChatGPT calls the familiar file tools without an extra edit-group argument. In AI auto mode a valid mutation is applied immediately, recorded, and protected by an automatic project snapshot. When the MCP client supplies a valid W3C `traceparent` header, RepoTunnel uses that request trace internally to collect every mutation from the same AI request into one history version. Clients that do not supply a usable trace continue to work, but their edits are saved as separate versions.

In AI Review mode, mutation tools create pending changes for local Apply/Reject. In AI Auto mode, compatible mutations apply immediately, so normal editing does not require per-file Apply clicks.

Read-only workspace mode always wins over either write policy.

## Change states

- `pending` — waiting for local review.
- `applied` — the project was changed successfully.
- `rejected` — a pending request was discarded without touching the project.
- `undone` — RepoTunnel successfully reversed a supported applied change.
- `failed` — validation or application failed; the project is not reported as successfully changed.

## Stale-change protection

RepoTunnel fingerprints accessible UTF-8 text before a full write, patch, or reviewed file deletion. When a pending change is later approved, the current file must still match the prepared state. If another editor changed it meanwhile, the old request fails instead of overwriting the newer content.

Undo uses the same conservative approach. RepoTunnel refuses to overwrite a file during undo when its current contents no longer match the change being reversed.

## Undo coverage

Safe automatic undo is supported for:

- created UTF-8 files, when they are still unchanged
- full-file writes and targeted patches
- created directories when normal non-recursive deletion remains safe
- rename and move operations when the original path is still free
- deleted accessible UTF-8 files when the original path remains free

Recursive directory deletion and deletion of files that cannot be safely backed up as bounded UTF-8 text remain auditable but may not expose Undo.

## Storage

Change-history metadata, pending requests, and undo data live in the RepoTunnel application-data directory. Pending and backup data are separate from history, and sensitive request/backup files are written atomically with owner-only permissions on Unix.
