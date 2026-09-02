import { useEffect, useMemo, useState } from "react";
import type { ModelHubSnapshot, ModelSelection, ModelTrialSnapshot, TrialMode } from "../types";
import { cancelModelTrial, getModelTrial, runModelTrial } from "../lib/backend";
import { modelTrialKey, sameModelIdentity } from "../lib/modelTrial";

function formatTested(timestamp: number): string {
  if (!timestamp) return "Never";
  return new Date(timestamp).toLocaleString([], { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit" });
}

type Props = { hub: ModelHubSnapshot; onNotice: (message: string) => void };

export default function ModelTrial({ hub, onNotice }: Props) {
  const [trial, setTrial] = useState<ModelTrialSnapshot | null>(null);
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [busy, setBusy] = useState(false);

  const models = useMemo(() => hub.runtimes.flatMap((runtime) => runtime.models.map((model) => ({
    model,
    runtime,
    selection: { provider: runtime.provider, modelId: model.id, endpoint: runtime.endpoint } satisfies ModelSelection,
  }))), [hub]);

  useEffect(() => {
    let disposed = false;
    getModelTrial().then((snapshot) => { if (!disposed) setTrial(snapshot); }).catch((error) => onNotice(`Model Trial: ${error instanceof Error ? error.message : String(error)}`));
    return () => { disposed = true; };
  }, [hub.refreshedAt]);

  useEffect(() => {
    if (selected.size > 0 || models.length === 0) return;
    const preferred = hub.selectedModel
      ? models.find((item) => modelTrialKey(item.selection) === modelTrialKey(hub.selectedModel!))
      : models[0];
    if (preferred) setSelected(new Set([modelTrialKey(preferred.selection)]));
  }, [models, hub.selectedModel, selected.size]);

  function toggle(selection: ModelSelection) {
    const key = modelTrialKey(selection);
    setSelected((current) => {
      const next = new Set(current);
      if (next.has(key)) next.delete(key); else next.add(key);
      return next;
    });
  }

  async function run(mode: TrialMode) {
    const selections = models.filter((item) => selected.has(modelTrialKey(item.selection))).map((item) => item.selection);
    if (selections.length === 0) { onNotice("Model Trial: select at least one discovered local model."); return; }
    if (selections.length > 12) { onNotice("Model Trial: select at most 12 models per trial."); return; }
    setBusy(true);
    try {
      setTrial(await runModelTrial(mode, selections));
      onNotice(`${mode === "quick" ? "Quick" : "Full"} Model Trial completed. Results are local and evidence-backed.`);
    } catch (error) {
      onNotice(`Model Trial: ${error instanceof Error ? error.message : String(error)}`);
      try { setTrial(await getModelTrial()); } catch { /* preserve last UI state */ }
    } finally { setBusy(false); }
  }

  async function cancel() {
    try {
      const cancelled = await cancelModelTrial();
      if (cancelled) onNotice("Model Trial cancellation requested. The local model runtime remains running.");
      setTrial(await getModelTrial());
    } catch (error) { onNotice(`Model Trial: ${error instanceof Error ? error.message : String(error)}`); }
  }

  return (
    <section className="model-trial" aria-labelledby="model-trial-title">
      <header className="model-trial-head">
        <div><span className="section-kicker">Local evidence</span><h3 id="model-trial-title">Model Trial</h3><p>Small fixed tasks compare only models already exposed by Ollama, LM Studio, or llama.cpp. Trials run one model and one inference at a time.</p></div>
        <div className="model-trial-actions">
          <button type="button" disabled={busy || trial?.running || selected.size === 0} onClick={() => void run("quick")}>Quick Trial</button>
          <button type="button" className="primary-button" disabled={busy || trial?.running || selected.size === 0} onClick={() => void run("full")}>Full Trial</button>
          {trial?.running || busy ? <button type="button" className="danger-subtle" onClick={() => void cancel()}>Cancel Trial</button> : null}
        </div>
      </header>

      <div className="model-trial-models">
        {models.map(({ model, runtime, selection }) => {
          const key = modelTrialKey(selection);
          const result = trial?.results.find((item) => item.identity.provider === selection.provider && item.identity.endpoint === selection.endpoint && item.identity.modelId === selection.modelId) ?? null;
          const running = sameModelIdentity(selection, trial?.activeModel ?? null);
          return <label className={`model-trial-row ${result && !result.current ? "stale" : ""}`} key={key}>
            <input type="checkbox" checked={selected.has(key)} disabled={Boolean(trial?.running || busy)} onChange={() => toggle(selection)} />
            <span className="model-trial-name"><strong>{model.name}</strong><small>{runtime.label}</small></span>
            <span className={`model-trial-status ${running ? "running" : result?.status ?? "untested"}`}><strong>{running ? "Running" : result ? (result.current ? result.status : "Stale") : "Not tested"}</strong><small>{result?.failureReason ?? result?.staleReason ?? (result ? `${result.failedCases} failed · ${result.malformedCases} malformed` : "No stored evidence")}</small></span>
            <span className="model-trial-tested"><strong>{result ? formatTested(result.testedAt) : "Never"}</strong><small>{result ? `${Math.round(result.averageLatencyMs / 100) / 10}s avg` : "Last tested"}</small></span>
          </label>;
        })}
      </div>

      <footer className="model-trial-footnote">Suite {trial?.suiteVersion ?? "stage10-v1"} · scores are rounded local comparison indicators, not scientific absolutes · no model download or cloud fallback is performed.</footer>
    </section>
  );
}
