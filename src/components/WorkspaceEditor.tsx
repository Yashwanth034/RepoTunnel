import { useEffect, useMemo, useRef, useState, type CSSProperties, type KeyboardEvent, type PointerEvent as ReactPointerEvent, type ReactNode } from "react";
import { NavIcon } from "./AppSidebar";
import type { GitFileChange } from "../types";
import DeveloperDock, { type EditorProblem } from "./DeveloperDock";

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
  canReopenClosed: boolean;
  onReopenClosed: () => void;
  onNotice: (message: string) => void;
};

const KEYWORDS: Record<string, Set<string>> = {
  javascript: new Set(["as", "async", "await", "break", "case", "catch", "class", "const", "continue", "default", "delete", "do", "else", "export", "extends", "false", "finally", "for", "from", "function", "if", "import", "in", "instanceof", "interface", "let", "new", "null", "of", "return", "static", "super", "switch", "this", "throw", "true", "try", "type", "typeof", "undefined", "var", "void", "while", "yield"]),
  python: new Set(["and", "as", "assert", "async", "await", "break", "class", "continue", "def", "del", "elif", "else", "except", "False", "finally", "for", "from", "global", "if", "import", "in", "is", "lambda", "None", "nonlocal", "not", "or", "pass", "raise", "return", "True", "try", "while", "with", "yield"]),
  rust: new Set(["as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern", "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub", "ref", "return", "self", "Self", "static", "struct", "super", "trait", "true", "type", "unsafe", "use", "where", "while"]),
  sql: new Set(["SELECT", "FROM", "WHERE", "INSERT", "INTO", "VALUES", "UPDATE", "DELETE", "CREATE", "ALTER", "DROP", "TABLE", "JOIN", "LEFT", "RIGHT", "INNER", "OUTER", "ON", "AS", "AND", "OR", "NOT", "NULL", "ORDER", "BY", "GROUP", "HAVING", "LIMIT", "OFFSET", "DISTINCT"]),
};

function normalizedLanguage(language: string): string {
  if (["typescript", "tsx", "jsx", "javascript"].includes(language)) return "javascript";
  return language;
}

function commentStart(language: string, line: string, index: number): boolean {
  if (["javascript", "rust", "css"].includes(language) && line.startsWith("//", index)) return true;
  if (["python", "yaml", "shell"].includes(language) && line[index] === "#") return true;
  return language === "sql" && line.startsWith("--", index);
}

function tokenizedLine(
  line: string,
  language: string,
  lineIndex: number,
  lineStart = 0,
  matchedOffsets: Set<number> = new Set(),
): ReactNode {
  if (language === "markdown" && /^\s{0,3}#{1,6}\s/.test(line)) {
    return <span className="tok-heading">{line || " "}</span>;
  }
  if (language === "markdown" && /^\s*>/.test(line)) {
    return <span className="tok-comment">{line || " "}</span>;
  }

  const nodes: ReactNode[] = [];
  const normalized = normalizedLanguage(language);
  const keywords = KEYWORDS[normalized] ?? new Set<string>();
  let index = 0;

  while (index < line.length) {
    if (commentStart(normalized, line, index)) {
      nodes.push(<span key={`${lineIndex}-${index}`} className="tok-comment">{line.slice(index)}</span>);
      break;
    }

    const char = line[index];
    if (char === '"' || char === "'" || char === "`") {
      let end = index + 1;
      let escaped = false;
      while (end < line.length) {
        const current = line[end];
        if (!escaped && current === char) {
          end += 1;
          break;
        }
        if (current === "\\" && !escaped) escaped = true;
        else escaped = false;
        end += 1;
      }
      nodes.push(<span key={`${lineIndex}-${index}`} className="tok-string">{line.slice(index, end)}</span>);
      index = end;
      continue;
    }

    if (/\d/.test(char)) {
      let end = index + 1;
      while (end < line.length && /[\d._xXa-fA-F]/.test(line[end])) end += 1;
      nodes.push(<span key={`${lineIndex}-${index}`} className="tok-number">{line.slice(index, end)}</span>);
      index = end;
      continue;
    }

    if (/[A-Za-z_$]/.test(char)) {
      let end = index + 1;
      while (end < line.length && /[A-Za-z0-9_$]/.test(line[end])) end += 1;
      const word = line.slice(index, end);
      const lookup = normalized === "sql" ? word.toUpperCase() : word;
      if (keywords.has(lookup)) {
        nodes.push(<span key={`${lineIndex}-${index}`} className="tok-keyword">{word}</span>);
      } else if (/^(TODO|FIXME|NOTE)$/.test(word)) {
        nodes.push(<span key={`${lineIndex}-${index}`} className="tok-note">{word}</span>);
      } else {
        nodes.push(word);
      }
      index = end;
      continue;
    }

    if (/[{}()[\];,:.+\-*=&|!?<>/]/.test(char)) {
      nodes.push(<span key={`${lineIndex}-${index}`} className={`tok-punctuation ${matchedOffsets.has(lineStart + index) ? "bracket-match" : ""}`}>{char}</span>);
    } else {
      nodes.push(char);
    }
    index += 1;
  }

  return nodes.length > 0 ? nodes : " ";
}

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

