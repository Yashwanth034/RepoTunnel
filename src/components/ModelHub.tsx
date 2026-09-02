import { useEffect, useMemo, useState } from "react";
import type {
  BooleanCapability,
  LocalModelInfo,
  ModelHubSnapshot,
  ModelProviderId,
  ModelSelection,
  ModelTestResult,
  NumberCapability,
  RuntimeStatus,
} from "../types";
import {
  getModelHub,
  refreshModelRuntime,
  setSelectedLocalModel,
  testLocalModel,
  updateModelRuntimeEndpoint,
} from "../lib/backend";
import { NavIcon } from "./AppSidebar";
import ModelTrial from "./ModelTrial";

type ModelHubProps = {
  snapshot: ModelHubSnapshot | null;
  loading: boolean;
  onSnapshotChange: (snapshot: ModelHubSnapshot) => void;
  onNotice: (message: string) => void;
};

const providerOrder: ModelProviderId[] = ["ollama", "lmStudio", "llamaCpp"];

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function modelSelection(model: LocalModelInfo, runtime: RuntimeStatus): ModelSelection {
  return { provider: runtime.provider, modelId: model.id, endpoint: runtime.endpoint };
}

function formatBytes(bytes: number | null): string | null {
  if (bytes === null) return null;
  if (bytes < 1024 ** 2) return `${Math.max(1, Math.round(bytes / 1024))} KB`;
  if (bytes < 1024 ** 3) return `${(bytes / 1024 ** 2).toFixed(1)} MB`;
  return `${(bytes / 1024 ** 3).toFixed(1)} GB`;
}

function formatContext(capability: NumberCapability): string {
  if (capability.value === null) return "Unknown";
  if (capability.value >= 1024) return `${Math.round(capability.value / 1024)}K`;
  return capability.value.toLocaleString();
}

function formatCapability(capability: BooleanCapability): string {
  if (capability.value === null) return "Unknown";
  return capability.value ? "Yes" : "No";
}

function capabilitySource(source: BooleanCapability["source"] | NumberCapability["source"]): string {
  if (source === "reported") return "reported";
  if (source === "detected") return "detected";
  return "unknown";
}

function setupGuidance(provider: ModelProviderId): string {
  if (provider === "ollama") return "Start Ollama locally, then refresh. RepoTunnel will inspect /api/tags only after the server is running.";
  if (provider === "lmStudio") return "Start LM Studio's local server with an OpenAI-compatible API, then refresh this runtime.";
  return "Start llama.cpp in local server mode, confirm its loopback port, then update the endpoint if needed.";
}

function RuntimeSummary({ runtime, active, onSelect }: { runtime: RuntimeStatus; active: boolean; onSelect: () => void }) {
  return (
    <button type="button" className={`model-runtime-summary ${active ? "active" : ""}`} onClick={onSelect}>
      <span className={`model-runtime-dot ${runtime.reachable ? "connected" : ""}`} />
      <span className="model-runtime-copy">
        <strong>{runtime.label}</strong>
        <small>{runtime.reachable ? (runtime.models.length === 0 ? "Connected · no models" : `Connected · ${runtime.models.length} model${runtime.models.length === 1 ? "" : "s"}`) : "Unavailable"}</small>
      </span>
      <span aria-hidden="true">›</span>
    </button>
  );
}

function Capability({ label, value, source }: { label: string; value: string; source: string }) {
  return (
    <span className="model-capability" title={`Capability source: ${source}`}>
      <small>{label}</small>
      <strong>{value}</strong>
      {source !== "unknown" ? <em>{source}</em> : null}
    </span>
  );
}

