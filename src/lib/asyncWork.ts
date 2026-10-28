/**
 * Bound concurrent read-only asynchronous work. Source order is preserved,
 * including when tasks settle in a different order.
 */
export async function mapBounded<T, R>(
  items: readonly T[],
  callback: (item: T, index: number) => Promise<R>,
  concurrency = 4,
): Promise<R[]> {
  const result = new Array<R>(items.length);
  let next = 0;
  const count = Math.min(items.length, Math.max(1, Math.floor(concurrency) || 1));
  await Promise.all(Array.from({ length: count }, async () => {
    while (next < items.length) {
      const index = next++;
      result[index] = await callback(items[index], index);
    }
  }));
  return result;
}

/**
 * Merge multiple refresh triggers into one outstanding task and, if necessary,
 * one trailing run. The trailing scan captures fresh state after the first
 * read, so an in-flight file check cannot bury a newer change notification.
 */
export function coalesceAsync(task: () => Promise<void>): () => Promise<void> {
  let inFlight: Promise<void> | null = null;
  let queued = false;

  return () => {
    if (inFlight) {
      queued = true;
      return inFlight;
    }
    const run = (async () => {
      do {
        queued = false;
        await task();
      } while (queued);
    })();
    inFlight = run;
    // Attach both handlers: a failed read must not lock the scheduler forever.
    void run.then(
      () => { if (inFlight === run) inFlight = null; },
      () => { if (inFlight === run) inFlight = null; },
    );
    return run;
  };
}
