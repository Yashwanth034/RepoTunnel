// @vitest-environment jsdom

import React, { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const backend = vi.hoisted(() => ({
  listTeamSessions: vi.fn(),
  getTeamSession: vi.fn(),
  disconnectTeamAgent: vi.fn(),
  createTeamSession: vi.fn(),
  pauseTeamSession: vi.fn(),
  resumeTeamSession: vi.fn(),
  completeTeamSession: vi.fn(),
  deleteTeamSession: vi.fn(),
}));

vi.mock("../lib/backend", () => backend);
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => undefined) }));

import TeamPanel from "./TeamPanel";
import type { TeamSnapshot, Workspace } from "../types";

(globalThis as typeof globalThis & { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

const workspace = { id: "workspace-repotunnel-test", name: "RepoTunnel",
  changePolicy:"automatic", accessMode:"readWrite" } as Workspace;
const agents = [
  {id:"agent-a-existing",name:"Drive Core",role:"Engineer A",status:"active",
    joinedAt:100,lastSeenAt:100,clientLabel:"Old Chat A",currentTaskId:"task-a",
    manuallyDisconnected:false},
  {id:"agent-b-existing",name:"Drive Experience",role:"Engineer B",status:"active",
    joinedAt:100,lastSeenAt:100,clientLabel:"Existing Chat B",currentTaskId:null,
    manuallyDisconnected:false},
];
function snapshot(disconnected=false) {
  return {
    session: {
      id:"team-existing",workspaceId:workspace.id,workspaceName:workspace.name,
      goal:"Continue existing Drive work",currentRequest:"Continue existing Drive work",
      status:"active",phase:"executing",cycleNumber:1,revision:disconnected?9:8,
      successCriteria:[],criterionChecks:[],completedCycles:[],locks:[],
      tasks:[{id:"task-a",title:"Existing Drive work",status:"inProgress",
        ownerAgentId:"agent-a-existing",reviewerAgentId:"agent-b-existing",
        contributorAgentIds:[],cycleNumber:1,priority:3,createdAt:1}],
      agents:agents.map((agent,index)=>index===0 && disconnected ? {
        ...agent,status:"offline",manuallyDisconnected:true,clientLabel:null,
        lastSeenAt:null,
      } : {...agent}),
    },
    progress:{progressPercent:50,doneTaskCount:0,openTaskCount:1,
      blockedTaskCount:0,verifiedCriterionCount:0,totalCriterionCount:0},
    recommendedAction:null,
  } as unknown as TeamSnapshot;
}

let root:Root|null=null;
let host:HTMLDivElement|null=null;
const notice=vi.fn();
const copied=vi.fn(async (_text:string)=>undefined);
const settle=async()=>{await act(async()=>{await new Promise(resolve=>setTimeout(resolve,0));});};
const click=async(button:HTMLButtonElement)=>{
  await act(async()=>{button.dispatchEvent(new MouseEvent("click",{bubbles:true}));});
  await settle();
};

beforeEach(()=>{
  vi.clearAllMocks();
  backend.listTeamSessions.mockResolvedValue([
    {id:"team-existing",status:"active",workspaceId:workspace.id,updatedAt:100},
  ]);
  backend.getTeamSession.mockResolvedValue(snapshot(false));
  backend.disconnectTeamAgent.mockImplementation(async () => {
    backend.getTeamSession.mockResolvedValue(snapshot(true));
    return snapshot(true);
  });
  Object.defineProperty(navigator,"clipboard",{configurable:true,value:{writeText:copied}});
});
afterEach(()=>{
  act(()=>root?.unmount());
  host?.remove();
  host=null;root=null;
});
describe("Team Mode manual recovery button",()=>{
  it("never disconnects an engineer without explicit confirmation",async()=>{
    host=document.createElement("div");document.body.appendChild(host);
    root=createRoot(host);
    await act(async()=>{root?.render(
      <TeamPanel workspaces={[workspace]} selectedWorkspaceId={workspace.id}
        onSelectWorkspace={vi.fn()} onNotice={notice}/>
    );});
    await settle();
    expect(host.textContent).toContain("2/2 attached");
    const cards=host.querySelectorAll(".team-agent-card");
    expect(cards[0].textContent).toContain("Drive Core");
    expect(cards[1].textContent).toContain("Drive Experience");
    await click(cards[0].querySelector("button.team-agent-disconnect-button") as HTMLButtonElement);
    expect(backend.disconnectTeamAgent).not.toHaveBeenCalled();
    const cancel=Array.from(host.querySelectorAll('[role="dialog"] button'))
      .find((button)=>button.textContent==="Cancel") as HTMLButtonElement;
    await click(cancel);
    expect(host.querySelector('[role="dialog"]')).toBeNull();
    expect(backend.disconnectTeamAgent).not.toHaveBeenCalled();
    expect(host.textContent).toContain("2/2 attached");
    expect(host.querySelectorAll("button.team-agent-disconnect-button").length).toBe(2);
  });

  it("disconnects only selected old chat after confirmation without resetting the Team",async()=>{
    host=document.createElement("div");document.body.appendChild(host);
    root=createRoot(host);
    await act(async()=>{root?.render(
      <TeamPanel workspaces={[workspace]} selectedWorkspaceId={workspace.id}
        onSelectWorkspace={vi.fn()} onNotice={notice}/>
    );});
    await settle();
    const cards=host.querySelectorAll(".team-agent-card");
    expect(cards.length).toBe(2);
    expect(cards[0].textContent).toContain("Drive Core");
    expect(cards[1].textContent).toContain("Drive Experience");
    const button=cards[0].querySelector("button.team-agent-disconnect-button") as HTMLButtonElement;
    expect(button).not.toBeNull();
    await click(button);
    expect(backend.disconnectTeamAgent).not.toHaveBeenCalled();
    const dialog=host.querySelector('[role="dialog"]');
    expect(dialog?.textContent).toContain("Disconnect Drive Core");
    const confirm=Array.from(dialog?.querySelectorAll("button")??[])
      .find((item)=>item.textContent==="Disconnect AI") as HTMLButtonElement;
    expect(confirm).toBeDefined();
    await click(confirm);
    expect(backend.disconnectTeamAgent).toHaveBeenCalledTimes(1);
    expect(backend.disconnectTeamAgent).toHaveBeenCalledWith("team-existing","agent-a-existing");
    expect(host.textContent).toContain("AI disconnected");
    expect(host.textContent).toContain("Existing Drive work");
    const updatedCards=host.querySelectorAll(".team-agent-card");
    expect(updatedCards[0].textContent).toContain("Disconnected");
    expect(updatedCards[1].textContent).toContain("Drive Experience");
    expect(updatedCards[1].querySelector("button.team-agent-disconnect-button")).not.toBeNull();
    const resume=updatedCards[0].querySelector("button.team-agent-resume-button") as HTMLButtonElement;
    await click(resume);
    expect(copied).toHaveBeenCalledOnce();
    expect(String(copied.mock.calls[0][0])).toContain("agent-a-existing");
    expect(String(copied.mock.calls[0][0])).toContain("team-existing");
    expect(String(copied.mock.calls[0][0])).toContain("do not create a Team");
  });
});
