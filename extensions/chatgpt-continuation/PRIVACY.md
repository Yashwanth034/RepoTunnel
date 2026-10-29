# Privacy

RepoTunnel Continuation is a local companion extension for RepoTunnel.

## Data it stores

The extension stores only the information needed to identify conversations you explicitly save and operate the selected mode:

- exact ChatGPT conversation URL
- ChatGPT tab title for RepoTunnel continuation targets
- the fixed default message you explicitly configure for a manual default-message chat
- bounded local connection/delivery status

This data is stored in the browser's extension-local storage. Original automatic-discard settings and the associated saved-chat tab IDs/URLs are stored locally so they can be restored after an extension reload. Old tab IDs are invalidated on browser startup. Temporary bridge-health, interruption-recovery, and manual one-shot delivery incident state are kept in extension session storage.

## Data it sends

The extension communicates only with:

- the ChatGPT page you explicitly saved, to detect safe idle state and deliver either an authorized RepoTunnel continuation message or the fixed default message you configured for that manual chat; supported interrupted manual chats may be stopped before that configured message is sent
- RepoTunnel's dedicated loopback continuation bridge at `127.0.0.1:43185` for RepoTunnel continuation targets

The extension has no analytics, advertising, telemetry, remote configuration, or third-party tracking.

## Scope

The extension does not read unrelated websites. Its declared host access is limited to `chatgpt.com` and RepoTunnel's local loopback bridge.

Removing a saved conversation removes it from the extension's saved-target list. Removing the extension clears its browser-managed local extension storage.
