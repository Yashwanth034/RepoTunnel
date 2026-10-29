// @vitest-environment jsdom
import React, { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { useVisibleViewRefresh } from "./visibleViewRefresh";

(globalThis as typeof globalThis & { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

let root: Root | null = null;
let host: HTMLDivElement | null = null;
const onEnter = vi.fn<() => Promise<void>>();
const onPoll = vi.fn<() => Promise<void>>();
function Fixture({ visible }: { visible: boolean }) {
  useVisibleViewRefresh(visible, onEnter, onPoll, 10_000);
  return <span>{visible ? "Model Hub" : "Home"}</span>;
}
async function show(visible: boolean) {
  await act(async () => root?.render(<Fixture visible={visible} />));
}

beforeEach(() => {
  vi.useFakeTimers();
  vi.clearAllMocks();
  onEnter.mockResolvedValue(undefined);
  onPoll.mockResolvedValue(undefined);
  host = document.createElement("div");
  document.body.append(host);
  root = createRoot(host);
});

afterEach(async () => {
  if (root) await act(async () => root?.unmount());
  root = null;
  host?.remove();
  host = null;
  vi.useRealTimers();
});

describe("view-scoped Model Hub observation", () => {
  it("makes no model-status requests while Home is visible, even after timers or focus", async () => {
    await show(false);
    await act(async () => vi.advanceTimersByTime(60_000));
    await act(async () => window.dispatchEvent(new Event("focus")));
    expect(onEnter).not.toHaveBeenCalled();
    expect(onPoll).not.toHaveBeenCalled();
  });

  it("loads Model Hub immediately on entry, then polls and refreshes on focus only while visible", async () => {
    await show(false);
    await show(true);
    expect(onEnter).toHaveBeenCalledTimes(1);
    await act(async () => { await vi.advanceTimersByTimeAsync(20_000); });
    expect(onPoll).toHaveBeenCalledTimes(2);
    await act(async () => window.dispatchEvent(new Event("focus")));
    expect(onPoll).toHaveBeenCalledTimes(3);
    await show(false);
    await act(async () => vi.advanceTimersByTime(30_000));
    await act(async () => window.dispatchEvent(new Event("focus")));
    expect(onPoll).toHaveBeenCalledTimes(3);
    await show(true);
    expect(onEnter).toHaveBeenCalledTimes(2);
  });

  it("coalesces slow model discovery without overlapping status IPC and cancels on unmount", async () => {
    let resolve!: () => void;
    const slow = new Promise<void>((done) => { resolve = done; });
    onEnter.mockImplementationOnce(() => slow);
    await show(true);
    expect(onEnter).toHaveBeenCalledTimes(1);
    await act(async () => vi.advanceTimersByTime(30_000));
    await act(async () => window.dispatchEvent(new Event("focus")));
    expect(onPoll).not.toHaveBeenCalled();
    await act(async () => resolve());
    await act(async () => vi.advanceTimersByTime(10_000));
    expect(onPoll).toHaveBeenCalledTimes(1);
    await act(async () => root?.unmount());
    root = null;
    await act(async () => vi.advanceTimersByTime(30_000));
    expect(onPoll).toHaveBeenCalledTimes(1);
  });
});
