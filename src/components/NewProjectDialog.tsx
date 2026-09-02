import { FormEvent, useState } from "react";

type NewProjectDialogProps = {
  busy: boolean;
  onCancel: () => void;
  onCreate: (name: string) => Promise<void>;
};

function NewProjectDialog({ busy, onCancel, onCreate }: NewProjectDialogProps) {
  const [name, setName] = useState("");

  async function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    const value = name.trim();
    if (!value) return;
    await onCreate(value);
  }

  return (
    <div className="feature-dialog-backdrop" role="presentation" onMouseDown={() => !busy && onCancel()}>
      <form
        className="feature-dialog new-project-dialog"
        role="dialog"
        aria-modal="true"
        aria-labelledby="new-project-title"
        onSubmit={(event) => void submit(event)}
        onMouseDown={(event) => event.stopPropagation()}
      >
        <div className="feature-dialog-header">
          <div>
            <span className="feature-dialog-kicker">New project</span>
            <h3 id="new-project-title">Create from scratch</h3>
          </div>
          <button type="button" onClick={onCancel} disabled={busy} aria-label="Close">×</button>
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
          <button type="button" onClick={onCancel} disabled={busy}>Cancel</button>
          <button type="submit" className="primary" disabled={busy || !name.trim()}>{busy ? "Creating…" : "Create project"}</button>
        </div>
      </form>
    </div>
  );
}

export default NewProjectDialog;
