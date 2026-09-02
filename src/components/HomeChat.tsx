import { useEffect, useMemo, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import {
  beginHomeChat,
  cancelHomeChat,
  createHomeConversation,
  deleteHomeConversation,
  getHomeConversation,
  listHomeConversations,
  setSelectedLocalModel,
} from "../lib/backend";
import type {
  HomeChatStreamEvent,
  HomeContextErrorInput,
  HomeContextSource,
  HomeContextTextInput,
  HomeConversation,
  HomeConversationSummary,
  HomeProjectContextRequest,
  ModelHubSnapshot,
  Workspace,
} from "../types";
import { NavIcon } from "./AppSidebar";
import MarkdownMessage from "./MarkdownMessage";

type HomeChatStatus = "ready" | "thinking" | "streaming" | "complete" | "cancelled" | "failed";

type HomeChatProps = {
  workspace: Workspace | null;
  modelHub: ModelHubSnapshot | null;
  currentFile: HomeContextTextInput | null;
  selection: HomeContextTextInput | null;
  errors: HomeContextErrorInput[];
  onModelHubChange: (snapshot: ModelHubSnapshot) => void;
};

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function ContextUsed({ sources }: { sources: HomeContextSource[] }) {
  if (sources.length === 0) return null;
  return (
    <details className="home-context-used">
      <summary>Context used · {sources.length}</summary>
      <div>
        {sources.map((source, index) => (
          <span key={`${source.kind}:${source.path ?? source.label}:${index}`}>
            {source.label}
            {source.lineStart ? ` · L${source.lineStart}${source.lineEnd && source.lineEnd !== source.lineStart ? `–${source.lineEnd}` : ""}` : ""}
          </span>
        ))}
      </div>
    </details>
  );
}

function ContextChip({ label, kind, onRemove }: { label: string; kind: string; onRemove: () => void }) {
  return (
    <span className="home-context-chip">
      <small>{kind}</small>
      <span>{label}</span>
      <button type="button" onClick={onRemove} aria-label={`Remove ${label} context`}>×</button>
    </span>
  );
}

function statusText(status: HomeChatStatus): string {
  if (status === "thinking") return "Thinking";
  if (status === "streaming") return "Streaming response";
  if (status === "complete") return "Complete";
  if (status === "cancelled") return "Cancelled";
  if (status === "failed") return "Failed";
  return "Ready";
}

function HomeChat({ workspace, modelHub, currentFile, selection, errors, onModelHubChange }: HomeChatProps) {
  const [prompt, setPrompt] = useState("");
  const [conversation, setConversation] = useState<HomeConversation | null>(null);
  const [recent, setRecent] = useState<HomeConversationSummary[]>([]);
  const [showRecent, setShowRecent] = useState(false);
  const [status, setStatus] = useState<HomeChatStatus>("ready");
  const [generationId, setGenerationId] = useState<string | null>(null);
  const [streamingContent, setStreamingContent] = useState("");
  const [streamingSources, setStreamingSources] = useState<HomeContextSource[]>([]);
  const [failure, setFailure] = useState<string | null>(null);
  const [contextNotice, setContextNotice] = useState<string | null>(null);
  const [includeSelection, setIncludeSelection] = useState(true);
  const [allowChanges, setAllowChanges] = useState(false);
  const [attachedError, setAttachedError] = useState<HomeContextErrorInput | null>(null);
  const [modelSelectionBusy, setModelSelectionBusy] = useState(false);
  const [modelMenuOpen, setModelMenuOpen] = useState(false);
  const [deletingConversationId, setDeletingConversationId] = useState<string | null>(null);
  const activeConversationRef = useRef<string | null>(null);
  const generationRef = useRef<string | null>(null);
  const modelPickerRef = useRef<HTMLDivElement | null>(null);
  const endedGenerationsRef = useRef(new Set<string>());

  const selectedModel = modelHub?.selectedModel ?? null;
  const selectedRuntime = selectedModel
    ? modelHub?.runtimes.find((runtime) => runtime.provider === selectedModel.provider) ?? null
    : null;
  const selectedModelInfo = selectedModel
    ? selectedRuntime?.models.find((model) => model.id === selectedModel.modelId) ?? null
    : null;
  const modelLabel = selectedModel ? `${selectedModel.modelId} · ${selectedRuntime?.label ?? "Local"}` : "No model selected";
  const availableModels = useMemo(() => (modelHub?.runtimes ?? []).flatMap((runtime) =>
    runtime.reachable ? runtime.models.map((model) => ({ runtime, model })) : [],
  ), [modelHub]);
  const selectedModelKey = selectedModel ? `${selectedModel.provider}::${selectedModel.modelId}` : "";
  const contextWindow = selectedModelInfo?.capabilities.contextWindow.value ?? null;
  const generating = status === "thinking" || status === "streaming";
  const canSend = Boolean(selectedModel && prompt.trim()) && !generating;

  useEffect(() => { activeConversationRef.current = conversation?.id ?? null; }, [conversation?.id]);
  useEffect(() => { generationRef.current = generationId; }, [generationId]);

  useEffect(() => {
    if (!modelMenuOpen) return;
    const closeOnPointerDown = (event: PointerEvent) => {
      if (!modelPickerRef.current?.contains(event.target as Node)) setModelMenuOpen(false);
    };
    const closeOnEscape = (event: KeyboardEvent) => {
      if (event.key === "Escape") setModelMenuOpen(false);
    };
    document.addEventListener("pointerdown", closeOnPointerDown);
    window.addEventListener("keydown", closeOnEscape);
    return () => {
      document.removeEventListener("pointerdown", closeOnPointerDown);
      window.removeEventListener("keydown", closeOnEscape);
    };
  }, [modelMenuOpen]);

  useEffect(() => {
    setAttachedError(null);
    setAllowChanges(false);
    setConversation(null);
    setRecent([]);
    setStreamingContent("");
    setStreamingSources([]);
    setGenerationId(null);
    setStatus("ready");
    setFailure(null);
    setContextNotice(null);
    endedGenerationsRef.current.clear();
    let cancelled = false;
    listHomeConversations(workspace?.id ?? null)
      .then(async (summaries) => {
        if (cancelled) return;
        setRecent(summaries);
        const first = summaries[0];
        if (!first) return;
        const loaded = await getHomeConversation(first.id);
        if (!cancelled) setConversation(loaded);
      })
      .catch((error) => {
        if (!cancelled) setFailure(`Could not load local conversations: ${errorMessage(error)}`);
      });
    return () => { cancelled = true; };
  }, [workspace?.id]);

  useEffect(() => { setIncludeSelection(false); }, [selection?.path, selection?.content]);

  useEffect(() => {
    let disposed = false;
    let unlisten: (() => void) | null = null;
    listen<HomeChatStreamEvent>("repotunnel://home-chat-stream", (event) => {
      if (disposed) return;
      const payload = event.payload;
      if (activeConversationRef.current && payload.conversationId !== activeConversationRef.current) return;
      if (generationRef.current && payload.generationId !== generationRef.current) return;
      if (!generationRef.current) {
        generationRef.current = payload.generationId;
        setGenerationId(payload.generationId);
      }
      if (payload.kind === "chunk") {
        setStatus("streaming");
        if (payload.delta) setStreamingContent((current) => current + payload.delta);
        return;
      }
      const finalStatus: HomeChatStatus = payload.kind === "complete"
        ? "complete"
        : payload.kind === "cancelled"
          ? "cancelled"
          : "failed";
      setStatus(finalStatus);
      setFailure(payload.kind === "failed" ? (payload.message ?? "Local model generation failed.") : null);
      if (payload.kind === "cancelled") setContextNotice("Generation stopped.");
      setStreamingSources(payload.contextSources);
      endedGenerationsRef.current.add(payload.generationId);
      setGenerationId(null);
      generationRef.current = null;
      const conversationId = payload.conversationId;
      void Promise.all([getHomeConversation(conversationId), listHomeConversations(workspace?.id ?? null)])
        .then(([loaded, summaries]) => {
          if (activeConversationRef.current === conversationId) {
            setConversation(loaded);
            setStreamingContent("");
          }
          setRecent(summaries);
        })
        .catch(() => undefined);
    })
      .then((stop) => {
        if (disposed) stop();
        else unlisten = stop;
      })
      .catch(() => undefined);
    return () => {
      disposed = true;
      unlisten?.();
    };
  }, [workspace?.id]);

  async function refreshRecent() {
    try {
      setRecent(await listHomeConversations(workspace?.id ?? null));
    } catch {
      // Recent conversations are convenience-only; the active conversation remains usable.
    }
  }

  async function newConversation() {
    if (generating) return;
    setFailure(null);
    setContextNotice(null);
    try {
      const created = await createHomeConversation(workspace?.id ?? null);
      setConversation(created);
      activeConversationRef.current = created.id;
      setStreamingContent("");
      setStreamingSources([]);
      setStatus("ready");
      setPrompt("");
      setShowRecent(false);
      await refreshRecent();
    } catch (error) {
      setFailure(`Could not start a new conversation: ${errorMessage(error)}`);
    }
  }

  async function switchConversation(id: string) {
    if (generating || id === conversation?.id) return;
    setFailure(null);
    setContextNotice(null);
    try {
      const loaded = await getHomeConversation(id);
      setConversation(loaded);
      activeConversationRef.current = loaded.id;
      setStreamingContent("");
      setStreamingSources([]);
      setStatus("ready");
      setShowRecent(false);
    } catch (error) {
      setFailure(`Could not open that conversation: ${errorMessage(error)}`);
    }
  }

  async function deleteConversation(id: string, title: string) {
    if (generating || deletingConversationId) return;
    if (!window.confirm(`Delete “${title}”? This removes the chat from this device.`)) return;
    setDeletingConversationId(id);
    setFailure(null);
    try {
      await deleteHomeConversation(id);
      const summaries = await listHomeConversations(workspace?.id ?? null);
      setRecent(summaries);
      if (conversation?.id === id) {
        const next = summaries[0];
        if (next) {
          const loaded = await getHomeConversation(next.id);
          setConversation(loaded);
          activeConversationRef.current = loaded.id;
        } else {
          setConversation(null);
          activeConversationRef.current = null;
          setShowRecent(false);
        }
        setStreamingContent("");
        setStreamingSources([]);
        setStatus("ready");
      }
    } catch (error) {
      setFailure(`Could not delete that conversation: ${errorMessage(error)}`);
    } finally {
      setDeletingConversationId(null);
    }
  }

  async function chooseInlineModel(value: string) {
    if (!modelHub || !value || modelSelectionBusy) return;
    const candidate = availableModels.find(({ runtime, model }) => `${runtime.provider}::${model.id}` === value);
    if (!candidate) return;
    setModelMenuOpen(false);
    setModelSelectionBusy(true);
    setFailure(null);
    try {
      const nextSelection = await setSelectedLocalModel({
        provider: candidate.runtime.provider,
        modelId: candidate.model.id,
        endpoint: candidate.runtime.endpoint,
      });
      onModelHubChange({ ...modelHub, selectedModel: nextSelection });
    } catch (error) {
      setFailure(`Could not select local model: ${errorMessage(error)}`);
    } finally {
      setModelSelectionBusy(false);
    }
  }

  async function send() {
    if (!selectedModel || !prompt.trim() || generating) return;
    setFailure(null);
    setContextNotice(null);
    setStreamingContent("");
    setStreamingSources([]);
    setStatus("thinking");
    const question = prompt.trim();
    let active = conversation;
    try {
      if (!active) {
        active = await createHomeConversation(workspace?.id ?? null);
        setConversation(active);
      }
      activeConversationRef.current = active.id;
      const context: HomeProjectContextRequest = {
        includeProject: Boolean(workspace),
        attachments: [],
        currentFile: workspace ? currentFile : null,
        selection: workspace && includeSelection ? selection : null,
        error: workspace ? attachedError : null,
        history: [],
        contextWindow,
      };
      setPrompt("");
      const editRequest = allowChanges && Boolean(workspace);
      setAllowChanges(false);
      const started = await beginHomeChat(
        workspace?.id ?? null,
        active.id,
        question,
        context,
        editRequest,
      );
      if (!endedGenerationsRef.current.has(started.generationId)) {
        generationRef.current = started.generationId;
        setGenerationId(started.generationId);
      }
      setConversation((current) => endedGenerationsRef.current.has(started.generationId) ? current : started.conversation);
      setStreamingSources(started.contextSources);
      if (started.contextReduced) {
        setContextNotice("RepoTunnel reduced project context to fit this model safely.");
      } else if (started.contextWarnings.length > 0) {
        setContextNotice(started.contextWarnings[0]);
      }
      await refreshRecent();
    } catch (error) {
      setStatus("failed");
      setGenerationId(null);
      generationRef.current = null;
      setFailure(errorMessage(error));
      setPrompt(question);
    }
  }

  async function stop() {
    const id = generationRef.current;
    if (!id) return;
    try {
      await cancelHomeChat(id);
      setContextNotice("Stopping local generation…");
    } catch (error) {
      setFailure(`Could not stop generation: ${errorMessage(error)}`);
    }
  }

  const hasConversation = Boolean(conversation?.messages.length || streamingContent);

  return (
    <div className={`home-chat ${hasConversation ? "active" : "empty"}`}>
      {hasConversation ? (
        <div className="home-chat-topbar">
          <div className="home-chat-titlebar">
            <strong>{conversation?.title ?? "Local AI conversation"}</strong>
            <span className={`home-chat-status ${status}`}>{statusText(status)}</span>
          </div>
          <div className="home-chat-top-actions">
            <button type="button" onClick={() => setShowRecent((current) => !current)} disabled={generating}>Conversations</button>
            <button type="button" onClick={() => void newConversation()} disabled={generating}>+ New</button>
          </div>
          {showRecent ? (
            <div className="home-recent-conversations">
              <div className="home-recent-heading"><strong>Recent conversations</strong><span>Stored only on this device</span></div>
              {recent.length === 0 ? <p>No saved conversations yet.</p> : recent.slice(0, 8).map((item) => (
                <div key={item.id} className={`home-recent-conversation-row ${item.id === conversation?.id ? "active" : ""}`}>
                  <button type="button" className="home-recent-conversation-open" onClick={() => void switchConversation(item.id)} disabled={generating || deletingConversationId === item.id}>
                    <span>{item.title}</span><small>{item.messageCount} messages</small>
                  </button>
                  <button
                    type="button"
                    className="home-recent-conversation-delete"
                    onClick={() => void deleteConversation(item.id, item.title)}
                    disabled={generating || deletingConversationId !== null}
                    aria-label={`Delete ${item.title}`}
                    title="Delete chat"
                  >
                    {deletingConversationId === item.id ? "…" : "Delete"}
                  </button>
                </div>
              ))}
            </div>
          ) : null}
        </div>
      ) : null}

      {!hasConversation ? (
        <div className="home-chat-empty-brand" aria-hidden="true">
          <span>RepoTunnel</span>
        </div>
      ) : null}

      {hasConversation ? (
        <div className="home-chat-messages" aria-live="polite">
          {conversation?.messages.map((message) => (
            <article className={`home-chat-message ${message.role}`} key={message.id}>
              {message.role === "assistant" ? <MarkdownMessage content={message.content} /> : <p>{message.content}</p>}
              {message.state !== "complete" ? <span className="home-chat-message-state">{message.state}</span> : null}
              {message.role === "assistant" ? <ContextUsed sources={message.contextSources} /> : null}
            </article>
          ))}
          {generationId || streamingContent ? (
            <article className="home-chat-message assistant streaming">
              {streamingContent ? <MarkdownMessage content={streamingContent} /> : <div className="home-thinking"><i /><i /><i /><span>Thinking…</span></div>}
              <ContextUsed sources={streamingSources} />
            </article>
          ) : null}
        </div>
      ) : null}

      <div className="home-chat-composer-shell">
        <div className="home-context-chips" aria-label="Context included with next question">
          {selection && includeSelection ? <ContextChip kind="Selection" label="Selected code" onRemove={() => setIncludeSelection(false)} /> : null}
          {attachedError ? <ContextChip kind="Error" label={attachedError.message.slice(0, 48)} onRemove={() => setAttachedError(null)} /> : null}
        </div>

        <label className="home-chat-prompt">
          <textarea
            value={prompt}
            onChange={(event) => setPrompt(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === "Enter" && !event.shiftKey && !event.nativeEvent.isComposing) {
                event.preventDefault();
                if (canSend) void send();
              }
            }}
            placeholder={selectedModel ? (workspace ? `Ask anything about ${workspace.name}…` : "Ask anything…") : "Waiting for a local model…"}
            rows={hasConversation ? 3 : 4}
            disabled={generating}
          />
        </label>

        {hasConversation && ((!includeSelection && selection) || (errors.length > 0 && !attachedError)) ? (
          <div className="home-chat-context-add">
            {!includeSelection && selection ? <button type="button" onClick={() => setIncludeSelection(true)}>+ Selection</button> : null}
            {errors.length > 0 && !attachedError ? (
              <select value="" onChange={(event) => {
                const index = Number(event.target.value);
                if (Number.isInteger(index) && errors[index]) setAttachedError(errors[index]);
              }} aria-label="Attach an existing diagnostic">
                <option value="">+ Error</option>
                {errors.slice(0, 12).map((error, index) => <option value={index} key={`${error.source}:${error.path}:${index}`}>{error.message.slice(0, 70)}</option>)}
              </select>
            ) : null}
          </div>
        ) : null}

        <div className="home-chat-empty-actions home-chat-compact-actions">
          <div className="home-chat-empty-tools">
            {workspace ? (
              <button
                type="button"
                className={`home-chat-edit-toggle ${allowChanges ? "active" : ""}`}
                onClick={() => setAllowChanges((current) => !current)}
                disabled={generating}
                aria-pressed={allowChanges}
                title="Allow this message to propose safe project edits through RepoTunnel History"
              >
                <span aria-hidden="true">✎</span>
                <span>{allowChanges ? "Edits enabled" : "Allow edits"}</span>
              </button>
            ) : null}
            {availableModels.length === 0 ? (
              <span className="home-chat-model-empty" title="Start a local runtime to make models available">
                <i aria-hidden="true" />
                <span>No local model</span>
              </span>
            ) : (
              <div className={`home-chat-model-picker ${modelMenuOpen ? "open" : ""}`} ref={modelPickerRef}>
                <button
                  type="button"
                  className="home-chat-model-trigger"
                  onClick={() => setModelMenuOpen((open) => !open)}
                  disabled={modelSelectionBusy}
                  aria-label="Choose local model"
                  aria-haspopup="listbox"
                  aria-expanded={modelMenuOpen}
                  title="Choose a local model"
                >
                  <span className="home-chat-model-trigger-label">{modelSelectionBusy ? "Switching model…" : modelLabel}</span>
                  <span className="home-chat-model-caret" aria-hidden="true">⌄</span>
                </button>
                {modelMenuOpen ? (
                  <div className="home-chat-model-menu" role="listbox" aria-label="Available local models">
                    {availableModels.map(({ runtime, model }) => {
                      const value = `${runtime.provider}::${model.id}`;
                      const selected = value === selectedModelKey;
                      return (
                        <button
                          type="button"
                          role="option"
                          aria-selected={selected}
                          className={`home-chat-model-option ${selected ? "selected" : ""}`}
                          key={`${runtime.provider}:${model.id}`}
                          onClick={() => void chooseInlineModel(value)}
                        >
                          <span className="home-chat-model-option-dot" aria-hidden="true" />
                          <span className="home-chat-model-option-copy">
                            <strong>{model.name}</strong>
                            <small>{runtime.label}</small>
                          </span>
                          <span className="home-chat-model-option-check" aria-hidden="true">{selected ? "✓" : ""}</span>
                        </button>
                      );
                    })}
                  </div>
                ) : null}
              </div>
            )}
          </div>
          {generating ? (
            <button type="button" className="home-chat-empty-send stop" onClick={() => void stop()} disabled={!generationId} aria-label="Stop generation"><NavIcon name="stop" size={14} /></button>
          ) : (
            <button type="button" className="home-chat-empty-send" disabled={!canSend} onClick={() => void send()} aria-label="Send question to selected local model">↑</button>
          )}
        </div>

        {contextNotice ? <div className="home-chat-notice" role="status">{contextNotice}</div> : null}
        {failure ? <div className="home-chat-error" role="alert">{failure}</div> : null}
      </div>
    </div>
  );
}

export default HomeChat;
