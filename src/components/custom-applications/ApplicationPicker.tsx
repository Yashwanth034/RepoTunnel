import { useEffect, useMemo, useState } from "react";
import {
  addDiscoveredCustomApplication, discoverCustomApplications, listLaunchableApplications,
  pickCustomApplication, startAiWorkspace, launchDiscoveredDesktopApplication,
} from "../../lib/backend";
import type { DiscoverableApplication, LaunchApplication } from "../../types";
import "./ApplicationPicker.css";

type Item = { id: string; name: string; kind: "builtin" | "desktop" };
type PickItem = { id: string; name: string; kind: "native" | "builtin" | "desktop" };
const KEY = "repotunnel.application-shortcuts.v1";

function readSaved(): Item[] {
  try {
    const raw: unknown = JSON.parse(localStorage.getItem(KEY) ?? "[]");
    if (!Array.isArray(raw)) return [];
    return raw.slice(0, 100).flatMap((v): Item[] => {
      if (typeof v?.id !== "string" || typeof v?.name !== "string"
        || v.id.length > 2100 || v.name.length > 120 || !v.name.trim()) return [];
      // Legacy website shortcuts are intentionally not imported. This
      // picker grants access only to applications installed on the computer.
      return v.kind === "builtin" || v.kind === "desktop"
        ? [{ id: v.id, name: v.name, kind: v.kind }]
        : [];
    });
  } catch { return []; }
}

