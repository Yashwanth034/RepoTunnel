import { type FormEvent, type KeyboardEvent, useRef, useState } from "react";

type NewProjectDialogProps = {
  busy: boolean;
  onCancel: () => void;
  onCreate: (name: string) => Promise<void>;
};

function NewProjectDialog({ busy, onCancel, onCreate }: NewProjectDialogProps) {
  const [name, setName] = useState("");
  // Captured during render, before autoFocus moves focus into the dialog.
  const previousFocus = useRef<HTMLElement | null>(
    typeof document !== "undefined" && document.activeElement instanceof HTMLElement
      ? document.activeElement
      : null,
  );

  function restoreFocus() {
    const previous = previousFocus.current;
    window.setTimeout(() => {
      if (previous?.isConnected) previous.focus({ preventScroll: true });
      else document.querySelector<HTMLElement>(".sidebar-nav-item.active")?.focus({ preventScroll: true });
    }, 0);
  }

  function dismiss() {
    if (busy) return;
    onCancel();
    restoreFocus();
  }

  async function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    const value = name.trim();
    if (!value || busy) return;
    await onCreate(value);
    restoreFocus();
  }

  function handleKeyDown(event: KeyboardEvent<HTMLFormElement>) {
    if (event.key === "Escape" && !busy) {
      event.preventDefault();
      event.stopPropagation();
      dismiss();
      return;
    }
    if (event.key !== "Tab") return;
    // Traverse actual DOM order. Grouped CSS selectors can be returned in
    // selector-group order by some WebKit/accessibility test environments.
    const focusable = Array.from(event.currentTarget.querySelectorAll<HTMLElement>("*"))
      .filter((element) =>
        ["BUTTON", "INPUT", "SELECT", "TEXTAREA"].includes(element.tagName)
        && !element.hasAttribute("disabled"),
      );
    const first = focusable[0];
    const last = focusable[focusable.length - 1];
    if (!first || !last) return;
    if (event.shiftKey && document.activeElement === first) {
      event.preventDefault();
      last.focus();
    } else if (!event.shiftKey && document.activeElement === last) {
      event.preventDefault();
      first.focus();
    }
  }

  return (
    <div className="feature-dialog-backdrop" role="presentation" onMouseDown={dismiss}>
      <form
        className="feature-dialog new-project-dialog"
        role="dialog"
        aria-modal="true"
        aria-labelledby="new-project-title"
        onSubmit={(event) => void submit(event)}
        onKeyDown={handleKeyDown}
        onMouseDown={(event) => event.stopPropagation()}
      >
        <div className="feature-dialog-header">
          <div>
            <span className="feature-dialog-kicker">New project</span>
            <h3 id="new-project-title">Create from scratch</h3>
          </div>
          <button type="button" onClick={dismiss} disabled={busy} aria-label="Close">×</button>
        </div>
        <p className="new-project-copy">RepoTunnel creates an empty project inside your Projects folder and approves only that new folder for AI work.</p>
        <label className="field-label" htmlFor="new-project-name">
          Project name
          <input
            id="new-project-name"
            autoFocus
            maxLength={80}
            value={name}
            onChange={(event) => setName(event.target.value)}
            placeholder="My new app"
            disabled={busy}
          />
        </label>
        <div className="feature-dialog-actions">
          <button type="button" onClick={dismiss} disabled={busy}>Cancel</button>
          <button type="submit" className="primary" disabled={busy || !name.trim()}>{busy ? "Creating…" : "Create project"}</button>
        </div>
      </form>
    </div>
  );
}

export default NewProjectDialog;
