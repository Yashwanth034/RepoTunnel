import { useCallback, useEffect, useState } from "react";
import {
  getHistorySettings,
  getRuntimeDiagnostics,
  setLaunchAtLogin,
  updateHistorySettings,
} from "../lib/backend";
import type { HistorySettings, RuntimeDiagnostics } from "../types";

type ProductionPanelProps = {
  onError: (message: string) => void;
  onNotice: (message: string) => void;
  uiScale: number;
  onUiScaleChange: (scale: number) => void;
};

function ProductionPanel({ onError, onNotice, uiScale, onUiScaleChange }: ProductionPanelProps) {
  const [diagnostics, setDiagnostics] = useState<RuntimeDiagnostics | null>(null);
  const [historySettings, setHistorySettings] = useState<HistorySettings | null>(null);
  const [loading, setLoading] = useState(true);
  const [refreshing, setRefreshing] = useState(false);
  const [busy, setBusy] = useState(false);
  const [retentionBusy, setRetentionBusy] = useState(false);
  const [lastRefreshedAt, setLastRefreshedAt] = useState<number | null>(null);

  const refresh = useCallback(async (notify = false) => {
    setRefreshing(true);
    try {
      const [runtime, history] = await Promise.all([getRuntimeDiagnostics(), getHistorySettings()]);
      setDiagnostics(runtime);
      setHistorySettings(history);
      if (notify) {
        setLastRefreshedAt(Date.now());
        onNotice("Settings refreshed.");
      }
    } catch (error) {
      onError(error instanceof Error ? error.message : String(error));
    } finally {
      setLoading(false);
      setRefreshing(false);
    }
  }, [onError, onNotice]);

  useEffect(() => {
    refresh(false).catch(() => undefined);
  }, [refresh]);

  async function toggleAutostart() {
    if (!diagnostics) return;
    setBusy(true);
    try {
      setDiagnostics(await setLaunchAtLogin(!diagnostics.launchAtLogin));
    } catch (error) {
      onError(error instanceof Error ? error.message : String(error));
    } finally {
      setBusy(false);
    }
  }

  async function updateRetention(
    key: "version" | "checkpoint",
    value: string,
  ) {
    if (!historySettings) return;
    const limit = value === "all" ? null : Number(value);
    const versionHistoryLimit = key === "version" ? limit : historySettings.versionHistoryLimit;
    const checkpointLimit = key === "checkpoint" ? limit : historySettings.checkpointLimit;
    setRetentionBusy(true);
    try {
      const updated = await updateHistorySettings(versionHistoryLimit, checkpointLimit);
      setHistorySettings(updated);
      onNotice("History retention updated.");
    } catch (error) {
      onError(error instanceof Error ? error.message : String(error));
    } finally {
      setRetentionBusy(false);
    }
  }

  return (
    <section className="panel production-panel">
      <div className="panel-heading">
        <div>
          <span className="eyebrow">Production</span>
          <h2>Runtime diagnostics</h2>
          <p>Local release health, required tools, startup behavior, and support paths.</p>
        </div>
        <div className="production-refresh-control">
          <button
            type="button"
            className="secondary-button"
            onClick={() => void refresh(true)}
            disabled={refreshing}
          >
            {refreshing ? "Refreshing…" : "Refresh"}
          </button>
          {lastRefreshedAt ? <span aria-live="polite">Updated just now</span> : null}
        </div>
      </div>

      {loading ? <div className="panel-empty">Checking this installation…</div> : null}

      {diagnostics ? (
        <>
          <div className="diagnostic-grid">
            <div className="diagnostic-item">
              <span>Version</span>
              <strong>{diagnostics.version}</strong>
            </div>
            <div className="diagnostic-item">
              <span>Platform</span>
              <strong>{diagnostics.platform} / {diagnostics.architecture}</strong>
            </div>
            <div className="diagnostic-item">
              <span>Bubblewrap</span>
              <strong>{diagnostics.sandboxAvailable ? "Ready" : "Missing"}</strong>
            </div>
            <div className="diagnostic-item">
              <span>tunnel-client</span>
              <strong>{diagnostics.tunnelClientAvailable ? "Ready" : "Missing"}</strong>
            </div>
            <div className="diagnostic-item">
              <span>Git</span>
              <strong>{diagnostics.gitAvailable ? "Ready" : "Missing"}</strong>
            </div>
            <div className="diagnostic-item">
              <span>Launch at login</span>
              <strong>{diagnostics.launchAtLogin ? "Enabled" : "Disabled"}</strong>
            </div>
          </div>

          <div className="production-actions">
            <button type="button" className="secondary-button" onClick={toggleAutostart} disabled={busy}>
              {busy ? "Updating…" : diagnostics.launchAtLogin ? "Disable launch at login" : "Enable launch at login"}
            </button>
            <span>Startup is intentionally disconnected; RepoTunnel never persists your tunnel runtime API key.</span>
          </div>

          <div className="interface-settings">
            <div className="history-retention-heading">
              <div>
                <strong>Interface</strong>
                <span>Scale the entire RepoTunnel desktop UI independently from Linux or Windows display scaling.</span>
              </div>
              <span className="interface-scale-value">{uiScale}%</span>
            </div>
            <div className="interface-scale-controls">
              {[100, 110, 125, 140, 150].map((scale) => (
                <button
                  type="button"
                  key={scale}
                  className={uiScale === scale ? "active" : ""}
                  onClick={() => onUiScaleChange(scale)}
                >
                  {scale}%
                </button>
              ))}
            </div>
            <small>Shortcuts: Ctrl + + / Ctrl + - to scale, Ctrl + 0 to reset. Your choice is remembered after restart.</small>
          </div>

          {historySettings ? (
            <div className="history-retention-settings">
              <div className="history-retention-heading">
                <div>
                  <strong>History &amp; recovery</strong>
                  <span>Limit local recovery metadata without changing project files.</span>
                </div>
              </div>
              <div className="history-retention-grid">
                <label>
                  <span>Version history</span>
                  <select
                    value={historySettings.versionHistoryLimit ?? "all"}
                    disabled={retentionBusy}
                    onChange={(event) => void updateRetention("version", event.target.value)}
                  >
                    <option value="all">Keep all</option>
                    <option value="100">Keep latest 100</option>
                    <option value="250">Keep latest 250</option>
                    <option value="500">Keep latest 500</option>
                  </select>
                </label>
                <label>
                  <span>Checkpoints</span>
                  <select
                    value={historySettings.checkpointLimit ?? "all"}
                    disabled={retentionBusy}
                    onChange={(event) => void updateRetention("checkpoint", event.target.value)}
                  >
                    <option value="all">Keep all</option>
                    <option value="10">Keep latest 10</option>
                    <option value="25">Keep latest 25</option>
                    <option value="50">Keep latest 50</option>
                  </select>
                </label>
              </div>
              <small>Pinned checkpoints are never removed by automatic retention.</small>
            </div>
          ) : null}

          <div className="runtime-paths">
            <div><span>Data</span><code>{diagnostics.dataDirectory}</code></div>
            <div><span>Logs</span><code>{diagnostics.logFile}</code></div>
          </div>

          {diagnostics.warnings.length > 0 ? (
            <div className="diagnostic-warnings">
              {diagnostics.warnings.map((warning) => <p key={warning}>{warning}</p>)}
            </div>
          ) : (
            <div className="diagnostic-ready">Core local runtime dependencies are available.</div>
          )}
        </>
      ) : null}
    </section>
  );
}

export default ProductionPanel;
