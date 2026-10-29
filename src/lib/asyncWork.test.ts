import { describe, expect, it, vi } from "vitest";
import { coalesceAsync, mapBounded } from "./asyncWork";

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason?: unknown) => void;
  const promise = new Promise<T>((success, failure) => {
    resolve = success;
    reject = failure;
  });
  return { promise, resolve, reject };
}

describe("bounded editor background reads", () => {
  it("never opens more than four concurrent tasks and preserves input order", async () => {
    const waits = Array.from({ length: 24 }, () => deferred<number>());
    let current = 0;
    let maximum = 0;
    const called: number[] = [];
    const reads = mapBounded(
      Array.from({ length: 24 }, (_, i) => i),
      async (index) => {
        current++;
        maximum = Math.max(maximum, current);
        called.push(index);
        try {
          return await waits[index].promise;
        } finally {
          current--;
        }
      }, 4,
    );
    expect(called).toEqual([0, 1, 2, 3]);
    for (let i = 0; i < 24; i++) {
      waits[i].resolve(i * 2);
      await Promise.resolve();
      await Promise.resolve();
    }
    expect(await reads).toEqual(Array.from({ length: 24 }, (_, i) => i * 2));
    expect(maximum).toBe(4);
    expect(called).toHaveLength(24);
  });

  it("handles empty input, a single worker, and a read failure", async () => {
    expect(await mapBounded([], async (x: number) => x)).toEqual([]);
    expect(await mapBounded([3, 2, 1], async (x) => x + 1, 1)).toEqual([4, 3, 2]);
    await expect(mapBounded([1], async () => { throw new Error("filesystem unavailable"); }))
      .rejects.toThrow("filesystem unavailable");
  });
});

describe("single-flight editor file-change synchronization", () => {
  it("merges repeated refresh requests into one trailing scan of the latest state", async () => {
    const first = deferred<void>();
    const second = deferred<void>();
    const task = vi.fn()
      .mockImplementationOnce(() => first.promise)
      .mockImplementationOnce(() => second.promise);
    const sync = coalesceAsync(task);
    const running = sync();
    expect(task).toHaveBeenCalledTimes(1);
    expect(sync()).toBe(running);
    expect(sync()).toBe(running);
    expect(sync()).toBe(running);
    expect(task).toHaveBeenCalledTimes(1);
    first.resolve();
    await Promise.resolve();
    await Promise.resolve();
    expect(task).toHaveBeenCalledTimes(2);
    second.resolve();
    await running;
    await Promise.resolve();
    expect(task).toHaveBeenCalledTimes(2);
  });

  it("recovers after a read failure and can run another scan", async () => {
    const task = vi.fn().mockRejectedValueOnce(new Error("temporary IPC failure"))
      .mockResolvedValueOnce(undefined);
    const sync = coalesceAsync(task);
    await expect(sync()).rejects.toThrow("temporary IPC failure");
    // The scheduler clears its running marker after a rejected read.
    await Promise.resolve();
    await expect(sync()).resolves.toBeUndefined();
    expect(task).toHaveBeenCalledTimes(2);
  });

  it("does not trigger an extra scan when no refresh arrives during the first scan", async () => {
    const task = vi.fn(async () => undefined);
    const sync = coalesceAsync(task);
    await sync();
    await Promise.resolve();
    expect(task).toHaveBeenCalledTimes(1);
    await sync();
    expect(task).toHaveBeenCalledTimes(2);
  });
});
