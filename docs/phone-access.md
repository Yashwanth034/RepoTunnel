# RepoTunnel Phone Access

## Purpose

Phone Access gives RepoTunnel a user-controlled, persistent connection to a selected Android device.

It is not unrestricted ADB access exposed to an AI. RepoTunnel owns device selection, transport choice, capability authorization, output bounds, semantic guards, payment-sensitive blocking, and the persistent live screen/control runtime.

A general-purpose RepoTunnel Android application is not required. Precise semantic interaction uses the small bundled **RepoTunnel Phone Helper** accessibility service when the user installs/enables it.

## Current implementation status — 2026-10-05

The core Phone implementation is complete and has been exercised on a real Android device.

Current product capabilities include:

- a global **Phone** page between **Commands** and **HTTPS Setup**
- authorized USB and Wireless Debugging discovery
- six-digit Wireless Debugging pairing with mDNS endpoint discovery
- opaque RepoTunnel device IDs rather than exposing raw ADB serials/addresses to normal AI tools
- wireless-preferred reconnect with authorized USB fallback for the same selected physical device
- **Off / Limited / Full** AI access
- separate **Pause AI**
- all nine capability groups:
  - View screen
  - Touch & typing
  - Apps
  - Files
  - Install/remove apps
  - Device settings
  - Shell/debugging
  - Logs
  - Network tools
- a persistent logical phone runtime
- persistent live video/control using the bundled scrcpy 4.1 Android-side server
- private FFmpeg decoding for the live stream when required by the runtime
- normalized tap/swipe, bounded key/text input, and rapid multi-action sequences
- semantic snapshots/find/actions and bounded semantic transactions through RepoTunnel Phone Helper
- app list/launch/stop and APK install/remove
- bounded file list/stat/read/write/delete
- settings reads and device-specific settings write/delete availability
- bounded diagnostic shell, logs, network status, and ping
- structured Phone error codes instead of converting every failure into a generic operation error

MCP cannot pair/select a phone, change Off/Limited/Full access, alter Limited capability grants, or unpause Phone Access. Those controls remain local/user-owned.

## Verified real-device state

Final device audit used:

- **realme RMX2156**
- Android 12
- wireless RepoTunnel runtime
- Full Phone Access
- bundled Phone Helper integrity verification
- Accessibility enabled

The audited path passed:

- connection/runtime status
- all Full Access capability groups
- helper install/update and pinned-integrity verification
- normal and fast screen retrieval
- semantic find/click/action
- stale semantic-ref rejection
- wrong/stale foreground-package rejection
- tap, swipe, navigation keys, and normal/fast sequences
- non-payment semantic transaction flow
- file list/write/stat/read/delete
- application list/launch/stop
- generic APK install through the guarded Phone path
- APK staging cleanup
- network status and ping
- bounded diagnostic shell
- generic-shell rejection of Android UI/screen-capture bypass commands
- logs
- settings reads
- payment-sensitive foreground privacy behavior

This is evidence for the tested device/runtime path. It is not a claim that Android guarantees every capability identically across all vendors, Android versions, policies, or permission states.

## Connection and device identity

RepoTunnel uses one logical selected phone over interchangeable transports.

1. **Wireless ADB / Wireless Debugging** is preferred.
2. RepoTunnel reuses an established authorization when Android exposes the paired device again.
3. **Authorized USB ADB** is the fallback for the same selected phone.
4. A transport change does not change the AI-facing opaque device ID when RepoTunnel can prove it is the same phone.
5. If the selected phone is unavailable, operations fail/disconnect rather than silently switching to another device.

Wireless availability is best-effort because Android controls whether Wireless Debugging remains enabled after reboots, network changes, or device-policy changes.

## Access model

### Off

AI Phone operations are denied.

### Limited

Only the capability groups explicitly enabled by the user are granted.

Authorization is enforced centrally before the underlying Phone operation runs. A tool cannot grant itself a missing capability.

### Full

All RepoTunnel Phone capability groups are granted without RepoTunnel per-operation approval prompts.

Full Access does **not** bypass Android permissions, device policy, application restrictions, or OS confirmation dialogs.

### Pause AI

Pause AI is a separate immediate user boundary. While paused, AI Phone operations are denied even if the saved access mode is Full or Limited.

## Persistent live screen and control

The main Phone page uses a persistent scrcpy-style stream rather than launching a fresh screenshot command for every visible frame.

RepoTunnel:

- verifies the bundled scrcpy server before use
- places/starts the Android-side server through the selected ADB transport
- creates a private local ADB tunnel
- keeps a persistent video/control channel
- routes direct human Phone-panel tap/swipe/key/text input through a dedicated low-latency local UI path instead of synchronously probing the semantic helper on every human action
- keeps AI/MCP Phone mutations on the separate guarded path that enforces payment-sensitive foreground policy and stale-state checks
- decodes the live stream privately
- caches only the newest bounded frame required by the UI/tool path
- keeps normalized coordinate operations tied to known display/stream geometry

One-shot/fast screenshots remain available as guarded AI observations and fallbacks where appropriate.

## Semantic Phone Helper

The bundled RepoTunnel Phone Helper provides accessibility-based semantic grounding.

RepoTunnel can:

- report helper install/integrity/readiness state
- install/update the pinned helper APK through the guarded app-install path
- open the relevant Android Accessibility settings for the user
- produce bounded semantic snapshots with short-lived refs
- find elements server-side
- click/type through revalidated semantic refs
- execute bounded semantic/state transactions

