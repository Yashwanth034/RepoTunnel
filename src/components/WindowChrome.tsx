import { getCurrentWindow } from "@tauri-apps/api/window";

const appWindow = getCurrentWindow();

function WindowChrome() {
  return (
    <header className="window-chrome" data-tauri-drag-region>
      <div className="window-title" data-tauri-drag-region>RepoTunnel</div>
      <div className="window-controls">
        <button type="button" aria-label="Minimize" title="Minimize" onClick={() => appWindow.minimize()}>
          <span className="window-minimize" />
        </button>
        <button type="button" aria-label="Maximize" title="Maximize" onClick={() => appWindow.toggleMaximize()}>
          <span className="window-maximize" />
        </button>
        <button className="window-close" type="button" aria-label="Close" title="Close" onClick={() => appWindow.close()}>
          <span>×</span>
        </button>
      </div>
    </header>
  );
}

export default WindowChrome;