function lineColumnAt(content: string, position: number): { line: number; column: number } {
  const safe = Math.max(0, Math.min(position, content.length));
  const before = content.slice(0, safe);
  const line = before.split("\n").length;
  const lastBreak = before.lastIndexOf("\n");
  return { line, column: safe - lastBreak };
}

function lineStartOffsets(content: string): number[] {
  const starts = [0];
  for (let index = 0; index < content.length; index += 1) if (content[index] === "\n") starts.push(index + 1);
  return starts;
}

function matchingBracketOffsets(content: string, caret: number): Set<number> {
  const pairs: Record<string, string> = { "(": ")", "[": "]", "{": "}" };
  const reverse: Record<string, string> = { ")": "(", "]": "[", "}": "{" };
  const candidates = [caret, caret - 1].filter((value) => value >= 0 && value < content.length);
  const source = candidates.find((value) => pairs[content[value]] || reverse[content[value]]);
  if (source === undefined) return new Set();
  const char = content[source];
  const forward = Boolean(pairs[char]);
  const open = forward ? char : reverse[char];
  const close = forward ? pairs[char] : char;
  let depth = 0;
  if (forward) {
    for (let index = source; index < content.length; index += 1) {
      if (content[index] === open) depth += 1;
      else if (content[index] === close) depth -= 1;
      if (depth === 0) return new Set([source, index]);
    }
  } else {
    for (let index = source; index >= 0; index -= 1) {
      if (content[index] === close) depth += 1;
      else if (content[index] === open) depth -= 1;
      if (depth === 0) return new Set([source, index]);
    }
  }
  return new Set([source]);
}