Sensitive fields remain protected. Typed sensitive text is never echoed in normal tool results.

Accessibility remains a user-controlled Android permission; RepoTunnel does not silently enable it.

## Rapid and transactional interaction

### Fast sequence

The fast path groups already-grounded operations such as tap, swipe, key, type, app launch, and bounded waits into one persistent control request.

All steps are validated before the first mutation, and coordinate work fails closed when the expected stream/display geometry is stale.

### Semantic transaction

The transaction path combines semantic/state operations such as:

- launch app
- find
- click
- set text
- key
- wait for state
- verify state

Short-lived aliases/refs are revalidated before mutation. The transaction stops on failure rather than guessing or blindly replaying state-changing work.

## Payment-sensitive privacy boundary

RepoTunnel does not attempt to bypass banking/payment application security.

When the helper/runtime determines that a payment-sensitive app is foreground:

- RepoTunnel Accessibility remains enabled by default
- payment-sensitive state is reported
- semantic refs are invalidated
- screen observation is blocked
- semantic observation/action is blocked
- raw tap/swipe/key/type control is blocked
- generic shell access is blocked
- log snapshots are blocked

Leaving the payment-sensitive app automatically restores normal guarded Phone access.

A transaction may verify only the foreground package without inspecting payment UI. If successful work intentionally leaves a payment-sensitive app foreground, the optional final semantic snapshot is omitted and reported as payment-blocked rather than converting the transaction into a false failure.

`phone_pause_accessibility_for_payment` remains only a compatibility fallback for an application that genuinely refuses to operate while Accessibility is enabled. Android's official self-disable behavior requires the user to re-enable the helper manually afterward.

Real balances, PINs, OTPs, card data, or financial transactions are not required or appropriate for RepoTunnel validation.

## Guarded shell

Phone shell is a bounded diagnostic/developer surface, not an alternate UI-automation escape hatch.

The generic shell path rejects direct Android UI/screen-capture primitives such as the guarded `input`, `monkey`, `uiautomator`, `screencap`, `screenrecord`, and activity-manager UI-launch patterns. UI work must go through the dedicated guarded Phone tools.

Command/output size and execution duration are bounded.

## Files, apps, and packages

Phone file paths are validated/bounded and operations return structured metadata rather than exposing raw transport details.

App operations use validated Android package names.

APK installation stages only the requested approved APK and cleans up RepoTunnel-owned staging state. It does not provide arbitrary host filesystem access.

## Device settings

Settings capability is deliberately factual and device-specific.

Reads can be available while writes/deletes are unavailable. RepoTunnel learns deterministic Android permission denial and exposes effective availability instead of repeatedly pretending the operation should work.

On the audited Android 12 device:

- settings read: available
- settings write: unavailable after Android permission denial
- settings delete: unavailable after Android permission denial

That is an Android/device-policy result, not treated as a RepoTunnel failure in other Phone capabilities.

## Security and privacy invariants

- default Phone access is Off for a newly ungranted device
- access cannot silently transfer to another phone identity
- MCP cannot escalate Phone access
- raw ADB target details stay outside the normal AI targeting contract
- screenshots/app contents are not persisted as Phone authorization metadata
- helper/APK and bundled video-server integrity are verified
- sensitive text is not echoed
- Android confirmation/permission dialogs remain authoritative
- payment-sensitive foreground blocking is fail-safe
- RepoTunnel self-imposed capability checks remain active even under Full Access

## Failure and recovery behavior

- wireless loss: rediscover/reconnect when the selected paired device becomes available
- authorized USB for the same device: use as fallback when appropriate
- authorization revoked: stop AI Phone work and report authorization-required state
- multiple devices: never silently change the selected target
- stale semantic/document state: reject rather than apply to a guessed target
- helper unavailable: return a structured helper error rather than reporting false success
- disconnect: preserve the user's configured access policy while denying operations until the selected device is usable again

### Helper transport reliability

Older v0.3.1 builds could intermittently surface:

`PHONE_HELPER_UNAVAILABLE: Resource temporarily unavailable (os error 11)`

The cause was a timed Linux socket read returning `WouldBlock`/`EAGAIN` while the Android semantic worker could still be processing the same request. v0.4.1 keeps waiting on that same request/socket through transient `WouldBlock`/timeout polls until a bounded response deadline. It does **not** resend or replay the semantic action, so the fix does not introduce duplicate click/type risk.

Regression coverage now forces transient `WouldBlock` reads and a bounded helper timeout. The raw `os error 11` condition is no longer treated as an immediate helper outage.

## Acceptance status

The Phone feature is accepted for the tested core workflow on the audited real device and packaged build.

Future device/vendor testing should be treated as compatibility coverage, not as a reason to reopen already-verified core behavior. In particular, do not promise vendor-independent settings writes or permanent Wireless Debugging availability because Android remains authoritative.

The former intermittent helper `os error 11` transport failure is fixed and covered by bounded host-side regression tests. v0.4.1 also separates local human Phone-panel control from the AI/MCP payment-policy helper path so user taps/swipes/keys/text stay low-latency while AI/MCP control keeps the strict payment-sensitive guard. Empty helper socket closures are treated as transient helper unavailability rather than protocol corruption. Real-device vendor compatibility remains separate from these fixes.
