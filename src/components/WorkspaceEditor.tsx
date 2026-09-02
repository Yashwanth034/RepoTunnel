import { useCallback, useEffect, useMemo, useRef, useState, type CSSProperties, type PointerEvent as ReactPointerEvent } from "react";
import { NavIcon } from "./AppSidebar";
import type { GitFileChange } from "../types";
import DeveloperDock, { type EditorProblem } from "./DeveloperDock";
import CodeMirrorSurface from "./CodeMirrorSurface";

export type EditorDocument = {
  key: string;
  workspaceId: string;
  workspaceName: string;
  path: string;
  name: string;
  kind: "text" | "image" | "binary";
  language: string;
  content: string;
  savedContent: string;
  size: number;
  modifiedAt: number | null;
  imageDataUrl: string | null;
  dirty: boolean;
  externalContent: string | null;
  externalModifiedAt: number | null;
  conflict: boolean;
  externalDeleted: boolean;
  updatedExternally: boolean;
  readonly: boolean;
};

export type EditorRevealLocation = {
  key: string;
  line: number;
  column: number;
  token: number;
};

export type EditorSelectionContext = {
  workspaceId: string;
  path: string;
  content: string;
  start: number;
  end: number;
};

type WorkspaceEditorProps = {
  tabs: EditorDocument[];
  activeKey: string | null;
  savingKey: string | null;
  workspacePathById: Record<string, string>;
  gitChanges: GitFileChange[];
  revealLocation: EditorRevealLocation | null;
  secondaryKey: string | null;
  onSecondaryChange: (key: string | null) => void;
  onSelect: (key: string) => void;
  onClose: (key: string) => void;
  onChange: (key: string, content: string) => void;
  onSave: (key: string) => Promise<boolean>;
  onOpenExternal: (key: string) => void;
  onReloadExternal: (key: string) => void;
  onKeepLocal: (key: string) => void;
  onDismissExternalNotice: (key: string) => void;
  onOpenProblem: (workspaceId: string, path: string, line: number, column: number) => void;
  onSelectionContext?: (selection: EditorSelectionContext | null) => void;
  onProblemsChange?: (problems: EditorProblem[]) => void;
  canReopenClosed: boolean;
  onReopenClosed: () => void;
  onNotice: (message: string) => void;
};

function languageLabel(language: string): string {
  const labels: Record<string, string> = {
    javascript: "JavaScript",
    typescript: "TypeScript",
    jsx: "React JSX",
    tsx: "React TSX",
    python: "Python",
    rust: "Rust",
    html: "HTML",
    css: "CSS",
    json: "JSON",
    markdown: "Markdown",
    yaml: "YAML",
    toml: "TOML",
    sql: "SQL",
    shell: "Shell",
    text: "Plain text",
  };
  return labels[language] ?? language.toUpperCase();
}

function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

function indentUnit(language: string): string {
  return language === "python" ? "    " : "  ";
}

function gitMarker(change: GitFileChange | undefined): string {
  if (!change) return "";
  if (change.conflicted) return "!";
  if (change.untracked) return "U";
  if (change.staged && change.unstaged) return "M";
  if (change.staged) return "S";
  if (change.unstaged) return "M";
  return "";
}

