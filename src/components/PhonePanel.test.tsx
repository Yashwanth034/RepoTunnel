// @vitest-environment jsdom

import React, { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const backend = vi.hoisted(() => ({
  clearPhoneRuntime: vi.fn(),
  ensurePhoneRuntime: vi.fn(),
  getPhoneAccessStatus: vi.fn(),
  getPhoneDiscovery: vi.fn(),
  getPhoneScreenFrame: vi.fn(),
  pairPhoneWirelessly: vi.fn(),
  phoneKeyEvent: vi.fn(),
  phoneSwipe: vi.fn(),
  phoneTap: vi.fn(),
  phoneTypeText: vi.fn(),
  selectPhoneDevice: vi.fn(),
  setPhoneAccessMode: vi.fn(),
  setPhoneAccessPaused: vi.fn(),
}));

vi.mock("../lib/backend", () => backend);

import PhonePanel from "./PhonePanel";

(globalThis as typeof globalThis & { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

let root: Root | null = null;
let host: HTMLDivElement | null = null;

async function settle() {
  await act(async () => {
    await new Promise((resolve) => window.setTimeout(resolve, 0));
  });
}

beforeEach(() => {
  vi.clearAllMocks();
  backend.clearPhoneRuntime.mockResolvedValue({
    active: false,
    deviceId: null,
    name: null,
    transport: null,
    sessionStartedAt: null,
    lastUsedAt: null,
  });
  backend.ensurePhoneRuntime.mockResolvedValue({
    active: true,
    deviceId: "phone-test",
    name: "Google Pixel 8 Pro",
    transport: "wireless",
    sessionStartedAt: 1,
    lastUsedAt: 1,
  });
  backend.getPhoneAccessStatus.mockResolvedValue({
    selectedDeviceId: null,
    mode: "off",
    paused: false,
    limitedCapabilities: [],
    grantedCapabilities: [],
  });
  backend.getPhoneDiscovery.mockResolvedValue({
    adbAvailable: true,
    devices: [],
    recommendedDeviceId: null,
    message: "No Android phone is currently connected.",
  });
  backend.getPhoneScreenFrame.mockResolvedValue({
    mimeType: "image/png",
    dataBase64: "iVBORw0KGgo=",
    sizeBytes: 8,
    width: 1080,
    height: 2400,
    capturedAt: 1,
  });
  backend.selectPhoneDevice.mockImplementation(async (deviceId: string | null) => ({
    selectedDeviceId: deviceId,
    mode: "off",
    paused: false,
    limitedCapabilities: [],
    grantedCapabilities: [],
  }));
  backend.setPhoneAccessMode.mockImplementation(
    async (deviceId: string, mode: "off" | "limited" | "full", limitedCapabilities: string[]) => ({
      selectedDeviceId: deviceId,
      mode,
      paused: false,
      limitedCapabilities: mode === "limited" ? limitedCapabilities : [],
      grantedCapabilities: mode === "full" ? [
        "viewScreen",
        "controlInput",
        "appControl",
        "files",
        "appInstall",
        "deviceSettings",
        "shell",
        "logs",
        "networkTools",
      ] : mode === "limited" ? limitedCapabilities : [],
    }),
  );
  backend.setPhoneAccessPaused.mockResolvedValue({
    selectedDeviceId: "phone-test",
    mode: "full",
    paused: true,
    limitedCapabilities: [],
    grantedCapabilities: [],
  });
  backend.phoneKeyEvent.mockResolvedValue(undefined);
  backend.phoneSwipe.mockResolvedValue(undefined);
  backend.phoneTap.mockResolvedValue(undefined);
  backend.phoneTypeText.mockResolvedValue(undefined);
  backend.pairPhoneWirelessly.mockResolvedValue({
    adbAvailable: true,
    devices: [
      {
        id: "phone-paired",
        name: "Google Pixel 8 Pro",
        manufacturer: "Google",
        androidVersion: "16",
        state: "connected",
        transport: "wireless",
        availableTransports: ["wireless"],
      },
    ],
    recommendedDeviceId: "phone-paired",
    message: null,
  });
});

afterEach(async () => {
  if (root) await act(async () => root?.unmount());
  root = null;
  host?.remove();
  host = null;
});

async function renderPanel() {
  host = document.createElement("div");
  document.body.append(host);
  root = createRoot(host);
  await act(async () => {
    root?.render(<PhonePanel />);
  });
  await settle();
  await settle();
}

function mockConnectedPhone(deviceId = "phone-test") {
  backend.getPhoneDiscovery.mockResolvedValue({
    adbAvailable: true,
    devices: [
      {
        id: deviceId,
        name: "Google Pixel 8 Pro",
        manufacturer: "Google",
        androidVersion: "16",
        state: "connected",
        transport: "usb",
        availableTransports: ["usb", "wireless"],
      },
    ],
    recommendedDeviceId: deviceId,
    message: null,
  });
}

function mockFullAccessPhone(deviceId = "phone-test") {
  mockConnectedPhone(deviceId);
  backend.getPhoneAccessStatus.mockResolvedValue({
    selectedDeviceId: deviceId,
    mode: "full",
    paused: false,
    limitedCapabilities: [],
    grantedCapabilities: [
      "viewScreen",
      "controlInput",
      "appControl",
      "files",
      "appInstall",
      "deviceSettings",
      "shell",
      "logs",
      "networkTools",
    ],
  });
}

function setPhoneScreenRect(image: HTMLImageElement) {
  vi.spyOn(image, "getBoundingClientRect").mockReturnValue({
    x: 0,
    y: 0,
    left: 0,
    top: 0,
    right: 340,
    bottom: 755.5555555556,
    width: 340,
    height: 755.5555555556,
    toJSON: () => ({}),
  } as DOMRect);
}

function dispatchPointer(
  target: HTMLElement,
  type: "pointerdown" | "pointerup",
  clientX: number,
  clientY: number,
) {
  const event = new MouseEvent(type, {
    bubbles: true,
    button: 0,
    clientX,
    clientY,
  });
  Object.defineProperties(event, {
    pointerId: { value: 1 },
    pointerType: { value: "mouse" },
  });
  target.dispatchEvent(event);
}

describe("PhonePanel discovery", () => {
  it("keeps the disconnected phone workspace minimal and non-technical", async () => {
    await renderPanel();

    const text = host?.textContent ?? "";
    expect(text).toContain("No phone connected");
    expect(text).toContain("USB direct");
    expect(text).toContain("Wireless available");
    expect(text).toContain("Access Off");
    expect(text).toContain("Connect an Android phone");
    expect(text).not.toContain("adb");
    expect(text).not.toContain("5037");
  });

  it("pairs wirelessly with only the six-digit Android pairing code", async () => {
    await renderPanel();

    const pairButton = Array.from(host?.querySelectorAll("button") ?? [])
      .find((button) => button.textContent?.includes("Pair wirelessly")) as HTMLButtonElement;

    await act(async () => pairButton.click());

    const input = host?.querySelector(
      'input[aria-label="Wireless debugging pairing code"]',
    ) as HTMLInputElement;
    expect(input).toBeTruthy();
    expect(host?.querySelector('input[aria-label*="IP"]')).toBeNull();
    expect(host?.querySelector('input[aria-label*="port"]')).toBeNull();

    await act(async () => {
      const setter = Object.getOwnPropertyDescriptor(
        HTMLInputElement.prototype,
        "value",
      )?.set;
      setter?.call(input, "123456");
      input.dispatchEvent(new Event("input", { bubbles: true }));
    });

    const submit = Array.from(host?.querySelectorAll("button") ?? [])
      .find((button) => button.textContent?.includes("Pair phone")) as HTMLButtonElement;

    await act(async () => submit.click());
    await settle();

    expect(backend.pairPhoneWirelessly).toHaveBeenCalledTimes(1);
    expect(backend.pairPhoneWirelessly).toHaveBeenCalledWith("123456");
    expect(host?.textContent).toContain("Google Pixel 8 Pro");
    expect(host?.querySelector(".phone-connect-panel")).toBeNull();
  });

  it("keeps pairing open and clears an expired code when connection verification fails", async () => {
    backend.pairPhoneWirelessly.mockRejectedValueOnce(
      new Error("Android accepted the pairing, but RepoTunnel could not verify a wireless connection within 15 seconds."),
    );

    await renderPanel();

    const pairButton = Array.from(host?.querySelectorAll("button") ?? [])
      .find((button) => button.textContent?.includes("Pair wirelessly")) as HTMLButtonElement;
    await act(async () => pairButton.click());

    const input = host?.querySelector(
      'input[aria-label="Wireless debugging pairing code"]',
    ) as HTMLInputElement;

    await act(async () => {
      const setter = Object.getOwnPropertyDescriptor(
        HTMLInputElement.prototype,
        "value",
      )?.set;
      setter?.call(input, "123456");
      input.dispatchEvent(new Event("input", { bubbles: true }));
    });

    const submit = Array.from(host?.querySelectorAll("button") ?? [])
      .find((button) => button.textContent?.includes("Pair phone")) as HTMLButtonElement;
    await act(async () => submit.click());
    await settle();

    expect(host?.querySelector(".phone-connect-panel")).toBeTruthy();
    expect((host?.querySelector(
      'input[aria-label="Wireless debugging pairing code"]',
    ) as HTMLInputElement).value).toBe("");
    expect(host?.textContent).toContain("could not verify a wireless connection");
  });

  it("shows a discovered phone and its preferred transport", async () => {
    mockConnectedPhone();

    await renderPanel();

    const text = host?.textContent ?? "";
    expect(text).toContain("Google Pixel 8 Pro");
    expect(text).toContain("Android 16");
    expect(text).toContain("USB");
    expect(backend.ensurePhoneRuntime).toHaveBeenCalledWith("phone-test", "usb");
    expect(host?.querySelector(".phone-connection-dot.online")).toBeTruthy();
  });

  it("shows the live phone frame even while AI access is Off", async () => {
    mockConnectedPhone();
    await renderPanel();
    await settle();

    const image = host?.querySelector(".phone-live-screen") as HTMLImageElement;
    expect(image).toBeTruthy();
    expect(image.getAttribute("src")).toBe("data:image/png;base64,iVBORw0KGgo=");
    expect(host?.textContent).toContain("Access Off");
    expect(backend.getPhoneScreenFrame).toHaveBeenCalledWith("phone-test", undefined);

    await act(async () => {
      await new Promise((resolve) => window.setTimeout(resolve, 20));
    });
    expect(backend.getPhoneScreenFrame).toHaveBeenCalledWith("phone-test", 1);
  });

  it("keeps the persistent runtime intact when discovery temporarily reports the phone offline", async () => {
    backend.getPhoneDiscovery.mockResolvedValue({
      adbAvailable: true,
      devices: [
        {
          id: "phone-test",
          name: "RMX2156",
          manufacturer: "realme",
          androidVersion: "12",
          state: "offline",
          transport: "usb",
          availableTransports: ["usb"],
        },
      ],
      recommendedDeviceId: null,
      message: null,
    });

    await renderPanel();

    expect(backend.clearPhoneRuntime).not.toHaveBeenCalled();
    expect(backend.ensurePhoneRuntime).not.toHaveBeenCalled();
    expect(host?.textContent).toContain("Phone offline");
  });

  it("shows the actual live-screen backend error instead of hiding it", async () => {
    mockConnectedPhone();
    backend.getPhoneScreenFrame.mockRejectedValueOnce(
      new Error("Phone screen capture did not return a valid PNG frame."),
    );

    await renderPanel();
    await settle();

    expect(host?.textContent).toContain(
      "Phone screen capture did not return a valid PNG frame.",
    );
  });

  it("enables Full Access for the explicitly selected phone", async () => {
    mockConnectedPhone();
    await renderPanel();

    const accessButton = Array.from(host?.querySelectorAll("button") ?? [])
      .find((button) => button.textContent?.includes("Access Off")) as HTMLButtonElement;
    await act(async () => accessButton.click());

    const fullButton = Array.from(host?.querySelectorAll("button") ?? [])
      .find((button) => button.textContent?.includes("Full Phone Access")) as HTMLButtonElement;
    await act(async () => fullButton.click());
    await settle();

    expect(backend.setPhoneAccessMode).toHaveBeenCalledWith("phone-test", "full", []);
    expect(host?.textContent).toContain("Full Access");
  });

  it("sends only checked capabilities when enabling Limited Access", async () => {
    mockConnectedPhone();
    await renderPanel();

    const accessButton = Array.from(host?.querySelectorAll("button") ?? [])
      .find((button) => button.textContent?.includes("Access Off")) as HTMLButtonElement;
    await act(async () => accessButton.click());

    const labels = Array.from(host?.querySelectorAll(".phone-capability-list label") ?? []);
    const viewLabel = labels.find((label) => label.textContent?.includes("View screen"));
    const filesLabel = labels.find((label) => label.textContent?.includes("Files"));
    const viewInput = viewLabel?.querySelector("input") as HTMLInputElement;
    const filesInput = filesLabel?.querySelector("input") as HTMLInputElement;

    await act(async () => {
      viewInput.click();
      filesInput.click();
    });

    const limitedButton = Array.from(host?.querySelectorAll("button") ?? [])
      .find((button) => button.textContent?.includes("Enable Limited Access")) as HTMLButtonElement;
    await act(async () => limitedButton.click());
    await settle();

    expect(backend.setPhoneAccessMode).toHaveBeenCalledWith(
      "phone-test",
      "limited",
      ["viewScreen", "files"],
    );
    expect(host?.textContent).toContain("Limited Access");
  });

  it("does not show another phone as inheriting saved Full Access", async () => {
    backend.getPhoneAccessStatus.mockResolvedValue({
      selectedDeviceId: "phone-test",
      mode: "full",
      paused: false,
      limitedCapabilities: [],
      grantedCapabilities: ["viewScreen", "controlInput", "shell"],
    });
    backend.getPhoneDiscovery.mockResolvedValue({
      adbAvailable: true,
      devices: [
        {
          id: "phone-other",
          name: "Samsung Galaxy",
          manufacturer: "Samsung",
          androidVersion: "16",
          state: "connected",
          transport: "usb",
          availableTransports: ["usb"],
        },
      ],
      recommendedDeviceId: "phone-other",
      message: null,
    });

    await renderPanel();

    expect(host?.textContent).toContain("Samsung Galaxy");
    expect(host?.textContent).toContain("Access Off");
    expect(host?.textContent).not.toContain("Full Access");
  });

  it("explicitly selects a corrected phone identity before granting Full Access", async () => {
    backend.getPhoneAccessStatus.mockResolvedValue({
      selectedDeviceId: "phone-old",
      mode: "full",
      paused: false,
      limitedCapabilities: [],
      grantedCapabilities: ["viewScreen", "controlInput"],
    });
    backend.getPhoneDiscovery.mockResolvedValue({
      adbAvailable: true,
      devices: [
        {
          id: "phone-new",
          name: "RMX2156",
          manufacturer: "realme",
          androidVersion: "12",
          state: "connected",
          transport: "usb",
          availableTransports: ["usb"],
        },
      ],
      recommendedDeviceId: "phone-new",
      message: null,
    });
    backend.selectPhoneDevice.mockResolvedValueOnce({
      selectedDeviceId: "phone-new",
      mode: "off",
      paused: false,
      limitedCapabilities: [],
      grantedCapabilities: [],
    });
    backend.setPhoneAccessMode.mockResolvedValueOnce({
      selectedDeviceId: "phone-new",
      mode: "full",
      paused: false,
      limitedCapabilities: [],
      grantedCapabilities: [
        "viewScreen",
        "controlInput",
        "appControl",
        "files",
        "appInstall",
        "deviceSettings",
        "shell",
        "logs",
        "networkTools",
      ],
    });

    await renderPanel();

    const accessButton = Array.from(host?.querySelectorAll("button") ?? [])
      .find((button) => button.textContent?.includes("Access Off")) as HTMLButtonElement;
    await act(async () => accessButton.click());

    const fullButton = Array.from(host?.querySelectorAll("button") ?? [])
      .find((button) => button.textContent?.includes("Full Phone Access")) as HTMLButtonElement;
    await act(async () => fullButton.click());
    await settle();

    expect(backend.selectPhoneDevice).toHaveBeenCalledWith("phone-new");
    expect(backend.setPhoneAccessMode).toHaveBeenCalledWith("phone-new", "full", []);
    expect(host?.textContent).toContain("Full Access");
  });

  it("turns access off without changing phones", async () => {
    mockConnectedPhone();
    backend.getPhoneAccessStatus.mockResolvedValue({
      selectedDeviceId: "phone-test",
      mode: "full",
      paused: false,
      limitedCapabilities: [],
      grantedCapabilities: ["viewScreen", "controlInput"],
    });

    await renderPanel();

    const accessButton = Array.from(host?.querySelectorAll("button") ?? [])
      .find((button) => button.textContent?.includes("Full Access")) as HTMLButtonElement;
    await act(async () => accessButton.click());

    const offButton = Array.from(host?.querySelectorAll("button") ?? [])
      .find((button) => button.textContent?.includes("Turn off phone access")) as HTMLButtonElement;
    await act(async () => offButton.click());
    await settle();

    expect(backend.setPhoneAccessMode).toHaveBeenCalledWith("phone-test", "off", []);
    expect(host?.textContent).toContain("Access Off");
  });

  it("pauses and resumes Full Access without forgetting the selected phone", async () => {
    mockConnectedPhone();
    backend.getPhoneAccessStatus.mockResolvedValue({
      selectedDeviceId: "phone-test",
      mode: "full",
      paused: false,
      limitedCapabilities: [],
      grantedCapabilities: ["viewScreen", "controlInput"],
    });
    backend.setPhoneAccessPaused
      .mockResolvedValueOnce({
        selectedDeviceId: "phone-test",
        mode: "full",
        paused: true,
        limitedCapabilities: [],
        grantedCapabilities: [],
      })
      .mockResolvedValueOnce({
        selectedDeviceId: "phone-test",
        mode: "full",
        paused: false,
        limitedCapabilities: [],
        grantedCapabilities: ["viewScreen", "controlInput"],
      });

    await renderPanel();

    const pauseButton = Array.from(host?.querySelectorAll("button") ?? [])
      .find((button) => button.textContent?.includes("Pause AI")) as HTMLButtonElement;
    await act(async () => pauseButton.click());
    await settle();

    expect(backend.setPhoneAccessPaused).toHaveBeenNthCalledWith(1, true);
    const resumeButton = Array.from(host?.querySelectorAll("button") ?? [])
      .find((button) => button.textContent?.includes("Resume AI")) as HTMLButtonElement;
    await act(async () => resumeButton.click());
    await settle();

    expect(backend.setPhoneAccessPaused).toHaveBeenNthCalledWith(2, false);
    expect(host?.textContent).toContain("Full Access");
  });

  it("disconnects the selected phone without auto-selecting it again", async () => {
    mockFullAccessPhone();
    await renderPanel();

    const disconnect = Array.from(host?.querySelectorAll("button") ?? [])
      .find((button) => button.textContent?.includes("Disconnect")) as HTMLButtonElement;
    await act(async () => disconnect.click());
    await settle();

    expect(backend.selectPhoneDevice).toHaveBeenCalledWith(null);
    expect(host?.textContent).toContain("No phone connected");
    expect(host?.querySelector('select[aria-label="Connected phone"]')).toBeTruthy();

    const usbButton = Array.from(host?.querySelectorAll("button") ?? [])
      .find((button) => button.textContent?.includes("Use USB")) as HTMLButtonElement;
    await act(async () => usbButton.click());
    await settle();

    expect(backend.selectPhoneDevice).toHaveBeenCalledWith("phone-test");
    expect(host?.textContent).toContain("Google Pixel 8 Pro");
  });

  it("reconnects an already paired wireless phone after manual disconnect", async () => {
    mockFullAccessPhone();
    await renderPanel();

    const disconnect = Array.from(host?.querySelectorAll("button") ?? [])
      .find((button) => button.textContent?.includes("Disconnect")) as HTMLButtonElement;
    await act(async () => disconnect.click());
    await settle();

    const wirelessButton = Array.from(host?.querySelectorAll("button") ?? [])
      .find((button) => button.textContent?.includes("Pair wirelessly")) as HTMLButtonElement;
    await act(async () => wirelessButton.click());
    await settle();

    expect(backend.selectPhoneDevice).toHaveBeenCalledWith("phone-test");
    expect(host?.textContent).toContain("Google Pixel 8 Pro");
  });

  it("maps a click on the live screen to a phone tap", async () => {
    mockFullAccessPhone();
    await renderPanel();
    await settle();

    const image = host?.querySelector(".phone-live-screen") as HTMLImageElement;
    setPhoneScreenRect(image);

    await act(async () => {
      dispatchPointer(image, "pointerdown", 170, 377.7777777778);
      dispatchPointer(image, "pointerup", 170, 377.7777777778);
    });
    await settle();

    expect(backend.phoneTap).toHaveBeenCalledTimes(1);
    const [, xRatio, yRatio] = backend.phoneTap.mock.calls[0];
    expect(xRatio).toBeCloseTo(0.5, 3);
    expect(yRatio).toBeCloseTo(0.5, 3);
  });

  it("maps a drag on the live screen to a phone swipe", async () => {
    mockFullAccessPhone();
    await renderPanel();
    await settle();

    const image = host?.querySelector(".phone-live-screen") as HTMLImageElement;
    setPhoneScreenRect(image);

    await act(async () => {
      dispatchPointer(image, "pointerdown", 170, 600);
      dispatchPointer(image, "pointerup", 170, 200);
    });
    await settle();

    expect(backend.phoneSwipe).toHaveBeenCalledTimes(1);
    const [deviceId, startX, startY, endX, endY, durationMs] = backend.phoneSwipe.mock.calls[0];
    expect(deviceId).toBe("phone-test");
    expect(startX).toBeCloseTo(0.5, 3);
    expect(endX).toBeCloseTo(0.5, 3);
    expect(startY).toBeGreaterThan(endY);
    expect(durationMs).toBeGreaterThanOrEqual(50);
  });

  it("forwards keyboard text and navigation when phone control is enabled", async () => {
    mockFullAccessPhone();
    await renderPanel();
    await settle();

    const image = host?.querySelector(".phone-live-screen") as HTMLImageElement;

    await act(async () => {
      image.dispatchEvent(new KeyboardEvent("keydown", { key: "a", bubbles: true }));
      image.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true }));
    });
    await settle();
    await settle();

    expect(backend.phoneTypeText).toHaveBeenCalledWith("phone-test", "a");
    expect(backend.phoneKeyEvent).toHaveBeenCalledWith("phone-test", "enter");
  });

  it("uses the real frame aspect ratio without the persistent control tooltip", async () => {
    mockFullAccessPhone();
    await renderPanel();
    await settle();

    const image = host?.querySelector(".phone-live-screen") as HTMLImageElement;
    const frame = host?.querySelector(".phone-device-frame") as HTMLDivElement;

    expect(image.getAttribute("title")).toBeNull();
    expect(frame.style.aspectRatio).toBe("1080 / 2400");
  });

  it("provides Android-style Back Home and Recents navigation controls", async () => {
    mockFullAccessPhone();
    await renderPanel();
    await settle();

    const back = host?.querySelector('button[aria-label="Back"]') as HTMLButtonElement;
    const home = host?.querySelector('button[aria-label="Home"]') as HTMLButtonElement;
    const recents = host?.querySelector('button[aria-label="Recents"]') as HTMLButtonElement;

    expect(back).toBeTruthy();
    expect(home).toBeTruthy();
    expect(recents).toBeTruthy();

    await act(async () => back.click());
    await settle();
    await act(async () => home.click());
    await settle();
    await act(async () => recents.click());
    await settle();
    await settle();

    expect(backend.phoneKeyEvent).toHaveBeenCalledWith("phone-test", "back");
    expect(backend.phoneKeyEvent).toHaveBeenCalledWith("phone-test", "home");
    expect(backend.phoneKeyEvent).toHaveBeenCalledWith("phone-test", "recents");
  });

  it("maps mouse-wheel scrolling to a bounded vertical phone swipe", async () => {
    mockFullAccessPhone();
    await renderPanel();
    await settle();

    const image = host?.querySelector(".phone-live-screen") as HTMLImageElement;
    await act(async () => {
      image.dispatchEvent(new WheelEvent("wheel", {
        bubbles: true,
        cancelable: true,
        deltaY: 120,
      }));
    });
    await settle();
    await settle();

    expect(backend.phoneSwipe).toHaveBeenCalledWith(
      "phone-test",
      0.5,
      0.72,
      0.5,
      0.28,
      160,
    );
  });
});
