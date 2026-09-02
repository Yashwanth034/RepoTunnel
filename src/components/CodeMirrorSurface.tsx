import { useEffect, useRef } from "react";
import { EditorView, basicSetup } from "codemirror";
import { Annotation, Compartment, EditorState, Prec, Transaction, type Extension } from "@codemirror/state";
import { keymap } from "@codemirror/view";
import {
  copyLineDown,
  copyLineUp,
  deleteLine,
  indentLess,
  indentMore,
  indentWithTab,
  moveLineDown,
  moveLineUp,
  redo,
  selectLine,
  toggleComment,
  undo,
} from "@codemirror/commands";
import { HighlightStyle, StreamLanguage, indentUnit, syntaxHighlighting } from "@codemirror/language";
import { gotoLine } from "@codemirror/search";
import { setDiagnostics, type Diagnostic } from "@codemirror/lint";
import { tags } from "@lezer/highlight";
import type { EditorProblem } from "./DeveloperDock";

export type CodeMirrorRevealLocation = {
  key: string;
  line: number;
  column: number;
  token: number;
};

type CodeMirrorSurfaceProps = {
  documentKey: string;
  path: string;
  language: string;
  content: string;
  readonly: boolean;
  modifiedAt: number | null;
  problems: EditorProblem[];
  revealLocation?: CodeMirrorRevealLocation | null;
  goToLineToken: number;
  onChange: (content: string) => void;
  onSave: () => Promise<boolean>;
  onCursorChange: (line: number, column: number) => void;
  onSelectionChange?: (selection: { content: string; start: number; end: number } | null) => void;
};

const externalSync = Annotation.define<boolean>();

const repoTunnelHighlight = HighlightStyle.define([
  { tag: tags.keyword, color: "#d18ee9" },
  { tag: [tags.string, tags.special(tags.string)], color: "#a9cf82" },
  { tag: [tags.number, tags.bool, tags.null], color: "#e6b673" },
  { tag: [tags.lineComment, tags.blockComment, tags.comment], color: "#657688", fontStyle: "italic" },
  { tag: [tags.typeName, tags.className, tags.namespace], color: "#72b7d8" },
  { tag: [tags.function(tags.variableName), tags.function(tags.propertyName)], color: "#7db7e8" },
  { tag: [tags.definition(tags.variableName), tags.variableName], color: "#c6d0da" },
  { tag: tags.propertyName, color: "#a7c1d7" },
  { tag: [tags.operator, tags.punctuation], color: "#8296a9" },
  { tag: [tags.heading, tags.strong], color: "#69b3f5", fontWeight: "700" },
  { tag: tags.emphasis, fontStyle: "italic" },
  { tag: tags.link, color: "#66a9e8", textDecoration: "underline" },
  { tag: tags.invalid, color: "#e47c84" },
]);