function ModelHub({ snapshot, loading, onSnapshotChange, onNotice }: ModelHubProps) {
  const [activeProvider, setActiveProvider] = useState<ModelProviderId>("ollama");
  const [endpointDrafts, setEndpointDrafts] = useState<Partial<Record<ModelProviderId, string>>>({});
  const [busyProvider, setBusyProvider] = useState<ModelProviderId | null>(null);
  const [refreshingAll, setRefreshingAll] = useState(false);
  const [selectionBusy, setSelectionBusy] = useState<string | null>(null);
  const [testBusy, setTestBusy] = useState<string | null>(null);
  const [testResult, setTestResult] = useState<ModelTestResult | null>(null);

  useEffect(() => {
    if (!snapshot) return;
    setEndpointDrafts(Object.fromEntries(snapshot.runtimes.map((runtime) => [runtime.provider, runtime.endpoint])));
  }, [snapshot]);

  const runtime = useMemo(
    () => snapshot?.runtimes.find((item) => item.provider === activeProvider) ?? snapshot?.runtimes[0] ?? null,
    [activeProvider, snapshot],
  );

  useEffect(() => {
    if (snapshot && !snapshot.runtimes.some((item) => item.provider === activeProvider) && snapshot.runtimes[0]) {
      setActiveProvider(snapshot.runtimes[0].provider);
    }
  }, [activeProvider, snapshot]);

  async function refreshAll() {
    setRefreshingAll(true);
    setTestResult(null);
    try {
      const next = await getModelHub();
      onSnapshotChange(next);
    } catch (error) {
      onNotice(`Model Hub: ${errorMessage(error)}`);
    } finally {
      setRefreshingAll(false);
    }
  }

  async function refreshOne(provider: ModelProviderId, announce = false) {
    if (!snapshot) return;
    setBusyProvider(provider);
    try {
      const updated = await refreshModelRuntime(provider);
      const runtimes = snapshot.runtimes.map((item) => item.provider === provider ? updated : item);
      const next: ModelHubSnapshot = {
        ...snapshot,
        runtimes,
        availableModelCount: runtimes.reduce((sum, item) => sum + item.models.length, 0),
        connectedRuntimeCount: runtimes.filter((item) => item.reachable).length,
        refreshedAt: Date.now(),
      };
      onSnapshotChange(next);
      if (announce) onNotice(updated.message);
    } catch (error) {
      onNotice(`Model Hub: ${errorMessage(error)}`);
    } finally {
      setBusyProvider(null);
    }
  }

  async function saveEndpoint(provider: ModelProviderId) {
    const endpoint = endpointDrafts[provider]?.trim() ?? "";
    setBusyProvider(provider);
    setTestResult(null);
    try {
      await updateModelRuntimeEndpoint(provider, endpoint);
      const next = await getModelHub();
      onSnapshotChange(next);
      onNotice(`${provider === "ollama" ? "Ollama" : provider === "lmStudio" ? "LM Studio" : "llama.cpp"} endpoint saved. Loopback-only policy remains active.`);
    } catch (error) {
      onNotice(`Model Hub: ${errorMessage(error)}`);
    } finally {
      setBusyProvider(null);
    }
  }

  async function chooseModel(model: LocalModelInfo, selectedRuntime: RuntimeStatus) {
    const key = `${selectedRuntime.provider}:${model.id}`;
    setSelectionBusy(key);
    try {
      const selectedModel = await setSelectedLocalModel(modelSelection(model, selectedRuntime));
      if (snapshot) onSnapshotChange({ ...snapshot, selectedModel });
      onNotice(`${model.name} is now the default local model.`);
    } catch (error) {
      onNotice(`Model Hub: ${errorMessage(error)}`);
    } finally {
      setSelectionBusy(null);
    }
  }

  async function clearSelection() {
    setSelectionBusy("clear");
    try {
      const selectedModel = await setSelectedLocalModel(null);
      if (snapshot) onSnapshotChange({ ...snapshot, selectedModel });
      onNotice("Default local model cleared.");
    } catch (error) {
      onNotice(`Model Hub: ${errorMessage(error)}`);
    } finally {
      setSelectionBusy(null);
    }
  }

  async function runModelTest(model: LocalModelInfo, selectedRuntime: RuntimeStatus) {
    const key = `${selectedRuntime.provider}:${model.id}`;
    setTestBusy(key);
    setTestResult(null);
    try {
      setTestResult(await testLocalModel(modelSelection(model, selectedRuntime)));
    } catch (error) {
      onNotice(`Model test: ${errorMessage(error)}`);
    } finally {
      setTestBusy(null);
    }
  }

  if (loading && !snapshot) {
    return <section className="model-hub-loading"><div className="loader" /><span>Discovering local runtimes…</span></section>;
  }

  if (!snapshot || !runtime) {
    return (
      <section className="model-hub-empty">
        <NavIcon name="model" size={24} />
        <h2>Model Hub is unavailable</h2>
        <p>RepoTunnel could not read the local runtime state.</p>
        <button type="button" className="primary-button" onClick={() => void refreshAll()}>Try again</button>
      </section>
    );
  }

  const selected = snapshot.selectedModel;
  const endpointDraft = endpointDrafts[runtime.provider] ?? runtime.endpoint;
  const endpointChanged = endpointDraft.trim().replace(/\/$/, "") !== runtime.endpoint.replace(/\/$/, "");

  return (
    <section className="model-hub" aria-labelledby="model-hub-title">
      <header className="model-hub-heading">
        <div>
          <span className="section-kicker"><NavIcon name="model" size={15} /> Local AI runtime layer</span>
          <h2 id="model-hub-title">Model Hub</h2>
          <p>Discover and select models already exposed by local runtimes. RepoTunnel never scans the LAN or falls back to cloud inference.</p>
        </div>
        <button type="button" className="model-hub-refresh" onClick={() => void refreshAll()} disabled={refreshingAll}>
          <NavIcon name="resume" size={14} /> {refreshingAll ? "Refreshing…" : "Refresh all"}
        </button>
      </header>

      <div className="model-hub-overview">
        <div><span>Local runtimes</span><strong>{snapshot.connectedRuntimeCount} / 3 reachable</strong></div>
        <div><span>Available models</span><strong>{snapshot.availableModelCount}</strong></div>
        <div className="model-default-summary">
          <span>Default model</span>
          <strong>{selected ? selected.modelId : "Not selected"}</strong>
          {selected ? <button type="button" onClick={() => void clearSelection()} disabled={selectionBusy === "clear"}>Clear</button> : null}
        </div>
      </div>

      <div className="model-runtime-strip" aria-label="Local runtimes">
        {providerOrder.map((provider) => {
          const item = snapshot.runtimes.find((candidate) => candidate.provider === provider);
          return item ? <RuntimeSummary key={provider} runtime={item} active={runtime.provider === provider} onSelect={() => setActiveProvider(provider)} /> : null;
        })}
      </div>

      <section className="model-runtime-detail">
        <div className="model-runtime-detail-heading">
          <div>
            <div className="model-runtime-title-row">
              <span className={`model-runtime-dot ${runtime.reachable ? "connected" : ""}`} />
              <h3>{runtime.label}</h3>
              <span className={`model-runtime-state ${runtime.reachable ? "connected" : ""}`}>{runtime.reachable ? "Connected" : "Unavailable"}</span>
              {runtime.version ? <span className="model-runtime-version">v{runtime.version}</span> : null}
            </div>
            <p>{runtime.message}</p>
          </div>
          <div className="model-runtime-actions">
            <button type="button" onClick={() => void refreshOne(runtime.provider)} disabled={busyProvider === runtime.provider}>Refresh</button>
            <button type="button" onClick={() => void refreshOne(runtime.provider, true)} disabled={busyProvider === runtime.provider}>Test connection</button>
          </div>
        </div>

        <div className="model-endpoint-row">
          <label>
            <span>Endpoint <em>Loopback only</em></span>
            <input value={endpointDraft} onChange={(event) => setEndpointDrafts((current) => ({ ...current, [runtime.provider]: event.target.value }))} spellCheck={false} />
          </label>
          <button type="button" disabled={!endpointChanged || busyProvider === runtime.provider} onClick={() => void saveEndpoint(runtime.provider)}>Save endpoint</button>
        </div>

        {!runtime.reachable ? <div className="model-runtime-guidance"><NavIcon name="tip" size={15} /><span>{setupGuidance(runtime.provider)}</span></div> : null}
        {runtime.diagnostics ? <details className="model-runtime-diagnostics"><summary>Technical diagnostics</summary><pre>{runtime.diagnostics}</pre></details> : null}
      </section>

      <section className="model-list-section" aria-labelledby="local-models-title">
        <div className="model-list-heading">
          <div><span className="section-kicker">Selected runtime</span><h3 id="local-models-title">Models</h3></div>
          <span>{runtime.models.length} available through {runtime.label}</span>
        </div>

        {runtime.models.length === 0 ? (
          <div className="model-list-empty">
            <NavIcon name="model" size={19} />
            <div><strong>{runtime.reachable ? "No models exposed" : `${runtime.label} is unavailable`}</strong><span>{runtime.reachable ? "Load or install a model in the runtime itself, then refresh RepoTunnel." : "RepoTunnel will not install or start runtimes automatically."}</span></div>
          </div>
        ) : (
          <div className="model-list">
            {runtime.models.map((model) => {
              const key = `${runtime.provider}:${model.id}`;
              const isSelected = selected?.provider === runtime.provider && selected.modelId === model.id && selected.endpoint === runtime.endpoint;
              return (
                <article className={`model-row ${isSelected ? "selected" : ""}`} key={key}>
                  <div className="model-row-main">
                    <div className="model-name-line"><strong>{model.name}</strong>{isSelected ? <span>Default</span> : null}{model.loaded === true ? <span className="loaded">Loaded</span> : null}</div>
                    <small>{model.runtimeLabel}{model.parameterSize ? ` · ${model.parameterSize}` : ""}{model.quantization ? ` · ${model.quantization}` : ""}{formatBytes(model.sizeBytes) ? ` · ${formatBytes(model.sizeBytes)}` : ""}</small>
                  </div>
                  <div className="model-capabilities">
                    <Capability label="Context" value={formatContext(model.capabilities.contextWindow)} source={capabilitySource(model.capabilities.contextWindow.source)} />
                    <Capability label="Tools" value={formatCapability(model.capabilities.toolCalling)} source={capabilitySource(model.capabilities.toolCalling.source)} />
                    <Capability label="Structured" value={formatCapability(model.capabilities.structuredOutput)} source={capabilitySource(model.capabilities.structuredOutput.source)} />
                    <Capability label="Vision" value={formatCapability(model.capabilities.vision)} source={capabilitySource(model.capabilities.vision.source)} />
                  </div>
                  <div className="model-row-actions">
                    <button type="button" onClick={() => void runModelTest(model, runtime)} disabled={testBusy === key}>{testBusy === key ? "Testing…" : "Test model"}</button>
                    <button type="button" className={isSelected ? "selected" : "primary-button"} disabled={isSelected || selectionBusy === key} onClick={() => void chooseModel(model, runtime)}>{isSelected ? "Selected" : selectionBusy === key ? "Saving…" : "Use as default"}</button>
                  </div>
                </article>
              );
            })}
          </div>
        )}
      </section>

      {testResult ? (
        <section className={`model-test-result ${testResult.success ? "success" : "failure"}`} aria-live="polite">
          <span className="model-test-icon">{testResult.success ? "✓" : "!"}</span>
          <div><strong>{testResult.success ? "Model test passed" : "Model test did not pass"}</strong><span>{testResult.modelId} · {testResult.runtimeLabel} · {testResult.latencyMs} ms</span><p>{testResult.message}</p>{testResult.responseExcerpt ? <code>{testResult.responseExcerpt}</code> : null}</div>
          <button type="button" onClick={() => setTestResult(null)} aria-label="Dismiss model test result">×</button>
        </section>
      ) : null}

      <ModelTrial hub={snapshot} onNotice={onNotice} />

      <footer className="model-hub-footnote"><NavIcon name="shield" size={14} /><span>Stage 2 is local-only. No project files, terminal commands, browser actions, MCP tools or cloud APIs are used by model discovery or Test model.</span></footer>
    </section>
  );
}

export default ModelHub;