function EditorPane({
  document,
  savingKey,
  gitChange,
  revealLocation,
  problems,
  compact = false,
  onChange,
  onSave,
  onOpenExternal,
  onReloadExternal,
  onKeepLocal,
  onDismissExternalNotice,
  onSelectionContext,
}: {
  document: EditorDocument;
  savingKey: string | null;
  gitChange?: GitFileChange;
  revealLocation: EditorRevealLocation | null;
  problems: EditorProblem[];
  compact?: boolean;
  onChange: (key: string, content: string) => void;
  onSave: (key: string) => Promise<boolean>;
  onOpenExternal: (key: string) => void;
  onReloadExternal: (key: string) => void;
  onKeepLocal: (key: string) => void;
  onDismissExternalNotice: (key: string) => void;
  onSelectionContext?: (selection: EditorSelectionContext | null) => void;
}) {
  const [showConflictCompare, setShowConflictCompare] = useState(false);
  const [cursor, setCursor] = useState({ line: 1, column: 1 });
  const [goToLineToken, setGoToLineToken] = useState(0);
  useEffect(() => setShowConflictCompare(false), [document.key]);
  useEffect(() => setCursor({ line: 1, column: 1 }), [document.key]);
  const breadcrumbs = document.path.split("/");
  const marker = gitMarker(gitChange);
  const fileProblems = useMemo(() => problems.filter((problem) => problem.path === document.path), [problems, document.path]);
  const errorCount = fileProblems.filter((problem) => problem.severity === "error").length;
  const warningCount = fileProblems.length - errorCount;
  const indent = indentUnit(document.language).length;

  return (
    <section className={`editor-pane ${compact ? "secondary" : "primary"}`}>
      <div className="editor-pane-toolbar">
        <div className="editor-breadcrumbs" title={document.path}>
          {!compact ? <strong>{document.workspaceName}</strong> : null}
          {breadcrumbs.map((part, index) => (
            <span key={`${part}-${index}`}><b>›</b>{part}</span>
          ))}
          {marker ? <span className={`editor-git-marker ${marker === "U" ? "untracked" : marker === "!" ? "conflict" : "modified"}`}>{marker}</span> : null}
          {fileProblems.length > 0 ? (
            <span className="editor-diagnostic-badge" title={`${errorCount} errors, ${warningCount} warnings`}>
              {errorCount > 0 ? `× ${errorCount}` : ""}{errorCount > 0 && warningCount > 0 ? " · " : ""}{warningCount > 0 ? `! ${warningCount}` : ""}
            </span>
          ) : null}
        </div>
        <div className="editor-toolbar-actions">
          {document.kind === "text" ? (
            <div className="editor-view-controls" aria-label="Editor navigation controls">
              <button type="button" title="Go to line · Ctrl+G" onClick={() => setGoToLineToken((current) => current + 1)}>Line</button>
            </div>
          ) : null}
          <span className="editor-file-meta">
            {document.kind === "text" ? languageLabel(document.language) : document.kind === "image" ? "Image" : "File"}
            {` · ${formatBytes(document.size)}`}
          </span>
          <button type="button" className="editor-tool-button" onClick={() => onOpenExternal(document.key)}>Open externally</button>
          {document.kind === "text" && !document.readonly ? (
            <button
              type="button"
              className="primary-button editor-save-button"
              disabled={!document.dirty || savingKey === document.key || document.externalDeleted}
              onClick={() => void onSave(document.key)}
            >
              {savingKey === document.key ? "Saving…" : "Save"}
            </button>
          ) : document.kind === "text" ? <span className="editor-readonly-badge">Read only</span> : null}
        </div>
      </div>

      {document.conflict ? (
        <div className="editor-conflict-banner">
          <div>
            <strong>{document.path} changed externally</strong>
            <span>Your editor has unsaved changes, so RepoTunnel did not overwrite them.</span>
          </div>
          <div>
            <button type="button" onClick={() => setShowConflictCompare((current) => !current)}>{showConflictCompare ? "Hide compare" : "Compare"}</button>
            <button type="button" onClick={() => onReloadExternal(document.key)}>Reload external version</button>
            <button type="button" className="primary-button" onClick={() => onKeepLocal(document.key)}>Keep my version</button>
          </div>
        </div>
      ) : document.externalDeleted ? (
        <div className="editor-conflict-banner danger-banner">
          <div><strong>File deleted externally</strong><span>Save is disabled. Close this tab or recreate the file from the project explorer.</span></div>
        </div>
      ) : document.updatedExternally ? (
        <div className="editor-update-banner">
          <span><strong>Updated externally.</strong> The editor reloaded the latest file contents.</span>
          <button type="button" onClick={() => onDismissExternalNotice(document.key)}>Dismiss</button>
        </div>
      ) : null}

      {showConflictCompare && document.conflict ? (
        <div className="editor-compare-panel">
          <div><strong>Your unsaved version</strong><pre>{document.content}</pre></div>
          <div><strong>Latest external version</strong><pre>{document.externalContent ?? ""}</pre></div>
        </div>
      ) : null}

      <div className="editor-body">
        {document.kind === "text" ? (
          <CodeMirrorSurface
            key={document.key}
            documentKey={document.key}
            path={document.path}
            language={document.language}
            content={document.content}
            readonly={document.readonly}
            modifiedAt={document.modifiedAt}
            problems={fileProblems}
            revealLocation={revealLocation}
            goToLineToken={goToLineToken}
            onChange={(content) => onChange(document.key, content)}
            onSave={() => onSave(document.key)}
            onCursorChange={(line, column) => setCursor((current) => current.line === line && current.column === column ? current : { line, column })}
            onSelectionChange={(selection) => onSelectionContext?.(selection ? { workspaceId: document.workspaceId, path: document.path, ...selection } : null)}
          />
        ) : document.kind === "image" && document.imageDataUrl ? (
          <div className="image-preview-panel">
            <div className="image-preview-canvas"><img src={document.imageDataUrl} alt={document.name} /></div>
            <p>{document.path} · {formatBytes(document.size)}</p>
          </div>
        ) : (
          <div className="binary-file-panel">
            <div className="binary-file-icon">FILE</div>
            <h2>{document.name}</h2>
            <p>This file is not a UTF-8 code/text file supported by the built-in editor.</p>
            <button type="button" className="primary-button" onClick={() => onOpenExternal(document.key)}>Open with the system application</button>
          </div>
        )}
      </div>

      <footer className="editor-statusbar">
        <span className="editor-status-path">{document.path}</span>
        {document.kind === "text" ? <span>Ln {cursor.line}, Col {cursor.column}</span> : null}
        {document.kind === "text" ? <span>Spaces: {indent}</span> : null}
        {document.kind === "text" ? <span>UTF-8</span> : null}
        {document.kind === "text" ? <span>{languageLabel(document.language)}</span> : null}
        <span>{formatBytes(document.size)}</span>
        {marker ? <span>Git {marker}</span> : null}
        <span className={document.dirty ? "editor-status-dirty" : ""}>{document.readonly ? "Read only" : document.dirty ? "Unsaved" : "Saved"}</span>
      </footer>
    </section>
  );
}

