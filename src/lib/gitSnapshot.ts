import type { GitDiff, GitFileChange, GitRepositoryStatus } from "../types";

/** Preserve unchanged git status across read-only polls and real project edits. */
export function preserveGitStatus(
  current: GitRepositoryStatus | null,
  incoming: GitRepositoryStatus,
): GitRepositoryStatus {
  if (!current) return incoming;
  const fields = Object.keys(incoming) as Array<keyof GitRepositoryStatus>;
  const unchanged = fields.length === Object.keys(current).length
    && fields.every((key) => key === "changes"
      ? current.changes.length === incoming.changes.length
        && incoming.changes.every((change, index) => {
          const prior = current.changes[index];
          const keys = Object.keys(change) as Array<keyof GitFileChange>;
          return prior && keys.length === Object.keys(prior).length
            && keys.every((field) => Object.is(prior[field], change[field]));
        })
      : Object.is(current[key], incoming[key]));
  return unchanged ? current : incoming;
}

export function preserveGitDiff(current: GitDiff | null, incoming: GitDiff): GitDiff {
  return current && current.staged === incoming.staged
    && current.truncated === incoming.truncated
    && current.content === incoming.content ? current : incoming;
}
