# App-content coverage audit

Reviewed 2026-10-09 against the current RepoTunnel project files, using read-only access to the app project. This is a content/source audit, not a claim that every current source feature was tested in desktop v0.4.1.

Before this pass, eight of the eleven pictured areas had topic guides. Home, the full History workflow and Checks lacked dedicated walkthroughs. GPT Agents was described under Team Mode without clearly matching the app label. The optional extension had a guide but was difficult to discover.

| App label | Website walkthrough | Primary source evidence |
| --- | --- | --- |
| Home | /docs/getting-started/home-and-local-chat/ | src/components/HomeWorkspace.tsx; HomeChat.tsx; App.tsx |
| Projects | /docs/getting-started/first-project/ | src/components/WorkspaceList.tsx; ProjectSetupPanel.tsx; ProjectMemoryPanel.tsx |
| GPT Agents | /docs/team/two-ai-workflow/ | src/components/AppSidebar.tsx; App.tsx; TeamPanel.tsx; docs/team-mode.md |
| Video | /docs/video/production/ | src/components/VideoPanel.tsx; docs/video.md |
| History | /docs/history/activity-and-versions/ | src/App.tsx; PendingChangeReview.tsx; ChangeHistoryPanel.tsx; CheckpointManager.tsx; docs/safe-editing.md |
| Checks | /docs/checks/workflow-readiness/ | src/components/WorkflowPanel.tsx; docs/workflow.md |
| Git | /docs/git/workflow/ | docs/git.md |
| Connect | /docs/connections/overview/ | docs/connection.md; src/App.tsx |
| Commands | /docs/commands/verification-vs-live/ | docs/commands.md; src/components/ExecutionPanel.tsx |
| Phone | /docs/phone/overview/ | docs/phone-access.md |
| HTTPS Setup | /docs/connections/direct-https/ | src/components/HttpsSetupGuide.tsx; docs/direct-https.md |
| Settings | /docs/reference/settings-and-help/ | src/components/ProductionPanel.tsx; docs/release.md |
| Help | /docs/reference/settings-and-help/ | src/components/HelpPanel.tsx |
| Optional continuation extension | /docs/continuity/chatgpt-extension/ | README.md; extensions/chatgpt-continuation/README.md; PRIVACY.md |

## Boundaries preserved in the explanations

- Checks inspects readiness and is read-only; actual verification runs through Commands.
- Pending approvals can span approved projects. Activity/version timelines follow the selected project.
- Clear All Checkpoints follows Active project only / All projects scope and includes pinned snapshots. Restoration requires write access and removes accessible files added after the target snapshot.
- Home documents the available local-model picker and runtime discovery. A Model Hub navigation path was not found, so it is not presented as a required user step.
- GPT Agents is the visible entry to persistent two-engineer Team Mode.
- Extension installation uses the current main-branch source folder and Load unpacked. No separate v0.4.1 extension asset or browser-store listing is claimed.
- Current-source links do not prove installed-release behavior; the docs footer points readers to release notes for version-specific information.

## Coverage and verification

The site now has 40 documentation guides, 16 product articles and 9 solution pages. The homepage maps every app label directly to a guide; the extension is linked from the homepage, installation page, docs index and its product article.

The source audit checks the exact sidebar labels and related-guide targets. The full build verifies every internal URL/anchor and search entry. Browser QA checks every generated page at seven widths, along with copy controls, menus, search, themes, tabs and reduced motion. These checks validate the website; they do not exercise destructive operations in the RepoTunnel app.
