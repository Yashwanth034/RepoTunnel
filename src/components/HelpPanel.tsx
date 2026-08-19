const shortcutGroups = [
  {
    title: "Workspace",
    items: [
      ["Ctrl+P", "Quick Open"],
      ["Ctrl+Shift+P", "Command Palette"],
      ["Ctrl+Shift+F", "Search Project"],
      ["Ctrl+Shift+T", "Reopen Closed Tab"],
      ["Ctrl+Shift+Enter", "Focus Mode"],
      ["Ctrl++ / Ctrl+- / Ctrl+0", "Interface Scale"],
    ],
  },
  {
    title: "Editor",
    items: [
      ["Ctrl+S", "Save"],
      ["Ctrl+F", "Find in File"],
      ["Ctrl+G", "Go to Line"],
      ["Tab / Shift+Tab", "Indent / Outdent"],
      ["Shift+Alt+↓", "Duplicate Lines"],
      ["Alt+↑ / Alt+↓", "Move Lines"],
    ],
  },
  {
    title: "Terminal",
    items: [
      ["Enter", "Run Command"],
      ["Ctrl+Enter", "Start Managed Process"],
      ["↑ / ↓", "Command History"],
      ["Ctrl+F", "Search Output"],
      ["Ctrl+L", "Clear Terminal"],
      ["Esc", "Close Terminal Search"],
    ],
  },
];

function HelpPanel() {
  return (
    <section className="workspace-section help-panel">
      <div className="section-heading">
        <div>
          <span className="section-kicker">Help</span>
          <h2>Use RepoTunnel with confidence</h2>
          <p>Quick guidance for projects, AI modes, security, Git behavior, recovery, and the shortcuts already built into the workspace.</p>
        </div>
      </div>

      <div className="help-essentials-grid">
        <article>
          <span className="help-card-index">01</span>
          <strong>Start with a project</strong>
          <p>Add a trusted local folder or give the AI a supported repository link. RepoTunnel keeps AI file and terminal access inside the approved project.</p>
        </article>
        <article>
          <span className="help-card-index">02</span>
          <strong>Choose the right mode</strong>
          <p><b>AI Auto</b> runs compatible project actions immediately. <b>AI Review</b> keeps local approval for changes that need your confirmation.</p>
        </article>
        <article>
          <span className="help-card-index">03</span>
          <strong>Security stays enforced</strong>
          <p>Outside-project files remain user-controlled, external files use the native picker, and sensitive values are blocked or redacted before reaching an AI.</p>
        </article>
        <article>
          <span className="help-card-index">04</span>
          <strong>Git stays intentional</strong>
          <p>AI Auto can stage and commit without interruption, but pushing to a remote requires an explicit user instruction to push.</p>
        </article>
      </div>

      <div className="help-workflow-strip" aria-label="Recommended RepoTunnel workflow">
        <span><b>1</b> Approve project</span>
        <i>→</i>
        <span><b>2</b> Work with AI</span>
        <i>→</i>
        <span><b>3</b> Review History / Git</span>
        <i>→</i>
        <span><b>4</b> Verify in Checks / Terminal</span>
      </div>

      <section className="help-shortcuts-section" aria-labelledby="help-shortcuts-title">
        <div className="help-subheading">
          <div>
            <strong id="help-shortcuts-title">Keyboard shortcuts</strong>
            <span>Context-aware shortcuts stay out of the way until the related workspace is focused.</span>
          </div>
        </div>
        <div className="help-shortcut-groups">
          {shortcutGroups.map((group) => (
            <article className="help-shortcut-group" key={group.title}>
              <strong>{group.title}</strong>
              <div className="help-shortcut-grid">
                {group.items.map(([keys, label]) => (
                  <div className="help-shortcut" key={`${group.title}:${keys}`}>
                    <kbd>{keys}</kbd>
                    <span>{label}</span>
                  </div>
                ))}
              </div>
            </article>
          ))}
        </div>
      </section>

      <div className="help-footer-notes">
        <div><strong>History</strong><span>Every applied file edit keeps a local restore point.</span></div>
        <div><strong>Team</strong><span>Engineer A and B share plans, ownership, review, and verification state.</span></div>
        <div><strong>External files</strong><span>The AI cannot freely browse your computer; you choose files explicitly.</span></div>
      </div>
    </section>
  );
}

export default HelpPanel;
