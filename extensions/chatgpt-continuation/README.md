# RepoTunnel Continuation

RepoTunnel Continuation is the optional Chromium companion for RepoTunnel's ChatGPT continuation bridge.

It binds only the exact ChatGPT conversations you explicitly save. When RepoTunnel has an authorized continuation checkpoint, the extension waits until that saved conversation is safely idle before delivering it.

A separate **Default message chats** section can also bind an exact ChatGPT conversation to a fixed message you provide. That manual mode does not use RepoTunnel continuation checkpoints: after each safely completed reply it sends the configured message again, and supported interruption/timeout recovery uses that same per-chat message. Each saved manual chat can be edited or paused/resumed from the popup. Pause/resume and edits invalidate pending send authorization; uncertain sends are never replayed automatically. The existing RepoTunnel continuation list remains limited to five chats; the manual default-message list has no extension-imposed chat-count limit. One exact conversation cannot be active in both modes at the same time.

## Safety behavior

- exact saved ChatGPT conversations only
- checkpoint delivery uses the exact RepoTunnel-generated message
- when the latest saved-chat response shows **Connection interrupted. Waiting for the complete answer/message** while still generating (Stop visible), preserve the existing Stop → one recovery message flow
- when the latest saved-chat response shows **Message delivery timed out. Please try again.**, stop the response first if ChatGPT still shows Stop, wait until the composer is safely idle, then send one fixed resume-from-live-RepoTunnel-state message; if already idle, send without stopping; never click ChatGPT's **Try again** button
- both recovery paths check drafts, recent human input, saved-chat ownership, duplicate tabs, navigation races, and concurrent checkpoint delivery; an uncertain recovery send is never automatically replayed
- short operational waits run in the extension worker instead of throttled page timers
- exact saved tabs are protected from automatic discarding; removing a saved chat or navigating away restores its original setting
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

Up to five exact conversations can be saved in the RepoTunnel continuation list.

For fixed-message automation, use the separate **Default message chats** section: paste the exact conversation URL, enter the message, and save it. Re-saving the same URL updates its saved message. Active manual chats are monitored by the existing background alarm/tab-protection path, while Chrome is running and the page is not frozen or discarded.

After updating this folder, reload the extension at `chrome://extensions`, then refresh your saved ChatGPT tabs. Keep the existing unpacked installation pointed at this same project folder so its extension identity and bridge pairing are preserved.

## Background operation

Keep Chrome running and the computer awake. Browser alarms can be delayed, so background checks do not promise an exact second. Browser-frozen or manually discarded tabs cannot perform page actions until resumed. If your browser freezes ChatGPT under Energy Saver, add `chatgpt.com` to its always-active site list. Recovery itself does not require RepoTunnel to be available; checkpoint deliveries still require the local bridge.

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