function CodeSurface({
  document,
  problems,
  onChange,
  onSave,
  onCursorChange,
  revealLocation,
  goToLineToken,
}: {
  document: EditorDocument;
  problems: EditorProblem[];
  onChange: (content: string) => void;
  onSave: () => Promise<boolean>;
  onCursorChange: (line: number, column: number) => void;
  revealLocation?: EditorRevealLocation | null;
  goToLineToken: number;
}) {
  const textareaRef = useRef<HTMLTextAreaElement>(null);
  const preRef = useRef<HTMLPreElement>(null);
  const gutterRef = useRef<HTMLDivElement>(null);
  const [findOpen, setFindOpen] = useState(false);
  const [findQuery, setFindQuery] = useState("");
  const [findMiss, setFindMiss] = useState(false);
  const [goToLineOpen, setGoToLineOpen] = useState(false);
  const [goToLineValue, setGoToLineValue] = useState("");
  const [cursorPosition, setCursorPosition] = useState(0);
  const lines = useMemo(() => document.content.split("\n"), [document.content]);
  const starts = useMemo(() => lineStartOffsets(document.content), [document.content]);
  const cursor = useMemo(() => lineColumnAt(document.content, cursorPosition), [document.content, cursorPosition]);
  const matchedOffsets = useMemo(() => matchingBracketOffsets(document.content, cursorPosition), [document.content, cursorPosition]);
  const problemsByLine = useMemo(() => {
    const map = new Map<number, EditorProblem[]>();
    for (const problem of problems) {
      const current = map.get(problem.line) ?? [];
      current.push(problem);
      map.set(problem.line, current);
    }
    return map;
  }, [problems]);

  useEffect(() => onCursorChange(cursor.line, cursor.column), [cursor.line, cursor.column, onCursorChange]);

  useEffect(() => {
    const textarea = textareaRef.current;
    if (!textarea) return;
    // A CodeSurface instance is reused when switching tabs. Reset both the real
    // textarea and the syntax/gutter layers together so a previous file's scroll
    // transform can never leave the newly selected file looking blank.
    textarea.scrollTop = 0;
    textarea.scrollLeft = 0;
    setCursorPosition(0);
    const frame = window.requestAnimationFrame(syncScroll);
    return () => window.cancelAnimationFrame(frame);
  }, [document.key]);

  useEffect(() => {
    if (goToLineToken <= 0) return;
    setGoToLineValue(String(cursor.line));
    setGoToLineOpen(true);
  }, [goToLineToken]);

  useEffect(() => {
    if (!revealLocation || revealLocation.key !== document.key) return;
    const textarea = textareaRef.current;
    if (!textarea) return;
    const targetLine = Math.max(1, revealLocation.line);
    const targetColumn = Math.max(1, revealLocation.column);
    const start = starts[Math.min(targetLine - 1, starts.length - 1)] ?? 0;
    const lineEnd = document.content.indexOf("\n", start);
    const max = lineEnd < 0 ? document.content.length : lineEnd;
    const position = Math.min(max, start + targetColumn - 1);
    textarea.focus();
    textarea.setSelectionRange(position, position);
    setCursorPosition(position);
    const lineHeight = 19;
    textarea.scrollTop = Math.max(0, (targetLine - 3) * lineHeight);
    syncScroll();
  }, [revealLocation?.token, revealLocation?.key, revealLocation?.line, revealLocation?.column, document.key, document.content, starts]);

  function syncScroll() {
    const textarea = textareaRef.current;
    if (!textarea) return;
    if (preRef.current) preRef.current.style.transform = `translate(${-textarea.scrollLeft}px, ${-textarea.scrollTop}px)`;
    if (gutterRef.current) gutterRef.current.style.transform = `translateY(${-textarea.scrollTop}px)`;
  }

  function updateCursor(textarea: HTMLTextAreaElement) {
    setCursorPosition(textarea.selectionStart);
  }

  function findNext(reverse = false) {
    const textarea = textareaRef.current;
    const needle = findQuery;
    if (!textarea || !needle) return;
    const haystack = document.content.toLowerCase();
    const query = needle.toLowerCase();
    let index = reverse
      ? haystack.lastIndexOf(query, Math.max(0, textarea.selectionStart - 1))
      : haystack.indexOf(query, textarea.selectionEnd);
    if (index < 0) index = reverse ? haystack.lastIndexOf(query) : haystack.indexOf(query);
    setFindMiss(index < 0);
    if (index >= 0) {
      textarea.focus();
      textarea.setSelectionRange(index, index + needle.length);
      setCursorPosition(index);
    }
  }

  function goToLine() {
    const textarea = textareaRef.current;
    if (!textarea) return;
    const requested = Number.parseInt(goToLineValue, 10);
    if (!Number.isFinite(requested)) return;
    const line = Math.max(1, Math.min(lines.length, requested));
    const position = starts[line - 1] ?? 0;
    textarea.focus();
    textarea.setSelectionRange(position, position);
    setCursorPosition(position);
    textarea.scrollTop = Math.max(0, (line - 3) * 19);
    syncScroll();
    setGoToLineOpen(false);
  }

  function replaceSelection(nextText: string, selectionStart: number, selectionEnd = selectionStart) {
    const textarea = textareaRef.current;
    onChange(nextText);
    window.requestAnimationFrame(() => {
      if (!textarea) return;
      textarea.setSelectionRange(selectionStart, selectionEnd);
      setCursorPosition(selectionStart);
    });
  }

  function handleTab(event: KeyboardEvent<HTMLTextAreaElement>) {
    event.preventDefault();
    const textarea = event.currentTarget;
    const start = textarea.selectionStart;
    const end = textarea.selectionEnd;
    const unit = indentUnit(document.language);
    const lineStart = document.content.lastIndexOf("\n", Math.max(0, start - 1)) + 1;
    const selection = document.content.slice(lineStart, end);
    if (selection.includes("\n") || end > start) {
      const linesToIndent = selection.split("\n");
      const transformed = linesToIndent.map((line) => {
        if (!event.shiftKey) return `${unit}${line}`;
        if (line.startsWith(unit)) return line.slice(unit.length);
        return line.replace(/^ {1,4}/, "");
      }).join("\n");
      const next = `${document.content.slice(0, lineStart)}${transformed}${document.content.slice(end)}`;
      const delta = transformed.length - selection.length;
      replaceSelection(next, start + (event.shiftKey ? -Math.min(unit.length, start - lineStart) : unit.length), end + delta);
      return;
    }
    if (event.shiftKey) {
      const before = document.content.slice(lineStart, start);
      const remove = before.endsWith(unit) ? unit.length : Math.min((before.match(/\s+$/)?.[0].length ?? 0), unit.length);
      if (remove > 0) {
        const removeStart = start - remove;
        const next = `${document.content.slice(0, removeStart)}${document.content.slice(start)}`;
        replaceSelection(next, removeStart);
      }
      return;
    }
    const next = `${document.content.slice(0, start)}${unit}${document.content.slice(end)}`;
    replaceSelection(next, start + unit.length);
  }

  function handleEnter(event: KeyboardEvent<HTMLTextAreaElement>) {
    event.preventDefault();
    const textarea = event.currentTarget;
    const start = textarea.selectionStart;
    const end = textarea.selectionEnd;
    const beforeLineStart = document.content.lastIndexOf("\n", Math.max(0, start - 1)) + 1;
    const currentBefore = document.content.slice(beforeLineStart, start);
    const leading = currentBefore.match(/^\s*/)?.[0] ?? "";
    const trimmed = currentBefore.trimEnd();
    const unit = indentUnit(document.language);
    const opensBlock = /[\{\[\(:]$/.test(trimmed) || (document.language === "python" && trimmed.endsWith(":")) || /<([A-Za-z][\w:-]*)(?:\s[^<>]*)?>$/.test(trimmed);
    const after = document.content.slice(end);
    const closesBlock = /^\s*[}\])]/.test(after);
    if (opensBlock && closesBlock) {
      const inserted = `\n${leading}${unit}\n${leading}`;
      const next = `${document.content.slice(0, start)}${inserted}${after}`;
      replaceSelection(next, start + 1 + leading.length + unit.length);
      return;
    }
    const indent = opensBlock ? `${leading}${unit}` : leading;
    const inserted = `\n${indent}`;
    const next = `${document.content.slice(0, start)}${inserted}${after}`;
    replaceSelection(next, start + inserted.length);
  }

  function selectedLineRange(textarea: HTMLTextAreaElement) {
    const selectionStart = textarea.selectionStart;
    const selectionEnd = textarea.selectionEnd;
    const start = document.content.lastIndexOf("\n", Math.max(0, selectionStart - 1)) + 1;
    const endBreak = document.content.indexOf("\n", selectionEnd);
    const end = endBreak < 0 ? document.content.length : endBreak;
    return { start, end, selectionStart, selectionEnd };
  }

  function duplicateSelectedLines(event: KeyboardEvent<HTMLTextAreaElement>) {
    event.preventDefault();
    const textarea = event.currentTarget;
    const range = selectedLineRange(textarea);
    const block = document.content.slice(range.start, range.end);
    const inserted = `\n${block}`;
    const next = `${document.content.slice(0, range.end)}${inserted}${document.content.slice(range.end)}`;
    const shift = inserted.length;
    replaceSelection(next, range.selectionStart + shift, range.selectionEnd + shift);
  }

  function moveSelectedLines(event: KeyboardEvent<HTMLTextAreaElement>, direction: -1 | 1) {
    event.preventDefault();
    const textarea = event.currentTarget;
    const range = selectedLineRange(textarea);
    const block = document.content.slice(range.start, range.end);
    if (direction < 0) {
      if (range.start === 0) return;
      const previousEnd = range.start - 1;
      const previousStart = document.content.lastIndexOf("\n", Math.max(0, previousEnd - 1)) + 1;
      const previous = document.content.slice(previousStart, previousEnd);
      const next = `${document.content.slice(0, previousStart)}${block}\n${previous}${document.content.slice(range.end)}`;
      const delta = range.start - previousStart;
      replaceSelection(next, Math.max(0, range.selectionStart - delta), Math.max(0, range.selectionEnd - delta));
      return;
    }
    if (range.end >= document.content.length) return;
    const nextStart = range.end + 1;
    const nextBreak = document.content.indexOf("\n", nextStart);
    const nextEnd = nextBreak < 0 ? document.content.length : nextBreak;
    const following = document.content.slice(nextStart, nextEnd);
    const tail = nextEnd < document.content.length ? document.content.slice(nextEnd) : "";
    const next = `${document.content.slice(0, range.start)}${following}\n${block}${tail}`;
    const delta = following.length + 1;
    replaceSelection(next, range.selectionStart + delta, range.selectionEnd + delta);
  }

  function handleAutoPair(event: KeyboardEvent<HTMLTextAreaElement>): boolean {
    if (event.ctrlKey || event.metaKey || event.altKey) return false;
    const textarea = event.currentTarget;
    const start = textarea.selectionStart;
    const end = textarea.selectionEnd;
    const pairs: Record<string, string> = { "(": ")", "[": "]", "{": "}" };
    const quote = event.key === '"' || event.key === "'" || event.key === "`";
    const allowQuote = quote && !["text", "markdown"].includes(document.language);
    const closing = Object.values(pairs).includes(event.key) || quote;

    if (closing && start === end && document.content[start] === event.key) {
      event.preventDefault();
      textarea.setSelectionRange(start + 1, start + 1);
      setCursorPosition(start + 1);
      return true;
    }

    if (pairs[event.key] || allowQuote) {
      event.preventDefault();
      const close = allowQuote ? event.key : pairs[event.key];
      const selected = document.content.slice(start, end);
      const inserted = `${event.key}${selected}${close}`;
      const next = `${document.content.slice(0, start)}${inserted}${document.content.slice(end)}`;
      if (selected) replaceSelection(next, start + 1, start + 1 + selected.length);
      else replaceSelection(next, start + 1);
      return true;
    }

    if (event.key === "Backspace" && start === end && start > 0) {
      const open = document.content[start - 1];
      const close = document.content[start];
      const matching = pairs[open] === close || ((open === '"' || open === "'" || open === "`") && close === open);
      if (matching) {
        event.preventDefault();
        const next = `${document.content.slice(0, start - 1)}${document.content.slice(start + 1)}`;
        replaceSelection(next, start - 1);
        return true;
      }
    }
    return false;
  }

  function handleKeyDown(event: KeyboardEvent<HTMLTextAreaElement>) {
    if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === "s") {
      event.preventDefault();
      void onSave();
      return;
    }
    if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === "f") {
      event.preventDefault();
      setGoToLineOpen(false);
      setFindOpen(true);
      return;
    }
    if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === "g") {
      event.preventDefault();
      setFindOpen(false);
      setGoToLineValue(String(cursor.line));
      setGoToLineOpen(true);
      return;
    }
    if (document.readonly) return;
    if (event.altKey && event.shiftKey && event.key === "ArrowDown") {
      duplicateSelectedLines(event);
      return;
    }
    if (event.altKey && !event.shiftKey && event.key === "ArrowUp") {
      moveSelectedLines(event, -1);
      return;
    }
    if (event.altKey && !event.shiftKey && event.key === "ArrowDown") {
      moveSelectedLines(event, 1);
      return;
    }
    if (handleAutoPair(event)) return;
    if (event.key === "Tab") {
      handleTab(event);
      return;
    }
    if (event.key === "Enter" && !event.ctrlKey && !event.metaKey && !event.altKey) {
      handleEnter(event);
    }
  }

  return (
    <div className="code-surface-shell">
      {findOpen ? (
        <div className={`editor-find ${findMiss ? "miss" : ""}`}>
          <NavIcon name="search" size={14} />
          <input
            autoFocus
            value={findQuery}
            onChange={(event) => { setFindQuery(event.target.value); setFindMiss(false); }}
            onKeyDown={(event) => {
              if (event.key === "Enter") findNext(event.shiftKey);
              if (event.key === "Escape") setFindOpen(false);
            }}
            placeholder="Find in file"
          />
          <button type="button" onClick={() => findNext(true)} title="Previous match">↑</button>
          <button type="button" onClick={() => findNext(false)} title="Next match">↓</button>
          <button type="button" onClick={() => setFindOpen(false)} title="Close find">×</button>
        </div>
      ) : null}

      {goToLineOpen ? (
        <div className="editor-goto-line">
          <span>Go to line</span>
          <input
            autoFocus
            inputMode="numeric"
            value={goToLineValue}
            onChange={(event) => setGoToLineValue(event.target.value.replace(/[^0-9]/g, ""))}
            onKeyDown={(event) => {
              if (event.key === "Enter") goToLine();
              if (event.key === "Escape") setGoToLineOpen(false);
            }}
            aria-label="Line number"
          />
          <small>1–{lines.length}</small>
          <button type="button" onClick={goToLine}>Go</button>
          <button type="button" onClick={() => setGoToLineOpen(false)} aria-label="Close">×</button>
        </div>
      ) : null}

      <div className="code-gutter" aria-hidden="true">
        <div ref={gutterRef}>
          {lines.map((_, index) => {
            const lineNumber = index + 1;
            const diagnostics = problemsByLine.get(lineNumber) ?? [];
            const severity = diagnostics.some((problem) => problem.severity === "error") ? "error" : diagnostics.length > 0 ? "warning" : "";
            return (
              <span
                key={index}
                className={`${lineNumber === cursor.line ? "active" : ""} ${severity ? `diagnostic ${severity}` : ""}`}
                title={diagnostics.map((problem) => problem.message).join("\n")}
              >
                {severity ? <b className="gutter-diagnostic-dot">{severity === "error" ? "×" : "!"}</b> : null}
                {lineNumber}
              </span>
            );
          })}
        </div>
      </div>

      <div className="code-layer" aria-hidden="true">
        <pre ref={preRef}>
          {lines.map((line, index) => {
            const lineNumber = index + 1;
            const diagnostics = problemsByLine.get(lineNumber) ?? [];
            const severity = diagnostics.some((problem) => problem.severity === "error") ? "error" : diagnostics.length > 0 ? "warning" : "";
            const lineStart = starts[index] ?? 0;
            const firstProblem = diagnostics[0];
            return (
              <span className={`code-highlight-line ${lineNumber === cursor.line ? "active-line" : ""} ${severity ? `diagnostic-line ${severity}` : ""}`} key={index}>
                {tokenizedLine(line, document.language, index, lineStart, matchedOffsets)}
                {firstProblem ? <i className={`diagnostic-squiggle ${firstProblem.severity}`} style={{ left: `${Math.max(0, firstProblem.column - 1)}ch` }} /> : null}
              </span>
            );
          })}
        </pre>
      </div>

      <textarea
        ref={textareaRef}
        className="code-textarea"
        value={document.content}
        readOnly={document.readonly}
        onChange={(event) => {
          onChange(event.target.value);
          setCursorPosition(event.target.selectionStart);
        }}
        onScroll={syncScroll}
        onKeyDown={handleKeyDown}
        onSelect={(event) => updateCursor(event.currentTarget)}
        onClick={(event) => updateCursor(event.currentTarget)}
        onKeyUp={(event) => updateCursor(event.currentTarget)}
        spellCheck={false}
        autoCapitalize="off"
        autoCorrect="off"
        aria-label={`Edit ${document.path}`}
      />
    </div>
  );
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
}) {
  const [showConflictCompare, setShowConflictCompare] = useState(false);
  const [cursor, setCursor] = useState({ line: 1, column: 1 });
  const [goToLineToken, setGoToLineToken] = useState(0);
  useEffect(() => setShowConflictCompare(false), [document.key]);
  useEffect(() => setCursor({ line: 1, column: 1 }), [document.key]);
  const breadcrumbs = document.path.split("/");
  const marker = gitMarker(gitChange);
  const fileProblems = problems.filter((problem) => problem.path === document.path);
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
          <CodeSurface
            key={document.key}
            document={document}
            problems={fileProblems}
            revealLocation={revealLocation}
            goToLineToken={goToLineToken}
            onChange={(content) => onChange(document.key, content)}
            onSave={() => onSave(document.key)}
            onCursorChange={(line, column) => setCursor((current) => current.line === line && current.column === column ? current : { line, column })}
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
  canReopenClosed,
  onReopenClosed,
  onNotice,
}: WorkspaceEditorProps) {
  const active = tabs.find((tab) => tab.key === activeKey) ?? tabs[0] ?? null;
  const [problems, setProblems] = useState<EditorProblem[]>([]);
  const [pendingCloseKey, setPendingCloseKey] = useState<string | null>(null);
  const [splitRatio, setSplitRatio] = useState(50);
  const editorPanesRef = useRef<HTMLDivElement | null>(null);

  useEffect(() => {
    const secondary = secondaryKey ? tabs.find((tab) => tab.key === secondaryKey) : null;
    if (secondaryKey && !secondary) onSecondaryChange(null);
    else if (secondaryKey === active?.key) onSecondaryChange(null);
    else if (secondary && active && secondary.workspaceId !== active.workspaceId) onSecondaryChange(null);
  }, [tabs, active?.key, secondaryKey, onSecondaryChange]);

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
        onProblemsChange={setProblems}
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
