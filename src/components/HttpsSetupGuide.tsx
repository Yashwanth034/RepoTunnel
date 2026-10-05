import { useCallback, useEffect, useMemo, useState } from "react";
import {
  configurePublicTunnel,
  getHttpsSetupReadiness,
  installHttpsSetupWireguardConfig,
  openHttpsSetupResource,
  provisionDirectHttpsCertificate,
  verifyHttpsSetupHostname,
} from "../lib/backend";
import type {
  HttpsSetupHostnameVerification,
  HttpsSetupReadiness,
  HttpsSetupResource,
} from "../types";

type HttpsSetupGuideProps = {
  onError: (message: string) => void;
};

type CheckState = "ready" | "needed" | "checking" | "info";

function StatusPill({ state, label }: { state: CheckState; label: string }) {
  return <span className={`https-setup-status ${state}`}>{label}</span>;
}

function CheckRow({
  label,
  ready,
  pendingLabel = "Needs setup",
}: {
  label: string;
  ready: boolean;
  pendingLabel?: string;
}) {
  return (
    <div className="https-setup-check-row">
      <span>{label}</span>
      <StatusPill state={ready ? "ready" : "needed"} label={ready ? "Ready" : pendingLabel} />
    </div>
  );
}

function HttpsSetupGuide({ onError }: HttpsSetupGuideProps) {
  const [readiness, setReadiness] = useState<HttpsSetupReadiness | null>(null);
  const [hostname, setHostname] = useState("");
  const [hostnameStatus, setHostnameStatus] = useState<HttpsSetupHostnameVerification | null>(null);
  const [checking, setChecking] = useState(false);
  const [hostnameChecking, setHostnameChecking] = useState(false);
  const [installingWireguard, setInstallingWireguard] = useState(false);
  const [directBusy, setDirectBusy] = useState(false);

  const refresh = useCallback(async () => {
    setChecking(true);
    try {
      setReadiness(await getHttpsSetupReadiness());
    } catch (error) {
      onError(error instanceof Error ? error.message : String(error));
    } finally {
      setChecking(false);
    }
  }, [onError]);

  useEffect(() => {
    void refresh();
    const timer = window.setInterval(() => void refresh(), 15_000);
    return () => window.clearInterval(timer);
  }, [refresh]);

  const verifyHostname = useCallback(async (value: string) => {
    const trimmed = value.trim();
    if (!trimmed) {
      setHostnameStatus(null);
      return;
    }
    setHostnameChecking(true);
    try {
      setHostnameStatus(await verifyHttpsSetupHostname(trimmed));
    } catch (error) {
      onError(error instanceof Error ? error.message : String(error));
    } finally {
      setHostnameChecking(false);
    }
  }, [onError]);

  useEffect(() => {
    if (!hostname.trim()) {
      setHostnameStatus(null);
      return;
    }
    const timer = window.setTimeout(() => void verifyHostname(hostname), 900);
    return () => window.clearTimeout(timer);
  }, [hostname, verifyHostname]);

  async function openResource(resource: HttpsSetupResource) {
    try {
      await openHttpsSetupResource(resource);
    } catch (error) {
      onError(error instanceof Error ? error.message : String(error));
    }
  }

  async function importWireguardConfig() {
    setInstallingWireguard(true);
    try {
      const result = await installHttpsSetupWireguardConfig();
      if (!result.cancelled) await refresh();
    } catch (error) {
      onError(error instanceof Error ? error.message : String(error));
    } finally {
      setInstallingWireguard(false);
    }
  }

  async function configureDirectHttps() {
    const trimmed = hostname.trim();
    if (!trimmed) return;
    setDirectBusy(true);
    try {
      const host = trimmed.includes("://") ? new URL(trimmed).hostname : trimmed;
      await configurePublicTunnel("direct", "", `https://${host}`);
      await refresh();
      await verifyHostname(trimmed);
    } catch (error) {
      onError(error instanceof Error ? error.message : String(error));
    } finally {
      setDirectBusy(false);
    }
  }

  async function requestTrustedCertificate() {
    setDirectBusy(true);
    try {
      await provisionDirectHttpsCertificate(false);
      await refresh();
      if (hostname.trim()) await verifyHostname(hostname);
    } catch (error) {
      onError(error instanceof Error ? error.message : String(error));
    } finally {
      setDirectBusy(false);
    }
  }

  const localPrerequisitesReady = Boolean(
    readiness?.supportedPlatform
      && readiness.wireguardInstalled
      && readiness.wgQuickInstalled
      && readiness.nftablesInstalled
      && readiness.opensslInstalled
      && readiness.certbotReady
      && readiness.systemdAvailable,
  );

  const wireguardImportReady = Boolean(
    readiness?.supportedPlatform
      && readiness.wireguardInstalled
      && readiness.wgQuickInstalled
      && readiness.nftablesInstalled
      && readiness.systemdAvailable
      && readiness.pkexecAvailable,
  );

  const publicRouteReady = Boolean(
    readiness?.nativeGlobalIpv6Available
      || readiness?.wireguardInterfaceActive
      || readiness?.standardWireguardServiceActive,
  );

  const hostnameDnsReady = Boolean(
    hostnameStatus?.validHostname
      && hostnameStatus.dnsResolves
      && hostnameStatus.ipv6Available
      && hostnameStatus.ipv4Available,
  );

  const directRuntimeReady = Boolean(
    readiness?.directHttpsConfigured
      && readiness.directHttpsLocalReady
      && readiness.directHttpsTlsTrusted
      && readiness.directHttpsPublicReachable,
  );

  const endpointReady = Boolean(
    directRuntimeReady
      && hostnameStatus?.healthReachable
      && hostnameStatus.tlsTrusted
      && hostnameStatus.oauthResourceMetadataReachable
      && hostnameStatus.oauthServerMetadataReachable,
  );

  const completedSteps = useMemo(
    () => [localPrerequisitesReady, publicRouteReady, hostnameDnsReady, endpointReady].filter(Boolean).length,
    [endpointReady, hostnameDnsReady, localPrerequisitesReady, publicRouteReady],
  );

  const currentStep = !localPrerequisitesReady
    ? 1
    : !publicRouteReady
      ? 2
      : !hostnameDnsReady
        ? 3
        : !endpointReady
          ? 4
          : 5;

  const routeSummary = readiness?.nativeGlobalIpv6Available
    ? "Native public IPv6 detected. No additional network setup is required."
    : publicRouteReady
      ? "A WireGuard public IPv6 route is ready."
      : "A public IPv6 route is still needed.";

  return (
    <section className="https-setup-guide" aria-labelledby="https-setup-title">
      <div className="https-setup-heading">
        <div>
          <span className="section-kicker">HTTPS Setup</span>
          <h3 id="https-setup-title">Set up Direct HTTPS</h3>
          <p>
            Follow the four steps in order. RepoTunnel verifies each step without showing saved network
            addresses, credentials or MCP details here.
          </p>
        </div>
        <div className="https-setup-heading-actions">
          <span className="https-setup-progress">
            {checking ? "Checking…" : completedSteps === 4 ? "Setup complete" : `${completedSteps} of 4 complete`}
          </span>
          <button className="secondary-button" type="button" disabled={checking} onClick={() => void refresh()}>
            Refresh
          </button>
        </div>
      </div>

      {readiness && !readiness.supportedPlatform ? (
        <div className="https-setup-notice">
          Automatic system setup checks are currently verified for Linux. The existing Direct HTTPS connection
          remains unchanged on other platforms.
        </div>
      ) : null}

      <div className="https-setup-steps">
        <article className={`https-setup-step ${localPrerequisitesReady ? "complete" : currentStep === 1 ? "active" : ""}`}>
          <div className="https-setup-step-title">
            <span className="https-setup-step-number">{localPrerequisitesReady ? "✓" : "1"}</span>
            <div>
              <strong>Local requirements</strong>
              <span>
                {localPrerequisitesReady
                  ? "All required local components are available."
                  : "Check the local components RepoTunnel needs for Direct HTTPS."}
              </span>
            </div>
            {currentStep === 1 ? <span className="https-setup-current">Current step</span> : null}
          </div>

          {localPrerequisitesReady ? (
            <details className="https-setup-details">
              <summary>Show technical details</summary>
              <div className="https-setup-checks">
                <CheckRow label="WireGuard tools" ready={Boolean(readiness?.wireguardInstalled && readiness?.wgQuickInstalled)} />
                <CheckRow label="nftables" ready={Boolean(readiness?.nftablesInstalled)} />
                <CheckRow label="OpenSSL" ready={Boolean(readiness?.opensslInstalled)} />
                <CheckRow label="Certificate tooling" ready={Boolean(readiness?.certbotReady)} pendingLabel={readiness?.pipxInstalled ? "Available on demand" : "Needs setup"} />
                <CheckRow label="System service manager" ready={Boolean(readiness?.systemdAvailable)} />
              </div>
            </details>
          ) : (
            <>
              <div className="https-setup-checks">
                <CheckRow label="WireGuard tools" ready={Boolean(readiness?.wireguardInstalled && readiness?.wgQuickInstalled)} />
                <CheckRow label="nftables" ready={Boolean(readiness?.nftablesInstalled)} />
                <CheckRow label="OpenSSL" ready={Boolean(readiness?.opensslInstalled)} />
                <CheckRow label="Certificate tooling" ready={Boolean(readiness?.certbotReady)} pendingLabel={readiness?.pipxInstalled ? "Available on demand" : "Needs setup"} />
                <CheckRow label="System service manager" ready={Boolean(readiness?.systemdAvailable)} />
              </div>
              <div className="https-setup-actions">
                <button className="secondary-button" type="button" onClick={() => void openResource("wireguard")}>
                  Open install guide
                </button>
              </div>
            </>
          )}
        </article>

        <article className={`https-setup-step ${publicRouteReady ? "complete" : currentStep === 2 ? "active" : "future"}`}>
          <div className="https-setup-step-title">
            <span className="https-setup-step-number">{publicRouteReady ? "✓" : "2"}</span>
            <div>
              <strong>Internet connection</strong>
              <span>{routeSummary}</span>
            </div>
            {currentStep === 2 ? <span className="https-setup-current">Current step</span> : null}
          </div>

          {publicRouteReady ? (
            <details className="https-setup-details">
              <summary>Show technical details</summary>
              <div className="https-setup-checks">
                <CheckRow label="Native global IPv6" ready={Boolean(readiness?.nativeGlobalIpv6Available)} pendingLabel="Not detected" />
                <CheckRow label="Active WireGuard interface" ready={Boolean(readiness?.wireguardInterfaceActive)} pendingLabel="Not detected" />
                <CheckRow label="Standard guided configuration" ready={Boolean(readiness?.standardWireguardConfigPresent)} pendingLabel="Not detected" />
                <CheckRow label="WireGuard starts automatically" ready={Boolean(readiness?.standardWireguardServiceActive)} pendingLabel="Not active" />
              </div>
            </details>
          ) : currentStep === 2 ? (
            <div className="https-setup-guidance">
              <strong>No usable public IPv6 path was detected.</strong>
              <p>
                Create a routed IPv6 tunnel, download its WireGuard configuration, then import it here.
                RepoTunnel validates it before the normal OS privilege prompt.
              </p>
              <div className="https-setup-actions">
                <button className="secondary-button" type="button" onClick={() => void openResource("route64")}>
                  Open Route64
                </button>
                <button
                  className="primary-button"
                  type="button"
                  disabled={!wireguardImportReady || installingWireguard}
                  onClick={() => void importWireguardConfig()}
                >
                  {installingWireguard ? "Installing…" : "Import WireGuard config"}
                </button>
              </div>
              {!readiness?.pkexecAvailable ? <span>Automatic import needs the normal Linux OS privilege prompt.</span> : null}
            </div>
          ) : (
            <p className="https-setup-next-copy">Complete the previous step first.</p>
          )}
        </article>

        <article className={`https-setup-step ${hostnameDnsReady ? "complete" : currentStep === 3 ? "active" : "future"}`}>
          <div className="https-setup-step-title">
            <span className="https-setup-step-number">{hostnameDnsReady ? "✓" : "3"}</span>
            <div>
              <strong>Choose a hostname</strong>
              <span>
                {hostnameDnsReady
                  ? "Hostname and DNS checks passed."
                  : "Create or enter the hostname you want RepoTunnel to use."}
              </span>
            </div>
            {currentStep === 3 ? <span className="https-setup-current">Current step</span> : null}
          </div>

          {hostnameDnsReady ? (
            <details className="https-setup-details">
              <summary>Show verification details</summary>
              <div className="https-setup-checks">
                <CheckRow label="Hostname format" ready={Boolean(hostnameStatus?.validHostname)} />
                <CheckRow label="DNS resolution" ready={Boolean(hostnameStatus?.dnsResolves)} />
                <CheckRow label="IPv6 route in DNS" ready={Boolean(hostnameStatus?.ipv6Available)} />
                <CheckRow label="IPv4 compatibility" ready={Boolean(hostnameStatus?.ipv4Available)} pendingLabel="Not detected" />
              </div>
            </details>
          ) : currentStep === 3 ? (
            <>
              <div className="https-setup-hostname-row">
                <input
                  aria-label="Direct HTTPS hostname"
                  autoComplete="off"
                  spellCheck={false}
                  value={hostname}
                  onChange={(event) => setHostname(event.target.value)}
                  placeholder="your-name.duckdns.org"
                />
                <button className="secondary-button" type="button" onClick={() => void openResource("duckdns")}>
                  Open DuckDNS
                </button>
                <button
                  className="primary-button"
                  type="button"
                  disabled={!hostname.trim() || hostnameChecking}
                  onClick={() => void verifyHostname(hostname)}
                >
                  {hostnameChecking ? "Checking…" : "Verify hostname"}
                </button>
              </div>
              <p className="https-setup-privacy-copy">
                Entering a hostname does not save it. RepoTunnel saves it only when you choose Configure Direct HTTPS.
              </p>

              {hostnameStatus ? (
                <details className="https-setup-details" open>
                  <summary>Verification details</summary>
                  <div className="https-setup-checks">
                    <CheckRow label="Hostname format" ready={hostnameStatus.validHostname} />
                    <CheckRow label="DNS resolution" ready={hostnameStatus.dnsResolves} />
                    <CheckRow label="IPv6 route in DNS" ready={hostnameStatus.ipv6Available} />
                    <CheckRow label="IPv4 compatibility" ready={hostnameStatus.ipv4Available} pendingLabel="Not detected" />
                  </div>
                </details>
              ) : null}
              {hostnameStatus?.validHostname && !hostnameStatus.ipv6Available ? (
                <div className="https-setup-guidance">
                  <strong>IPv6 is not configured for this hostname yet.</strong>
                  <p>
                    Update the hostname&apos;s IPv6 record in DuckDNS, save it, then verify the hostname again.
                  </p>
                </div>
              ) : null}
              {hostnameStatus?.validHostname && hostnameStatus.ipv6Available && !hostnameStatus.ipv4Available ? (
                <div className="https-setup-guidance">
                  <strong>IPv4 compatibility is still needed.</strong>
                  <p>Follow the IPv4 compatibility instructions, update the hostname, then verify it again.</p>
                  <button className="secondary-button" type="button" onClick={() => void openResource("ipv4-compatibility")}>
                    Open IPv4 compatibility
                  </button>
                </div>
              ) : null}
            </>
          ) : (
            <p className="https-setup-next-copy">Complete the previous steps first.</p>
          )}
        </article>

        <article className={`https-setup-step ${endpointReady ? "complete" : currentStep === 4 ? "active" : "future"}`}>
          <div className="https-setup-step-title">
            <span className="https-setup-step-number">{endpointReady ? "✓" : "4"}</span>
            <div>
              <strong>Finish Direct HTTPS</strong>
              <span>
                {endpointReady
                  ? "Direct HTTPS is configured and verified."
                  : currentStep === 4
                    ? "Configure HTTPS, then verify the public connection."
                    : readiness?.directHttpsConfigured
                      ? "An existing Direct HTTPS setup was detected and will be verified next."
                      : "Ready after the hostname is verified."}
              </span>
            </div>
            {currentStep === 4 ? <span className="https-setup-current">Current step</span> : null}
          </div>

          {currentStep === 4 || endpointReady ? (
            <>
              {readiness?.directHttpsConfigured && !endpointReady ? (
                <div className="https-setup-notice">
                  <strong>Existing Direct HTTPS setup detected.</strong>
                  <span>RepoTunnel will verify it before making any changes.</span>
                </div>
              ) : null}

              {!endpointReady ? (
                <div className="https-setup-actions">
                  {!readiness?.directHttpsConfigured ? (
                    <button
                      className="primary-button"
                      type="button"
                      disabled={!hostnameDnsReady || directBusy}
                      onClick={() => void configureDirectHttps()}
                    >
                      {directBusy ? "Configuring…" : "Configure Direct HTTPS"}
                    </button>
                  ) : null}
                  {readiness?.directHttpsConfigured && readiness.directHttpsLocalReady && !readiness.directHttpsTlsTrusted ? (
                    <button
                      className="primary-button"
                      type="button"
                      disabled={directBusy}
                      onClick={() => void requestTrustedCertificate()}
                    >
                      {directBusy ? "Working…" : "Get trusted certificate"}
                    </button>
                  ) : null}
                  <button
                    className="secondary-button"
                    type="button"
                    disabled={checking || hostnameChecking}
                    onClick={() => {
                      void refresh();
                      if (hostname.trim()) void verifyHostname(hostname);
                    }}
                  >
                    Verify connection
                  </button>
                </div>
              ) : null}

              <details className="https-setup-details" open={!endpointReady}>
                <summary>{endpointReady ? "Show verification details" : "Verification details"}</summary>
                <div className="https-setup-checks">
                  <CheckRow label="Direct HTTPS configured" ready={Boolean(readiness?.directHttpsConfigured)} />
                  <CheckRow label="Local HTTPS listener" ready={Boolean(readiness?.directHttpsLocalReady)} />
                  <CheckRow label="Trusted certificate" ready={Boolean(readiness?.directHttpsTlsTrusted)} />
                  <CheckRow label="Network redirect rules" ready={Boolean(readiness?.nftablesRulesPresent)} pendingLabel={readiness?.nftablesRulesReadable ? "Not detected" : "Needs privileged verification"} />
                  <CheckRow label="External reachability" ready={Boolean(readiness?.directHttpsPublicReachable)} />
                  <CheckRow label="Trusted HTTPS health" ready={Boolean(hostnameStatus?.healthReachable && hostnameStatus?.tlsTrusted)} />
                  <CheckRow label="OAuth resource metadata" ready={Boolean(hostnameStatus?.oauthResourceMetadataReachable)} />
                  <CheckRow label="OAuth server metadata" ready={Boolean(hostnameStatus?.oauthServerMetadataReachable)} />
                </div>
              </details>

              {endpointReady ? (
                <div className="https-setup-complete">
                  <strong>Direct HTTPS is ready.</strong>
                  <span>All four setup steps passed.</span>
                </div>
              ) : null}
            </>
          ) : (
            <p className="https-setup-next-copy">Complete the hostname step first.</p>
          )}
        </article>
      </div>
    </section>
  );
}

export default HttpsSetupGuide;
