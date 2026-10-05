import {
  type KeyboardEvent as ReactKeyboardEvent,
  type PointerEvent as ReactPointerEvent,
  type WheelEvent as ReactWheelEvent,
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
} from "react";

import {
  ensurePhoneRuntime,
  getPhoneAccessStatus,
  getPhoneDiscovery,
  getPhoneScreenFrame,
  pairPhoneWirelessly,
  phoneKeyEvent,
  phoneSwipe,
  phoneTap,
  phoneTypeText,
  selectPhoneDevice,
  setPhoneAccessMode,
  setPhoneAccessPaused,
  type PhoneUiKey,
} from "../lib/backend";
import type {
  PhoneAccessStatus,
  PhoneCapability,
  PhoneDeviceSummary,
  PhoneDiscoveryStatus,
  PhoneScreenFrame,
} from "../types";
import { NavIcon } from "./AppSidebar";

const EMPTY_DISCOVERY: PhoneDiscoveryStatus = {
  adbAvailable: true,
  devices: [],
  recommendedDeviceId: null,
  message: null,
};

const EMPTY_ACCESS: PhoneAccessStatus = {
  selectedDeviceId: null,
  mode: "off",
  paused: false,
  limitedCapabilities: [],
  grantedCapabilities: [],
};

const CAPABILITY_OPTIONS: Array<{
  id: PhoneCapability;
  label: string;
}> = [
  { id: "viewScreen", label: "View screen" },
  { id: "controlInput", label: "Touch & typing" },
  { id: "appControl", label: "Apps" },
  { id: "files", label: "Files" },
  { id: "appInstall", label: "Install / remove apps" },
  { id: "deviceSettings", label: "Device settings" },
  { id: "shell", label: "Shell & debugging" },
  { id: "logs", label: "Logs" },
  { id: "networkTools", label: "Network tools" },
];

type ConnectPanel = "wireless" | "usb" | "access" | null;

function transportLabel(device: PhoneDeviceSummary): string {
  if (device.transport === "wireless") return "Wireless";
  if (device.transport === "usb") return "USB";
  return "Connected";
}

function stateCopy(device: PhoneDeviceSummary): string {
  if (device.state === "authorizationRequired") return "Authorization required";
  if (device.state === "offline") return "Phone offline";
  if (device.state === "unavailable") return "Phone unavailable";
  return transportLabel(device);
}