type Props = {
  workspaceId: string | null;
  approvedIds: string[];
  open: boolean;
  onClose: () => void;
  onApproved: () => Promise<void>;
  onError: (message: string) => void;
  onNotice: (message: string) => void;
};
export default function ApplicationPicker({
  workspaceId, approvedIds, open, onClose, onApproved, onError, onNotice,
}: Props) {
  const [saved, setSaved] = useState<Item[]>(readSaved);
  const [installed, setInstalled] = useState<DiscoverableApplication[]>([]);
  const [builtin, setBuiltin] = useState<LaunchApplication[]>([]);
  const [search, setSearch] = useState("");
  const [busy, setBusy] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);

  useEffect(() => {
    if (!open) return;
    let active = true;
    setLoading(true);
    void Promise.allSettled([
      discoverCustomApplications(),
      workspaceId ? listLaunchableApplications(workspaceId) : Promise.resolve([]),
    ]).then(([nativeResult, builtinResult]) => {
      if (!active) return;
      if (nativeResult.status === "fulfilled") setInstalled(nativeResult.value);
      else onError("Installed app search failed: " + String(nativeResult.reason));
      if (builtinResult.status === "fulfilled") setBuiltin(builtinResult.value);
      else onError("App catalog search failed: " + String(builtinResult.reason));
      setLoading(false);
    });
    return () => { active = false; };
  }, [open, workspaceId, onError]);

  function persist(next: Item[]) {
    try {
      localStorage.setItem(KEY, JSON.stringify(next));
      setSaved(next);
    } catch { onError("Could not save app shortcuts on this computer."); }
  }
  function addPinned(item: Item) {
    if (saved.some((s) => s.id === item.id)) return;
    persist([...saved, item]);
    onNotice(item.kind === "desktop"
      ? item.name + " added as an installed desktop shortcut. It opens outside AI Workspace."
      : item.name + " saved as a built-in AI Workspace app.");
    onClose();
  }
  async function addNative(item: Pick<DiscoverableApplication, "id" | "name">) {
    setBusy(item.id);
    try {
      const approved = await addDiscoveredCustomApplication(item.id);
      await onApproved();
      window.dispatchEvent(new Event("repotunnel:applications-updated"));
      onNotice(approved.name + " approved for isolated AI Workspace.");
      onClose();
    } catch (error) { onError(String(error)); }
    finally { setBusy(null); }
  }
  async function advanced() {
    setBusy("advanced");
    try {
      const app = await pickCustomApplication();
      if (app) {
        await onApproved();
        window.dispatchEvent(new Event("repotunnel:applications-updated"));
        onNotice(app.name + " approved for isolated AI Workspace.");
        onClose();
      }
    } catch (error) { onError(String(error)); }
    finally { setBusy(null); }
  }
  async function launchNative(item: Item) {
    if (!workspaceId) return;
    setBusy(item.id);
    try {
      await startAiWorkspace(workspaceId, item.id);
      onNotice(item.name + " launched in isolated AI Workspace.");
    } catch (error) { onError(String(error)); }
    finally { setBusy(null); }
  }
  async function launchDesktop(item: Item) {
    if (!workspaceId) return;
    setBusy(item.id);
    try {
      const outcome = await launchDiscoveredDesktopApplication(workspaceId, item.id);
      onNotice(outcome.queued
        ? item.name + " desktop launch is waiting for project approval."
        : item.name + " desktop launch " + outcome.launch.status + ".");
    } catch (error) { onError(String(error)); }
    finally { setBusy(null); }
  }
  const options: PickItem[] = useMemo(() => {
    const native = [
      ...builtin.filter((a) => a.category !== "browser" && a.id !== "docker"
        && !a.id.toLowerCase().includes("repotunnel")
        && !saved.some((s) => s.id === a.id))
        .map((a) => ({ id: a.id, name: a.name, kind: "builtin" as const })),
      ...installed.filter((a) => !approvedIds.includes(a.id)
        && !saved.some((s) => s.id === a.id)
        && !builtin.some((b) => b.name.toLowerCase() === a.name.toLowerCase()))
        .map((a) => ({
          id: a.id, name: a.name,
          kind: a.launchMode === "desktop" ? "desktop" as const : "native" as const,
        })),
    ];
    return native
      .filter((item) => item.name.toLowerCase().includes(search.trim().toLowerCase()))
      .sort((a, b) => a.name.localeCompare(b.name));
  }, [approvedIds, builtin, installed, saved, search]);

  return (
    <>
      {saved.length > 0 && (
        <div className="app-picker-added" aria-label="Saved applications">
          {saved.map((app) => (
            <div className="app-picker-row" key={app.id}>
              <div><strong>{app.name}</strong>
                <small>{app.kind === "desktop" ? "Flatpak/Snap · system desktop (not AI Workspace)"
                  : "Built-in · AI Workspace"}</small></div>
              <button type="button" className="secondary-button" disabled={!workspaceId || busy !== null}
                onClick={() => app.kind === "desktop" ? void launchDesktop(app) : void launchNative(app)}>
                Open
              </button>
              <button type="button" className="secondary-button" disabled={busy !== null}
                onClick={() => persist(saved.filter((s) => s.id !== app.id))}>Remove</button>
            </div>
          ))}
        </div>
      )}
      {open && (
        <div className="app-picker-backdrop">
          <div className="app-picker-dialog" role="dialog" aria-modal="true"
            aria-label="Choose an application"
            onKeyDown={(event) => { if (event.key === "Escape") onClose(); }}>
            <div className="app-picker-heading">
              <div><strong>Add installed application</strong><p>Search applications detected on this computer.</p></div>
              <button type="button" className="secondary-button" onClick={onClose}>Close</button>
            </div>
            <input autoFocus aria-label="Search installed applications" value={search}
              placeholder="Search installed applications..."
              onChange={(e) => setSearch(e.target.value)} />
            <div role="list" className="app-picker-results">
              {options.map((item) => (
                <div className="app-picker-row" role="listitem" key={item.kind + item.id}>
                  <div><strong>{item.name}</strong>
                    <small>{item.kind === "desktop" ? "Flatpak/Snap · system desktop" : "Native app · AI Workspace"}</small></div>
                  <button type="button" className="secondary-button" disabled={busy !== null}
                    onClick={() => item.kind === "native" ? void addNative(item)
                      : addPinned({ id: item.id, name: item.name, kind: item.kind })}>
                    {busy === item.id ? "Adding…" : "Add"}
                  </button>
                </div>
              ))}
              {!options.length && <p className="panel-empty">
                {loading ? "Looking for installed apps…" : "No matching apps found."}
              </p>}
            </div>
            <div className="app-picker-advanced">
              <button type="button" className="secondary-button" onClick={() => void advanced()}
                disabled={busy !== null}>Choose executable file (advanced)</button>
              <small>Only for apps that cannot be found automatically.</small>
            </div>
          </div>
        </div>
      )}
    </>
  );
}
