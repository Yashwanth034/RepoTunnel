/**
 * Merge one asynchronous editor-file check without publishing an unnecessary
 * state update or replacing a tab that changed while the I/O was in flight.
 *
 * Each update is based on the corresponding snapshot entry. If the user edits,
 * closes, or replaces that tab before the check finishes, its older result is
 * ignored; the next normal file check can inspect the newer tab.
 */
/** Avoid issuing another state update on every poll of an already-missing file. */
export function markMissingEditorFile<T extends { externalDeleted: boolean }>(tab: T): T {
  return tab.externalDeleted ? tab : { ...tab, externalDeleted: true };
}

export function reconcileEditorFileChecks<T extends { key: string }>(
  current: T[],
  checkedSnapshot: readonly T[],
  checkedResults: readonly T[],
): T[] {
  if (checkedSnapshot.length !== checkedResults.length) return current;

  const checkedByKey = new Map(
    checkedSnapshot.map((before, index) => [before.key, { before, after: checkedResults[index] }] as const),
  );
  let next: T[] | null = null;
  for (let index = 0; index < current.length; index++) {
    const tab = current[index];
    const result = checkedByKey.get(tab.key);
    if (!result || result.before !== tab || result.after === tab || result.after.key !== tab.key) continue;
    if (next === null) next = [...current];
    next[index] = result.after;
  }
  return next ?? current;
}
