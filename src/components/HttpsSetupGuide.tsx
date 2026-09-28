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

  return (
    <section className="https-setup-guide" aria-labelledby="https-setup-title">
      <div className="https-setup-heading">
        <div>
          <span className="section-kicker">HTTPS Setup</span>
          <h3 id="https-setup-title">Guided Direct HTTPS readiness</h3>
          <p>
            Checks the local requirements automatically and guides only the steps that still need attention.
            This setup view does not display saved IP addresses, credentials, public endpoint URLs or MCP URLs.
          </p>
        </div>
        <div className="https-setup-heading-actions">
          <StatusPill
            state={checking ? "checking" : completedSteps === 4 ? "ready" : "info"}
            label={checking ? "Checking…" : `${completedSteps}/4 ready`}
          />
          <button className="secondary-button" type="button" disabled={checking} onClick={() => void refresh()}>
            Verify again
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
        <article className="https-setup-step">
          <div className="https-setup-step-title">
            <span className="https-setup-step-number">1</span>
            <div>
              <strong>Local requirements</strong>
              <span>RepoTunnel checks installed tools without changing the computer.</span>
            </div>
            <StatusPill state={localPrerequisitesReady ? "ready" : "needed"} label={localPrerequisitesReady ? "Ready" : "Check"} />
          </div>
          <div className="https-setup-checks">
            <CheckRow label="WireGuard tools" ready={Boolean(readiness?.wireguardInstalled && readiness?.wgQuickInstalled)} />
            <CheckRow label="nftables" ready={Boolean(readiness?.nftablesInstalled)} />
            <CheckRow label="OpenSSL" ready={Boolean(readiness?.opensslInstalled)} />
            <CheckRow label="Certificate tooling" ready={Boolean(readiness?.certbotReady)} pendingLabel={readiness?.pipxInstalled ? "Available on demand" : "Needs setup"} />
            <CheckRow label="System service manager" ready={Boolean(readiness?.systemdAvailable)} />
          </div>
          {!localPrerequisitesReady ? (
            <div className="https-setup-actions">
              <button className="secondary-button" type="button" onClick={() => void openResource("wireguard")}>
                Open install guide
              </button>
            </div>
          ) : null}
        </article>

        <article className="https-setup-step">
          <div className="https-setup-step-title">
            <span className="https-setup-step-number">2</span>
            <div>
              <strong>Public IPv6 path</strong>
              <span>Native global IPv6 is used when available; otherwise a routed WireGuard path can be used.</span>
            </div>
            <StatusPill state={publicRouteReady ? "ready" : "needed"} label={publicRouteReady ? "Ready" : "Needs route"} />
          </div>
          <div className="https-setup-checks">
            <CheckRow label="Native global IPv6" ready={Boolean(readiness?.nativeGlobalIpv6Available)} pendingLabel="Not detected" />
            <CheckRow label="Active WireGuard interface" ready={Boolean(readiness?.wireguardInterfaceActive)} pendingLabel="Not detected" />
            <CheckRow label="Standard guided configuration" ready={Boolean(readiness?.standardWireguardConfigPresent)} pendingLabel="Not detected" />
            <CheckRow label="WireGuard starts automatically" ready={Boolean(readiness?.standardWireguardServiceActive)} pendingLabel="Not active" />
          </div>
          {!publicRouteReady ? (
            <div className="https-setup-guidance">
              <strong>No usable public IPv6 path was detected.</strong>
              <p>Create a routed IPv6 tunnel, choose WireGuard, and download the original provider configuration. Then import it here; RepoTunnel validates it before the OS privilege prompt installs the dedicated tunnel and redirect service.</p>
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
            <p className="https-setup-success-copy">
              A usable IPv6 path is detected. If this is native public IPv6, a Route64 account is not required.
            </p>
          )}
        </article>

        <article className="https-setup-step">
          <div className="https-setup-step-title">
            <span className="https-setup-step-number">3</span>
            <div>
              <strong>Hostname and DNS</strong>
              <span>Create a hostname, paste only that hostname here, and RepoTunnel verifies its DNS automatically.</span>
            </div>
            <StatusPill state={hostnameDnsReady ? "ready" : hostnameChecking ? "checking" : "needed"} label={hostnameDnsReady ? "Ready" : hostnameChecking ? "Checking…" : "Needs hostname"} />
          </div>

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
            <button className="secondary-button" type="button" disabled={!hostname.trim() || hostnameChecking} onClick={() => void verifyHostname(hostname)}>
              {hostnameChecking ? "Checking…" : "Verify now"}
            </button>
          </div>
          <p className="https-setup-privacy-copy">
            Typing a hostname does not save it. It is saved only if you explicitly choose Configure Direct HTTPS in the next step.
          </p>

          {hostnameStatus ? (
            <div className="https-setup-checks">
              <CheckRow label="Hostname format" ready={hostnameStatus.validHostname} />
              <CheckRow label="DNS resolution" ready={hostnameStatus.dnsResolves} />
              <CheckRow label="IPv6 route in DNS" ready={hostnameStatus.ipv6Available} />
              <CheckRow label="IPv4 compatibility" ready={hostnameStatus.ipv4Available} pendingLabel="Not detected" />
            </div>
          ) : null}
          {hostnameStatus?.validHostname && !hostnameStatus.ipv6Available ? (
            <div className="https-setup-guidance">
              <strong>The hostname does not have the required IPv6 record yet.</strong>
              <p>In DuckDNS, set the IPv6 field to the address assigned by your routed tunnel or stable public IPv6 provider, save it, then choose Verify now. This guide intentionally does not display or retain that address.</p>
            </div>
          ) : null}
          {hostnameStatus?.validHostname && hostnameStatus.ipv6Available && !hostnameStatus.ipv4Available ? (
            <div className="https-setup-guidance">
              <strong>IPv4 compatibility is still needed for the most compatible public endpoint.</strong>
              <p>Use the current IPv4-to-IPv6 frontend instructions, update the hostname&apos;s IPv4 record, then return here. RepoTunnel will verify it without displaying the address.</p>
              <button className="secondary-button" type="button" onClick={() => void openResource("ipv4-compatibility")}>
                Open IPv4 compatibility
              </button>
            </div>
          ) : null}
        </article>

        <article className="https-setup-step">
          <div className="https-setup-step-title">
            <span className="https-setup-step-number">4</span>
            <div>
              <strong>Direct HTTPS and final verification</strong>
              <span>Uses the existing Direct HTTPS backend from this new guide without changing other RepoTunnel sections.</span>
            </div>
            <StatusPill state={endpointReady ? "ready" : "needed"} label={endpointReady ? "Ready" : "Needs completion"} />
          </div>

          {readiness?.directHttpsConfigured ? (
            <div className="https-setup-notice">
              An existing Direct HTTPS configuration is already present. This guide will verify it but will not overwrite it automatically.
            </div>
          ) : null}

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
              Verify public path
            </button>
          </div>

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
          {endpointReady ? (
            <div className="https-setup-complete">
              <strong>Direct HTTPS checks passed.</strong>
              <span>The guide is complete without displaying endpoint, network, credential or MCP details in this view.</span>
            </div>
          ) : (
            <p className="https-setup-next-copy">
              Complete only the checks marked as needed. The guide reuses RepoTunnel&apos;s existing Direct HTTPS and certificate implementation instead of creating a second connection system.
            </p>
          )}
        </article>
      </div>
    </section>
  );
}

export default HttpsSetupGuide;
