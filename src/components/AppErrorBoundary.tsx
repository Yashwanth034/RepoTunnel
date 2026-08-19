import { Component, type ErrorInfo, type ReactNode } from "react";

type AppErrorBoundaryProps = {
  children: ReactNode;
  resetKey?: string;
  onGoHome?: () => void;
  compact?: boolean;
};

type AppErrorBoundaryState = {
  error: Error | null;
  details: string;
};

function copyText(value: string) {
  if (navigator.clipboard?.writeText) return navigator.clipboard.writeText(value);
  const textarea = document.createElement("textarea");
  textarea.value = value;
  textarea.style.position = "fixed";
  textarea.style.opacity = "0";
  document.body.appendChild(textarea);
  textarea.select();
  document.execCommand("copy");
  textarea.remove();
  return Promise.resolve();
}

export default class AppErrorBoundary extends Component<AppErrorBoundaryProps, AppErrorBoundaryState> {
  state: AppErrorBoundaryState = { error: null, details: "" };

  static getDerivedStateFromError(error: Error): Partial<AppErrorBoundaryState> {
    return { error };
  }

  componentDidCatch(error: Error, info: ErrorInfo) {
    this.setState({ details: `${error.stack ?? error.message}\n\nReact component stack:\n${info.componentStack}` });
  }

  componentDidUpdate(previous: AppErrorBoundaryProps) {
    if (previous.resetKey !== this.props.resetKey && this.state.error) {
      // Navigation itself is a safe recovery path; a broken panel should not poison other views.
      this.setState({ error: null, details: "" });
    }
  }

  private reset = () => this.setState({ error: null, details: "" });

  private goHome = () => {
    this.reset();
    if (this.props.onGoHome) this.props.onGoHome();
    else window.location.reload();
  };

  render() {
    const { error, details } = this.state;
    if (!error) return this.props.children;

    const diagnostic = details || error.stack || error.message;
    return (
      <section className={`app-error-boundary ${this.props.compact ? "compact" : ""}`} role="alert">
        <div className="app-error-icon" aria-hidden="true">!</div>
        <div className="app-error-copy">
          <span className="section-kicker">Recovered safely</span>
          <h2>This RepoTunnel view hit an unexpected error</h2>
          <p>The rest of RepoTunnel is still running. Retry this view, return Home, or copy the technical details for debugging.</p>
          <code>{error.message}</code>
        </div>
        <div className="app-error-actions">
          <button type="button" className="primary-button" onClick={this.reset}>Retry view</button>
          <button type="button" className="secondary-button" onClick={this.goHome}>{this.props.onGoHome ? "Go Home" : "Reload RepoTunnel"}</button>
          <button type="button" className="secondary-button" onClick={() => void copyText(diagnostic)}>Copy error details</button>
        </div>
      </section>
    );
  }
}