const repoTunnelTheme = EditorView.theme({
  "&": {
    height: "100%",
    minHeight: "0",
    backgroundColor: "#090f14",
    color: "#c6d0da",
    fontSize: "12px",
  },
  "&.cm-focused": { outline: "none" },
  ".cm-scroller": {
    overflow: "auto",
    overscrollBehavior: "contain",
    fontFamily: '"JetBrains Mono", "Cascadia Code", "Fira Code", monospace',
    lineHeight: "19px",
    scrollbarGutter: "stable",
  },
  ".cm-content": {
    minHeight: "100%",
    padding: "10px 0 30px",
    caretColor: "#e6edf3",
  },
  ".cm-line": { padding: "0 14px" },
  ".cm-cursor, .cm-dropCursor": { borderLeftColor: "#e6edf3" },
  ".cm-selectionBackground, &.cm-focused .cm-selectionBackground, ::selection": {
    backgroundColor: "rgba(58,126,196,.38) !important",
  },
  ".cm-activeLine": { backgroundColor: "rgba(81,126,167,.065)" },
  ".cm-gutters": {
    minHeight: "100%",
    backgroundColor: "#0b1117",
    color: "#44515e",
    border: "0",
    borderRight: "1px solid #18232d",
  },
  ".cm-lineNumbers .cm-gutterElement": {
    minWidth: "48px",
    padding: "0 8px 0 12px",
  },
  ".cm-activeLineGutter": {
    backgroundColor: "rgba(80,128,176,.08)",
    color: "#9fb4c6",
  },
  ".cm-foldGutter .cm-gutterElement": { color: "#536474" },
  ".cm-matchingBracket": {
    backgroundColor: "rgba(96,169,230,.16)",
    outline: "1px solid rgba(96,169,230,.65)",
    borderRadius: "2px",
  },
  ".cm-searchMatch": {
    backgroundColor: "rgba(211,164,78,.20)",
    outline: "1px solid rgba(211,164,78,.30)",
  },
  ".cm-searchMatch.cm-searchMatch-selected": { backgroundColor: "rgba(70,143,211,.34)" },
  ".cm-panels": {
    backgroundColor: "#111a22",
    color: "#b8c4ce",
  },
  ".cm-panel.cm-search": {
    padding: "6px 8px",
    borderBottom: "1px solid #2a3945",
  },
  ".cm-panel.cm-search input": {
    border: "1px solid #314150",
    borderRadius: "5px",
    outline: "none",
    backgroundColor: "#0b1218",
    color: "#dce5ed",
  },
  ".cm-panel.cm-search button": {
    border: "1px solid #2b3945",
    borderRadius: "5px",
    backgroundImage: "none",
    backgroundColor: "#16212a",
    color: "#b7c2cc",
  },
  ".cm-tooltip": {
    border: "1px solid #2b3945",
    backgroundColor: "#111a22",
    color: "#cbd5df",
  },
  ".cm-tooltip-autocomplete > ul > li[aria-selected]": {
    backgroundColor: "#1d3040",
    color: "#edf4fa",
  },
  ".cm-diagnostic-error": { borderLeftColor: "#df6d75" },
  ".cm-diagnostic-warning": { borderLeftColor: "#d9a449" },
}, { dark: true });

async function loadLanguageExtension(path: string, language: string): Promise<Extension> {
  const normalized = language.toLowerCase();
  const lowerPath = path.toLowerCase();
  if (normalized === "javascript" || lowerPath.endsWith(".js") || lowerPath.endsWith(".mjs") || lowerPath.endsWith(".cjs")) {
    const { javascript } = await import("@codemirror/lang-javascript");
    return javascript();
  }
  if (normalized === "typescript" || lowerPath.endsWith(".ts") || lowerPath.endsWith(".mts") || lowerPath.endsWith(".cts")) {
    const { javascript } = await import("@codemirror/lang-javascript");
    return javascript({ typescript: true });
  }
  if (normalized === "jsx" || lowerPath.endsWith(".jsx")) {
    const { javascript } = await import("@codemirror/lang-javascript");
    return javascript({ jsx: true });
  }
  if (normalized === "tsx" || lowerPath.endsWith(".tsx")) {
    const { javascript } = await import("@codemirror/lang-javascript");
    return javascript({ typescript: true, jsx: true });
  }
  if (normalized === "python" || lowerPath.endsWith(".py")) {
    const { python } = await import("@codemirror/lang-python");
    return python();
  }
  if (normalized === "rust" || lowerPath.endsWith(".rs")) {
    const { rust } = await import("@codemirror/lang-rust");
    return rust();
  }
  if (normalized === "html" || lowerPath.endsWith(".html") || lowerPath.endsWith(".htm")) {
    const { html } = await import("@codemirror/lang-html");
    return html();
  }
  if (normalized === "css" || lowerPath.endsWith(".css")) {
    const { css } = await import("@codemirror/lang-css");
    return css();
  }
  if (normalized === "json" || lowerPath.endsWith(".json")) {
    const { json } = await import("@codemirror/lang-json");
    return json();
  }
  if (normalized === "markdown" || lowerPath.endsWith(".md") || lowerPath.endsWith(".markdown")) {
    const { markdown } = await import("@codemirror/lang-markdown");
    return markdown();
  }
  if (normalized === "yaml" || lowerPath.endsWith(".yaml") || lowerPath.endsWith(".yml")) {
    const { yaml } = await import("@codemirror/lang-yaml");
    return yaml();
  }
  if (normalized === "sql" || lowerPath.endsWith(".sql")) {
    const { sql } = await import("@codemirror/lang-sql");
    return sql();
  }
  if (normalized === "shell" || lowerPath.endsWith(".sh") || lowerPath.endsWith(".bash") || lowerPath.endsWith(".zsh")) {
    const { shell } = await import("@codemirror/legacy-modes/mode/shell");
    return StreamLanguage.define(shell);
  }
  if (normalized === "toml" || lowerPath.endsWith(".toml")) {
    const { toml } = await import("@codemirror/legacy-modes/mode/toml");
    return StreamLanguage.define(toml);
  }
  return [];
}