function WorkspaceEditor({
  tabs,
  activeKey,
  savingKey,
  workspacePathById,
  gitChanges,
  revealLocation,
  secondaryKey,
  onSecondaryChange,
  onSelect,
  onClose,
  onChange,
  onSave,
  onOpenExternal,
  onReloadExternal,
  onKeepLocal,
  onDismissExternalNotice,
  onOpenProblem,
  onSelectionContext,
  onProblemsChange,
  canReopenClosed,
  onReopenClosed,
  onNotice,
}: WorkspaceEditorProps) {
  const active = tabs.find((tab) => tab.key === activeKey) ?? tabs[0] ?? null;
  const [problems, setProblems] = useState<EditorProblem[]>([]);
  const [pendingCloseKey, setPendingCloseKey] = useState<string | null>(null);
  const [splitRatio, setSplitRatio] = useState(50);
  const editorPanesRef = useRef<HTMLDivElement | null>(null);
  const handleSelectionContext = useCallback((selection: EditorSelectionContext | null) => {
    onSelectionContext?.(selection);
  }, [onSelectionContext]);

  useEffect(() => {
    const secondary = secondaryKey ? tabs.find((tab) => tab.key === secondaryKey) : null;
    if (secondaryKey && !secondary) onSecondaryChange(null);
    else if (secondaryKey === active?.key) onSecondaryChange(null);
    else if (secondary && active && secondary.workspaceId !== active.workspaceId) onSecondaryChange(null);
  }, [tabs, active?.key, secondaryKey, onSecondaryChange]);

  useEffect(() => {
    if (!active || active.kind !== "text") handleSelectionContext(null);
  }, [active?.key, active?.kind, handleSelectionContext]);

  useEffect(() => {
    function reopenShortcut(event: globalThis.KeyboardEvent) {
      if (!(event.ctrlKey || event.metaKey) || !event.shiftKey || event.key.toLowerCase() !== "t") return;
      event.preventDefault();
      if (canReopenClosed) onReopenClosed();
    }
    window.addEventListener("keydown", reopenShortcut, true);
    return () => window.removeEventListener("keydown", reopenShortcut, true);
  }, [canReopenClosed, onReopenClosed]);

  if (!active) {
    return (
      <section className="workspace-editor-empty">
        <div className="workspace-editor-empty-icon"><NavIcon name="folder" size={28} /></div>
        <h2>Open a file from the project tree</h2>
        <p>Expand a project in the Projects column, then choose a code or text file to edit it here.</p>
      </section>
    );
  }

  const secondary = secondaryKey ? tabs.find((tab) => tab.key === secondaryKey) ?? null : null;
  const gitByPath = new Map(gitChanges.map((change) => [change.path, change]));
  const workspacePath = workspacePathById[active.workspaceId] ?? "";
  const tabProblemCount = (tab: EditorDocument) => problems.filter((problem) => problem.path === tab.path).length;

  function requestClose(key: string) {
    const tab = tabs.find((item) => item.key === key);
    if (!tab) return;
    if (!tab.dirty) onClose(key);
    else setPendingCloseKey(key);
  }

  async function saveThenClose() {
    if (!pendingCloseKey) return;
    const key = pendingCloseKey;
    const saved = await onSave(key);
    if (!saved) return;
    setPendingCloseKey(null);
    onClose(key);
  }

  function updateSplitRatio(next: number) {
    setSplitRatio(Math.max(25, Math.min(75, Math.round(next))));
  }

  function beginSplitResize(event: ReactPointerEvent<HTMLDivElement>) {
    if (!secondary || !editorPanesRef.current) return;
    event.preventDefault();
    const container = editorPanesRef.current;
    const rect = container.getBoundingClientRect();
    const pointerId = event.pointerId;
    event.currentTarget.setPointerCapture?.(pointerId);

    const move = (moveEvent: PointerEvent) => {
      const ratio = ((moveEvent.clientX - rect.left) / Math.max(1, rect.width)) * 100;
      updateSplitRatio(ratio);
    };
    const up = () => {
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", up);
    };
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", up, { once: true });
  }

  const pendingClose = pendingCloseKey ? tabs.find((tab) => tab.key === pendingCloseKey) ?? null : null;

  return (
    <section className="workspace-editor">
      <div className="editor-tabs" role="tablist" aria-label="Open files">
        {tabs.map((tab) => {
          const marker = gitMarker(gitByPath.get(tab.path));
          const problemCount = tabProblemCount(tab);
          return (
            <button
              type="button"
              role="tab"
              aria-selected={tab.key === active.key}
              className={`editor-tab ${tab.key === active.key ? "active" : ""}`}
              key={tab.key}
              onClick={() => onSelect(tab.key)}
              title={tab.path}
            >
              <span className="editor-tab-type">{tab.kind === "image" ? "IMG" : tab.kind === "binary" ? "FILE" : tab.language.slice(0, 2).toUpperCase()}</span>
              <span>{tab.name}</span>
              {problemCount > 0 ? <em className="editor-tab-problems" title={`${problemCount} problem${problemCount === 1 ? "" : "s"}`}>{problemCount}</em> : null}
              {marker ? <em className={`editor-tab-git ${marker === "U" ? "untracked" : marker === "!" ? "conflict" : "modified"}`}>{marker}</em> : null}
              {tab.dirty ? <i title="Unsaved changes">•</i> : <i />}
              <span
                className="editor-tab-close"
                role="button"
                tabIndex={0}
                aria-label={`Close ${tab.name}`}
                onClick={(event) => { event.stopPropagation(); requestClose(tab.key); }}
                onKeyDown={(event) => {
                  if (event.key === "Enter" || event.key === " ") { event.preventDefault(); requestClose(tab.key); }
                }}
              >×</span>
            </button>
          );
        })}
        <div className="editor-productivity-shortcuts">
          {canReopenClosed ? <button type="button" title="Reopen closed tab · Ctrl+Shift+T" onClick={onReopenClosed}>Reopen</button> : null}
          <button type="button" title="Quick Open · Ctrl+P" onClick={() => window.dispatchEvent(new CustomEvent("repotunnel:productivity", { detail: "quick" }))}>Open</button>
          <button type="button" title="Search in Project · Ctrl+Shift+F" onClick={() => window.dispatchEvent(new CustomEvent("repotunnel:productivity", { detail: "search" }))}>Search</button>
          <button type="button" title="Command Palette · Ctrl+Shift+P" onClick={() => window.dispatchEvent(new CustomEvent("repotunnel:productivity", { detail: "command" }))}>⌘</button>
        </div>
        <div className="editor-split-control">
          <span>Split</span>
          <select value={secondaryKey ?? ""} onChange={(event) => onSecondaryChange(event.target.value || null)} title="Open a second file side-by-side">
            <option value="">Off</option>
            {tabs.filter((tab) => tab.key !== active.key && tab.workspaceId === active.workspaceId).map((tab) => <option key={tab.key} value={tab.key}>{tab.name}</option>)}
          </select>
        </div>
      </div>

      <div
        ref={editorPanesRef}
        className={`editor-panes ${secondary ? "split" : "single"}`}
        style={secondary ? ({ gridTemplateColumns: `minmax(0, ${splitRatio}fr) 6px minmax(0, ${100 - splitRatio}fr)` } as CSSProperties) : undefined}
      >
        <EditorPane
          document={active}
          savingKey={savingKey}
          gitChange={gitByPath.get(active.path)}
          revealLocation={revealLocation}
          problems={problems}
          onChange={onChange}
          onSave={onSave}
          onOpenExternal={onOpenExternal}
          onReloadExternal={onReloadExternal}
          onKeepLocal={onKeepLocal}
          onDismissExternalNotice={onDismissExternalNotice}
          onSelectionContext={handleSelectionContext}
        />
        {secondary ? (
          <div
            className="editor-split-resizer"
            role="separator"
            aria-label="Resize editor split"
            aria-orientation="vertical"
            tabIndex={0}
            onPointerDown={beginSplitResize}
            onDoubleClick={() => updateSplitRatio(50)}
            onKeyDown={(event) => {
              if (event.key === "ArrowLeft") { event.preventDefault(); updateSplitRatio(splitRatio - 5); }
              if (event.key === "ArrowRight") { event.preventDefault(); updateSplitRatio(splitRatio + 5); }
              if (event.key === "Home") { event.preventDefault(); updateSplitRatio(50); }
            }}
            title="Drag to resize split · double-click to reset"
          />
        ) : null}
        {secondary ? (
          <EditorPane
            compact
            document={secondary}
            savingKey={savingKey}
            gitChange={gitByPath.get(secondary.path)}
            revealLocation={revealLocation}
            problems={problems}
            onChange={onChange}
            onSave={onSave}
            onOpenExternal={onOpenExternal}
            onReloadExternal={onReloadExternal}
            onKeepLocal={onKeepLocal}
            onDismissExternalNotice={onDismissExternalNotice}
          />
        ) : null}
      </div>

      <DeveloperDock
        workspaceId={active.workspaceId}
        workspaceName={active.workspaceName}
        workspacePath={workspacePath}
        onOpenProblem={(path, line, column) => onOpenProblem(active.workspaceId, path, line, column)}
        onProblemsChange={(nextProblems) => { setProblems(nextProblems); onProblemsChange?.(nextProblems); }}
        onNotice={onNotice}
      />

      {pendingClose ? (
        <div className="editor-unsaved-overlay" role="presentation" onMouseDown={(event) => { if (event.target === event.currentTarget) setPendingCloseKey(null); }}>
          <section className="editor-unsaved-dialog" role="dialog" aria-modal="true" aria-label="Unsaved changes">
            <div className="editor-unsaved-icon">•</div>
            <div>
              <h3>Save changes to {pendingClose.name}?</h3>
              <p>Your edits are unsaved. Save them before closing, close without saving, or cancel.</p>
            </div>
            <div className="editor-unsaved-actions">
              <button type="button" onClick={() => setPendingCloseKey(null)}>Cancel</button>
              <button type="button" className="danger" onClick={() => { const key = pendingClose.key; setPendingCloseKey(null); onClose(key); }}>Don’t Save</button>
              <button type="button" className="primary-button" disabled={savingKey === pendingClose.key} onClick={() => void saveThenClose()}>{savingKey === pendingClose.key ? "Saving…" : "Save & Close"}</button>
            </div>
          </section>
        </div>
      ) : null}
    </section>
  );
}

export default WorkspaceEditor;
