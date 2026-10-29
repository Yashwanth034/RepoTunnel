# MCP Flight Recorder — bounded transport diagnostics

## Purpose

RepoTunnel's MCP Flight Recorder helps diagnose intermittent MCP disconnects without recording private prompts, tool inputs, file content, authentication details or host information. The recorder is **observational, local-only, nonblocking and read-only to AI**. It does not alter gateway authorization, retries, connection lifecycle, Task Integrity or workspace permissions.

## Where it records

- **Local MCP gateway:** an outermost Axum middleware observes the HTTP status even when the existing authorization guard rejects a request. It records only response-header latency, not full SSE stream duration.
- **Direct HTTPS proxy:** records the HTTP status returned by the loopback gateway and separately classifies failure to send to the upstream gateway.
- The recorder knows only fixed, allowlisted categories: gateway vs Direct HTTPS; MCP, OAuth, health or other route; GET, POST, DELETE or other method; response-headers vs upstream-connect stage; numeric HTTP status; pre-defined status category; local millisecond timestamp and time-to-response-headers.

**Never recorded:** raw path/URL/query, token, Authorization header, headers of any kind, cookies, IPs, hostnames, tool name, tool parameters, prompts, file paths, request or response body, error message, response content, or request correlation identifiers supplied by clients. No inspection of other applications occurs.

## How to use

Call `get_mcp_flight_recorder({limit:50})` over an active RepoTunnel MCP connection. The optional limit is clamped to 1–100 events (newest first). The response contains counts over the retained window, persistence status, dropped persistence snapshots, and explicit limitations. Diagnostic access uses normal RepoTunnel AI access checks and never grants extra filesystem or account permissions.

The recorder keeps at most **256 events** in memory. It saves bounded snapshots to RepoTunnel's private application-data file `mcp-flight-recorder.json` through the existing crash-recovery-safe JSON store. Snapshots are queued on a bounded asynchronous worker channel; network handling never awaits disk writes. HTTP errors enqueue a snapshot promptly; routine successful requests are throttled to approximately one persistence snapshot every 30 seconds. If the queue fills, networking proceeds and `droppedPersistenceSnapshots` increases. Recent in-memory successes can be lost in an abrupt crash before the next snapshot; this is not an audited, lossless event log.

The on-disk file is also bounded and validated before recovery. No diagnostic events are written into user project source files or forwarded to telemetry services.

## Interpretation

- **401/authChallenge:** OAuth challenges may be expected, especially during initialization. This alone is not proof of a connectivity fault.
- **403/accessDenied:** rejected by an authorization/security rule, which may be intentional.
- **503/serviceUnavailable:** gateway configuration or service unavailable; correlate with current gateway health.
- **502/upstreamFailure at Direct HTTPS upstreamConnect:** the HTTPS proxy attempted to reach the local gateway but did not receive an upstream response.
- **200/headersReturned:** HTTP response headers were returned; this is NOT proof the tool succeeded or an SSE stream remained healthy.

**Not observable:** DNS, remote TLS, tunnel/provider or network failures that occur *before a request reaches RepoTunnel*, connection loss after successful SSE response headers, reverse-proxy timeouts outside RepoTunnel, tool errors returned inside HTTP 200/MCP bodies, or client-side latency. A missing event is not evidence the client never tried. The gateway and Direct HTTPS events are not linked by a shared request ID; compare approximate timestamps and statuses, not fabricated correlation.

## Verification expectations

1. Verify the bounded event store drops oldest entries and retains the newest failure.
2. Verify serialization contains no attacker-controlled request text.
3. Verify durable private-store recovery and refusal of symlinked files.
4. Verify the read-only MCP tool is registered and classified correctly.
5. Verify gateway authorization and Direct HTTPS regression suites plus strict Clippy.
6. **Live acceptance remains a separate requirement:** load the rebuilt RepoTunnel application, exercise an expected OAuth challenge, an authorized MCP request and a controlled gateway-unreachable Direct HTTPS request without disturbing the active login session. Inspect the returned categories and confirm client-visible responses are unchanged.

No success claim should be based on unit tests alone for the live connection issue.