function diagnosticFor(state: EditorState, problem: EditorProblem): Diagnostic {
  const lineNumber = Math.max(1, Math.min(problem.line || 1, state.doc.lines));
  const line = state.doc.line(lineNumber);
  const columnOffset = Math.max(0, Math.min((problem.column || 1) - 1, line.length));
  const from = line.from + columnOffset;
  const to = Math.min(line.to, Math.max(from + 1, from));
  return {
    from,
    to,
    severity: problem.severity,
    message: problem.source ? `${problem.message} (${problem.source})` : problem.message,
  };
}

export default function CodeMirrorSurface({
  documentKey,
  path,
  language,
  content,
  readonly,
  modifiedAt,
  problems,
  revealLocation,
  goToLineToken,
  onChange,
  onSave,
  onCursorChange,
  onSelectionChange,
}: CodeMirrorSurfaceProps) {
  const hostRef = useRef<HTMLDivElement>(null);
  const viewRef = useRef<EditorView | null>(null);
  const languageCompartmentRef = useRef(new Compartment());
  const readOnlyCompartmentRef = useRef(new Compartment());
  const callbacksRef = useRef({ onChange, onSave, onCursorChange, onSelectionChange });
  const lastLocalContentRef = useRef(content);
  const statusFrameRef = useRef<number | null>(null);
  const languageLoadRef = useRef(0);

  callbacksRef.current = { onChange, onSave, onCursorChange, onSelectionChange };

  useEffect(() => {
    const host = hostRef.current;
    if (!host) return;

    const unit = language === "python" ? "    " : "  ";
    const readOnlyCompartment = readOnlyCompartmentRef.current;
    const languageCompartment = languageCompartmentRef.current;

    const notifyCursorAndSelection = (view: EditorView) => {
      if (statusFrameRef.current !== null) cancelAnimationFrame(statusFrameRef.current);
      statusFrameRef.current = requestAnimationFrame(() => {
        statusFrameRef.current = null;
        const current = viewRef.current;
        if (!current) return;
        const selection = current.state.selection.main;
        const line = current.state.doc.lineAt(selection.head);
        callbacksRef.current.onCursorChange(line.number, selection.head - line.from + 1);
        callbacksRef.current.onSelectionChange?.(selection.empty
          ? null
          : {
              content: current.state.sliceDoc(selection.from, selection.to),
              start: selection.from,
              end: selection.to,
            });
      });
    };

    const saveLatest = (view: EditorView) => {
      const latest = view.state.doc.toString();
      if (latest !== lastLocalContentRef.current) {
        lastLocalContentRef.current = latest;
        callbacksRef.current.onChange(latest);
      }
      void callbacksRef.current.onSave();
      return true;
    };

    const view = new EditorView({
      parent: host,
      doc: content,
      extensions: [
        basicSetup,
        repoTunnelTheme,
        syntaxHighlighting(repoTunnelHighlight),
        EditorState.tabSize.of(unit.length),
        indentUnit.of(unit),
        EditorView.contentAttributes.of({
          "aria-label": `Edit ${path}`,
          spellcheck: "false",
          autocapitalize: "off",
          autocomplete: "off",
          autocorrect: "off",
        }),
        languageCompartment.of([]),
        readOnlyCompartment.of([
          EditorState.readOnly.of(readonly),
          EditorView.editable.of(!readonly),
        ]),
        Prec.highest(keymap.of([
          { key: "Mod-s", run: saveLatest, preventDefault: true },
          { key: "Mod-z", run: undo, preventDefault: true },
          { key: "Mod-Shift-z", run: redo, preventDefault: true },
          { key: "Mod-y", run: redo, preventDefault: true },
          { key: "Mod-g", run: gotoLine, preventDefault: true },
          { key: "Mod-/", run: toggleComment, preventDefault: true },
          { key: "Mod-l", run: selectLine, preventDefault: true },
          { key: "Mod-Shift-k", run: deleteLine, preventDefault: true },
          { key: "Mod-[", run: indentLess, preventDefault: true },
          { key: "Mod-]", run: indentMore, preventDefault: true },
          { key: "Alt-ArrowUp", run: moveLineUp, preventDefault: true },
          { key: "Alt-ArrowDown", run: moveLineDown, preventDefault: true },
          { key: "Shift-Alt-ArrowUp", run: copyLineUp, preventDefault: true },
          { key: "Shift-Alt-ArrowDown", run: copyLineDown, preventDefault: true },
          indentWithTab,
        ])),
        EditorView.updateListener.of((update) => {
          if (update.docChanged) {
            const external = update.transactions.some((transaction) => transaction.annotation(externalSync));
            if (!external) {
              const latest = update.state.doc.toString();
              lastLocalContentRef.current = latest;
              callbacksRef.current.onChange(latest);
            }
          }
          if (update.docChanged || update.selectionSet) notifyCursorAndSelection(update.view);
        }),
      ],
    });

    viewRef.current = view;
    lastLocalContentRef.current = content;
    notifyCursorAndSelection(view);

    return () => {
      if (statusFrameRef.current !== null) {
        cancelAnimationFrame(statusFrameRef.current);
        statusFrameRef.current = null;
      }
      languageLoadRef.current += 1;
      viewRef.current = null;
      view.destroy();
    };
  }, [documentKey, path, language]);

  useEffect(() => {
    const view = viewRef.current;
    if (!view) return;
    view.dispatch({
      effects: readOnlyCompartmentRef.current.reconfigure([
        EditorState.readOnly.of(readonly),
        EditorView.editable.of(!readonly),
      ]),
    });
  }, [readonly]);

  useEffect(() => {
    const view = viewRef.current;
    if (!view) return;
    const current = view.state.doc.toString();
    if (content === current || content === lastLocalContentRef.current) return;
    lastLocalContentRef.current = content;
    view.dispatch({
      changes: { from: 0, to: view.state.doc.length, insert: content },
      annotations: [externalSync.of(true), Transaction.addToHistory.of(false)],
    });
  }, [content, modifiedAt]);

  useEffect(() => {
    const view = viewRef.current;
    if (!view) return;
    const request = ++languageLoadRef.current;
    void loadLanguageExtension(path, language).then((support) => {
      if (request !== languageLoadRef.current || viewRef.current !== view) return;
      view.dispatch({ effects: languageCompartmentRef.current.reconfigure(support) });
    }).catch(() => {
      if (request !== languageLoadRef.current || viewRef.current !== view) return;
      view.dispatch({ effects: languageCompartmentRef.current.reconfigure([]) });
    });
  }, [path, language]);

  useEffect(() => {
    const view = viewRef.current;
    if (!view) return;
    const diagnostics = problems.map((problem) => diagnosticFor(view.state, problem));
    view.dispatch(setDiagnostics(view.state, diagnostics));
  }, [problems]);

  useEffect(() => {
    if (!revealLocation || revealLocation.key !== documentKey) return;
    const view = viewRef.current;
    if (!view) return;
    const lineNumber = Math.max(1, Math.min(revealLocation.line, view.state.doc.lines));
    const line = view.state.doc.line(lineNumber);
    const position = Math.min(line.to, line.from + Math.max(0, revealLocation.column - 1));
    view.dispatch({
      selection: { anchor: position },
      effects: EditorView.scrollIntoView(position, { y: "center" }),
    });
    view.focus();
  }, [revealLocation?.token, revealLocation?.key, revealLocation?.line, revealLocation?.column, documentKey]);

  useEffect(() => {
    if (goToLineToken <= 0) return;
    const view = viewRef.current;
    if (!view) return;
    gotoLine(view);
  }, [goToLineToken]);

  return <div ref={hostRef} className="code-mirror-surface" data-editor-engine="codemirror-6" />;
}