function PhonePanel() {
  const [discovery, setDiscovery] = useState<PhoneDiscoveryStatus>(EMPTY_DISCOVERY);
  const [access, setAccess] = useState<PhoneAccessStatus>(EMPTY_ACCESS);
  const [selectedDeviceId, setSelectedDeviceId] = useState<string | null>(null);
  const [checking, setChecking] = useState(true);
  const [accessLoaded, setAccessLoaded] = useState(false);
  const [connectPanel, setConnectPanel] = useState<ConnectPanel>(null);
  const [pairCode, setPairCode] = useState("");
  const [pairing, setPairing] = useState(false);
  const [pairError, setPairError] = useState<string | null>(null);
  const [screenFrame, setScreenFrame] = useState<PhoneScreenFrame | null>(null);
  const [screenError, setScreenError] = useState<string | null>(null);
  const [accessBusy, setAccessBusy] = useState(false);
  const [accessError, setAccessError] = useState<string | null>(null);
  const [controlError, setControlError] = useState<string | null>(null);
  const [limitedDraft, setLimitedDraft] = useState<PhoneCapability[]>([]);
  const discoveryGenerationRef = useRef(0);
  const pairingRef = useRef(false);
  const connectionProbeRef = useRef(false);
  const accessRef = useRef(access);
  const manualDisconnectRef = useRef(false);
  const controlQueueRef = useRef<Promise<void>>(Promise.resolve());
  const lastScreenCapturedAtRef = useRef<number | null>(null);
  const pointerStartRef = useRef<{
    pointerId: number;
    clientX: number;
    clientY: number;
    xRatio: number;
    yRatio: number;
    startedAt: number;
  } | null>(null);
  const lastWheelAtRef = useRef(Number.NEGATIVE_INFINITY);

  useEffect(() => {
    accessRef.current = access;
  }, [access]);

  const applyDiscovery = useCallback((next: PhoneDiscoveryStatus) => {
    setDiscovery(next);
    setSelectedDeviceId((current) => {
      if (current && next.devices.some((device) => device.id === current)) return current;
      if (manualDisconnectRef.current) return null;

      const savedDeviceId = accessRef.current.selectedDeviceId;
      if (savedDeviceId && next.devices.some((device) => device.id === savedDeviceId)) {
        return savedDeviceId;
      }

      if (next.recommendedDeviceId) return next.recommendedDeviceId;
      if (next.devices.length === 1) return next.devices[0].id;
      return null;
    });
  }, []);

  const refresh = useCallback(async (showChecking = true) => {
    if (pairingRef.current || connectionProbeRef.current) return;

    const generation = ++discoveryGenerationRef.current;
    if (showChecking) setChecking(true);
    try {
      const next = await getPhoneDiscovery();
      if (generation !== discoveryGenerationRef.current) return;
      applyDiscovery(next);
    } catch (error) {
      if (generation !== discoveryGenerationRef.current) return;
      setDiscovery({
        adbAvailable: false,
        devices: [],
        recommendedDeviceId: null,
        message: error instanceof Error ? error.message : String(error),
      });
      // Preserve the user's selected phone across a transient discovery failure.
      // The persistent backend runtime owns the actual connection lifecycle.
    } finally {
      if (showChecking && generation === discoveryGenerationRef.current) {
        setChecking(false);
      }
    }
  }, [applyDiscovery]);

  useEffect(() => {
    void getPhoneAccessStatus()
      .then((next) => {
        setAccess(next);
        setLimitedDraft(next.limitedCapabilities);
        if (next.selectedDeviceId) setSelectedDeviceId(next.selectedDeviceId);
      })
      .catch(() => {
        setAccess(EMPTY_ACCESS);
        setAccessError("Phone access settings are unavailable. Access remains off.");
      })
      .finally(() => setAccessLoaded(true));
  }, []);

  useEffect(() => {
    if (!accessLoaded) return;

    let stopped = false;
    let timer: number | null = null;

    const poll = async (showChecking: boolean) => {
      await refresh(showChecking);
      if (!stopped) {
        timer = window.setTimeout(() => void poll(false), 5_000);
      }
    };

    void poll(true);
    return () => {
      stopped = true;
      if (timer !== null) window.clearTimeout(timer);
    };
  }, [accessLoaded, refresh]);

  useEffect(() => {
    if (
      !accessLoaded
      || access.mode !== "off"
      || !selectedDeviceId
      || access.selectedDeviceId === selectedDeviceId
    ) {
      return;
    }

    let cancelled = false;
    void selectPhoneDevice(selectedDeviceId)
      .then((next) => {
        if (cancelled) return;
        setAccess(next);
        setLimitedDraft(next.limitedCapabilities);
      })
      .catch((error) => {
        if (!cancelled) {
          setAccessError(error instanceof Error ? error.message : String(error));
        }
      });

    return () => {
      cancelled = true;
    };
  }, [accessLoaded, access.mode, access.selectedDeviceId, selectedDeviceId]);

  const selectedDevice = useMemo(
    () => discovery.devices.find((device) => device.id === selectedDeviceId) ?? null,
    [discovery.devices, selectedDeviceId],
  );

  const connected = selectedDevice?.state === "connected";

  useEffect(() => {
    if (!connected || !selectedDeviceId) return;

    const preferredTransport = selectedDevice?.transport === "usb"
      || selectedDevice?.transport === "wireless"
      ? selectedDevice.transport
      : undefined;
    void ensurePhoneRuntime(selectedDeviceId, preferredTransport).catch((error) => {
      setScreenError(error instanceof Error ? error.message : String(error));
    });
  }, [connected, selectedDeviceId, selectedDevice?.transport]);

  useEffect(() => {
    let stopped = false;
    let timer: number | null = null;
    lastScreenCapturedAtRef.current = null;
    setScreenFrame(null);
    setScreenError(null);

    if (!connected || !selectedDeviceId) {
      return () => undefined;
    }

    const capture = async () => {
      let retryDelayMs = 5;
      try {
        const next = await getPhoneScreenFrame(
          selectedDeviceId,
          lastScreenCapturedAtRef.current ?? undefined,
        );
        if (!stopped) {
          if (
            lastScreenCapturedAtRef.current === null
            || next.capturedAt > lastScreenCapturedAtRef.current
          ) {
            lastScreenCapturedAtRef.current = next.capturedAt;
            setScreenFrame(next);
          }
          setScreenError(null);
        }
      } catch (error) {
        retryDelayMs = 250;
        if (!stopped) {
          setScreenError(error instanceof Error ? error.message : String(error));
        }
      } finally {
        // The backend waits briefly for the next cached scrcpy frame. Healthy
        // streaming requeues immediately; failures back off so errors stay visible
        // and a broken transport cannot create a hot retry loop.
        if (!stopped) timer = window.setTimeout(() => void capture(), retryDelayMs);
      }
    };

    void capture();
    return () => {
      stopped = true;
      if (timer !== null) window.clearTimeout(timer);
    };
  }, [connected, selectedDeviceId]);

  useEffect(() => {
    if (connected && connectPanel === "usb") {
      setConnectPanel(null);
    }
  }, [connected, connectPanel]);

  const accessAppliesToSelected = Boolean(
    selectedDeviceId && access.selectedDeviceId === selectedDeviceId,
  );
  const selectedAccess = accessAppliesToSelected ? access : EMPTY_ACCESS;
  const controlEnabled = Boolean(
    connected
      && selectedAccess.grantedCapabilities.includes("controlInput")
      && !selectedAccess.paused,
  );

  const title = selectedDevice?.name
    ?? (discovery.devices.length > 1 ? "Choose a phone" : "No phone connected");

  const subtitle = selectedDevice
    ? [
        selectedDevice.androidVersion ? `Android ${selectedDevice.androidVersion}` : null,
        stateCopy(selectedDevice),
      ].filter(Boolean).join(" · ")
    : !discovery.adbAvailable
      ? "Phone connection setup is not available"
      : checking
        ? "Looking for your phone…"
        : "USB direct · Wireless available";

  const accessLabel = selectedAccess.mode === "full"
    ? "Full Access"
    : selectedAccess.mode === "limited"
      ? "Limited Access"
      : "Access Off";

  async function chooseDevice(deviceId: string | null) {
    manualDisconnectRef.current = deviceId === null;
    setSelectedDeviceId(deviceId);
    setAccessBusy(true);
    setAccessError(null);
    try {
      const next = await selectPhoneDevice(deviceId);
      setAccess(next);
      setLimitedDraft(next.limitedCapabilities);
    } catch (error) {
      setAccessError(error instanceof Error ? error.message : String(error));
      await refresh();
    } finally {
      setAccessBusy(false);
    }
  }

  async function connectDiscoveredTransport(
    transport: "usb" | "wireless",
    reportMissing = true,
  ) {
    if (accessBusy || pairing || connectionProbeRef.current) return;

    connectionProbeRef.current = true;
    discoveryGenerationRef.current += 1;
    setChecking(true);
    setAccessBusy(true);
    setAccessError(null);
    try {
      const next = await getPhoneDiscovery();
      setDiscovery(next);

      const candidates = next.devices.filter((device) => (
        device.state === "connected"
        && device.availableTransports.includes(transport)
      ));

      if (candidates.length !== 1) {
        if (reportMissing) {
          const label = transport === "usb" ? "USB" : "wireless";
          setAccessError(
            candidates.length === 0
              ? `No connected ${label} phone was found.`
              : `More than one ${label} phone is connected. Choose the phone from the device list.`,
          );
        }
        return;
      }

      const deviceId = candidates[0].id;
      const selected = await selectPhoneDevice(deviceId);
      manualDisconnectRef.current = false;
      setAccess(selected);
      setLimitedDraft(selected.limitedCapabilities);
      setSelectedDeviceId(deviceId);
      setConnectPanel(null);
    } catch (error) {
      setAccessError(error instanceof Error ? error.message : String(error));
    } finally {
      connectionProbeRef.current = false;
      setChecking(false);
      setAccessBusy(false);
    }
  }

  async function pairWireless() {
    if (pairing || pairCode.length !== 6) return;

    pairingRef.current = true;
    discoveryGenerationRef.current += 1;
    setPairing(true);
    setPairError(null);
    try {
      const next = await pairPhoneWirelessly(pairCode);
      manualDisconnectRef.current = false;
      discoveryGenerationRef.current += 1;
      applyDiscovery(next);

      const pairedDeviceId = next.recommendedDeviceId
        ?? next.devices.find((device) => (
          device.state === "connected"
          && device.availableTransports.includes("wireless")
        ))?.id
        ?? null;
      if (pairedDeviceId) {
        const selected = await selectPhoneDevice(pairedDeviceId);
        setAccess(selected);
        setLimitedDraft(selected.limitedCapabilities);
        setSelectedDeviceId(pairedDeviceId);
      }

      setPairCode("");
      setConnectPanel(null);
    } catch (error) {
      setPairError(error instanceof Error ? error.message : String(error));
      setPairCode("");
    } finally {
      pairingRef.current = false;
      setPairing(false);
    }
  }

  async function updateAccess(
    mode: "off" | "limited" | "full",
    capabilities: PhoneCapability[] = [],
  ) {
    const deviceId = selectedDevice?.id ?? access.selectedDeviceId;
    if (!deviceId || accessBusy) return;
    setAccessBusy(true);
    setAccessError(null);
    try {
      if (access.selectedDeviceId !== deviceId) {
        const selected = await selectPhoneDevice(deviceId);
        setAccess(selected);
        setLimitedDraft(selected.limitedCapabilities);
      }

      const next = await setPhoneAccessMode(deviceId, mode, capabilities);
      setAccess(next);
      setSelectedDeviceId(deviceId);
      setLimitedDraft(next.limitedCapabilities);
      setConnectPanel(null);
    } catch (error) {
      setAccessError(error instanceof Error ? error.message : String(error));
    } finally {
      setAccessBusy(false);
    }
  }

  async function togglePhonePause() {
    if (selectedAccess.mode === "off" || accessBusy) return;
    setAccessBusy(true);
    setAccessError(null);
    try {
      const next = await setPhoneAccessPaused(!selectedAccess.paused);
      setAccess(next);
    } catch (error) {
      setAccessError(error instanceof Error ? error.message : String(error));
    } finally {
      setAccessBusy(false);
    }
  }

  function closeConnectPanel() {
    if (pairing || accessBusy) return;
    setConnectPanel(null);
    setPairCode("");
    setPairError(null);
    setAccessError(null);
  }

  function openAccessPanel() {
    if (!selectedDeviceId) return;
    setLimitedDraft(selectedAccess.limitedCapabilities);
    setAccessError(null);
    setConnectPanel("access");
  }

  function toggleCapability(capability: PhoneCapability) {
    setLimitedDraft((current) => current.includes(capability)
      ? current.filter((item) => item !== capability)
      : [...current, capability]);
  }

  async function disconnectSelectedPhone() {
    if (!selectedDeviceId || accessBusy) return;

    manualDisconnectRef.current = true;
    setAccessBusy(true);
    setAccessError(null);
    setControlError(null);
    try {
      const next = await selectPhoneDevice(null);
      setAccess(next);
      setLimitedDraft(next.limitedCapabilities);
      setSelectedDeviceId(null);
      setScreenFrame(null);
      setScreenError(null);
      setConnectPanel(null);
    } catch (error) {
      manualDisconnectRef.current = false;
      setAccessError(error instanceof Error ? error.message : String(error));
    } finally {
      setAccessBusy(false);
    }
  }

  function enqueueControl(action: () => Promise<void>) {
    setControlError(null);
    const next = controlQueueRef.current
      .catch(() => undefined)
      .then(action)
      .catch((error) => {
        setControlError(error instanceof Error ? error.message : String(error));
      });
    controlQueueRef.current = next;
  }

  function screenRatios(
    event: ReactPointerEvent<HTMLImageElement>,
  ): { xRatio: number; yRatio: number } | null {
    if (!screenFrame) return null;

    const rect = event.currentTarget.getBoundingClientRect();
    if (rect.width <= 0 || rect.height <= 0) return null;

    const sourceAspect = screenFrame.width / screenFrame.height;
    const boxAspect = rect.width / rect.height;

    let width = rect.width;
    let height = rect.height;
    let left = rect.left;
    let top = rect.top;

    if (boxAspect > sourceAspect) {
      width = height * sourceAspect;
      left += (rect.width - width) / 2;
    } else {
      height = width / sourceAspect;
      top += (rect.height - height) / 2;
    }

    const xRatio = (event.clientX - left) / width;
    const yRatio = (event.clientY - top) / height;
    if (xRatio < 0 || xRatio > 1 || yRatio < 0 || yRatio > 1) return null;

    return { xRatio, yRatio };
  }

  function handleScreenPointerDown(event: ReactPointerEvent<HTMLImageElement>) {
    if (!controlEnabled || !selectedDeviceId) return;
    if (event.pointerType === "mouse" && event.button !== 0) return;

    const ratios = screenRatios(event);
    if (!ratios) return;

    event.preventDefault();
    event.currentTarget.focus();
    event.currentTarget.setPointerCapture?.(event.pointerId);
    pointerStartRef.current = {
      pointerId: event.pointerId,
      clientX: event.clientX,
      clientY: event.clientY,
      xRatio: ratios.xRatio,
      yRatio: ratios.yRatio,
      startedAt: performance.now(),
    };
  }

  function handleScreenPointerUp(event: ReactPointerEvent<HTMLImageElement>) {
    const start = pointerStartRef.current;
    pointerStartRef.current = null;
    if (!controlEnabled || !selectedDeviceId || !start || start.pointerId !== event.pointerId) return;

    const end = screenRatios(event);
    if (!end) return;

    event.preventDefault();
    const distance = Math.hypot(event.clientX - start.clientX, event.clientY - start.clientY);
    if (distance < 7) {
      enqueueControl(() => phoneTap(selectedDeviceId, end.xRatio, end.yRatio));
      return;
    }

    const durationMs = Math.min(
      1_200,
      Math.max(50, Math.round(performance.now() - start.startedAt)),
    );
    enqueueControl(() => phoneSwipe(
      selectedDeviceId,
      start.xRatio,
      start.yRatio,
      end.xRatio,
      end.yRatio,
      durationMs,
    ));
  }

  function handleScreenPointerCancel() {
    pointerStartRef.current = null;
  }

  function handleScreenWheel(event: ReactWheelEvent<HTMLImageElement>) {
    if (!controlEnabled || !selectedDeviceId || Math.abs(event.deltaY) < 1) return;

    event.preventDefault();
    const now = performance.now();
    if (now - lastWheelAtRef.current < 160) return;
    lastWheelAtRef.current = now;

    const scrollingDown = event.deltaY > 0;
    enqueueControl(() => phoneSwipe(
      selectedDeviceId,
      0.5,
      scrollingDown ? 0.72 : 0.28,
      0.5,
      scrollingDown ? 0.28 : 0.72,
      160,
    ));
  }

  function handleScreenKeyDown(event: ReactKeyboardEvent<HTMLImageElement>) {
    if (!controlEnabled || !selectedDeviceId || event.nativeEvent.isComposing) return;

    const keyMap: Record<string, PhoneUiKey> = {
      Backspace: "delete",
      Enter: "enter",
      Tab: "tab",
      Escape: "back",
      ArrowUp: "dpadUp",
      ArrowDown: "dpadDown",
      ArrowLeft: "dpadLeft",
      ArrowRight: "dpadRight",
    };
    const mapped = keyMap[event.key];
    if (mapped) {
      event.preventDefault();
      enqueueControl(() => phoneKeyEvent(selectedDeviceId, mapped));
      return;
    }

    if (
      event.key.length === 1
      && !event.ctrlKey
      && !event.metaKey
      && !event.altKey
    ) {
      event.preventDefault();
      enqueueControl(() => phoneTypeText(selectedDeviceId, event.key));
    }
  }

  return (
    <section className="phone-panel" aria-labelledby="phone-workspace-title">
      <div className="phone-toolbar">
        <div className="phone-device-summary">
          <span className="phone-device-icon" aria-hidden="true">
            <NavIcon name="phone" size={16} />
          </span>
          <div>
            <strong id="phone-workspace-title">{title}</strong>
            <span>{subtitle}</span>
          </div>
          <span
            className={`phone-connection-dot ${connected ? "online" : selectedDevice ? "attention" : ""}`}
            aria-hidden="true"
          />
        </div>

        <div className="phone-toolbar-actions">
          {discovery.devices.length > 1 || (!selectedDeviceId && discovery.devices.length > 0) ? (
            <select
              className="phone-device-select"
              aria-label="Connected phone"
              value={selectedDeviceId ?? ""}
              disabled={accessBusy}
              onChange={(event) => void chooseDevice(event.target.value || null)}
            >
              <option value="">Choose phone</option>
              {discovery.devices.map((device) => (
                <option key={device.id} value={device.id}>
                  {device.name}
                </option>
              ))}
            </select>
          ) : null}

          {!connected ? (
            <>
              <button
                className="secondary-button phone-connect-button"
                type="button"
                disabled={!discovery.adbAvailable}
                onClick={() => {
                  setPairError(null);
                  setAccessError(null);
                  setConnectPanel("wireless");
                  void connectDiscoveredTransport("wireless", false);
                }}
              >
                Pair wirelessly
              </button>
              <button
                className="secondary-button phone-connect-button"
                type="button"
                disabled={!discovery.adbAvailable}
                onClick={() => {
                  setPairError(null);
                  setAccessError(null);
                  setConnectPanel("usb");
                  void connectDiscoveredTransport("usb", false);
                }}
              >
                Use USB
              </button>
            </>
          ) : (
            <button
              className="secondary-button phone-disconnect-button"
              type="button"
              disabled={accessBusy || pairing}
              onClick={() => void disconnectSelectedPhone()}
            >
              Disconnect
            </button>
          )}

          <button
            className={`phone-access-state ${selectedAccess.mode !== "off" ? "enabled" : ""} ${selectedAccess.paused ? "paused" : ""}`}
            type="button"
            disabled={!selectedDeviceId || accessBusy}
            onClick={openAccessPanel}
          >
            {accessLabel}{selectedAccess.paused ? " · Paused" : ""}
          </button>
          <button
            className="secondary-button phone-pause-button"
            type="button"
            disabled={selectedAccess.mode === "off" || accessBusy}
            onClick={() => void togglePhonePause()}
          >
            <NavIcon name={selectedAccess.paused ? "play" : "pause"} size={13} />
            {selectedAccess.paused ? "Resume AI" : "Pause AI"}
          </button>
        </div>
      </div>

      <div className="phone-stage">
        <div className="phone-preview-column">
          <div
            className={`phone-device-frame ${connected ? "connected" : ""}`}
            aria-label="Phone preview"
            style={screenFrame
              ? { aspectRatio: `${screenFrame.width} / ${screenFrame.height}` }
              : undefined}
          >
            {connected && screenFrame ? (
              <img
                className={`phone-live-screen ${controlEnabled ? "interactive" : ""}`}
                src={`data:${screenFrame.mimeType};base64,${screenFrame.dataBase64}`}
                alt={`${selectedDevice?.name ?? "Android phone"} live screen`}
                draggable={false}
                tabIndex={controlEnabled ? 0 : -1}
                onPointerDown={handleScreenPointerDown}
                onPointerUp={handleScreenPointerUp}
                onPointerCancel={handleScreenPointerCancel}
                onWheel={handleScreenWheel}
                onKeyDown={handleScreenKeyDown}
                onContextMenu={(event) => {
                  if (controlEnabled) event.preventDefault();
                }}
              />
            ) : (
              <div className="phone-device-empty">
                <NavIcon name="phone" size={34} />
                {selectedDevice ? (
                  <>
                    <strong>{selectedDevice.name}</strong>
                    <span>
                      {selectedDevice.state === "connected"
                        ? screenError ?? `Connected by ${transportLabel(selectedDevice)}. Starting live screen…`
                        : selectedDevice.state === "authorizationRequired"
                          ? "Approve the debugging authorization shown on your phone."
                          : selectedDevice.state === "offline"
                            ? "The phone is known but currently offline."
                            : "The phone is not currently available."}
                    </span>
                  </>
                ) : (
                  <>
                    <strong>{discovery.adbAvailable ? "Connect an Android phone" : "Phone connection needs setup"}</strong>
                    <span>
                      {discovery.message
                        ?? "RepoTunnel uses authorized USB when a cable is connected and otherwise reconnects paired wireless phones."}
                    </span>
                  </>
                )}
              </div>
            )}
          </div>

          {connected ? (
            <div className="phone-navigation-bar" aria-label="Phone navigation">
              <button
                type="button"
                aria-label="Back"
                disabled={!controlEnabled}
                onClick={() => {
                  if (selectedDeviceId) enqueueControl(() => phoneKeyEvent(selectedDeviceId, "back"));
                }}
              >
                <svg viewBox="0 0 24 24" aria-hidden="true">
                  <path d="M16.5 5.5 7.5 12l9 6.5Z" />
                </svg>
              </button>
              <button
                type="button"
                aria-label="Home"
                disabled={!controlEnabled}
                onClick={() => {
                  if (selectedDeviceId) enqueueControl(() => phoneKeyEvent(selectedDeviceId, "home"));
                }}
              >
                <svg viewBox="0 0 24 24" aria-hidden="true">
                  <circle cx="12" cy="12" r="6.2" />
                </svg>
              </button>
              <button
                type="button"
                aria-label="Recents"
                disabled={!controlEnabled}
                onClick={() => {
                  if (selectedDeviceId) enqueueControl(() => phoneKeyEvent(selectedDeviceId, "recents"));
                }}
              >
                <svg viewBox="0 0 24 24" aria-hidden="true">
                  <rect x="6.4" y="6.4" width="11.2" height="11.2" rx="1" />
                </svg>
              </button>
            </div>
          ) : null}
        </div>

        {controlError ? (
          <div className="phone-control-error" role="status">
            {controlError}
          </div>
        ) : null}

        {connectPanel ? (
          <aside className="phone-connect-panel" aria-label="Phone connection setup">
            <div className="phone-connect-panel-head">
              <div>
                <strong>
                  {connectPanel === "wireless"
                    ? "Pair wirelessly"
                    : connectPanel === "usb"
                      ? "Use USB"
                      : "Phone access"}
                </strong>
                <span>
                  {connectPanel === "wireless"
                    ? "One-time setup"
                    : connectPanel === "usb"
                      ? "Direct connection"
                      : selectedDevice?.name ?? "Selected phone"}
                </span>
              </div>
              <button
                type="button"
                className="phone-connect-close"
                aria-label="Close phone setup"
                disabled={pairing || accessBusy}
                onClick={closeConnectPanel}
              >
                ×
              </button>
            </div>

            {connectPanel === "wireless" ? (
              <>
                <ol className="phone-connect-steps">
                  <li>Open Developer options → Wireless debugging.</li>
                  <li>Choose Pair device with pairing code.</li>
                  <li>Enter the 6-digit code below.</li>
                </ol>
                <input
                  className="phone-pair-code"
                  aria-label="Wireless debugging pairing code"
                  inputMode="numeric"
                  autoComplete="one-time-code"
                  maxLength={6}
                  value={pairCode}
                  onChange={(event) => setPairCode(event.target.value.replace(/\D/g, "").slice(0, 6))}
                  placeholder="000000"
                />
                {pairError ? <p className="phone-connect-error">{pairError}</p> : null}
                {accessError ? <p className="phone-connect-error">{accessError}</p> : null}
                <button
                  className="secondary-button"
                  type="button"
                  disabled={checking || accessBusy || pairing}
                  onClick={() => void connectDiscoveredTransport("wireless")}
                >
                  {checking ? "Checking…" : "Reconnect paired phone"}
                </button>
                <button
                  className="primary-button"
                  type="button"
                  disabled={pairCode.length !== 6 || pairing || accessBusy}
                  onClick={() => void pairWireless()}
                >
                  {pairing ? "Pairing…" : "Pair phone"}
                </button>
                <span className="phone-connect-note">
                  RepoTunnel discovers the pairing address automatically. You do not need to enter an IP address or port.
                </span>
              </>
            ) : connectPanel === "usb" ? (
              <>
                <div className="phone-connect-usb-copy">
                  <strong>Connect the phone with a data-capable USB cable.</strong>
                  <span>Unlock the phone and approve the debugging prompt if Android shows one. RepoTunnel detects it automatically.</span>
                </div>
                {accessError ? <p className="phone-connect-error">{accessError}</p> : null}
                <button
                  className="primary-button"
                  type="button"
                  disabled={checking || accessBusy}
                  onClick={() => void connectDiscoveredTransport("usb")}
                >
                  {checking ? "Checking…" : "Check again"}
                </button>
              </>
            ) : (
              <>
                <button
                  className={`phone-access-choice ${selectedAccess.mode === "full" ? "selected" : ""}`}
                  type="button"
                  disabled={!connected || accessBusy}
                  onClick={() => void updateAccess("full")}
                >
                  <strong>Full Phone Access</strong>
                  <span>Use every phone capability available to RepoTunnel without repeated RepoTunnel prompts.</span>
                </button>

                <div className={`phone-limited-box ${selectedAccess.mode === "limited" ? "selected" : ""}`}>
                  <div>
                    <strong>Limited Access</strong>
                    <span>Choose exactly what AI can use.</span>
                  </div>
                  <div className="phone-capability-list">
                    {CAPABILITY_OPTIONS.map((capability) => (
                      <label key={capability.id}>
                        <input
                          type="checkbox"
                          checked={limitedDraft.includes(capability.id)}
                          disabled={!connected || accessBusy}
                          onChange={() => toggleCapability(capability.id)}
                        />
                        <span>{capability.label}</span>
                      </label>
                    ))}
                  </div>
                  <button
                    className="primary-button"
                    type="button"
                    disabled={!connected || accessBusy || limitedDraft.length === 0}
                    onClick={() => void updateAccess("limited", limitedDraft)}
                  >
                    Enable Limited Access
                  </button>
                </div>

                {accessError ? <p className="phone-connect-error">{accessError}</p> : null}

                <button
                  className="secondary-button phone-access-off"
                  type="button"
                  disabled={selectedAccess.mode === "off" || accessBusy}
                  onClick={() => void updateAccess("off")}
                >
                  Turn off phone access
                </button>
              </>
            )}
          </aside>
        ) : null}
      </div>
    </section>
  );
}

export default PhonePanel;
