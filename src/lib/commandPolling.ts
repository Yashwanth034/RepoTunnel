/**
 * Retain React identities for unchanged snapshots from periodic read-only
 * Commands/Terminals/Processes polling. Their serialized records contain
 * scalar values only; compare every field to avoid hiding status transitions.
 */
function sameSnapshot<T extends object>(left: T, right: T): boolean {
  if (left === right) return true;
  const keys = Object.keys(left) as Array<keyof T>;
  return keys.length === Object.keys(right).length
    && keys.every((key) => Object.is(left[key], right[key]));
}

export function reconcilePolledRecords<T extends { id: string }>(
  current: T[],
  incoming: readonly T[],
): T[] {
  if (current === incoming) return current;
  let changed = current.length !== incoming.length;
  const merged = incoming.map((item, index) => {
    const previous = current[index];
    if (previous && sameSnapshot(previous, item)) return previous;
    changed = true;
    return item;
  });
  return changed ? merged : current;
}

export type PolledProcessLog = {
  processId: string;
  stdout: string;
  stderr: string;
  stdoutOffset: number;
  stderrOffset: number;
  outputTruncated: boolean;
};

export type PolledOutputChunk = {
  processId: string;
  stdout: string;
  stderr: string;
  nextStdoutOffset: number;
  nextStderrOffset: number;
  outputTruncated: boolean;
};

/** No-op backend output checks should not re-render a visible terminal log. */
export function mergePolledProcessLog<T extends PolledProcessLog>(
  current: T | null,
  chunk: PolledOutputChunk,
): PolledProcessLog {
  const same = current?.processId === chunk.processId;
  const truncated = Boolean((same && current.outputTruncated) || chunk.outputTruncated);
  if (same && !chunk.stdout && !chunk.stderr
      && current.stdoutOffset === chunk.nextStdoutOffset
      && current.stderrOffset === chunk.nextStderrOffset
      && current.outputTruncated === truncated) {
    return current;
  }
  return {
    processId: chunk.processId,
    stdout: `${same ? current.stdout : ""}${chunk.stdout}`,
    stderr: `${same ? current.stderr : ""}${chunk.stderr}`,
    stdoutOffset: chunk.nextStdoutOffset,
    stderrOffset: chunk.nextStderrOffset,
    outputTruncated: truncated,
  };
}
