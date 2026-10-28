// @vitest-environment jsdom
import React, { act, useState } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, describe, expect, it, vi } from "vitest";
import NewProjectDialog from "./NewProjectDialog";

(globalThis as typeof globalThis & { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

let root: Root | null = null;
let mount: HTMLDivElement | null = null;

async function renderDialog() {
  mount = document.createElement("div");
  document.body.append(mount);
  root = createRoot(mount);
  const created = vi.fn(async (_name: string) => {});
  function Harness() {
    const [open, setOpen] = useState(false);
    return (
      <>
        <button type="button" data-testid="trigger" onClick={() => setOpen(true)}>New project</button>
        {open ? (
          <NewProjectDialog
            busy={false}
            onCancel={() => setOpen(false)}
            onCreate={async (name) => { await created(name); setOpen(false); }}
          />
        ) : null}
      </>
    );
  }
  await act(async () => root?.render(<Harness />));
  const trigger = mount.querySelector<HTMLButtonElement>('[data-testid="trigger"]')!;
  trigger.focus();
  await act(async () => trigger.click());
  const dialog = mount.querySelector<HTMLElement>('[role="dialog"]')!;
  return { trigger, dialog, created };
}

async function settle() {
  await act(async () => new Promise<void>((resolve) => window.setTimeout(resolve, 0)));
}

afterEach(async () => {
  if (root) await act(async () => root?.unmount());
  root = null;
  mount?.remove();
  mount = null;
});

describe("New project dialog keyboard behavior", () => {
  it("focuses the input and returns focus to the opener after Cancel", async () => {
    const { trigger, dialog } = await renderDialog();
    const input = dialog.querySelector<HTMLInputElement>('input[name], input[id="new-project-name"]')!;
    expect(document.activeElement).toBe(input);
    await act(async () => {
      const cancel = Array.from(dialog.querySelectorAll("button"))
        .find((button) => button.textContent === "Cancel")!;
      cancel.click();
    });
    await settle();
    expect(mount?.querySelector('[role="dialog"]')).toBeNull();
    expect(document.activeElement).toBe(trigger);
  });

  it("closes on Escape and restores the invoking button", async () => {
    const { trigger, dialog } = await renderDialog();
    const input = dialog.querySelector<HTMLInputElement>("input")!;
    await act(async () => {
      input.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
    });
    await settle();
    expect(mount?.querySelector('[role="dialog"]')).toBeNull();
    expect(document.activeElement).toBe(trigger);
  });

  it("keeps keyboard Tab focus inside the dialog", async () => {
    const { dialog } = await renderDialog();
    const close = dialog.querySelector<HTMLButtonElement>('button[aria-label="Close"]')!;
    const cancel = Array.from(dialog.querySelectorAll("button")).find(x => x.textContent === "Cancel")!;
    const tabbable = Array.from(dialog.querySelectorAll<HTMLElement>("*")).filter(node => ["BUTTON", "INPUT"].includes(node.tagName) && !node.hasAttribute("disabled"));
    expect(tabbable.map(node => node.tagName + ":" + (node.textContent || ""))).toEqual(["BUTTON:×", "INPUT:", "BUTTON:Cancel"]);
    cancel.focus();
    await act(async () => cancel.dispatchEvent(new KeyboardEvent("keydown", { key: "Tab", bubbles: true, cancelable: true })));
    expect(document.activeElement).toBe(close);
    await act(async () => close.dispatchEvent(new KeyboardEvent("keydown", { key: "Tab", shiftKey: true, bubbles: true, cancelable: true })));
    expect(document.activeElement).toBe(cancel);
  });

  it("creates a project from the keyboard and returns focus", async () => {
    const { trigger, dialog, created } = await renderDialog();
    const input = dialog.querySelector<HTMLInputElement>("input")!;
    await act(async () => {
      const setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")?.set;
      setter?.call(input, "  Sample keyboard project  ");
      input.dispatchEvent(new Event("input", { bubbles: true }));
    });
    const form = dialog as HTMLFormElement;
    await act(async () => form.dispatchEvent(new Event("submit", { bubbles: true, cancelable: true })));
    await settle();
    expect(created).toHaveBeenCalledWith("Sample keyboard project");
    expect(document.activeElement).toBe(trigger);
  });
});
