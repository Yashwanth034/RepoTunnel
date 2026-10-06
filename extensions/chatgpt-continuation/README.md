# RepoTunnel Continuation

RepoTunnel Continuation is the optional Chromium companion for RepoTunnel's ChatGPT continuation bridge.

It binds only the exact ChatGPT conversations you explicitly save. When RepoTunnel has an authorized continuation checkpoint, the extension waits until that saved conversation is safely idle before delivering it.

## Safety behavior

- exact saved ChatGPT conversations only
- RepoTunnel-generated checkpoints only
- waits for a completed, idle assistant response
- refuses to overwrite text already in the composer
- refuses to race recent human typing
- prepares the message before RepoTunnel authorizes the final send
- performs one send attempt after authorization
- treats an unverified post-send result as uncertain instead of replaying it

## Requirements

- RepoTunnel running locally
- Chrome/Chromium 102 or newer, or a compatible Chromium-based browser
- the target ChatGPT conversation open at an exact `https://chatgpt.com/c/...` URL

## Install

1. Open `chrome://extensions`.
2. Enable **Developer mode**.
3. Choose **Load unpacked**.
4. Select the `extensions/chatgpt-continuation` folder.
5. Open the ChatGPT conversation you want RepoTunnel to resume.
6. Open **RepoTunnel Continuation** from the browser toolbar.
7. Choose **Add current ChatGPT chat**.

Up to five exact conversations can be saved.

## Status

- **Not set** — no ChatGPT conversation is saved.
- **Waiting** — a conversation is saved, but the local RepoTunnel bridge has not recently registered it.
- **Connected** — the saved conversation has recently registered with RepoTunnel.

## Permissions

The extension requests:

- `activeTab` — read the current tab when you explicitly open the extension and save it
- `alarms` — maintain a low-frequency local bridge heartbeat
- `scripting` — restore the packaged content script after an extension reload if needed
- `storage` — remember saved conversations and temporary bridge health

Host access is limited to:

- `https://chatgpt.com/*`
- `http://127.0.0.1:43185/*`

The packaged content script is declared only for `https://chatgpt.com/c/*`.

## Pairing note

RepoTunnel pins the first valid Chrome-extension origin that registers with its local continuation bridge. Keep an unpacked installation in a stable folder after pairing.

If this machine is already paired to another installed copy, keep that working copy until you intentionally migrate the pairing. Do not delete RepoTunnel application data merely to change the extension folder.

## Privacy

No analytics, advertising, telemetry, remote configuration, or third-party tracking is included.

See [PRIVACY.md](PRIVACY.md) for the exact stored and transmitted data.
