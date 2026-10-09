/** Relevant next steps for each capability and workflow guide. */
export const relatedPages: Record<string, Record<string, string[]>> = {
  product: {
    projects: ['local-model-chat', 'history', 'terminal'],
    mcp: ['direct-https', 'browser-automation', 'local-model-chat'],
    terminal: ['workflow-checks', 'git-github', 'browser-automation'],
    'git-github': ['history', 'projects', 'team-mode'],
    'browser-automation': ['ai-workspace', 'workflow-checks', 'terminal'],
    'ai-workspace': ['desktop-control', 'browser-automation', 'android-phone'],
    'desktop-control': ['ai-workspace', 'browser-automation', 'android-phone'],
    'android-phone': ['desktop-control', 'ai-workspace', 'video'],
    'team-mode': ['continuity', 'git-github', 'projects'],
    continuity: ['chatgpt-extension', 'history', 'team-mode'],
    video: ['ai-workspace', 'projects', 'terminal'],
    'direct-https': ['mcp', 'workflow-checks', 'projects'],
    'local-model-chat': ['projects', 'history', 'mcp'],
    history: ['continuity', 'git-github', 'projects'],
    'workflow-checks': ['terminal', 'projects', 'git-github'],
    'chatgpt-extension': ['continuity', 'team-mode', 'history']
  },
  solutions: {
    'chatgpt-local-projects': ['local-mcp-development', 'secure-ai-local-files', 'long-running-ai-work'],
    'secure-ai-local-files': ['chatgpt-local-projects', 'ai-terminal-access', 'chatgpt-git-workflow'],
    'local-mcp-development': ['chatgpt-local-projects', 'ai-browser-automation', 'ai-terminal-access'],
    'ai-browser-automation': ['ai-terminal-access', 'secure-ai-local-files', 'chatgpt-local-projects'],
    'ai-terminal-access': ['ai-browser-automation', 'chatgpt-git-workflow', 'secure-ai-local-files'],
    'chatgpt-git-workflow': ['secure-ai-local-files', 'two-ai-coding', 'long-running-ai-work'],
    'android-ai-control': ['secure-ai-local-files', 'local-mcp-development', 'ai-browser-automation'],
    'two-ai-coding': ['chatgpt-git-workflow', 'long-running-ai-work', 'ai-terminal-access'],
    'long-running-ai-work': ['two-ai-coding', 'chatgpt-git-workflow', 'chatgpt-local-projects']
  }
};
