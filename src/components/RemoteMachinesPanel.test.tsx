// @vitest-environment jsdom
import React, { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
const bridge = vi.hoisted(() => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/core", () => bridge);
import RemoteMachinesPanel from "./RemoteMachinesPanel";

(globalThis as typeof globalThis & { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

let host: HTMLDivElement | null = null;
let root: Root | null = null;
const nodes = [{
  id: "node-test-1", label: "Windows Test", endpoint: "https://win.example/mcp",
  connectedAt: 1, connectionNonce:"node-0123456789abcdef01234567", availableTools: ["browser_action", "ai_workspace_session", "start_process"],
  remoteWorkspaceId: "approved-project",
  identityFingerprint:"d".repeat(64),
  ownerClaimed: false,
}];

async function display() {
  host = document.createElement("div");
  document.body.append(host);
  root = createRoot(host);
  await act(async () => { root?.render(<RemoteMachinesPanel />); });
}
beforeEach(() => {
  vi.clearAllMocks();
  bridge.invoke.mockImplementation(async (command: string) => {
    if (command === "list_remote_machines") return nodes;
    if (command === "get_local_remote_machine_identity") return "a".repeat(64);
    if (command === "list_workspaces") return [{ id: "approved-project", name: "Approved Project" }];
    if (command === "list_downloaded_remote_artifacts") return [];
    if (command === "list_remote_native_test_runs_desktop") return [];
    if (command === "list_remote_machine_access_grants") return [];
    if (command === "probe_remote_machine") return {
      nodeId: "node-test-1", checkedAt: 1000000,
      advertisedToolCount: 3, capabilitiesChanged: false,
      note: "Native GUI, browser and SDK readiness have not been verified.",
    };
    if (command === "issue_remote_machine_access") return {
      secret: "a".repeat(64), expiresAt: 1000000, workspaceId: "approved-project", warning: "Treat as credential",
    };
    return undefined;
  });
});
afterEach(async () => {
  if (root) await act(async () => root?.unmount());
  root = null; host?.remove(); host = null;
});
describe("Remote Computers dashboard navigation", () => {
  it("defaults to a compact overview with summary counts and two setup cards", async () => {
    bridge.invoke.mockImplementation(async (command: string) => {
      if (command === "list_remote_machines") return [];
      if (command === "get_local_remote_machine_identity") return "a".repeat(64);
      if (command === "list_workspaces") return [{ id: "approved-project", name: "Approved Project" }];
      if (command === "list_downloaded_remote_artifacts") return [];
      if (command === "list_remote_native_test_runs_desktop") return [];
      if (command === "list_remote_machine_access_grants") return [];
      return undefined;
    });
    await display();
    expect(host?.querySelector(".remote-fleet-page")?.getAttribute("data-view")).toBe("overview");
    expect(host?.querySelectorAll(".remote-fleet-stat").length).toBe(4);
    expect(host?.querySelector(".remote-fleet-connect")).toBeTruthy();
    expect(host?.querySelector(".remote-fleet-grants")).toBeTruthy();
    expect(host?.textContent).toContain("No computers connected yet");
    expect(host?.querySelector(".remote-fleet-source")).toBeTruthy();
    expect(bridge.invoke).not.toHaveBeenCalledWith("issue_remote_machine_access", expect.anything());
    expect(bridge.invoke).not.toHaveBeenCalledWith("connect_remote_machine", expect.anything());
  });

  it("keeps each advanced workflow reachable without auto-issuing credentials", async () => {
    await display();
    const tab = (label: string) => [...(host?.querySelectorAll(".remote-fleet-tab") ?? [])]
      .find((button) => button.textContent?.trim() === label) as HTMLButtonElement;
    for (const [label, expected] of [
      ["Connect", "connect"], ["Credentials", "credentials"], ["Connected machines", "machines"],
      ["Activity & artifacts", "activity"], ["Source approvals", "source"], ["Overview", "overview"],
    ]) {
      await act(async () => { tab(label).click(); });
      expect(host?.querySelector(".remote-fleet-page")?.getAttribute("data-view")).toBe(expected);
      expect(tab(label).getAttribute("aria-current")).toBe("page");
    }
    expect(bridge.invoke).not.toHaveBeenCalledWith("issue_remote_machine_access", expect.anything());
    expect(bridge.invoke).not.toHaveBeenCalledWith("revoke_remote_machine_access", expect.anything());
  });

  it("navigates to connection setup from the no-computers empty state", async () => {
    bridge.invoke.mockImplementation(async (command: string) => {
      if (command === "list_remote_machines") return [];
      if (command === "list_workspaces") return [];
      if (command === "list_downloaded_remote_artifacts") return [];
      if (command === "list_remote_native_test_runs_desktop") return [];
      if (command === "list_remote_machine_access_grants") return [];
      if (command === "get_local_remote_machine_identity") return "a".repeat(64);
      return undefined;
    });
    await display();
    const action = [...(host?.querySelectorAll("button") ?? [])]
      .find((button) => button.textContent?.trim() === "Connect a computer");
    expect(action).toBeTruthy();
    await act(async () => { action?.click(); });
    expect(host?.querySelector(".remote-fleet-page")?.getAttribute("data-view")).toBe("connect");
    expect(bridge.invoke).not.toHaveBeenCalledWith("connect_remote_machine", expect.anything());
  });
});

describe("Remote Computers controls", () => {
  it("rejects an incoming source proposal addressed to a different receiver installation before local authorization", async () => {
    const original=bridge.invoke.getMockImplementation();
    const grant={fingerprint:"d".repeat(64),expiresAt:4102444800,
      workspaceId:"approved-project",scopes:["source"]};
    bridge.invoke.mockImplementation(async(command:string,args?:Record<string,unknown>)=>{
      if(command==="list_remote_machine_access_grants") return [grant];
      if(command==="list_incoming_source_approvals_desktop") return [];
      return original?.(command,args);
    });
    await display();
    const mismatched={
      schemaVersion:1,kind:"RepoTunnelIncomingSourceReviewV1",
      remoteWorkspaceId:"approved-project",receiverMachineFingerprint:"f".repeat(64),
      coordinatorMachineFingerprint:"b".repeat(64),coordinatorWorkspaceId:"sender",
      coordinatorConnectionNonce:"node-0123456789abcdef01234567",
      coordinatorSourceFingerprint:"c".repeat(64),
      sourceReviewId:"source-review-"+"1".repeat(32),
      files:[{path:"src/new.rs",sha256:"a".repeat(64),bytes:1}],
    };
    const textarea=host?.querySelector<HTMLTextAreaElement>("#receiver-review-draft");
    const select=host?.querySelector<HTMLSelectElement>("#receiver-grant-choice");
    await act(async()=>{
      if(textarea){
        Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype,"value")?.set?.call(
          textarea,JSON.stringify(mismatched));
        textarea.dispatchEvent(new Event("input",{bubbles:true}));
      }
      if(select){select.value=grant.fingerprint;select.dispatchEvent(new Event("change",{bubbles:true}));}
    });
    const approval=Array.from(host?.querySelectorAll("button")??[])
      .find(b=>b.textContent?.trim()==="Confirm independent receiver create-only approval (no import)");
    await act(async()=>{approval?.click();});
    expect(bridge.invoke).not.toHaveBeenCalledWith("approve_incoming_source_review_desktop",expect.anything());
    expect(host?.textContent).toContain("Incoming source review proposal invalid");
    expect(bridge.invoke).not.toHaveBeenCalledWith("remote_machine_call",expect.anything());
  });
  it("receiver independently confirms exact source files and recovers a lost local approval ACK without remote import", async () => {
    const base=bridge.invoke.getMockImplementation();
    const confirm=vi.spyOn(window,"confirm").mockReturnValue(false);
    const approved: Array<Record<string,unknown>>=[];
    let writes=0;
    const proposal={
      schemaVersion:1,kind:"RepoTunnelIncomingSourceReviewV1",
      remoteWorkspaceId:"approved-project",
      receiverMachineFingerprint:"a".repeat(64),
      coordinatorMachineFingerprint:"a".repeat(64),
      coordinatorWorkspaceId:"origin-project",
      coordinatorConnectionNonce:"node-0123456789abcdef01234567",
      coordinatorSourceFingerprint:"b".repeat(64),
      sourceReviewId:"source-review-"+"1".repeat(32),
      files:[{path:"src/newfile.rs",sha256:"c".repeat(64),bytes:100}],
      identityPublicKey:"A".repeat(43)+"=",
      identitySignature:"A".repeat(86)+"==",
    };
    const grant={
      fingerprint:"d".repeat(64),expiresAt:4102444800,
      workspaceId:"approved-project",scopes:["source","files"],
    };
    bridge.invoke.mockImplementation(async(command:string,args?:Record<string,unknown>)=>{
      if(command==="list_remote_machine_access_grants") return [grant];
      if(command==="list_incoming_source_approvals_desktop") return approved;
      if(command==="approve_incoming_source_review_desktop"){
        writes++;
        expect(args?.request).toEqual({
          workspaceId:"approved-project",credentialFingerprint:grant.fingerprint,
          coordinatorMachineFingerprint:proposal.coordinatorMachineFingerprint,
          coordinatorWorkspaceId:proposal.coordinatorWorkspaceId,
          coordinatorConnectionNonce:proposal.coordinatorConnectionNonce,
          coordinatorSourceFingerprint:proposal.coordinatorSourceFingerprint,
          sourceReviewId:proposal.sourceReviewId,files:proposal.files,
          receiverMachineFingerprint:proposal.receiverMachineFingerprint,
          identityPublicKey:proposal.identityPublicKey,
          identitySignature:proposal.identitySignature,
        });
        approved.push({
          ...(args?.request as Record<string,unknown>),
          id:"receiver-source-"+"f".repeat(32),state:"approved",createdAt:123,
        });
        throw new Error("receiver-local acknowledgment dropped");
      }
      return base?.(command,args);
    });
    try {
      await display();
      const textarea=host?.querySelector<HTMLTextAreaElement>("#receiver-review-draft");
      const select=host?.querySelector<HTMLSelectElement>("#receiver-grant-choice");
      const approve=()=>Array.from(host?.querySelectorAll("button")??[])
        .find(b=>b.textContent?.trim()==="Confirm independent receiver create-only approval (no import)");
      await act(async()=>{
        if(textarea){
          Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype,"value")?.set?.call(
            textarea,JSON.stringify(proposal));
          textarea.dispatchEvent(new Event("input",{bubbles:true}));
        }
        if(select){select.value=grant.fingerprint;select.dispatchEvent(new Event("change",{bubbles:true}));}
      });
      expect(textarea?.value).toBe(JSON.stringify(proposal));
      expect(select?.value).toBe(grant.fingerprint);
      expect(approve()).toBeDefined();
      expect((approve() as HTMLButtonElement).disabled).toBe(false);
      await act(async()=>{approve()?.click();});
      expect(writes).toBe(0);
      confirm.mockReturnValue(true);
      await act(async()=>{approve()?.click();});
      expect(writes).toBe(1);
      expect(confirm).toHaveBeenCalledWith(expect.stringContaining("src/newfile.rs · 100 bytes · SHA-256 "+ "c".repeat(64)));
      expect(host?.textContent).toContain("Saved receiver approvals (1)");
      expect(host?.textContent).toContain("Inspect saved receiver approvals");
      expect(bridge.invoke).not.toHaveBeenCalledWith("advance_remote_project_sync",expect.anything());
      expect(bridge.invoke).not.toHaveBeenCalledWith("stage_remote_source_chunk",expect.anything());
      expect(bridge.invoke).not.toHaveBeenCalledWith("remote_machine_call",expect.anything());
    } finally {confirm.mockRestore();}
  });
  it("receiver read-only freshness detects an occupied destination without sending import", async () => {
    const original=bridge.invoke.getMockImplementation();
    const approved={
      id:"receiver-source-"+"1".repeat(32),createdAt:123,state:"approved",
      workspaceId:"approved-project",credentialFingerprint:"d".repeat(64),
      coordinatorMachineFingerprint:"a".repeat(64),
      coordinatorWorkspaceId:"origin-project",
      coordinatorConnectionNonce:"node-0123456789abcdef01234567",
      coordinatorSourceFingerprint:"b".repeat(64),
      sourceReviewId:"source-review-"+"e".repeat(32),
      files:[{path:"src/raced.rs",sha256:"c".repeat(64),bytes:10}],
    };
    bridge.invoke.mockImplementation(async(command:string,args?:Record<string,unknown>)=>{
      if(command==="list_incoming_source_approvals_desktop") return [approved];
      if(command==="inspect_incoming_source_approval_desktop"){
        expect(args).toEqual({approvalId:approved.id});
        return {
          approvalId:approved.id,state:"approved",credentialActive:false,
          destinationProjectApprovedWritable:true,humanImportSupported:true,canStillProposeCreateOnly:false,
          files:[{path:approved.files[0].path,expectedSha256:approved.files[0].sha256,
            expectedBytes:approved.files[0].bytes,status:"conflict"}],
          note:"Read-only, existing remote file blocks any create-only import.",
        };
      }
      return original?.(command,args);
    });
    await display();
    const press=Array.from(host?.querySelectorAll("button")??[])
      .find(button=>button.textContent?.trim()==="Recheck receiver source approval (read-only)");
    await act(async()=>{press?.click();});
    expect(host?.textContent).toContain("src/raced.rs — conflict");
    expect(host?.textContent).toContain("Credential active: false");
    expect(host?.textContent).toContain("Proposed destinations still missing: false");
    expect(bridge.invoke).not.toHaveBeenCalledWith("advance_remote_project_sync",expect.anything());
    expect(bridge.invoke).not.toHaveBeenCalledWith("stage_remote_source_chunk",expect.anything());
    expect(bridge.invoke).not.toHaveBeenCalledWith("temp_workspace_file_action",expect.anything());
  });
  it("shows an UNKNOWN receiver import reservation without offering replay or withdrawal", async () => {
    const original=bridge.invoke.getMockImplementation();
    const receipt={
      id:"receiver-source-"+"4".repeat(32),createdAt:123,state:"dispatchUnknown",
      workspaceId:"approved-project",credentialFingerprint:"d".repeat(64),
      coordinatorMachineFingerprint:"a".repeat(64),coordinatorWorkspaceId:"origin-project",
      coordinatorConnectionNonce:"node-0123456789abcdef01234567",
      coordinatorSourceFingerprint:"b".repeat(64),
      sourceReviewId:"source-review-"+"e".repeat(32),
      receiverMachineFingerprint:"a".repeat(64),identityPublicKey:"A".repeat(43)+"=",
      identitySignature:"A".repeat(86)+"==",
      files:[{path:"src/present.rs",sha256:"c".repeat(64),bytes:6}],
      dispatch:{sessionBindingSha256:"1".repeat(64),sourceOwnerSha256:"2".repeat(64),
        stagingTaskId:"remote-sync-"+"3".repeat(32),markedUnknownAt:124},
    };
    bridge.invoke.mockImplementation(async(command:string,args?:Record<string,unknown>)=>{
      if(command==="list_incoming_source_approvals_desktop")return [receipt];
      if(command==="inspect_incoming_source_approval_desktop"){
        expect(args).toEqual({approvalId:receipt.id});
        return {approvalId:receipt.id,state:"dispatchUnknown",credentialActive:true,
          destinationProjectApprovedWritable:true,humanImportSupported:true,canStillProposeCreateOnly:false,
          files:[{path:receipt.files[0].path,expectedSha256:receipt.files[0].sha256,
            expectedBytes:receipt.files[0].bytes,status:"matchingBytes"}],
          note:"Read-only matchingBytes proves current content, not who installed it. Do not replay."};
      }
      return original?.(command,args);
    });
    await display();
    const press=(label:string)=>Array.from(host?.querySelectorAll("button")??[])
      .find(button=>button.textContent?.trim()===label);
    expect(host?.textContent).toContain("dispatchUnknown");
    expect(press("Withdraw receiver source approval")).toBeUndefined();
    await act(async()=>{press("Recheck receiver source approval (read-only)")?.click();});
    expect(host?.textContent).toContain("src/present.rs — matchingBytes");
    expect(host?.textContent).toContain("not who installed it");
    expect(bridge.invoke).not.toHaveBeenCalledWith("approve_incoming_source_review_desktop",expect.anything());
    expect(bridge.invoke).not.toHaveBeenCalledWith("remote_machine_call",expect.anything());
    expect(bridge.invoke).not.toHaveBeenCalledWith("advance_remote_project_sync",expect.anything());
  });
  it("displays actual advertised machine capabilities without auto-launching remote screen", async () => {
    await display();
    expect(host?.textContent).toContain("Windows Test");
    expect(host?.textContent).toContain("approved-project");
    expect(host?.textContent).toContain("a".repeat(64));
    expect(host?.textContent).toContain("d".repeat(64));
    expect(host?.textContent).toContain("Verified machine key SHA-256");
    expect(host?.textContent).toContain("Browser tool advertised");
    expect(host?.textContent).toContain("AI Workspace tool advertised");
    expect(host?.textContent).toContain("Build tool advertised");
    expect(bridge.invoke).not.toHaveBeenCalledWith("view_remote_machine_screen", expect.anything());
  });
  it("remembers and explicitly forgets only non-secret pairing metadata", async () => {
    const saved = {
      id:"node-test-1", label:"Windows Test",endpoint:"https://win.example/mcp",
      remoteWorkspaceId:"approved-project",identityFingerprint:"b".repeat(64),savedAt:100,
    };
    let profiles:typeof saved[] = [];
    const confirmed = vi.spyOn(window,"confirm").mockReturnValue(false);
    bridge.invoke.mockImplementation(async (command:string, args?:Record<string,unknown>) => {
      if (command === "list_remote_machines") return nodes;
      if (command === "list_remembered_remote_machines") return profiles;
      if (command === "list_workspaces") return [{id:"approved-project",name:"Approved Project"}];
      if (command === "list_downloaded_remote_artifacts"
        || command === "list_remote_machine_access_grants"
        || command === "list_remote_native_test_runs_desktop") return [];
      if (command === "remember_remote_machine") {
        expect(args).toEqual({nodeId:"node-test-1"});
        profiles = [saved]; return saved;
      }
      if (command === "forget_remembered_remote_machine") {
        expect(args).toEqual({nodeId:"node-test-1"});
        profiles=[]; return true;
      }
      return undefined;
    });
    try {
      await display();
      const button = (name:string) => Array.from(host?.querySelectorAll("button") ?? [])
        .find((item)=>item.textContent === name);
      expect(host?.textContent).toContain("Remembered remote computers (0)");
      expect(bridge.invoke).not.toHaveBeenCalledWith("remember_remote_machine",expect.anything());
      await act(async () => {button("Remember pairing details")?.click();});
      expect(host?.textContent).toContain("Remembered remote computers (1)");
      expect(host?.textContent).toContain("No credential, AI session or process is saved.");
      await act(async () => {button("Forget saved details")?.click();});
      expect(profiles).toHaveLength(1);
      confirmed.mockReturnValue(true);
      await act(async () => {button("Forget saved details")?.click();});
      expect(profiles).toHaveLength(0);
      expect(host?.textContent).toContain("Remembered remote computers (0)");
      expect(bridge.invoke).not.toHaveBeenCalledWith("disconnect_remote_machine",expect.anything());
    } finally {confirmed.mockRestore();}
  });
  it("fills remembered endpoint but requires new secret and clears failed connection bearer", async () => {
    const profile = {
      id:"node-saved-2",label:"Saved macOS",
      endpoint:"https://saved.example/mcp",remoteWorkspaceId:"saved-approved",identityFingerprint:"c".repeat(64),savedAt:101,
    };
    bridge.invoke.mockImplementation(async (command:string, args?:Record<string,unknown>) => {
      if (command === "list_remote_machines") return [];
      if (command === "list_remembered_remote_machines") return [profile];
      if (command === "list_workspaces" || command === "list_remote_machine_access_grants"
          || command === "list_downloaded_remote_artifacts"
          || command === "list_remote_native_test_runs_desktop") return [];
      if (command === "connect_remote_machine") {
        expect(args?.name).toBe(profile.label);
        expect(args?.url).toBe(profile.endpoint);
        expect(args?.remoteWorkspaceId).toBe(profile.remoteWorkspaceId);
        throw new Error("expired credential");
      }
      return undefined;
    });
    await display();
    const button = (name:string) => Array.from(host?.querySelectorAll("button") ?? [])
      .find((item)=>item.textContent === name);
    const form = host?.querySelector<HTMLFormElement>("form.remote-fleet-form");
    const inputs = form?.querySelectorAll("input") ?? [];
    await act(async () => {button("Fill connection form")?.click();});
    expect(inputs[0]?.value).toBe(profile.label);
    expect(inputs[1]?.value).toBe(profile.endpoint);
    expect(inputs[3]?.value).toBe(profile.remoteWorkspaceId);
    const secret = inputs[2];
    expect(secret?.value).toBe("");
    expect(bridge.invoke).not.toHaveBeenCalledWith("connect_remote_machine",expect.anything());
    await act(async () => {
      if (secret) {
        Object.getOwnPropertyDescriptor(HTMLInputElement.prototype,"value")?.set?.call(secret,"a".repeat(64));
        secret.dispatchEvent(new Event("input",{bubbles:true}));
      }
    });
    await act(async () => {
      form?.dispatchEvent(new Event("submit",{bubbles:true,cancelable:true}));
    });
    expect(bridge.invoke).toHaveBeenCalledWith("connect_remote_machine",expect.objectContaining({
      name:profile.label,url:profile.endpoint,remoteWorkspaceId:profile.remoteWorkspaceId,
      credential:"a".repeat(64),
    }));
    expect(secret?.value).toBe("");
    expect(host?.textContent).toContain("expired credential");
  });
  it("performs an MCP metadata health check only when the human requests it", async () => {
    await display();
    expect(bridge.invoke).not.toHaveBeenCalledWith("probe_remote_machine", expect.anything());
    const check = Array.from(host?.querySelectorAll("button") ?? [])
      .find((button) => button.textContent === "Check connection");
    await act(async () => { check?.dispatchEvent(new MouseEvent("click", { bubbles: true })); });
    expect(bridge.invoke).toHaveBeenCalledWith("probe_remote_machine", { nodeId: "node-test-1" });
    expect(host?.textContent).toContain("capability list unchanged");
    expect(host?.textContent).toContain("readiness have not been verified");
  });
  it("checks scoped browser availability without launching or reading browser tabs", async () => {
    const original = bridge.invoke.getMockImplementation();
    bridge.invoke.mockImplementation(async (command: string, args?: Record<string,unknown>) => {
      if (command === "inspect_remote_machine_browser") {
        expect(args).toEqual({nodeId:"node-test-1"});
        return {nodeId:"node-test-1",checkedAt:123,available:true,
          runningForProbeSession:false,browserName:"Chromium",
          note:"Read-only scoped diagnostic; no tabs inspected."};
      }
      return original?.(command,args);
    });
    await display();
    expect(bridge.invoke).not.toHaveBeenCalledWith("inspect_remote_machine_browser", expect.anything());
    const button = Array.from(host?.querySelectorAll("button") ?? [])
      .find((element) => element.textContent === "Inspect browser runtime");
    await act(async () => { button?.dispatchEvent(new MouseEvent("click",{bubbles:true})); });
    expect(host?.textContent).toContain("Browser installed: reported yes");
    expect(host?.textContent).toContain("running in diagnostic session: no");
    expect(host?.textContent).toContain("no tabs inspected");
    expect(bridge.invoke).not.toHaveBeenCalledWith("browser_action", expect.anything());
    expect(bridge.invoke).not.toHaveBeenCalledWith("remote_machine_call", expect.anything());
  });
  it("inspects native OS resources only on explicit click without starting a process", async () => {
    bridge.invoke.mockImplementation(async (command: string) => {
      if (command === "list_remote_machines") return nodes;
      if (command === "list_workspaces") return [{ id:"approved-project", name:"Approved Project" }];
      if (command === "list_downloaded_remote_artifacts") return [];
      if (command === "list_remote_native_test_runs_desktop") return [];
      if (command === "list_remote_machine_access_grants") return [];
      if (command === "inspect_remote_machine_capacity") return {
        nodeId:"node-test-1", checkedAt:1000000, platform:"windows",
        architecture:"x86_64", memoryAvailableBytes:8589934592,
        workspaceDiskAvailableBytes:17179869184, gpuAvailable:false,
        gpuDeviceCount:0, note:"Native GUI, browser and SDK readiness not certified.",
      };
      return undefined;
    });
    await display();
    expect(bridge.invoke).not.toHaveBeenCalledWith("inspect_remote_machine_capacity", expect.anything());
    const inspect = Array.from(host?.querySelectorAll("button") ?? [])
      .find((button) => button.textContent === "Inspect native capacity");
    await act(async () => { inspect?.dispatchEvent(new MouseEvent("click", { bubbles:true })); });
    expect(bridge.invoke).toHaveBeenCalledWith("inspect_remote_machine_capacity", {nodeId:"node-test-1"});
    expect(host?.textContent).toContain("windows / x86_64");
    expect(host?.textContent).toContain("8.0 GiB");
    expect(host?.textContent).toContain("16.0 GiB");
    expect(host?.textContent).toContain("not verified");
    expect(host?.textContent).toContain("Native GUI, browser and SDK readiness not certified");
    expect(bridge.invoke).not.toHaveBeenCalledWith("start_process", expect.anything());
    expect(bridge.invoke).not.toHaveBeenCalledWith("remote_machine_call", expect.anything());
  });
  it("shows bounded project-scoped remote jobs without invoking process mutations", async () => {
    bridge.invoke.mockImplementation(async (command: string) => {
      if (command === "list_remote_machines") return nodes;
      if (command === "list_workspaces") return [{id:"approved-project",name:"Approved Project"}];
      if (command === "list_downloaded_remote_artifacts") return [];
      if (command === "list_remote_machine_access_grants") return [];
      if (command === "list_remote_native_test_runs_desktop") return [];
      if (command === "inspect_remote_machine_jobs") return {
        nodeId:"node-test-1", checkedAt:1000000, remoteWorkspaceId:"approved-project",
        runningProcesses:1, note:"No commands or output shown.",
        jobs:[{processId:"process-123",label:"Native Matrix macOS",status:"running",createdAt:200,exitCode:null}],
      };
      return undefined;
    });
    await display();
    expect(bridge.invoke).not.toHaveBeenCalledWith("inspect_remote_machine_jobs", expect.anything());
    const button = Array.from(host?.querySelectorAll("button") ?? [])
      .find((item) => item.textContent === "Inspect remote jobs");
    await act(async () => { button?.dispatchEvent(new MouseEvent("click", {bubbles:true})); });
    expect(bridge.invoke).toHaveBeenCalledWith("inspect_remote_machine_jobs", {nodeId:"node-test-1"});
    expect(host?.textContent).toContain("Remote job snapshot — 1 running");
    expect(host?.textContent).toContain("Native Matrix macOS");
    expect(host?.textContent).toContain("process-123");
    expect(host?.textContent).toContain("No jobs were started or stopped");
    expect(bridge.invoke).not.toHaveBeenCalledWith("remote_machine_call", expect.anything());
    expect(bridge.invoke).not.toHaveBeenCalledWith("stop_process", expect.anything());
  });
  it("monitors only authenticated read-only health on explicit opt-in and stops when switched off", async () => {
    await display();
    const toggle = Array.from(host?.querySelectorAll("input[type=checkbox]") ?? [])
      .find((item) => item.parentElement?.textContent?.includes("Monitor MCP connection health")) as HTMLInputElement | undefined;
    expect(toggle).toBeDefined();
    expect(toggle?.checked).toBe(false);
    expect(bridge.invoke).not.toHaveBeenCalledWith("probe_remote_machine", expect.anything());
    vi.useFakeTimers();
    try {
      await act(async () => { toggle?.click(); });
      expect(toggle?.checked).toBe(true);
      expect(bridge.invoke).toHaveBeenCalledWith("probe_remote_machine", {nodeId:"node-test-1"});
      expect(host?.textContent).toContain("capabilities unchanged");
      const checksBeforeDisable = bridge.invoke.mock.calls.filter(
        ([command]) => command === "probe_remote_machine"
      ).length;
      await act(async () => { toggle?.click(); });
      expect(toggle?.checked).toBe(false);
      await act(async () => { vi.advanceTimersByTime(120_000); });
      expect(bridge.invoke.mock.calls.filter(([command]) => command === "probe_remote_machine")).toHaveLength(checksBeforeDisable);
      expect(bridge.invoke).not.toHaveBeenCalledWith("reconnect_remote_machine", expect.anything());
      expect(bridge.invoke).not.toHaveBeenCalledWith("remote_machine_call", expect.anything());
    } finally { vi.useRealTimers(); }
  });
  it("previews paginated source conflicts without transferring files or claiming AI ownership", async () => {
    bridge.invoke.mockImplementation(async (command: string, args?: Record<string,unknown>) => {
      if (command === "list_remote_machines") return nodes;
      if (command === "list_workspaces") return [{id:"approved-project",name:"Approved Project"}];
      if (command === "list_downloaded_remote_artifacts") return [];
      if (command === "list_remote_machine_access_grants") return [];
      if (command === "list_remote_native_test_runs_desktop") return [];
      if (command === "preview_remote_source_diff_desktop") {
        expect(args?.localWorkspaceId).toBe("approved-project");
        expect(args?.nodeId).toBe("node-test-1");
        if (args?.cursorIndex === 0) {
          expect(args?.expectedSourceSha256).toBeNull();
          return {
            localWorkspaceId:"approved-project",remoteWorkspaceId:"approved-project",nodeId:"node-test-1",
            sourceFingerprint:"1".repeat(64),totalSourceFiles:10,cursorIndex:0,nextCursorIndex:8,
            complete:false,sourceVerified:false,
            rows:[{path:"src/main.ts",status:"conflict",localSha256:"a".repeat(64),localBytes:123,
              remoteSha256:"b".repeat(64),remoteBytes:99},
              ...Array.from({length:7},(_,i)=>({path:`src/unchanged-${i}.ts`,status:"identical",
                localSha256:"c".repeat(64),localBytes:1,remoteSha256:"c".repeat(64),remoteBytes:1}))],
            note:"Read-only preview",
          };
        }
        if (args?.cursorIndex === 10) {
          expect(args?.expectedSourceSha256).toBe("1".repeat(64));
          return {
            localWorkspaceId:"approved-project",remoteWorkspaceId:"approved-project",nodeId:"node-test-1",
            sourceFingerprint:"1".repeat(64),totalSourceFiles:10,cursorIndex:10,nextCursorIndex:10,
            complete:true,sourceVerified:true,rows:[],note:"Verified fresh aggregate parity",
          };
        }
        expect(args?.cursorIndex).toBe(8);
        expect(args?.expectedSourceSha256).toBe("1".repeat(64));
        return {
          localWorkspaceId:"approved-project",remoteWorkspaceId:"approved-project",nodeId:"node-test-1",
          sourceFingerprint:"1".repeat(64),totalSourceFiles:10,cursorIndex:8,nextCursorIndex:10,
          complete:true,sourceVerified:false,
          rows:[{path:"docs/help.md",status:"missing",localSha256:"d".repeat(64),localBytes:5,
            remoteSha256:null,remoteBytes:null},
            {path:"docs/unchanged.md",status:"identical",localSha256:"c".repeat(64),
              localBytes:1,remoteSha256:"c".repeat(64),remoteBytes:1}],
          note:"Remote source mismatch",
        };
      }
      return undefined;
    });
    await display();
    const select = host?.querySelector<HTMLSelectElement>("#remote-diff-node-test-1");
    expect(select).toBeTruthy();
    await act(async () => {
      if (select) { select.value = "approved-project"; select.dispatchEvent(new Event("change",{bubbles:true})); }
    });
    const press = (name:string) => Array.from(host?.querySelectorAll("button") ?? [])
      .find((item) => item.textContent === name);
    await act(async () => { press("Preview source differences")?.dispatchEvent(new MouseEvent("click",{bubbles:true})); });
    expect(host?.textContent).toContain("src/main.ts");
    expect(host?.textContent).toContain("conflict");
    expect(host?.textContent).toContain("Partial preview");
    await act(async () => { press("Next 8 files")?.dispatchEvent(new MouseEvent("click",{bubbles:true})); });
    expect(host?.textContent).toContain("docs/help.md");
    expect(host?.textContent).toContain("missing");
    expect(host?.textContent).toContain("full source parity NOT verified");
    expect(host?.textContent).toContain("src/main.ts");
    expect(host?.textContent).toContain("8 identical");
    expect(host?.textContent).toContain("1 missing");
    expect(host?.textContent).toContain("1 conflicting");
    expect(host?.textContent).toContain("local: 123 bytes");
    expect(host?.textContent).toContain("remote: 99 bytes");
    expect(host?.textContent).toContain("a".repeat(16));
    expect(host?.textContent).toContain("b".repeat(16));
    expect(press("Next 8 files")?.disabled).toBe(true);
    await act(async () => { press("Recheck whole-source parity")?.dispatchEvent(new MouseEvent("click",{bubbles:true})); });
    expect(bridge.invoke).toHaveBeenCalledWith("preview_remote_source_diff_desktop",{
      localWorkspaceId:"approved-project",nodeId:"node-test-1",
      expectedSourceSha256:"1".repeat(64),cursorIndex:10,
      expectedMachineIdentity:"d".repeat(64),expectedConnectionTime:1,
      expectedConnectionNonce:"node-0123456789abcdef01234567",
    });
    expect(host?.textContent).toContain("Complete aggregate source parity verified");
    expect(host?.textContent).toContain("Fresh source hash, file count and byte count all match");
    expect(host?.textContent).not.toContain("8 identical");
    const watch = Array.from(host?.querySelectorAll("input[type=checkbox]") ?? [])
      .find((item) => item.parentElement?.textContent?.includes("Monitor source parity")) as HTMLInputElement;
    expect(watch.disabled).toBe(false);
    vi.useFakeTimers();
    try {
      await act(async () => {watch.click();});
      expect(watch.checked).toBe(true);
      const calls = () => bridge.invoke.mock.calls.filter(([command, args]) =>
        command === "preview_remote_source_diff_desktop" && args?.cursorIndex === 10).length;
      const before = calls();
      await act(async () => {vi.advanceTimersByTime(120_000);});
      expect(calls()).toBe(before+1);
      await act(async () => {watch.click();});
      const stopped = calls();
      await act(async () => {vi.advanceTimersByTime(240_000);});
      expect(calls()).toBe(stopped);
      expect(watch.checked).toBe(false);
      expect(host?.textContent).toContain("Source parity monitoring stopped");
    } finally {vi.useRealTimers();}
    expect(bridge.invoke).not.toHaveBeenCalledWith("remote_machine_call", expect.anything());
    expect(bridge.invoke).not.toHaveBeenCalledWith("advance_remote_project_sync", expect.anything());
  });
  it("allows reviewing every inspected source conflict beyond the old 40-row sample without writes", async () => {
    const original = bridge.invoke.getMockImplementation();
    bridge.invoke.mockImplementation(async (command:string,args?:Record<string,unknown>) => {
      if (command === "preview_remote_source_diff_desktop") {
        const cursor = Number(args?.cursorIndex ?? 0);
        expect(args?.expectedSourceSha256).toBe(cursor ? "2".repeat(64) : null);
        return {
          nodeId:"node-test-1",localWorkspaceId:"approved-project",remoteWorkspaceId:"approved-project",
          sourceFingerprint:"2".repeat(64),totalSourceFiles:48,
          cursorIndex:cursor,nextCursorIndex:cursor+8,complete:cursor+8===48,sourceVerified:false,
          rows:Array.from({length:8},(_,i)=>({
            path:`source/conflict-${cursor+i}.txt`,status:"conflict",localSha256:"e".repeat(64),
            localBytes:4,remoteSha256:"f".repeat(64),remoteBytes:9,
          })),
          note:"Read-only inspection",
        };
      }
      return original?.(command,args);
    });
    await display();
    const select = host?.querySelector<HTMLSelectElement>("#remote-diff-node-test-1");
    await act(async () => {
      if (select) { select.value = "approved-project"; select.dispatchEvent(new Event("change",{bubbles:true})); }
    });
    const press = (name:string) => Array.from(host?.querySelectorAll("button") ?? [])
      .find((button)=>button.textContent === name);
    for (let page=0;page<6;page++) {
      await act(async () => {
        press(page===0 ? "Preview source differences" : "Next 8 files")?.click();
      });
    }
    expect(host?.textContent).toContain("48 conflicting");
    expect(host?.textContent).toContain("Issues 1–40 of 48");
    expect(host?.textContent).toContain("source/conflict-0.txt");
    expect(host?.textContent).not.toContain("source/conflict-47.txt");
    await act(async () => {press("Next issues")?.click();});
    expect(host?.textContent).toContain("Issues 41–48 of 48");
    expect(host?.textContent).toContain("source/conflict-47.txt");
    expect(host?.textContent).not.toContain("source/conflict-0.txt");
    expect((press("Next issues") as HTMLButtonElement)?.disabled).toBe(true);
    await act(async () => {press("Previous issues")?.click();});
    expect(host?.textContent).toContain("source/conflict-0.txt");
    expect(bridge.invoke).not.toHaveBeenCalledWith("advance_remote_project_sync",expect.anything());
    expect(bridge.invoke).not.toHaveBeenCalledWith("remote_machine_call",expect.anything());
  });
  it("copies an explicitly reviewed conflict draft without authorizing any remote writes", async () => {
    const original = bridge.invoke.getMockImplementation();
    bridge.invoke.mockImplementation(async (command:string,args?:Record<string,unknown>) => {
      if (command === "preview_remote_source_diff_desktop") {
        return {
          nodeId:"node-test-1",localWorkspaceId:"approved-project",remoteWorkspaceId:"approved-project",
          sourceFingerprint:"3".repeat(64),totalSourceFiles:3,
          cursorIndex:0,nextCursorIndex:2,complete:false,sourceVerified:false,
          rows:[
            {path:"assets/missing.bin",status:"missing",localSha256:"a".repeat(64),
              localBytes:4,remoteSha256:null,remoteBytes:null},
            {path:"src/conflicting.rs",status:"conflict",localSha256:"b".repeat(64),
              localBytes:6,remoteSha256:"c".repeat(64),remoteBytes:9},
          ],
          note:"Read-only comparison",
        };
      }
      return original?.(command,args);
    });
    const oldClipboard = Object.getOwnPropertyDescriptor(navigator, "clipboard");
    const writeText = vi.fn(async (_draft:string) => undefined);
    Object.defineProperty(navigator, "clipboard", {
      configurable:true, value:{writeText},
    });
    try {
      await display();
      const project = host?.querySelector<HTMLSelectElement>("#remote-diff-node-test-1");
      await act(async () => {
        if (project) {project.value="approved-project";project.dispatchEvent(new Event("change",{bubbles:true}));}
      });
      const press = (name:string) => Array.from(host?.querySelectorAll("button") ?? [])
        .find((button)=>button.textContent?.trim()===name);
      await act(async () => {press("Preview source differences")?.click();});
      const missing = host?.querySelector<HTMLSelectElement>(
        'select[aria-label="Review decision for assets/missing.bin"]');
      const conflict = host?.querySelector<HTMLSelectElement>(
        'select[aria-label="Review decision for src/conflicting.rs"]');
      expect(missing?.value).toBe("defer");
      expect(conflict?.value).toBe("manualMerge");
      await act(async () => {
        if (missing) {missing.value="proposeCreate";missing.dispatchEvent(new Event("change",{bubbles:true}));}
        if (conflict) {conflict.value="keepRemote";conflict.dispatchEvent(new Event("change",{bubbles:true}));}
      });
      await act(async () => {press("Copy source conflict review draft")?.click();});
      expect(writeText).toHaveBeenCalledTimes(1);
      const draft = JSON.parse(writeText.mock.calls[0][0]);
      expect(draft).toMatchObject({
        schemaVersion:1,kind:"readOnlySourceConflictReview",nodeId:"node-test-1",
        localWorkspaceId:"approved-project",remoteWorkspaceId:"approved-project",
        sourceFingerprint:"3".repeat(64),complete:false,
        machineIdentityFingerprint:"d".repeat(64),
        connectionNonce:"node-0123456789abcdef01234567",
        connectedAt:1,
        requiresFreshPreflightAndSeparateApproval:true,canOverwriteRemoteSource:false,
      });
      expect(draft.issues).toHaveLength(2);
      expect(draft.issues[0]).toMatchObject({path:"assets/missing.bin",reviewChoice:"proposeCreate"});
      expect(draft.issues[1]).toMatchObject({path:"src/conflicting.rs",reviewChoice:"keepRemote"});
      expect(host?.textContent).toContain("not a transfer authorization");
      expect(bridge.invoke).not.toHaveBeenCalledWith("advance_remote_project_sync",expect.anything());
      expect(bridge.invoke).not.toHaveBeenCalledWith("remote_machine_call",expect.anything());
      expect(bridge.invoke).not.toHaveBeenCalledWith("stage_remote_source_chunk",expect.anything());
      // New read-only comparisons must discard previous human choices.
      await act(async () => {press("Preview source differences")?.click();});
      expect(host?.querySelector<HTMLSelectElement>(
        'select[aria-label="Review decision for assets/missing.bin"]')?.value).toBe("defer");
    } finally {
      if (oldClipboard) Object.defineProperty(navigator,"clipboard",oldClipboard);
      else Reflect.deleteProperty(navigator,"clipboard");
    }
  });
  it("preflights a human-proposed create-only file without staging or importing and invalidates stale receipts", async () => {
    const original=bridge.invoke.getMockImplementation();
    let preflights=0;
    let alteredReceipt=false;
    bridge.invoke.mockImplementation(async (command:string,args?:Record<string,unknown>)=>{
      if(command==="preview_remote_source_diff_desktop"){
        return {
          nodeId:"node-test-1",localWorkspaceId:"approved-project",remoteWorkspaceId:"approved-project",
          sourceFingerprint:"f".repeat(64),totalSourceFiles:1,
          cursorIndex:0,nextCursorIndex:1,complete:true,sourceVerified:false,
          rows:[{path:"assets/new.bin",status:"missing",localSha256:"a".repeat(64),
            localBytes:11,remoteSha256:null,remoteBytes:null}],
          note:"Read only",
        };
      }
      if(command==="preflight_remote_create_only_desktop"){
        preflights++;
        expect(args).toEqual({
          localWorkspaceId:"approved-project",nodeId:"node-test-1",
          expectedSourceSha256:"f".repeat(64),
          expectedMachineIdentity:"d".repeat(64),expectedConnectionTime:1,
          expectedConnectionNonce:"node-0123456789abcdef01234567",
          proposed:[{path:"assets/new.bin",sha256:"a".repeat(64),bytes:11}],
        });
        return {
          nodeId:"node-test-1",localWorkspaceId:"approved-project",remoteWorkspaceId:"approved-project",
          sourceFingerprint:"f".repeat(64),
          machineIdentityFingerprint: alteredReceipt ? "e".repeat(64) : "d".repeat(64),
          connectedAt:1,connectionNonce:"node-0123456789abcdef01234567",inspected:1,
          proposed:[{path:"assets/new.bin",sha256:"a".repeat(64),bytes:11}],
          note:"Read only, not an import authorization",
        };
      }
      return original?.(command,args);
    });
    await display();
    const select=host?.querySelector<HTMLSelectElement>("#remote-diff-node-test-1");
    await act(async()=>{if(select){select.value="approved-project";select.dispatchEvent(new Event("change",{bubbles:true}));}});
    const press=(name:string)=>Array.from(host?.querySelectorAll("button")??[])
      .find((button)=>button.textContent?.trim()===name) as HTMLButtonElement|undefined;
    await act(async()=>{press("Preview source differences")?.click();});
    const review=host?.querySelector<HTMLSelectElement>(
      'select[aria-label="Review decision for assets/new.bin"]');
    expect(press("Preflight next 8 proposed files (read-only)")?.disabled).toBe(true);
    await act(async()=>{
      if(review){review.value="proposeCreate";review.dispatchEvent(new Event("change",{bubbles:true}));}
    });
    await act(async()=>{press("Preflight next 8 proposed files (read-only)")?.click();});
    expect(preflights).toBe(1);
    expect(host?.textContent).toContain("Create-only preflight: 1 proposed files");
    expect(press("Preflight next 8 proposed files (read-only)")?.disabled).toBe(true);
    await act(async()=>{
      if(review){review.value="defer";review.dispatchEvent(new Event("change",{bubbles:true}));}
    });
    expect(host?.textContent).not.toContain("Create-only preflight: 1 proposed files");
    await act(async()=>{
      if(review){review.value="proposeCreate";review.dispatchEvent(new Event("change",{bubbles:true}));}
    });
    alteredReceipt=true;
    await act(async()=>{press("Preflight next 8 proposed files (read-only)")?.click();});
    expect(preflights).toBe(2);
    expect(host?.textContent).toContain("Preflight result did not match");
    expect(host?.textContent).not.toContain("Create-only preflight: 1 proposed files");
    expect(bridge.invoke).not.toHaveBeenCalledWith("stage_remote_source_chunk",expect.anything());
    expect(bridge.invoke).not.toHaveBeenCalledWith("advance_remote_project_sync",expect.anything());
    expect(bridge.invoke).not.toHaveBeenCalledWith("remote_machine_call",expect.anything());
  });

  it("discards a late preflight when a human revises and restores the same choices", async () => {
    const original = bridge.invoke.getMockImplementation();
    let finishPreflight: ((receipt: unknown) => void) | undefined;
    const pending = new Promise<unknown>((resolve) => { finishPreflight = resolve; });
    bridge.invoke.mockImplementation(async (command:string,args?:Record<string,unknown>) => {
      if (command === "preview_remote_source_diff_desktop") return {
        nodeId:"node-test-1",localWorkspaceId:"approved-project",remoteWorkspaceId:"approved-project",
        sourceFingerprint:"f".repeat(64),totalSourceFiles:1,
        cursorIndex:0,nextCursorIndex:1,complete:true,sourceVerified:false,
        rows:[{path:"assets/race.bin",status:"missing",localSha256:"a".repeat(64),
          localBytes:11,remoteSha256:null,remoteBytes:null}],note:"Read only",
      };
      if (command === "preflight_remote_create_only_desktop") return pending;
      return original?.(command,args);
    });
    await display();
    const project = host?.querySelector<HTMLSelectElement>("#remote-diff-node-test-1");
    await act(async () => {
      if (project) {
        project.value="approved-project";
        project.dispatchEvent(new Event("change",{bubbles:true}));
      }
    });
    const press = (label:string) => Array.from(host?.querySelectorAll("button")??[])
      .find((button) => button.textContent?.trim() === label);
    await act(async () => { press("Preview source differences")?.click(); });
    const review = host?.querySelector<HTMLSelectElement>(
      'select[aria-label="Review decision for assets/race.bin"]');
    const choose = async (value:string) => {
      await act(async () => {
        if (review) {
          review.value=value;
          review.dispatchEvent(new Event("change",{bubbles:true}));
        }
      });
    };
    await choose("proposeCreate");
    await act(async () => { press("Preflight next 8 proposed files (read-only)")?.click(); });
    expect(bridge.invoke).toHaveBeenCalledWith("preflight_remote_create_only_desktop",expect.anything());
    // Both the final selected path and the machine pairing are unchanged.
    // The *revision history* still invalidates any outstanding old receipt.
    await choose("defer");
    await choose("proposeCreate");
    await act(async () => { finishPreflight?.({
      nodeId:"node-test-1",localWorkspaceId:"approved-project",
      remoteWorkspaceId:"approved-project",
      sourceFingerprint:"f".repeat(64),machineIdentityFingerprint:"d".repeat(64),
      connectedAt:1,connectionNonce:"node-0123456789abcdef01234567",
      inspected:1,proposed:[{path:"assets/race.bin",sha256:"a".repeat(64),bytes:11}],
      note:"No writes",
    }); });
    expect(host?.textContent).toContain("human review choices changed");
    expect(host?.textContent).not.toContain("Create-only preflight: 1 proposed files");
    expect(bridge.invoke).not.toHaveBeenCalledWith("advance_remote_project_sync",expect.anything());
    expect(bridge.invoke).not.toHaveBeenCalledWith("stage_remote_source_chunk",expect.anything());
  });
  it("records exact human create-only review intent after confirmation without transferring bytes", async () => {
    const original = bridge.invoke.getMockImplementation();
    const confirm = vi.spyOn(window, "confirm").mockReturnValue(false);
    const saved: Array<Record<string,unknown>> = [];
    const source = {
      nodeId:"node-test-1", localWorkspaceId:"approved-project", remoteWorkspaceId:"approved-project",
      sourceFingerprint:"f".repeat(64), totalSourceFiles:1, cursorIndex:0,
      nextCursorIndex:1, complete:true, sourceVerified:false,
      rows:[{path:"assets/human.bin",status:"missing",localSha256:"a".repeat(64),
        localBytes:42,remoteSha256:null,remoteBytes:null}],note:"Read only",
    };
    const proposed = [{path:"assets/human.bin",sha256:"a".repeat(64),bytes:42}];
    bridge.invoke.mockImplementation(async (command:string,args?:Record<string,unknown>) => {
      if(command==="preview_remote_source_diff_desktop") return source;
      if(command==="preflight_remote_create_only_desktop") return {
        nodeId:source.nodeId, localWorkspaceId:source.localWorkspaceId,
        remoteWorkspaceId:source.remoteWorkspaceId, sourceFingerprint:source.sourceFingerprint,
        machineIdentityFingerprint:"d".repeat(64), connectedAt:1,
        connectionNonce:"node-0123456789abcdef01234567", inspected:1, proposed,
      };
      if(command==="list_remote_source_review_intents_desktop") return saved;
      if(command==="record_remote_source_review_intent_desktop") {
        expect(args).toMatchObject({
          localWorkspaceId:"approved-project", nodeId:"node-test-1",
          expectedSourceSha256:"f".repeat(64),
          expectedMachineIdentity:"d".repeat(64), expectedConnectionTime:1,
          expectedConnectionNonce:"node-0123456789abcdef01234567", proposed,
        });
        const receipt={
          id:"source-review-0123456789abcdef0123456789abcdef",createdAt:123456,
          state:"awaitingReceiverApproval",
          nodeId:"node-test-1",localWorkspaceId:"approved-project",
          remoteWorkspaceId:"approved-project",machineIdentityFingerprint:"d".repeat(64),
          connectedAt:1,connectionNonce:"node-0123456789abcdef01234567",
          sourceFingerprint:"f".repeat(64),proposed,
        };
        saved.push(receipt);
        return receipt;
      }
      return original?.(command,args);
    });
    try {
      await display();
      const project=host?.querySelector<HTMLSelectElement>("#remote-diff-node-test-1");
      await act(async () => {
        if(project){project.value="approved-project";project.dispatchEvent(new Event("change",{bubbles:true}));}
      });
      const press=(label:string) => Array.from(host?.querySelectorAll("button")??[])
        .find((button)=>button.textContent?.trim()===label) as HTMLButtonElement | undefined;
      await act(async()=>{press("Preview source differences")?.click();});
      const choice=host?.querySelector<HTMLSelectElement>(
        'select[aria-label="Review decision for assets/human.bin"]');
      await act(async()=>{
        if(choice){choice.value="proposeCreate";choice.dispatchEvent(new Event("change",{bubbles:true}));}
      });
      await act(async()=>{press("Preflight next 8 proposed files (read-only)")?.click();});
      const save=()=>press("Record reviewed create-only intent (no transfer)");
      expect(save()?.disabled).toBe(false);
      await act(async()=>{save()?.click();});
      expect(confirm).toHaveBeenCalledWith(expect.stringContaining("assets/human.bin · 42 bytes · SHA-256 " + "a".repeat(64)));
      expect(bridge.invoke).not.toHaveBeenCalledWith("record_remote_source_review_intent_desktop",expect.anything());
      confirm.mockReturnValue(true);
      await act(async()=>{save()?.click();});
      expect(saved).toHaveLength(1);
      expect(host?.textContent).toContain("Saved human source-review intents (1)");
      expect(host?.textContent).toContain("Awaiting receiver authorization");
      await act(async()=>{press("Refresh")?.click();});
      expect(host?.textContent).toContain("Saved human source-review intents (1)");
      expect(bridge.invoke).not.toHaveBeenCalledWith("advance_remote_project_sync",expect.anything());
      expect(bridge.invoke).not.toHaveBeenCalledWith("stage_remote_source_chunk",expect.anything());
      expect(bridge.invoke).not.toHaveBeenCalledWith("remote_machine_call",expect.anything());
    } finally {confirm.mockRestore();}
  });
  it("reconciles a saved create-only review using exact read-only receiver hashes, never claims imported", async () => {
    const original=bridge.invoke.getMockImplementation();
    const proposed=[{path:"src/maybe.rs",sha256:"a".repeat(64),bytes:7}];
    const intent={
      id:"source-review-0123456789abcdef0123456789abcdef",createdAt:123,
      state:"awaitingReceiverApproval",
      localWorkspaceId:"approved-project",remoteWorkspaceId:"approved-project",
      nodeId:"node-test-1",machineIdentityFingerprint:"d".repeat(64),
      connectedAt:1,connectionNonce:"node-0123456789abcdef01234567",
      sourceFingerprint:"f".repeat(64),proposed,
    };
    bridge.invoke.mockImplementation(async(command:string,args?:Record<string,unknown>)=>{
      if(command==="list_remote_source_review_intents_desktop") return [intent];
      if(command==="reconcile_remote_source_review_intent_desktop"){
        expect(args).toEqual({intentId:intent.id});
        return {
          intentId:intent.id,localWorkspaceId:intent.localWorkspaceId,
          remoteWorkspaceId:intent.remoteWorkspaceId,nodeId:intent.nodeId,
          machineIdentityFingerprint:intent.machineIdentityFingerprint,
          connectedAt:intent.connectedAt,connectionNonce:intent.connectionNonce,
          sourceFingerprint:intent.sourceFingerprint,
          rows:[{path:proposed[0].path,status:"identical",localSha256:proposed[0].sha256,
            localBytes:proposed[0].bytes,remoteSha256:proposed[0].sha256,
            remoteBytes:proposed[0].bytes}],
          note:"Matching bytes do not prove installation.",
        };
      }
      return original?.(command,args);
    });
    await display();
    const press=(label:string)=>Array.from(host?.querySelectorAll("button")??[])
      .find((button)=>button.textContent?.trim()===label);
    await act(async()=>{press("Recheck saved proposal status (read-only)")?.click();});
    expect(bridge.invoke).toHaveBeenCalledWith("reconcile_remote_source_review_intent_desktop",
      {intentId:intent.id});
    expect(host?.textContent).toContain("Read-only receiver status, not import provenance:");
    expect(host?.textContent).toContain("src/maybe.rs — identical");
    expect(host?.textContent).toContain("Matching bytes do not prove installation.");
    expect(bridge.invoke).not.toHaveBeenCalledWith("advance_remote_project_sync",expect.anything());
    expect(bridge.invoke).not.toHaveBeenCalledWith("remote_machine_call",expect.anything());
  });

  it("exports only a freshly rechecked native Ed25519 signed exact review, never assembles unsigned source JSON", async () => {
    const original=bridge.invoke.getMockImplementation();
    const intent={
      id:"source-review-"+"1".repeat(32),createdAt:123,
      state:"awaitingReceiverApproval",
      localWorkspaceId:"approved-project",remoteWorkspaceId:"approved-project",
      nodeId:"node-test-1",machineIdentityFingerprint:"d".repeat(64),
      connectedAt:1,connectionNonce:"node-0123456789abcdef01234567",
      sourceFingerprint:"f".repeat(64),
      proposed:[{path:"src/approved.rs",sha256:"c".repeat(64),bytes:9}],
    };
    const proof={
      schemaVersion:1,kind:"RepoTunnelIncomingSourceReviewV1",
      remoteWorkspaceId:intent.remoteWorkspaceId,
      receiverMachineFingerprint:intent.machineIdentityFingerprint,
      coordinatorMachineFingerprint:"a".repeat(64),
      coordinatorWorkspaceId:intent.localWorkspaceId,
      coordinatorConnectionNonce:intent.connectionNonce,
      coordinatorSourceFingerprint:intent.sourceFingerprint,
      sourceReviewId:intent.id,files:intent.proposed,
      identityPublicKey:"A".repeat(43)+"=",
      identitySignature:"A".repeat(86)+"==",
    };
    const written=vi.fn().mockResolvedValue(undefined);
    const previous=Object.getOwnPropertyDescriptor(navigator,"clipboard");
    Object.defineProperty(navigator,"clipboard",{configurable:true,value:{writeText:written}});
    bridge.invoke.mockImplementation(async(command:string,args?:Record<string,unknown>)=>{
      if(command==="list_remote_source_review_intents_desktop")return [intent];
      if(command==="export_signed_incoming_source_review_desktop"){
        expect(args).toEqual({intentId:intent.id});
        return proof;
      }
      return original?.(command,args);
    });
    try{
      await display();
      const copy=Array.from(host?.querySelectorAll("button")??[])
        .find(button=>button.textContent?.trim()==="Copy receiver approval review JSON");
      await act(async()=>{copy?.click();});
      expect(bridge.invoke).toHaveBeenCalledWith(
        "export_signed_incoming_source_review_desktop",{intentId:intent.id});
      expect(written).toHaveBeenCalledTimes(1);
      const exported=JSON.parse(written.mock.calls[0][0]) as typeof proof;
      expect(exported).toEqual(proof);
      expect(host?.textContent).toContain("installation-SIGNED");
      expect(bridge.invoke).not.toHaveBeenCalledWith("remote_machine_call",expect.anything());
      expect(bridge.invoke).not.toHaveBeenCalledWith("advance_remote_project_sync",expect.anything());
    }finally{
      if(previous)Object.defineProperty(navigator,"clipboard",previous);
      else Reflect.deleteProperty(navigator,"clipboard");
    }
  });

  it("restores a durable unknown import after restart without allowing replay or withdrawal", async () => {
    const original=bridge.invoke.getMockImplementation();
    const approvalId="receiver-source-"+"a".repeat(32);
    const taskId="remote-sync-"+"b".repeat(32);
    let observedTask=taskId;
    const intent={
      id:"source-review-"+"c".repeat(32),createdAt:123,
      state:"awaitingReceiverApproval",nodeId:"node-test-1",
      localWorkspaceId:"approved-project",remoteWorkspaceId:"approved-project",
      machineIdentityFingerprint:"d".repeat(64),connectedAt:1,
      connectionNonce:"node-0123456789abcdef01234567",
      sourceFingerprint:"e".repeat(64),
      proposed:[{path:"src/approved.bin",bytes:8,sha256:"f".repeat(64)}],
      importAttempt:{approvalId,taskId,state:"dispatchUnknown",importedFiles:0},
    };
    bridge.invoke.mockImplementation(async(command:string,args?:Record<string,unknown>)=>{
      if(command==="list_remote_source_review_intents_desktop") return [intent];
      if(command==="inspect_remote_receiver_source_approval_desktop") {
        expect(args).toEqual({intentId:intent.id,approvalId});
        return {
          approvalId,state:"dispatchUnknown",importTaskId:observedTask,credentialActive:true,
          destinationProjectApprovedWritable:true,humanImportSupported:true,canStillProposeCreateOnly:false,
          reviewBinding:{
            sourceReviewId:intent.id,remoteWorkspaceId:intent.remoteWorkspaceId,
            receiverMachineFingerprint:intent.machineIdentityFingerprint,
            coordinatorMachineFingerprint:"a".repeat(64),
            coordinatorWorkspaceId:intent.localWorkspaceId,
            coordinatorConnectionNonce:intent.connectionNonce,
            coordinatorSourceFingerprint:intent.sourceFingerprint,
          },
          files:[{path:intent.proposed[0].path,
            expectedSha256:intent.proposed[0].sha256,
            expectedBytes:intent.proposed[0].bytes,status:"matchingBytes"}],
          note:"Read-only matching evidence, but no completion receipt.",
        };
      }
      return original?.(command,args);
    });
    await display();
    expect(host?.textContent).toContain("UNKNOWN — never replay");
    expect(host?.textContent).toContain(taskId);
    const matching=Array.from(host?.querySelectorAll("button")??[])
      .filter((button)=>button.textContent?.includes("Import the approved source files")
        || button.textContent?.includes("Withdraw saved source proposal")
        || button.textContent?.includes("Copy receiver approval review JSON"));
    expect(matching).toHaveLength(0);
    expect(bridge.invoke).not.toHaveBeenCalledWith(
      "import_approved_remote_source_desktop",expect.anything());
    const field=host?.querySelector<HTMLInputElement>(
      `input[aria-label="Receiver approval ID for ${intent.id}"]`);
    expect(field?.value).toBe(approvalId);
    const verify=Array.from(host?.querySelectorAll("button")??[])
      .find(button=>button.textContent?.trim()==="Reconcile receiver import receipt and hashes (read-only)");
    expect(verify).toBeDefined();
    await act(async()=>{verify?.click();});
    expect(host?.textContent).toContain("Matching content, but import outcome is NOT proven");
    expect(host?.textContent).toContain("Receiver journal state: dispatchUnknown");
    expect(bridge.invoke).not.toHaveBeenCalledWith(
      "execute_machine_source_import",expect.anything());
    expect(bridge.invoke).not.toHaveBeenCalledWith(
      "import_approved_remote_source_desktop",expect.anything());
  });

  it("reconciles a proven lost import ACK only by reading receiver evidence and recording the original local task", async () => {
    const original=bridge.invoke.getMockImplementation();
    const approvalId="receiver-source-"+"a".repeat(32);
    const taskId="remote-sync-"+"b".repeat(32);
    let savedState:"dispatchUnknown"|"imported"="dispatchUnknown";
    const intent={
      id:"source-review-"+"c".repeat(32),createdAt:123,
      state:"awaitingReceiverApproval",nodeId:"node-test-1",
      localWorkspaceId:"approved-project",remoteWorkspaceId:"approved-project",
      machineIdentityFingerprint:"d".repeat(64),connectedAt:1,
      connectionNonce:"node-0123456789abcdef01234567",
      sourceFingerprint:"e".repeat(64),
      proposed:[{path:"src/approved.bin",bytes:8,sha256:"f".repeat(64)}],
      importAttempt:{approvalId,taskId,state:savedState,importedFiles:0},
    };
    bridge.invoke.mockImplementation(async(command:string,args?:Record<string,unknown>)=>{
      if(command==="list_remote_source_review_intents_desktop")return [{
        ...intent,importAttempt:{...intent.importAttempt,state:savedState,importedFiles:savedState==="imported"?1:0}
      }];
      if(command==="inspect_remote_receiver_source_approval_desktop")return {
        approvalId,state:"imported",importTaskId:taskId,
        credentialActive:true,destinationProjectApprovedWritable:true,
        humanImportSupported:true,canStillProposeCreateOnly:false,
        reviewBinding:{
          sourceReviewId:intent.id,remoteWorkspaceId:intent.remoteWorkspaceId,
          receiverMachineFingerprint:intent.machineIdentityFingerprint,
          coordinatorMachineFingerprint:"a".repeat(64),
          coordinatorWorkspaceId:intent.localWorkspaceId,
          coordinatorConnectionNonce:intent.connectionNonce,
          coordinatorSourceFingerprint:intent.sourceFingerprint,
        },
        files:[{path:intent.proposed[0].path,expectedSha256:intent.proposed[0].sha256,
          expectedBytes:intent.proposed[0].bytes,status:"matchingBytes"}],
        note:"Receiver original one-shot imported and verified",
      };
      if(command==="reconcile_verified_remote_source_import_desktop"){
        expect(args).toEqual({intentId:intent.id});
        savedState="imported";
        return {approvalId,taskId,state:"imported",importedFiles:1};
      }
      return original?.(command,args);
    });
    await display();
    const press=(label:string)=>Array.from(host?.querySelectorAll("button")??[])
      .find(button=>button.textContent?.trim()===label);
    expect(press("Record verified import in local journal (receiver read-only; no replay)")).toBeUndefined();
    await act(async()=>{
      press("Reconcile receiver import receipt and hashes (read-only)")?.click();
    });
    expect(host?.textContent).toContain("Verified: imported receipt and all current file hashes");
    const confirmation=vi.spyOn(window,"confirm").mockReturnValue(true);
    try {
      await act(async()=>{
        press("Record verified import in local journal (receiver read-only; no replay)")?.click();
      });
      expect(bridge.invoke).toHaveBeenCalledWith(
        "reconcile_verified_remote_source_import_desktop",{intentId:intent.id});
      expect(host?.textContent).toContain("Saved human import: Installed");
      expect(press("Record verified import in local journal (receiver read-only; no replay)")).toBeUndefined();
      expect(bridge.invoke).not.toHaveBeenCalledWith(
        "import_approved_remote_source_desktop",expect.anything());
      expect(bridge.invoke).not.toHaveBeenCalledWith(
        "execute_machine_source_import",expect.anything());
    }finally{confirmation.mockRestore();}
  });

  it("reads an independently approved receiver receipt only through the original saved human review and pinned machine", async () => {
    const prior=bridge.invoke.getMockImplementation();
    const approvalId="receiver-source-"+"2".repeat(32);
    const taskId="remote-sync-"+"3".repeat(32);
    const proposed=[{path:"src/remote.rs",sha256:"e".repeat(64),bytes:20}];
    let imported=false;
    let platformSupported=false;
    const intent={
      id:"source-review-"+"1".repeat(32),createdAt:123,
      state:"awaitingReceiverApproval",nodeId:"node-test-1",
      localWorkspaceId:"approved-project",remoteWorkspaceId:"approved-project",
      machineIdentityFingerprint:"d".repeat(64),
      connectedAt:1,connectionNonce:"node-0123456789abcdef01234567",
      sourceFingerprint:"f".repeat(64),proposed,
    };
    bridge.invoke.mockImplementation(async(command:string,args?:Record<string,unknown>)=>{
      if(command==="list_remote_source_review_intents_desktop") return [intent];
      if(command==="import_approved_remote_source_desktop"){
        expect(args).toEqual({intentId:intent.id,approvalId});
        imported=true;
        return {approvalId,state:"imported",taskId,importedFiles:1,
          note:"All approved bytes verified and installed."};
      }
      if(command==="inspect_remote_receiver_source_approval_desktop"){
        expect(args).toEqual({intentId:intent.id,approvalId});
        return {
          approvalId,state:imported?"imported":"approved",
          ...(imported?{importTaskId:taskId}:{}),credentialActive:true,
          destinationProjectApprovedWritable:true,humanImportSupported:platformSupported,canStillProposeCreateOnly:!imported,
          reviewBinding:{
            remoteWorkspaceId:intent.remoteWorkspaceId,
            receiverMachineFingerprint:intent.machineIdentityFingerprint,
            coordinatorMachineFingerprint:"a".repeat(64),
            coordinatorWorkspaceId:intent.localWorkspaceId,
            coordinatorConnectionNonce:intent.connectionNonce,
            coordinatorSourceFingerprint:intent.sourceFingerprint,
            sourceReviewId:intent.id,
          },
          files:[{path:proposed[0].path,expectedSha256:proposed[0].sha256,
            expectedBytes:proposed[0].bytes,status:imported?"matchingBytes":"missing"}],
          note:"Read only",
        };
      }
      return prior?.(command,args);
    });
    await display();
    const field=host?.querySelector<HTMLInputElement>(
      `input[aria-label="Receiver approval ID for ${intent.id}"]`);
    expect(field).toBeTruthy();
    const findButton=()=>Array.from(host?.querySelectorAll("button")??[])
      .find(button=>button.textContent?.trim()==="Inspect receiver human approval on paired machine (read-only)") as HTMLButtonElement | undefined;
    expect(findButton()?.disabled).toBe(true);
    await act(async()=>{
      if(field){
        Object.getOwnPropertyDescriptor(HTMLInputElement.prototype,"value")?.set?.call(field,approvalId);
        field.dispatchEvent(new Event("input",{bubbles:true}));
      }
    });
    expect(findButton()?.disabled).toBe(false);
    await act(async()=>{findButton()?.click();});
    expect(bridge.invoke).toHaveBeenCalledWith("inspect_remote_receiver_source_approval_desktop",
      {intentId:intent.id,approvalId});
    expect(host?.textContent).toContain("Receiver approval: approved");
    expect(host?.textContent).toContain("src/remote.rs — missing");
    expect(host?.textContent).toContain("Read-only approval evidence only");
    expect(host?.textContent).toContain("Human import supported: false");
    expect(host?.textContent).toContain("does not currently support safe human source installation");
    expect(Array.from(host?.querySelectorAll("button")??[])
      .find(button=>button.textContent?.trim()==="Import the approved source files (create only)")).toBeUndefined();
    expect(bridge.invoke).not.toHaveBeenCalledWith("import_approved_remote_source_desktop",expect.anything());
    platformSupported=true;
    await act(async()=>{findButton()?.click();});
    expect(host?.textContent).toContain("Human import supported: true");
    expect(bridge.invoke).not.toHaveBeenCalledWith("advance_remote_project_sync",expect.anything());
    expect(bridge.invoke).not.toHaveBeenCalledWith("remote_machine_call",expect.anything());
    expect(bridge.invoke).not.toHaveBeenCalledWith("stage_remote_source_chunk",expect.anything());
    const confirmation=vi.spyOn(window,"confirm").mockReturnValue(false);
    try {
      const importButton=()=>Array.from(host?.querySelectorAll("button")??[])
        .find(button=>button.textContent?.trim()==="Import the approved source files (create only)");
      expect(importButton()).toBeDefined();
      await act(async()=>{importButton()?.click();});
      expect(confirmation).toHaveBeenCalledWith(expect.stringContaining(
        "src/remote.rs · 20 bytes · SHA-256 "+"e".repeat(64)));
      expect(bridge.invoke).not.toHaveBeenCalledWith("import_approved_remote_source_desktop",expect.anything());
      confirmation.mockReturnValue(true);
      await act(async()=>{importButton()?.click();});
      expect(bridge.invoke).toHaveBeenCalledWith("import_approved_remote_source_desktop",
        {intentId:intent.id,approvalId});
      expect(bridge.invoke.mock.calls.filter(([name])=>name==="import_approved_remote_source_desktop")).toHaveLength(1);
      expect(host?.textContent).toContain("Import outcome: Installed and verified");
      expect(importButton()).toBeUndefined();
      const verify=Array.from(host?.querySelectorAll("button")??[])
        .find(button=>button.textContent?.trim()==="Verify receiver import receipt and files (read-only)");
      expect(verify).toBeDefined();
      await act(async()=>{verify?.click();});
      expect(host?.textContent).toContain("Verified: imported receipt and all current file hashes");
      expect(bridge.invoke.mock.calls.filter(([name])=>name==="import_approved_remote_source_desktop")).toHaveLength(1);
    } finally { confirmation.mockRestore(); }
  });

  it("discards an in-flight receiver approval result after the human edits and restores the same receipt ID", async () => {
    const original=bridge.invoke.getMockImplementation();
    const approvalId="receiver-source-"+"2".repeat(32);
    const proposed=[{path:"src/race.rs",sha256:"e".repeat(64),bytes:3}];
    const intent={
      id:"source-review-"+"1".repeat(32),createdAt:123,state:"awaitingReceiverApproval",
      nodeId:"node-test-1",localWorkspaceId:"approved-project",
      remoteWorkspaceId:"approved-project",machineIdentityFingerprint:"d".repeat(64),
      connectedAt:1,connectionNonce:"node-0123456789abcdef01234567",
      sourceFingerprint:"f".repeat(64),proposed,
    };
    let complete:((receipt:unknown)=>void)|undefined;
    const pending=new Promise<unknown>(resolve=>{complete=resolve;});
    bridge.invoke.mockImplementation(async(command:string,args?:Record<string,unknown>)=>{
      if(command==="list_remote_source_review_intents_desktop")return [intent];
      if(command==="inspect_remote_receiver_source_approval_desktop"){
        expect(args).toEqual({intentId:intent.id,approvalId});
        return pending;
      }
      return original?.(command,args);
    });
    await display();
    const field=host?.querySelector<HTMLInputElement>(
      `input[aria-label="Receiver approval ID for ${intent.id}"]`);
    const update=async(value:string)=>{
      await act(async()=>{
        if(field){
          Object.getOwnPropertyDescriptor(HTMLInputElement.prototype,"value")?.set?.call(field,value);
          field.dispatchEvent(new Event("input",{bubbles:true}));
        }
      });
    };
    const press=()=>Array.from(host?.querySelectorAll("button")??[])
      .find(button=>button.textContent?.trim()==="Inspect receiver human approval on paired machine (read-only)");
    await update(approvalId);
    await act(async()=>{press()?.click();});
    await update("receiver-source-"+"3".repeat(32));
    await update(approvalId);
    await act(async()=>{complete?.({
      approvalId,state:"approved",credentialActive:true,
      destinationProjectApprovedWritable:true,humanImportSupported:true,canStillProposeCreateOnly:true,
      reviewBinding:{
        remoteWorkspaceId:intent.remoteWorkspaceId,
        receiverMachineFingerprint:intent.machineIdentityFingerprint,
        coordinatorMachineFingerprint:"a".repeat(64),
        coordinatorWorkspaceId:intent.localWorkspaceId,
        coordinatorConnectionNonce:intent.connectionNonce,
        coordinatorSourceFingerprint:intent.sourceFingerprint,
        sourceReviewId:intent.id,
      },
      files:[{path:proposed[0].path,expectedSha256:proposed[0].sha256,
        expectedBytes:proposed[0].bytes,status:"missing"}],
      note:"Read only",
    });});
    expect(host?.textContent).toContain("Receiver approval lookup was stale");
    expect(host?.textContent).not.toContain("Receiver approval: approved");
    expect(bridge.invoke).not.toHaveBeenCalledWith("advance_remote_project_sync",expect.anything());
  });
  it("hides inspected receiver approval after a refresh discovers a different machine connection generation", async () => {
    const original=bridge.invoke.getMockImplementation();
    const intent={
      id:"source-review-"+"1".repeat(32),createdAt:1,
      state:"awaitingReceiverApproval",nodeId:"node-test-1",
      localWorkspaceId:"approved-project",remoteWorkspaceId:"approved-project",
      machineIdentityFingerprint:"d".repeat(64),connectedAt:1,
      connectionNonce:"node-0123456789abcdef01234567",sourceFingerprint:"f".repeat(64),
      proposed:[{path:"src/stale.rs",sha256:"e".repeat(64),bytes:10}],
    };
    const approvalId="receiver-source-"+"2".repeat(32);
    let rotated=false;
    let finish:((value:unknown)=>void)|undefined;
    const pending=new Promise<unknown>(resolve=>{finish=resolve;});
    bridge.invoke.mockImplementation(async(command:string,args?:Record<string,unknown>)=>{
      if(command==="list_remote_source_review_intents_desktop") return [intent];
      if(command==="list_remote_machines") return rotated
        ? [{...nodes[0],connectionNonce:"node-ffffffffffffffffffffffff"}] : nodes;
      if(command==="inspect_remote_receiver_source_approval_desktop"){
        expect(args).toEqual({intentId:intent.id,approvalId});
        return pending;
      }
      return original?.(command,args);
    });
    await display();
    const field=host?.querySelector<HTMLInputElement>(
      `input[aria-label="Receiver approval ID for ${intent.id}"]`);
    await act(async()=>{
      if(field){
        Object.getOwnPropertyDescriptor(HTMLInputElement.prototype,"value")?.set?.call(field,approvalId);
        field.dispatchEvent(new Event("input",{bubbles:true}));
      }
    });
    const press=(label:string)=>Array.from(host?.querySelectorAll("button")??[])
      .find(button=>button.textContent?.trim()===label);
    await act(async()=>{press("Inspect receiver human approval on paired machine (read-only)")?.click();});
    await act(async()=>{finish?.({
      approvalId,state:"approved",credentialActive:true,
      destinationProjectApprovedWritable:true,humanImportSupported:true,canStillProposeCreateOnly:true,
      reviewBinding:{
        remoteWorkspaceId:intent.remoteWorkspaceId,
        receiverMachineFingerprint:intent.machineIdentityFingerprint,
        coordinatorMachineFingerprint:"a".repeat(64),
        coordinatorWorkspaceId:intent.localWorkspaceId,
        coordinatorConnectionNonce:intent.connectionNonce,
        coordinatorSourceFingerprint:intent.sourceFingerprint,
        sourceReviewId:intent.id,
      },
      files:[{path:"src/stale.rs",expectedSha256:"e".repeat(64),
        expectedBytes:10,status:"missing"}],
      note:"Old machine session result",
    });});
    expect(host?.textContent).toContain("Receiver approval: approved");
    rotated=true;
    await act(async()=>{press("Refresh")?.click();});
    expect(host?.textContent).not.toContain("Receiver approval: approved");
    expect(host?.textContent).toContain("historical/disconnected pairing");
    expect(bridge.invoke).not.toHaveBeenCalledWith("remote_machine_call",expect.anything());
    expect(bridge.invoke).not.toHaveBeenCalledWith("advance_remote_project_sync",expect.anything());
  });
  it("never reconciles an old review using a re-paired connection generation", async () => {
    const original=bridge.invoke.getMockImplementation();
    const intent={
      id:"source-review-0123456789abcdef0123456789abcdef",createdAt:12,
      state:"awaitingReceiverApproval",
      localWorkspaceId:"approved-project",remoteWorkspaceId:"approved-project",
      nodeId:"node-test-1",machineIdentityFingerprint:"d".repeat(64),
      connectedAt:1,connectionNonce:"node-aaaaaaaaaaaaaaaaaaaaaaaa",
      sourceFingerprint:"f".repeat(64),
      proposed:[{path:"src/old.rs",sha256:"a".repeat(64),bytes:1}],
    };
    bridge.invoke.mockImplementation(async(command:string,args?:Record<string,unknown>)=>{
      if(command==="list_remote_source_review_intents_desktop") return [intent];
      return original?.(command,args);
    });
    await display();
    const recheck=Array.from(host?.querySelectorAll("button")??[])
      .find((button)=>button.textContent?.trim()==="Recheck saved proposal status (read-only)") as HTMLButtonElement | undefined;
    expect(recheck?.disabled).toBe(true);
    const copy=Array.from(host?.querySelectorAll("button")??[])
      .find((button)=>button.textContent?.trim()==="Copy receiver approval review JSON") as HTMLButtonElement | undefined;
    expect(copy?.disabled).toBe(true);
    expect(host?.textContent).toContain("historical/disconnected pairing");
    expect(bridge.invoke).not.toHaveBeenCalledWith("reconcile_remote_source_review_intent_desktop",expect.anything());
  });

  it("withdraws a saved human review after explicit confirmation and reconciles a lost desktop ACK without replay", async () => {
    const original = bridge.invoke.getMockImplementation();
    const confirm = vi.spyOn(window,"confirm").mockReturnValue(false);
    let writes=0;
    const proposal={
      id:"source-review-0123456789abcdef0123456789abcdef",
      createdAt:123, state:"awaitingReceiverApproval",
      localWorkspaceId:"approved-project",remoteWorkspaceId:"approved-project",
      nodeId:"node-test-1",machineIdentityFingerprint:"d".repeat(64),
      connectedAt:1,connectionNonce:"node-0123456789abcdef01234567",
      sourceFingerprint:"f".repeat(64),
      proposed:[{path:"src/only.rs",sha256:"a".repeat(64),bytes:5}],
    };
    bridge.invoke.mockImplementation(async (command:string,args?:Record<string,unknown>) => {
      if(command==="list_remote_source_review_intents_desktop") return [proposal];
      if(command==="withdraw_remote_source_review_intent_desktop") {
        expect(args).toEqual({intentId:proposal.id});
        writes++;
        proposal.state="withdrawn";
        throw new Error("local receipt acknowledgment lost");
      }
      return original?.(command,args);
    });
    try {
      await display();
      const press=(label:string)=>Array.from(host?.querySelectorAll("button")??[])
        .find((button)=>button.textContent?.trim()===label);
      expect(host?.textContent).toContain("Saved human source-review intents (1)");
      await act(async()=>{press("Withdraw saved source proposal")?.click();});
      expect(writes).toBe(0);
      confirm.mockReturnValue(true);
      await act(async()=>{press("Withdraw saved source proposal")?.click();});
      expect(writes).toBe(1);
      expect(host?.textContent).toContain("withdrawn");
      expect(host?.textContent).toContain("do not retry blindly");
      expect(press("Withdraw saved source proposal")).toBeUndefined();
      expect(bridge.invoke).not.toHaveBeenCalledWith("advance_remote_project_sync",expect.anything());
      expect(bridge.invoke).not.toHaveBeenCalledWith("remote_machine_call",expect.anything());
      await act(async()=>{press("Refresh")?.click();});
      expect(host?.textContent).toContain("withdrawn");
      expect(writes).toBe(1);
    } finally {confirm.mockRestore();}
  });

  it("recovers a lost intent-writing acknowledgment using local journal inspection only", async () => {
    const original = bridge.invoke.getMockImplementation();
    const confirm=vi.spyOn(window,"confirm").mockReturnValue(true);
    let writes=0;
    let recorded=false;
    const proposed=[{path:"src/ack.rs",sha256:"a".repeat(64),bytes:11}];
    bridge.invoke.mockImplementation(async(command:string,args?:Record<string,unknown>)=>{
      if(command==="preview_remote_source_diff_desktop") return {
        nodeId:"node-test-1",localWorkspaceId:"approved-project",remoteWorkspaceId:"approved-project",
        sourceFingerprint:"f".repeat(64),totalSourceFiles:1,cursorIndex:0,nextCursorIndex:1,
        complete:true,sourceVerified:false,
        rows:[{path:"src/ack.rs",status:"missing",localSha256:"a".repeat(64),
          localBytes:11,remoteSha256:null,remoteBytes:null}],note:"Read only",
      };
      if(command==="preflight_remote_create_only_desktop") return {
        nodeId:"node-test-1",localWorkspaceId:"approved-project",remoteWorkspaceId:"approved-project",
        sourceFingerprint:"f".repeat(64),machineIdentityFingerprint:"d".repeat(64),
        connectedAt:1,connectionNonce:"node-0123456789abcdef01234567",inspected:1,proposed,
      };
      if(command==="record_remote_source_review_intent_desktop") {
        writes++; recorded=true; throw new Error("desktop ACK lost");
      }
      if(command==="list_remote_source_review_intents_desktop") return recorded ? [{
        id:"source-review-0123456789abcdef0123456789abcdef",
        createdAt:100,state:"awaitingReceiverApproval",
        localWorkspaceId:"approved-project",remoteWorkspaceId:"approved-project",nodeId:"node-test-1",
        machineIdentityFingerprint:"d".repeat(64),connectedAt:1,
        connectionNonce:"node-0123456789abcdef01234567",sourceFingerprint:"f".repeat(64),proposed,
      }] : [];
      return original?.(command,args);
    });
    try {
      await display();
      const project=host?.querySelector<HTMLSelectElement>("#remote-diff-node-test-1");
      await act(async()=>{if(project){project.value="approved-project";project.dispatchEvent(new Event("change",{bubbles:true}));}});
      const press=(label:string)=>Array.from(host?.querySelectorAll("button")??[])
        .find((button)=>button.textContent?.trim()===label);
      await act(async()=>{press("Preview source differences")?.click();});
      const choice=host?.querySelector<HTMLSelectElement>(
        'select[aria-label="Review decision for src/ack.rs"]');
      await act(async()=>{if(choice){choice.value="proposeCreate";choice.dispatchEvent(new Event("change",{bubbles:true}));}});
      await act(async()=>{press("Preflight next 8 proposed files (read-only)")?.click();});
      await act(async()=>{press("Record reviewed create-only intent (no transfer)")?.click();});
      expect(writes).toBe(1);
      expect(host?.textContent).toContain("Saved human source-review intents (1)");
      expect(host?.textContent).toContain("do not retry blindly");
      expect(bridge.invoke).not.toHaveBeenCalledWith("advance_remote_project_sync",expect.anything());
    } finally {confirm.mockRestore();}
  });
  it("does not resurrect a completed preflight after the source pairing changes mid-request", async () => {
    const original=bridge.invoke.getMockImplementation();
    let pendingRefresh=false;
    let releaseNodes:((value:typeof nodes)=>void)|undefined;
    let releasePreflight:((value:unknown)=>void)|undefined;
    const nextNodes=new Promise<typeof nodes>((resolve)=>{releaseNodes=resolve;});
    const nextPreflight=new Promise<unknown>((resolve)=>{releasePreflight=resolve;});
    bridge.invoke.mockImplementation(async (command:string,args?:Record<string,unknown>)=>{
      if(command==="list_remote_machines" && pendingRefresh) return nextNodes;
      if(command==="preview_remote_source_diff_desktop") return {
        nodeId:"node-test-1",localWorkspaceId:"approved-project",remoteWorkspaceId:"approved-project",
        sourceFingerprint:"f".repeat(64),totalSourceFiles:1,
        cursorIndex:0,nextCursorIndex:1,complete:true,sourceVerified:false,
        rows:[{path:"assets/proposed.bin",status:"missing",localSha256:"a".repeat(64),
          localBytes:4,remoteSha256:null,remoteBytes:null}],note:"Read only",
      };
      if(command==="preflight_remote_create_only_desktop") return nextPreflight;
      return original?.(command,args);
    });
    await display();
    const project=host?.querySelector<HTMLSelectElement>("#remote-diff-node-test-1");
    await act(async()=>{if(project){project.value="approved-project";project.dispatchEvent(new Event("change",{bubbles:true}));}});
    const press=(text:string)=>Array.from(host?.querySelectorAll("button")??[])
      .find((button)=>button.textContent?.trim()===text);
    await act(async()=>{press("Preview source differences")?.click();});
    const choice=host?.querySelector<HTMLSelectElement>(
      'select[aria-label="Review decision for assets/proposed.bin"]');
    await act(async()=>{if(choice){choice.value="proposeCreate";choice.dispatchEvent(new Event("change",{bubbles:true}));}});
    pendingRefresh=true;
    await act(async()=>{press("Refresh")?.click();});
    await act(async()=>{press("Preflight next 8 proposed files (read-only)")?.click();});
    expect(bridge.invoke).toHaveBeenCalledWith("preflight_remote_create_only_desktop",expect.objectContaining({
      expectedConnectionNonce:"node-0123456789abcdef01234567",
    }));
    await act(async()=>{releaseNodes?.([{...nodes[0],connectionNonce:"node-ffffffffffffffffffffffff"}]);});
    await act(async()=>{releasePreflight?.({
      nodeId:"node-test-1",localWorkspaceId:"approved-project",remoteWorkspaceId:"approved-project",
      machineIdentityFingerprint:"d".repeat(64),connectedAt:1,
      connectionNonce:"node-0123456789abcdef01234567",sourceFingerprint:"f".repeat(64),
      inspected:1,proposed:[{path:"assets/proposed.bin",sha256:"a".repeat(64),bytes:4}],
      note:"Old session result",
    });});
    expect(host?.textContent).not.toContain("Create-only preflight: 1 proposed files");
    expect(host?.textContent).not.toContain("assets/proposed.bin");
    expect(host?.textContent).toContain("Create-only preflight failed");
    expect(bridge.invoke).not.toHaveBeenCalledWith("stage_remote_source_chunk",expect.anything());
    expect(bridge.invoke).not.toHaveBeenCalledWith("advance_remote_project_sync",expect.anything());
  });
  it("rejects untrusted source SHA, path, status, and sizes before creating review decisions", async () => {
    const original = bridge.invoke.getMockImplementation();
    const trusted = {
      path:"src/safe.ts",status:"missing",localSha256:"a".repeat(64),
      localBytes:13,remoteSha256:null,remoteBytes:null,
    };
    let supplied:Record<string,unknown> = {...trusted};
    bridge.invoke.mockImplementation(async (command:string,args?:Record<string,unknown>) => {
      if (command==="preview_remote_source_diff_desktop") {
        return {
          nodeId:"node-test-1",localWorkspaceId:"approved-project",remoteWorkspaceId:"approved-project",
          sourceFingerprint:"f".repeat(64),totalSourceFiles:1,
          cursorIndex:0,nextCursorIndex:1,complete:true,sourceVerified:false,
          rows:[supplied],note:"Untrusted MCP source evidence",
        };
      }
      return original?.(command,args);
    });
    await display();
    const select=host?.querySelector<HTMLSelectElement>("#remote-diff-node-test-1");
    await act(async()=>{if(select){select.value="approved-project";select.dispatchEvent(new Event("change",{bubbles:true}));}});
    const preview=()=>Array.from(host?.querySelectorAll("button")??[])
      .find((b)=>b.textContent==="Preview source differences");
    for (const bad of [
      {path:"../escape.txt"}, {path:"src/../escape.txt"},
      {path:"C:/private.txt"}, {path:"src\\evil.txt"},
      {path:"src/evil\u0000file.txt"}, {path:"src/safe.ts",localSha256:"not-sha"},
      {localBytes:NaN}, {localBytes:-1},
      {status:"missing",remoteSha256:"b".repeat(64),remoteBytes:1},
      {status:"identical",remoteSha256:"b".repeat(64),remoteBytes:13},
      {status:"conflict",remoteSha256:null,remoteBytes:null},
      {status:"unknown"},
    ]) {
      supplied={...trusted,...bad};
      await act(async()=>{preview()?.click();});
      expect(host?.textContent).toContain("Source comparison failed");
      expect(host?.textContent).not.toContain("src/safe.ts");
      expect(host?.textContent).not.toContain("Copy source conflict review draft");
    }
    supplied={...trusted};
    await act(async()=>{preview()?.click();});
    expect(host?.textContent).toContain("src/safe.ts");
    expect(host?.textContent).toContain("Copy source conflict review draft");
    expect(bridge.invoke).not.toHaveBeenCalledWith("remote_machine_call",expect.anything());
    expect(bridge.invoke).not.toHaveBeenCalledWith("advance_remote_project_sync",expect.anything());
  });

  it("refuses repeated and case-aliased source paths across pages including identical files", async () => {
    const original=bridge.invoke.getMockImplementation();
    bridge.invoke.mockImplementation(async (command:string,args?:Record<string,unknown>)=>{
      if(command==="preview_remote_source_diff_desktop"){
        const cursor=Number(args?.cursorIndex??0);
        return {
          nodeId:"node-test-1",localWorkspaceId:"approved-project",remoteWorkspaceId:"approved-project",
          sourceFingerprint:"f".repeat(64),totalSourceFiles:2,
          cursorIndex:cursor,nextCursorIndex:cursor+1,complete:cursor===1,sourceVerified:false,
          rows:[cursor===0
            ? {path:"Src/Main.ts",status:"identical",localSha256:"a".repeat(64),localBytes:5,
               remoteSha256:"a".repeat(64),remoteBytes:5}
            : {path:"src/main.TS",status:"missing",localSha256:"b".repeat(64),localBytes:9,
               remoteSha256:null,remoteBytes:null}],
          note:"Read-only source preview",
        };
      }
      return original?.(command,args);
    });
    await display();
    const select=host?.querySelector<HTMLSelectElement>("#remote-diff-node-test-1");
    await act(async()=>{if(select){select.value="approved-project";select.dispatchEvent(new Event("change",{bubbles:true}));}});
    const press=(name:string)=>Array.from(host?.querySelectorAll("button")??[]).find((b)=>b.textContent===name);
    await act(async()=>{press("Preview source differences")?.click();});
    expect(host?.textContent).toContain("Inspected 1 files");
    await act(async()=>{press("Next 8 files")?.click();});
    expect(host?.textContent).toContain("Source comparison failed");
    expect(host?.textContent).not.toContain("Inspected 1 files");
    expect(host?.textContent).not.toContain("Copy source conflict review draft");
    expect(bridge.invoke).not.toHaveBeenCalledWith("remote_machine_call",expect.anything());
    expect(bridge.invoke).not.toHaveBeenCalledWith("advance_remote_project_sync",expect.anything());
  });

  it("scans bounded 8-page read-only batches with exact source cursor and preserves reviewed issues", async () => {
    const original = bridge.invoke.getMockImplementation();
    bridge.invoke.mockImplementation(async (command:string,args?:Record<string,unknown>) => {
      if (command === "preview_remote_source_diff_desktop") {
        const cursor = Number(args?.cursorIndex ?? 0);
        expect(args?.expectedSourceSha256).toBe(cursor ? "4".repeat(64) : null);
        const next = Math.min(cursor + 8, 73);
        return {
          nodeId:"node-test-1",localWorkspaceId:"approved-project",remoteWorkspaceId:"approved-project",
          sourceFingerprint:"4".repeat(64),totalSourceFiles:73,
          cursorIndex:cursor,nextCursorIndex:next,complete:next===73,sourceVerified:false,
          rows:Array.from({length:next-cursor},(_,i)=>({
            path:`source/issue-${cursor+i}.txt`,status:"missing",localSha256:"e".repeat(64),
            localBytes:1,remoteSha256:null,remoteBytes:null,
          })),note:"Read-only inspection",
        };
      }
      return original?.(command,args);
    });
    await display();
    const select = host?.querySelector<HTMLSelectElement>("#remote-diff-node-test-1");
    await act(async () => {
      if (select) { select.value="approved-project";select.dispatchEvent(new Event("change",{bubbles:true})); }
    });
    const press = (name:string) => Array.from(host?.querySelectorAll("button") ?? [])
      .find((item)=>item.textContent === name);
    await act(async () => {press("Scan next 64 files (read-only)")?.click();});
    let calls = bridge.invoke.mock.calls.filter(([name])=>name==="preview_remote_source_diff_desktop");
    expect(calls.map(([,args])=>args.cursorIndex)).toEqual([0,8,16,24,32,40,48,56]);
    expect(host?.textContent).toContain("Inspected 64 files");
    expect(host?.textContent).toContain("64 missing");
    expect(host?.textContent).toContain("Partial preview");
    await act(async () => {press("Scan next 64 files (read-only)")?.click();});
    calls = bridge.invoke.mock.calls.filter(([name])=>name==="preview_remote_source_diff_desktop");
    expect(calls.map(([,args])=>args.cursorIndex)).toEqual([0,8,16,24,32,40,48,56,64,72]);
    expect(host?.textContent).toContain("73 missing");
    expect(host?.textContent).toContain("Issues 1–40 of 73");
    expect(press("Scan next 64 files (read-only)")?.getAttribute("disabled")).not.toBeNull();
    await act(async () => {press("Next issues")?.click();});
    expect(host?.textContent).toContain("source/issue-72.txt");
    expect(bridge.invoke).not.toHaveBeenCalledWith("advance_remote_project_sync",expect.anything());
    expect(bridge.invoke).not.toHaveBeenCalledWith("remote_machine_call",expect.anything());
  });

  it("stops an active bounded source scan after its current page without replaying mutations", async () => {
    const original = bridge.invoke.getMockImplementation();
    let finishFirst: ((value:unknown)=>void) | undefined;
    bridge.invoke.mockImplementation((command:string,args?:Record<string,unknown>) => {
      if (command === "preview_remote_source_diff_desktop") {
        expect(args?.cursorIndex).toBe(0);
        return new Promise((resolve)=>{ finishFirst=resolve; });
      }
      return original?.(command,args);
    });
    await display();
    const select = host?.querySelector<HTMLSelectElement>("#remote-diff-node-test-1");
    await act(async () => {
      if (select) { select.value="approved-project";select.dispatchEvent(new Event("change",{bubbles:true})); }
    });
    const press = (name:string) => Array.from(host?.querySelectorAll("button") ?? [])
      .find((item)=>item.textContent === name);
    await act(async () => {press("Scan next 64 files (read-only)")?.click();});
    expect(press("Stop after current page")).toBeTruthy();
    await act(async () => {press("Stop after current page")?.click();});
    expect(host?.textContent).toContain("Stop requested");
    await act(async () => {finishFirst?.({
      nodeId:"node-test-1",localWorkspaceId:"approved-project",remoteWorkspaceId:"approved-project",
      sourceFingerprint:"5".repeat(64),totalSourceFiles:24,
      cursorIndex:0,nextCursorIndex:8,complete:false,sourceVerified:false,
      rows:Array.from({length:8},(_,i)=>({
        path:`src/file-${i}.txt`,status:"missing",localSha256:"e".repeat(64),localBytes:2,
        remoteSha256:null,remoteBytes:null,
      })),note:"Read-only",
    });});
    expect(bridge.invoke.mock.calls.filter(([name])=>name==="preview_remote_source_diff_desktop")).toHaveLength(1);
    expect(host?.textContent).toContain("Inspected 8 files");
    expect(host?.textContent).toContain("scan stopped after its latest verified page");
    expect(press("Stop after current page")).toBeUndefined();
    expect(bridge.invoke).not.toHaveBeenCalledWith("advance_remote_project_sync",expect.anything());
  });

  it("does not request another remote source page after Computers unmounts", async () => {
    const original = bridge.invoke.getMockImplementation();
    let finishFirst: ((value:unknown)=>void) | undefined;
    bridge.invoke.mockImplementation((command:string,args?:Record<string,unknown>)=>{
      if (command==="preview_remote_source_diff_desktop") {
        expect(args?.cursorIndex).toBe(0);
        return new Promise((resolve)=>{finishFirst=resolve;});
      }
      return original?.(command,args);
    });
    await display();
    const select=host?.querySelector<HTMLSelectElement>("#remote-diff-node-test-1");
    await act(async()=>{if(select){select.value="approved-project";select.dispatchEvent(new Event("change",{bubbles:true}));}});
    const button=Array.from(host?.querySelectorAll("button")??[])
      .find((item)=>item.textContent==="Scan next 64 files (read-only)");
    await act(async()=>{button?.click();});
    expect(finishFirst).toBeTypeOf("function");
    await act(async()=>{root?.unmount();});
    root=null;
    await act(async()=>{finishFirst?.({
      nodeId:"node-test-1",localWorkspaceId:"approved-project",remoteWorkspaceId:"approved-project",
      sourceFingerprint:"6".repeat(64),totalSourceFiles:24,cursorIndex:0,nextCursorIndex:8,
      complete:false,sourceVerified:false,
      rows:Array.from({length:8},(_,i)=>({
        path:`src/unmounted-${i}.txt`,status:"missing",localSha256:"7".repeat(64),localBytes:1,
        remoteSha256:null,remoteBytes:null,
      })),note:"Read-only",
    });});
    expect(bridge.invoke.mock.calls.filter(([name])=>name==="preview_remote_source_diff_desktop")).toHaveLength(1);
    expect(bridge.invoke).not.toHaveBeenCalledWith("advance_remote_project_sync",expect.anything());
  });

  it("clears accumulated scan history if a later automatic page changes source identity", async () => {
    const original=bridge.invoke.getMockImplementation();
    bridge.invoke.mockImplementation(async (command:string,args?:Record<string,unknown>)=>{
      if (command==="preview_remote_source_diff_desktop") {
        const cursor=Number(args?.cursorIndex??0);
        const wrong=cursor===8;
        return {
          nodeId:"node-test-1",localWorkspaceId:"approved-project",remoteWorkspaceId:"approved-project",
          sourceFingerprint:wrong?"7".repeat(64):"8".repeat(64),totalSourceFiles:24,
          cursorIndex:cursor,nextCursorIndex:cursor+8,complete:false,sourceVerified:false,
          rows:Array.from({length:8},(_,i)=>({
            path:`src/stale-${cursor+i}.txt`,status:"conflict",localSha256:"e".repeat(64),localBytes:1,
            remoteSha256:"9".repeat(64),remoteBytes:2,
          })),note:"Read-only",
        };
      }
      return original?.(command,args);
    });
    await display();
    const select=host?.querySelector<HTMLSelectElement>("#remote-diff-node-test-1");
    await act(async()=>{if(select){select.value="approved-project";select.dispatchEvent(new Event("change",{bubbles:true}));}});
    const button=Array.from(host?.querySelectorAll("button")??[])
      .find((item)=>item.textContent==="Scan next 64 files (read-only)");
    await act(async()=>{button?.click();});
    expect(host?.textContent).toContain("Restart the read-only comparison");
    expect(host?.textContent).not.toContain("src/stale-0.txt");
    expect(bridge.invoke.mock.calls.filter(([name])=>name==="preview_remote_source_diff_desktop")).toHaveLength(2);
    expect(bridge.invoke).not.toHaveBeenCalledWith("advance_remote_project_sync",expect.anything());
  });

  it("clears stale partial source results after a failed continuation", async () => {
    const original = bridge.invoke.getMockImplementation();
    bridge.invoke.mockImplementation(async (command:string,args?:Record<string,unknown>) => {
      if (command === "preview_remote_source_diff_desktop") {
        if (args?.cursorIndex) throw new Error("Local source fingerprint changed");
        return {
          nodeId:"node-test-1",localWorkspaceId:"approved-project",remoteWorkspaceId:"approved-project",
          sourceFingerprint:"9".repeat(64),totalSourceFiles:12,cursorIndex:0,nextCursorIndex:8,
          complete:false,sourceVerified:false,
          rows:Array.from({length:8},(_,i)=>({
            path:`source/stale-${i}.txt`,status:"conflict",localSha256:"8".repeat(64),
            localBytes:1,remoteSha256:"9".repeat(64),remoteBytes:2,
          })),note:"Before local edit",
        };
      }
      return original?.(command,args);
    });
    await display();
    const select = host?.querySelector<HTMLSelectElement>("#remote-diff-node-test-1");
    await act(async () => {
      if (select) { select.value="approved-project"; select.dispatchEvent(new Event("change",{bubbles:true})); }
    });
    const press = (name:string) => Array.from(host?.querySelectorAll("button") ?? [])
      .find((button)=>button.textContent === name);
    await act(async()=>{press("Preview source differences")?.click();});
    expect(host?.textContent).toContain("source/stale-0.txt");
    await act(async()=>{press("Next 8 files")?.click();});
    expect(host?.textContent).not.toContain("source/stale-0.txt");
    expect(host?.textContent).toContain("Local source fingerprint changed");
    expect(host?.textContent).toContain("Restart the read-only comparison");
    expect(bridge.invoke).not.toHaveBeenCalledWith("advance_remote_project_sync",expect.anything());
  });
  it("stops read-only parity monitoring and removes stale verification when remote hashes differ", async () => {
    let checks = 0;
    bridge.invoke.mockImplementation(async (command:string) => {
      if (command === "list_remote_machines") return nodes;
      if (command === "list_workspaces") return [{id:"approved-project",name:"Approved Project"}];
      if (command === "list_remembered_remote_machines"
          || command === "list_downloaded_remote_artifacts"
          || command === "list_remote_machine_access_grants"
          || command === "list_remote_native_test_runs_desktop") return [];
      if (command === "preview_remote_source_diff_desktop") {
        checks += 1;
        return {
          nodeId:"node-test-1",localWorkspaceId:"approved-project",
          remoteWorkspaceId:"approved-project",sourceFingerprint:"a".repeat(64),
          totalSourceFiles:0,cursorIndex:0,nextCursorIndex:0,complete:true,
          sourceVerified:checks === 1,rows:[],note:"Read-only aggregate result",
        };
      }
      return undefined;
    });
    await display();
    const select = host?.querySelector<HTMLSelectElement>("#remote-diff-node-test-1");
    await act(async () => {
      if(select){select.value="approved-project";select.dispatchEvent(new Event("change",{bubbles:true}));}
    });
    const preview = Array.from(host?.querySelectorAll("button") ?? [])
      .find((item)=>item.textContent==="Preview source differences");
    await act(async () => {preview?.click();});
    expect(host?.textContent).toContain("Complete aggregate source parity verified");
    const watch = Array.from(host?.querySelectorAll("input[type=checkbox]") ?? [])
      .find((item)=>item.parentElement?.textContent?.includes("Monitor source parity")) as HTMLInputElement;
    expect(watch.disabled).toBe(false);
    await act(async () => {watch.click();});
    expect(checks).toBe(2);
    expect(watch.checked).toBe(false);
    expect(host?.textContent).toContain("Source parity LOST");
    expect(host?.textContent).not.toContain("Complete aggregate source parity verified");
    expect(bridge.invoke).not.toHaveBeenCalledWith("advance_remote_project_sync", expect.anything());
    expect(bridge.invoke).not.toHaveBeenCalledWith("remote_machine_call", expect.anything());
  });
  it("ignores out-of-order refresh results rather than restoring an older connection generation", async () => {
    const original = bridge.invoke.getMockImplementation();
    let refreshes=0;
    let releaseOld:((value:typeof nodes)=>void)|undefined;
    let releaseNew:((value:typeof nodes)=>void)|undefined;
    const oldRefresh=new Promise<typeof nodes>((resolve)=>{releaseOld=resolve;});
    const newRefresh=new Promise<typeof nodes>((resolve)=>{releaseNew=resolve;});
    bridge.invoke.mockImplementation(async (command:string,args?:Record<string,unknown>)=>{
      if(command==="list_remote_machines"){
        refreshes++;
        return refreshes===1 ? nodes : refreshes===2 ? oldRefresh : newRefresh;
      }
      if(command==="preview_remote_source_diff_desktop") return {
        nodeId:"node-test-1",localWorkspaceId:"approved-project",remoteWorkspaceId:"approved-project",
        sourceFingerprint:"f".repeat(64),totalSourceFiles:0,
        cursorIndex:0,nextCursorIndex:0,complete:true,sourceVerified:true,
        rows:[],note:"Read only",
      };
      return original?.(command,args);
    });
    await display();
    const press=(name:string)=>Array.from(host?.querySelectorAll("button")??[])
      .find((button)=>button.textContent?.trim()===name);
    await act(async()=>{press("Refresh")?.click();press("Refresh")?.click();});
    await act(async()=>{releaseNew?.([{...nodes[0],connectionNonce:"node-ffffffffffffffffffffffff"}]);});
    await act(async()=>{releaseOld?.(nodes);});
    const selector=host?.querySelector<HTMLSelectElement>("#remote-diff-node-test-1");
    await act(async()=>{if(selector){selector.value="approved-project";selector.dispatchEvent(new Event("change",{bubbles:true}));}});
    await act(async()=>{press("Preview source differences")?.click();});
    const preview=bridge.invoke.mock.calls.filter(([command])=>command==="preview_remote_source_diff_desktop");
    expect(preview).toHaveLength(1);
    expect(preview[0][1]).toMatchObject({expectedConnectionNonce:"node-ffffffffffffffffffffffff"});
    expect(host?.textContent).toContain("Complete aggregate source parity verified");
    expect(bridge.invoke).not.toHaveBeenCalledWith("advance_remote_project_sync",expect.anything());
  });
  it("discards an in-flight source page after a refresh discovers a re-pair", async () => {
    const original=bridge.invoke.getMockImplementation();
    let refreshPending=false;
    let releaseNodes:((value:typeof nodes)=>void)|undefined;
    let releasePage:((value:unknown)=>void)|undefined;
    const pendingNodes=new Promise<typeof nodes>((resolve)=>{releaseNodes=resolve;});
    const pendingPage=new Promise<unknown>((resolve)=>{releasePage=resolve;});
    bridge.invoke.mockImplementation(async (command:string,args?:Record<string,unknown>)=>{
      if(command==="list_remote_machines" && refreshPending) return pendingNodes;
      if(command==="preview_remote_source_diff_desktop") return pendingPage;
      return original?.(command,args);
    });
    await display();
    const select=host?.querySelector<HTMLSelectElement>("#remote-diff-node-test-1");
    await act(async()=>{if(select){select.value="approved-project";select.dispatchEvent(new Event("change",{bubbles:true}));}});
    const press=(name:string)=>Array.from(host?.querySelectorAll("button")??[])
      .find((button)=>button.textContent?.trim()===name);
    refreshPending=true;
    await act(async()=>{press("Refresh")?.click();});
    await act(async()=>{press("Preview source differences")?.click();});
    expect(bridge.invoke).toHaveBeenCalledWith("preview_remote_source_diff_desktop",expect.objectContaining({
      expectedConnectionNonce:"node-0123456789abcdef01234567",
    }));
    await act(async()=>{releaseNodes?.([{...nodes[0],connectionNonce:"node-ffffffffffffffffffffffff"}]);});
    await act(async()=>{releasePage?.({
      nodeId:"node-test-1",localWorkspaceId:"approved-project",remoteWorkspaceId:"approved-project",
      sourceFingerprint:"f".repeat(64),totalSourceFiles:1,
      cursorIndex:0,nextCursorIndex:1,complete:true,sourceVerified:false,
      rows:[{path:"stale.txt",status:"missing",localSha256:"a".repeat(64),localBytes:1,
        remoteSha256:null,remoteBytes:null}],note:"Old connection result",
    });});
    expect(host?.textContent).toContain("Source comparison failed");
    expect(host?.textContent).not.toContain("stale.txt");
    expect(host?.textContent).not.toContain("Copy source conflict review draft");
    expect(bridge.invoke).not.toHaveBeenCalledWith("preflight_remote_create_only_desktop",expect.anything());
    expect(bridge.invoke).not.toHaveBeenCalledWith("advance_remote_project_sync",expect.anything());
  });
  it("stops opt-in parity watch and invalidates old review when pinned machine identity changes", async () => {
    const original = bridge.invoke.getMockImplementation();
    let identityRotated = false;
    let checks = 0;
    bridge.invoke.mockImplementation(async (command:string,args?:Record<string,unknown>) => {
      if (command === "list_remote_machines") return identityRotated
        ? [{...nodes[0], identityFingerprint:"e".repeat(64)}] : nodes;
      if (command === "preview_remote_source_diff_desktop") {
        checks += 1;
        return {
          nodeId:"node-test-1",localWorkspaceId:"approved-project",
          remoteWorkspaceId:"approved-project",sourceFingerprint:"a".repeat(64),
          totalSourceFiles:0,cursorIndex:0,nextCursorIndex:0,complete:true,
          sourceVerified:true,rows:[],note:"Read-only aggregate parity",
        };
      }
      return original?.(command,args);
    });
    await display();
    const select=host?.querySelector<HTMLSelectElement>("#remote-diff-node-test-1");
    await act(async()=>{if(select){select.value="approved-project";select.dispatchEvent(new Event("change",{bubbles:true}));}});
    const press=(name:string)=>Array.from(host?.querySelectorAll("button")??[]).find((button)=>button.textContent?.trim()===name);
    await act(async()=>{press("Preview source differences")?.click();});
    expect(host?.textContent).toContain("Complete aggregate source parity verified");
    const watch=Array.from(host?.querySelectorAll("input[type=checkbox]")??[])
      .find((input)=>input.parentElement?.textContent?.includes("Monitor source parity")) as HTMLInputElement;
    await act(async()=>{watch.click();});
    expect(watch.checked).toBe(true);
    const beforeRefresh=checks;
    identityRotated=true;
    await act(async()=>{press("Refresh")?.click();});
    expect(checks).toBe(beforeRefresh);
    expect(watch.checked).toBe(false);
    expect(host?.textContent).toContain("paired machine identity or approved project changed");
    expect(host?.textContent).not.toContain("Complete aggregate source parity verified");
    expect(bridge.invoke).not.toHaveBeenCalledWith("advance_remote_project_sync",expect.anything());
    expect(bridge.invoke).not.toHaveBeenCalledWith("remote_machine_call",expect.anything());
  });
  it("invalidates parity monitoring after a same-second re-pair with unchanged signing key", async () => {
    const original=bridge.invoke.getMockImplementation();
    let sameSecondRepaired=false;
    let checks=0;
    bridge.invoke.mockImplementation(async (command:string,args?:Record<string,unknown>)=>{
      if(command==="list_remote_machines") return sameSecondRepaired
        ? [{...nodes[0],connectionNonce:"node-ffffffffffffffffffffffff"}] : nodes;
      if(command==="preview_remote_source_diff_desktop"){
        checks++;
        return {
          nodeId:"node-test-1",localWorkspaceId:"approved-project",
          remoteWorkspaceId:"approved-project",sourceFingerprint:"a".repeat(64),
          totalSourceFiles:0,cursorIndex:0,nextCursorIndex:0,complete:true,
          sourceVerified:true,rows:[],note:"Read-only",
        };
      }
      return original?.(command,args);
    });
    await display();
    const project=host?.querySelector<HTMLSelectElement>("#remote-diff-node-test-1");
    await act(async()=>{if(project){project.value="approved-project";project.dispatchEvent(new Event("change",{bubbles:true}));}});
    const press=(text:string)=>Array.from(host?.querySelectorAll("button")??[]).find((button)=>button.textContent?.trim()===text);
    await act(async()=>{press("Preview source differences")?.click();});
    const watch=Array.from(host?.querySelectorAll("input[type=checkbox]")??[])
      .find((input)=>input.parentElement?.textContent?.includes("Monitor source parity")) as HTMLInputElement;
    await act(async()=>{watch.click();});
    expect(watch.checked).toBe(true);
    const previous=checks;
    sameSecondRepaired=true;
    await act(async()=>{press("Refresh")?.click();});
    expect(checks).toBe(previous);
    expect(watch.checked).toBe(false);
    expect(host?.textContent).toContain("paired machine identity or approved project changed");
    expect(host?.textContent).not.toContain("Complete aggregate source parity verified");
    expect(bridge.invoke).not.toHaveBeenCalledWith("advance_remote_project_sync",expect.anything());
  });
  it("issues a credential only after a user click, then hides it", async () => {
    await display();
    expect(bridge.invoke).not.toHaveBeenCalledWith("issue_remote_machine_access");
    const select = host?.querySelector<HTMLSelectElement>("select");
    expect(select).not.toBeNull();
    await act(async () => {
      if (select) { select.value = "approved-project"; select.dispatchEvent(new Event("change", { bubbles: true })); }
    });
    const issue = Array.from(host?.querySelectorAll("button") ?? [])
      .find((button) => button.textContent?.includes("Issue 24-hour"));
    await act(async () => { issue?.dispatchEvent(new MouseEvent("click", { bubbles: true })); });
    expect(bridge.invoke).toHaveBeenCalledWith("issue_remote_machine_access", {
      workspaceId: "approved-project", scopes: ["browser", "desktop"],
    });
    expect(host?.querySelector<HTMLTextAreaElement>("#remote-issued-key")?.value).toBe("a".repeat(64));
    expect(host?.textContent).toContain("Approved remote project ID");
    const hide = Array.from(host?.querySelectorAll("button") ?? [])
      .find((button) => button.textContent?.includes("Hide credential"));
    await act(async () => { hide?.dispatchEvent(new MouseEvent("click", { bubbles: true })); });
    expect(host?.querySelector("#remote-issued-key")).toBeNull();
  });
  it("inspects exact saved native process identity without replay, cancel or forged GUI success", async () => {
    bridge.invoke.mockImplementation(async (command: string) => {
      if (command === "list_remote_machines") return nodes;
      if (command === "list_workspaces") return [{id:"approved-project",name:"Approved Project"}];
      if (command === "list_downloaded_remote_artifacts" || command === "list_remote_machine_access_grants") return [];
      if (command === "list_remote_native_test_runs_desktop") return [{
        id:"native-test-1",createdAt:123,localWorkspaceId:"approved-project",
        dispatchComplete:true,limitations:"Physical GUI not verified",
        jobs:[{platform:"windows",nodeId:"node-test-1",remoteWorkspaceId:"approved-project",
          processId:"process-real",state:"submitted",evidence:"awaiting exit"}],
      }];
      if (command === "reconcile_remote_native_test_job_desktop") return {
        id:"native-test-1",createdAt:123,localWorkspaceId:"approved-project",
        dispatchComplete:true,limitations:"Physical GUI not verified",
        jobs:[{platform:"windows",nodeId:"node-test-1",remoteWorkspaceId:"approved-project",
          processId:"process-real",state:"commandExitedZero",evidence:"Observed remote managed process status exited, exit code 0."}],
      };
      return undefined;
    });
    await display();
    const inspect = Array.from(host?.querySelectorAll("button") ?? [])
      .find((item) => item.textContent === "Inspect live status");
    expect(inspect).toBeDefined();
    await act(async () => { inspect?.dispatchEvent(new MouseEvent("click",{bubbles:true})); });
    expect(bridge.invoke).toHaveBeenCalledWith("reconcile_remote_native_test_job_desktop",{
      runId:"native-test-1",jobIndex:0,expectedProcessId:"process-real",
    });
    expect(host?.textContent).toContain("commandExitedZero");
    expect(host?.textContent).toContain("Verified remote process receipt saved");
    expect(host?.textContent).toContain("Native GUI acceptance remains unverified");
    for (const forbidden of ["stop_process", "start_process", "cancel_remote_native_test_job",
      "refresh_remote_native_test_run", "remote_machine_call"]) {
      expect(bridge.invoke).not.toHaveBeenCalledWith(forbidden, expect.anything());
    }
  });
  it("requires explicit confirmation and sends only one durable native job stop request", async () => {
    const consent = vi.spyOn(window, "confirm").mockReturnValue(false);
    bridge.invoke.mockImplementation(async (command: string) => {
      if (command === "list_remote_machines") return nodes;
      if (command === "list_workspaces") return [{id:"approved-project",name:"Approved Project"}];
      if (command === "list_downloaded_remote_artifacts" || command === "list_remote_machine_access_grants") return [];
      const run = {
        id:"native-stop-1",createdAt:123,localWorkspaceId:"approved-project",dispatchComplete:true,
        limitations:"Native GUI not verified",
        jobs:[{platform:"linux",nodeId:"node-test-1",remoteWorkspaceId:"approved-project",
          processId:"process-5",state:"running",evidence:"Running"}],
      };
      if (command === "list_remote_native_test_runs_desktop") return [run];
      if (command === "cancel_remote_native_test_job_desktop") return {
        ...run,jobs:[{...run.jobs[0],state:"cancelOutcomeUnknown",
          evidence:"Stop acknowledgement lost. Never replay automatically."}],
      };
      return undefined;
    });
    try {
      await display();
      const button = () => Array.from(host?.querySelectorAll("button") ?? [])
        .find((item) => item.textContent === "Stop native job…");
      expect(button()).toBeDefined();
      await act(async () => { button()?.dispatchEvent(new MouseEvent("click",{bubbles:true})); });
      expect(bridge.invoke).not.toHaveBeenCalledWith("cancel_remote_native_test_job_desktop",expect.anything());
      consent.mockReturnValue(true);
      await act(async () => { button()?.dispatchEvent(new MouseEvent("click",{bubbles:true})); });
      expect(bridge.invoke).toHaveBeenCalledWith("cancel_remote_native_test_job_desktop",{
        runId:"native-stop-1",jobIndex:0,expectedProcessId:"process-5",
      });
      expect(host?.textContent).toContain("cancelOutcomeUnknown");
      expect(host?.textContent).toContain("Never replay automatically");
      expect(button()?.disabled).toBe(true);
      expect(bridge.invoke).not.toHaveBeenCalledWith("remote_machine_call",expect.anything());
      expect(bridge.invoke).not.toHaveBeenCalledWith("stop_process",expect.anything());
    } finally {consent.mockRestore();}
  });
  it("shows bounded persisted native test outcomes without sending remote commands", async () => {
    bridge.invoke.mockImplementation(async (command: string) => {
      if (command === "list_remote_machines") return nodes;
      if (command === "list_workspaces") return [{ id: "approved-project", name: "Approved Project" }];
      if (command === "list_downloaded_remote_artifacts") return [];
      if (command === "list_remote_machine_access_grants") return [];
      if (command === "list_remote_native_test_runs_desktop") return [{
        id: "native-test1", createdAt: 1000000, localWorkspaceId: "local-project",
        dispatchComplete: true, limitations: "Native GUI acceptance not verified",
        jobs: [{ platform: "macos", nodeId: "node-test-1", remoteWorkspaceId: "approved-project",
          processId: "process-1", state: "cancelOutcomeUnknown", evidence: "Lost stop receipt",
          cancellationObservation: "Read-only remote inspection: recorded process is stopped",
          cancellationObservedAt: 1000010 }],
      }];
      return undefined;
    });
    await display();
    expect(host?.textContent).toContain("Native test history");
    expect(host?.textContent).toContain("cancelOutcomeUnknown");
    expect(host?.textContent).toContain("Lost stop receipt");
    expect(host?.textContent).toContain("Read-only remote inspection");
    expect(host?.textContent).toContain("Native GUI acceptance not verified");
    expect(bridge.invoke).not.toHaveBeenCalledWith("remote_machine_call", expect.anything());
    expect(bridge.invoke).not.toHaveBeenCalledWith("cancel_remote_native_test_job", expect.anything());
  });
  it("revokes only the selected credential by full fingerprint, without exposing bearer", async () => {
    const grants = [
      {fingerprint:"b".repeat(64),expiresAt:2000000,workspaceId:"approved-project",scopes:["browser"]},
      {fingerprint:"c".repeat(64),expiresAt:2000000,workspaceId:"another-project",scopes:["desktop"]},
    ];
    bridge.invoke.mockImplementation(async (command: string, args?: Record<string,unknown>) => {
      if (command === "list_remote_machines") return nodes;
      if (command === "list_workspaces") return [{ id:"approved-project",name:"Approved Project" }];
      if (command === "list_downloaded_remote_artifacts") return [];
      if (command === "list_remote_native_test_runs_desktop") return [];
      if (command === "list_remote_machine_access_grants") return grants;
      if (command === "revoke_remote_machine_access_grant") {
        expect(args).toEqual({fingerprint:"b".repeat(64)});
        grants.splice(0, 1);
        return true;
      }
      return undefined;
    });
    const previousConfirm = window.confirm;
    window.confirm = vi.fn(() => true);
    try {
      await display();
      expect(host?.textContent).toContain("Active issued credentials (2)");
      expect(host?.textContent).not.toContain("b".repeat(64));
      const revoke = Array.from(host?.querySelectorAll("button") ?? [])
        .filter((button) => button.textContent === "Revoke this credential");
      expect(revoke).toHaveLength(2);
      await act(async () => { revoke[0]?.dispatchEvent(new MouseEvent("click", { bubbles:true })); });
      expect(bridge.invoke).toHaveBeenCalledWith("revoke_remote_machine_access_grant",
        {fingerprint:"b".repeat(64)});
      expect(host?.textContent).toContain("Active issued credentials (1)");
      expect(host?.textContent).toContain("another-project");
      expect(host?.textContent).toContain("Selected remote credential revoked");
      expect(bridge.invoke).not.toHaveBeenCalledWith("revoke_remote_machine_access");
    } finally { window.confirm = previousConfirm; }
  });
  it("rotates only the chosen grant with explicit human approval and shows the new one-time credential", async () => {
    const grants = [
      { fingerprint: "b".repeat(64), expiresAt: 2000000, workspaceId: "approved-project", scopes:["browser"] },
      { fingerprint: "c".repeat(64), expiresAt: 2000000, workspaceId: "other-project", scopes:["source"] },
    ];
    const newSecret = "d3".repeat(32);
    bridge.invoke.mockImplementation(async (command: string, args?: Record<string,unknown>) => {
      if (command === "list_remote_machines") return nodes;
      if (command === "list_workspaces") return [{ id:"approved-project", name:"Approved Project" }];
      if (command === "list_downloaded_remote_artifacts") return [];
      if (command === "list_remote_native_test_runs_desktop") return [];
      if (command === "list_remote_machine_access_grants") return grants;
      if (command === "rotate_remote_machine_access_grant") {
        expect(args).toEqual({ fingerprint: "b".repeat(64) });
        grants[0] = { ...grants[0], fingerprint: "e".repeat(64), expiresAt: 3000000 };
        return { secret: newSecret, expiresAt: 3000000, workspaceId: "approved-project",
          warning: "The old credential and its sessions are invalid" };
      }
      return undefined;
    });
    const previousConfirm = window.confirm;
    window.confirm = vi.fn(() => true);
    try {
      await display();
      expect(bridge.invoke).not.toHaveBeenCalledWith("rotate_remote_machine_access_grant", expect.anything());
      const rotate = Array.from(host?.querySelectorAll("button") ?? [])
        .filter((button) => button.textContent === "Rotate credential");
      expect(rotate).toHaveLength(2);
      await act(async () => { rotate[0]?.dispatchEvent(new MouseEvent("click", { bubbles:true })); });
      expect(window.confirm).toHaveBeenCalledTimes(1);
      expect(bridge.invoke).toHaveBeenCalledWith("rotate_remote_machine_access_grant", { fingerprint:"b".repeat(64) });
      expect(host?.querySelector<HTMLTextAreaElement>("#remote-issued-key")?.value).toBe(newSecret);
      expect(host?.textContent).toContain("The old credential and its sessions are invalid");
      expect(host?.textContent).toContain("other-project");
      expect(bridge.invoke).not.toHaveBeenCalledWith("revoke_remote_machine_access");
    } finally { window.confirm = previousConfirm; }
  });
  it("re-pairs an idle remote only after an explicit credential entry, then clears the secret", async () => {
    bridge.invoke.mockImplementation(async (command: string, args?: Record<string,unknown>) => {
      if (command === "list_remote_machines") return nodes;
      if (command === "list_workspaces") return [{ id:"approved-project", name:"Approved Project" }];
      if (command === "list_remote_machine_access_grants") return [];
      if (command === "list_downloaded_remote_artifacts") return [];
      if (command === "list_remote_native_test_runs_desktop") return [];
      if (command === "reconnect_remote_machine") {
        expect(args).toEqual({nodeId:"node-test-1",credential:"ab".repeat(32)});
        return nodes[0];
      }
      return undefined;
    });
    await display();
    const rePair = Array.from(host?.querySelectorAll("button") ?? [])
      .find((button) => button.textContent === "Re-pair with new credential");
    expect(rePair).toBeDefined();
    expect(rePair?.disabled).toBe(true);
    const field = host?.querySelector<HTMLInputElement>("#remote-repair-node-test-1");
    expect(field).toBeTruthy();
    await act(async () => {
      const setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")?.set;
      if (field) {
        setter?.call(field, "ab".repeat(32));
        field.dispatchEvent(new Event("input", { bubbles: true }));
      }
    });
    expect(rePair?.disabled).toBe(false);
    await act(async () => { rePair?.dispatchEvent(new MouseEvent("click", { bubbles: true })); });
    expect(bridge.invoke).toHaveBeenCalledWith("reconnect_remote_machine",
      {nodeId:"node-test-1",credential:"ab".repeat(32)});
    expect(host?.textContent).toContain("successfully re-paired");
    expect(field?.value).toBe("");
  });
  it("requires artifact SHA completion before Save artifact as becomes available", async () => {
    bridge.invoke.mockImplementation(async (command: string) => {
      if (command === "list_remote_machines") return nodes;
      if (command === "list_workspaces") return [{ id: "approved-project", name: "Approved Project" }];
      if (command === "list_remote_native_test_runs_desktop") return [];
      if (command === "list_remote_machine_access_grants") return [];
      if (command === "list_downloaded_remote_artifacts") return [
        { transferId: "artifact-one", relativePath:"dist/setup.exe", receivedBytes: 200,
          totalBytes:1000, completed:false, sha256:"hash" },
        { transferId: "artifact-two", relativePath:"dist/verified.exe", receivedBytes: 1000,
          totalBytes:1000, completed:true, sha256:"hash" },
      ];
      return undefined;
    });
    await display();
    const buttons = Array.from(host?.querySelectorAll("button") ?? [])
      .filter((button) => button.textContent?.includes("Save artifact as"));
    expect(buttons).toHaveLength(2);
    expect(buttons[0]?.disabled).toBe(true);
    expect(buttons[1]?.disabled).toBe(false);
    expect(bridge.invoke).not.toHaveBeenCalledWith("export_remote_artifact", expect.anything());
  });
});
