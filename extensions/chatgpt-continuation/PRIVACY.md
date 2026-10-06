# Privacy

RepoTunnel Continuation is a local companion extension for RepoTunnel.

## Data it stores

The extension stores only the information needed to identify conversations you explicitly save:

- exact ChatGPT conversation URL
- ChatGPT tab title
- bounded local connection/delivery status

This data is stored in the browser's extension-local storage. Temporary bridge-health state is kept in extension session storage.

## Data it sends

The extension communicates only with:

- the ChatGPT page you explicitly saved, to detect safe idle state and deliver an authorized continuation message
- RepoTunnel's dedicated loopback continuation bridge at `127.0.0.1:43185`

The extension has no analytics, advertising, telemetry, remote configuration, or third-party tracking.

## Scope

The extension does not read unrelated websites. Its declared host access is limited to `chatgpt.com` and RepoTunnel's local loopback bridge.

Removing a saved conversation removes it from the extension's saved-target list. Removing the extension clears its browser-managed local extension storage.
