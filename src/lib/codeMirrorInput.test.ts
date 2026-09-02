// @vitest-environment jsdom

import { afterEach, beforeAll, describe, expect, it } from "vitest";
import { EditorView, basicSetup } from "codemirror";
import { EditorState } from "@codemirror/state";

class ResizeObserverStub {
  observe() {}
  unobserve() {}
  disconnect() {}
}

beforeAll(() => {
  Object.defineProperty(globalThis, "ResizeObserver", {
    value: ResizeObserverStub,
    configurable: true,
    writable: true,
  });
  Object.defineProperty(Range.prototype, "getClientRects", {
    value: () => [],
    configurable: true,
  });
  Object.defineProperty(Range.prototype, "getBoundingClientRect", {
    value: () => new DOMRect(0, 0, 0, 0),
    configurable: true,
  });
  if (!globalThis.requestAnimationFrame) {
    Object.defineProperty(globalThis, "requestAnimationFrame", {
      value: (callback: FrameRequestCallback) => window.setTimeout(() => callback(performance.now()), 0),
      configurable: true,
      writable: true,
    });
  }
  if (!globalThis.cancelAnimationFrame) {
    Object.defineProperty(globalThis, "cancelAnimationFrame", {
      value: (id: number) => window.clearTimeout(id),
      configurable: true,
      writable: true,
    });
  }
});

let activeView: EditorView | null = null;

afterEach(() => {
  activeView?.destroy();
  activeView = null;
  document.body.replaceChildren();
});

function createView(doc = "abcdef") {
  const host = document.createElement("div");
  document.body.append(host);
  const view = new EditorView({
    parent: host,
    state: EditorState.create({
      doc,
      selection: { anchor: doc.length },
      extensions: [basicSetup],
    }),
  });
  activeView = view;
  view.focus();
  return view;
}

function keyboard(view: EditorView, type: "keydown" | "keyup", key: string, init: KeyboardEventInit = {}) {
  return view.contentDOM.dispatchEvent(new KeyboardEvent(type, {
    key,
    bubbles: true,
    cancelable: true,
    ...init,
  }));
}

describe("CodeMirror editor input regressions", () => {
  it("stops deleting as soon as Backspace key events stop", async () => {
    const view = createView();

    keyboard(view, "keydown", "Backspace");
    keyboard(view, "keydown", "Backspace", { repeat: true });
    keyboard(view, "keydown", "Backspace", { repeat: true });
    expect(view.state.doc.toString()).toBe("abc");

    keyboard(view, "keyup", "Backspace");
    const afterRelease = view.state.doc.toString();
    await new Promise((resolve) => window.setTimeout(resolve, 30));

    expect(view.state.doc.toString()).toBe(afterRelease);
    expect(afterRelease).toBe("abc");
  });

  it("supports Ctrl+Z and Ctrl+Y after a Backspace burst", () => {
    const view = createView();

    keyboard(view, "keydown", "Backspace");
    keyboard(view, "keydown", "Backspace", { repeat: true });
    keyboard(view, "keydown", "Backspace", { repeat: true });
    keyboard(view, "keyup", "Backspace");
    expect(view.state.doc.toString()).toBe("abc");

    keyboard(view, "keydown", "z", { ctrlKey: true });
    expect(view.state.doc.toString()).toBe("abcdef");

    keyboard(view, "keydown", "y", { ctrlKey: true });
    expect(view.state.doc.toString()).toBe("abc");
  });

  it("keeps the caret at the edited location instead of jumping to the end", () => {
    const view = createView("first\nsecond\nthird");
    const middle = view.state.doc.line(2).from + 3;
    view.dispatch({ selection: { anchor: middle } });

    keyboard(view, "keydown", "Backspace");

    expect(view.state.selection.main.head).toBe(middle - 1);
    expect(view.state.doc.toString()).toBe("first\nseond\nthird");
  });
});
