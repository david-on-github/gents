import { readFileSync } from "node:fs";
import { join } from "node:path";
import { render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import type {
  CausedRequestView,
  RenderedToolCallView,
  SessionSummary,
} from "@source-inc/gents-desktop-client";

vi.mock("@/lib/router", () => ({ href: () => "#", navigate: vi.fn() }));

import { SubagentList, WorkerStep, workerNow } from "../src/ui/screens/WorkerStep";
import {
  NO_WORKERS,
  type Reached,
  type Subagent,
  type Workers,
} from "../src/ui/screens/workers";
import { WorkerActionsContext } from "../src/ui/screens/WorkerActions";

const call = (
  statusKind = "running",
  action: "start" | "message" = "start",
): RenderedToolCallView =>
  ({
    itemKey: "tool-1",
    toolName: action === "start" ? "create_session" : "send_message",
    toolCallId: "call-1",
    requestId: "parent-req",
    statusKind,
    awaitMode: "background",
    presentation: {
      kind: "subagent",
      action,
      name: "reviewer",
      sessionId: "child-session",
      description: "Review the diff",
      output: null,
    },
  }) as unknown as RenderedToolCallView;

/* what the bridge sends in production: message_count is always None */
const summary = (turnState: string | null): SessionSummary =>
  ({
    sessionId: "child-session",
    title: "Reviewer",
    behaviorId: null,
    turnState,
    latestRequestId: "child-req-2",
    messageCount: null,
    updatedAt: null,
  }) as unknown as SessionSummary;

const caused = (requestId: string, lifecycleState: string): CausedRequestView => ({
  requestId,
  requestDocId: `doc-${requestId}`,
  sessionId: "child-session",
  agentDid: "did:key:reviewer",
  requesterDid: null,
  behaviorId: null,
  lifecycleState,
  interruptRequestedAt: null,
  createdAt: null,
  hop: 1,
  causedByRequestId: "parent-req",
  causedByRequestDocId: "doc-parent-req",
  causedByToolCallId: "call-1",
  causedByToolCallDocId: "doc-call-1",
  causedBySessionId: "parent-session",
});

const subagent = (turnState: string | null): Subagent => ({
  sessionId: "child-session",
  agentDid: "did:key:reviewer",
  summary: summary(turnState),
  origin: caused("child-req", "completed"),
  requests: [caused("child-req", "completed"), caused("child-req-2", "processing")],
});

/* this row's call caused `request`; the session may be working on others */
const reached = (
  request: CausedRequestView,
  s: Subagent | null = subagent("running"),
): Reached => ({ request, summary: s?.summary ?? summary("running"), subagent: s });

function workersWith(r: Reached | null): Workers {
  return {
    ...NO_WORKERS,
    all: r?.subagent ? [r.subagent] : [],
    byToolCall: () => r,
    loaded: true,
  };
}

function renderStep(
  tool: RenderedToolCallView,
  r: Reached | null,
  actions = { kill: vi.fn(), interrupt: vi.fn() },
) {
  render(
    <WorkerActionsContext.Provider value={actions}>
      <WorkerStep tool={tool} workers={workersWith(r)} />
    </WorkerActionsContext.Provider>,
  );
  return actions;
}

describe("a subagent row", () => {
  it("shows the request this row's call caused, not the session's latest", () => {
    /* the session is working on a later request; this call's has finished */
    expect(workerNow(call(), reached(caused("child-req", "completed")))).toEqual({
      tone: "done",
      text: "finished",
    });
    expect(workerNow(call(), reached(caused("child-req", "processing"))).tone).toBe(
      "running",
    );
  });

  it("reads the caused request's lifecycle", () => {
    const at = (state: string) => workerNow(call(), reached(caused("r", state)));
    expect(at("pending").text).toBe("waiting for the agent to pick it up");
    expect(at("processing").tone).toBe("running");
    expect(at("completed").tone).toBe("done");
    expect(at("dead").tone).toBe("failed");
    expect(at("interrupted").tone).toBe("stopped");
    expect(at("superseded").tone).toBe("stopped");
  });

  it("never infers replication from a missing message count", () => {
    for (const state of ["processing", "completed", "pending"]) {
      expect(workerNow(call(), reached(caused("r", state))).text).not.toMatch(/sync/i);
    }
  });

  it("never reads the tool call's own status as a live request", () => {
    expect(workerNow(call("success"), null)).toEqual({ tone: "done", text: "sent" });
    expect(workerNow(call("error"), null).tone).toBe("failed");
    expect(workerNow(call("unknown"), null)).toEqual({
      tone: "unknown",
      text: "state unknown",
    });
    expect(workerNow(call("running"), null)).toEqual({
      tone: "running",
      text: "starting",
    });
  });

  it("stops by interrupting only the request this row's call caused", () => {
    /* the session has moved on to a later request; Stop names this call's */
    const request = caused("child-req", "processing");
    const actions = renderStep(call("running"), reached(request));
    screen.getByRole("button", { name: "Stop Reviewer" }).click();
    expect(actions.interrupt).toHaveBeenCalledTimes(1);
    expect(actions.interrupt).toHaveBeenCalledWith(request);
    expect(
      actions.kill,
      "a session-message row is never process-killed",
    ).not.toBeCalled();
  });

  it("offers no Stop once the caused request is terminal, even while the row runs", () => {
    renderStep(call("running"), reached(caused("child-req", "completed")));
    expect(screen.queryByRole("button", { name: /^Stop / })).toBeNull();
  });

  it("offers no Stop without a caused request to interrupt", () => {
    renderStep(call("running"), null);
    expect(screen.queryByRole("button", { name: /^Stop / })).toBeNull();
  });

  it("labels a start as Started and a message as Messaged", () => {
    renderStep(call("success"), reached(caused("child-req", "completed")));
    expect(screen.getByText("Started")).toBeInTheDocument();
  });

  it("labels a message to a session this one did not start as Messaged", () => {
    const existing = reached(caused("req-existing", "processing"), null);
    renderStep(call("running", "message"), existing);
    expect(screen.getByText("Messaged")).toBeInTheDocument();
  });

  it("links the row to the session it reached, as an ordinary session", () => {
    renderStep(call("success"), reached(caused("child-req", "completed")));
    expect(screen.getByRole("link", { name: "Open Reviewer" })).toBeInTheDocument();
  });

  it("keeps replication claims out of the transcript, which has no owner for them", () => {
    for (const file of ["SessionScreen.tsx", "WorkerStep.tsx", "workers.ts"]) {
      const source = readFileSync(join(__dirname, "../src/ui/screens", file), "utf8");
      expect(source, file).not.toMatch(/has not replicated to this desktop/);
      expect(source, file).not.toMatch(/not synced to this desktop/);
      expect(source, file).not.toMatch(/messageCount == null/);
    }
  });
});

describe("a background process row", () => {
  const process = (statusKind: string) =>
    ({
      itemKey: "proc-1",
      toolName: "spawn_process",
      toolCallId: "call-proc",
      requestId: "parent-req",
      statusKind,
      awaitMode: "background",
      presentation: {
        kind: "process",
        action: "spawn",
        target: "cargo test",
        description: null,
        output: null,
      },
    }) as unknown as RenderedToolCallView;

  it("offers Stop while it runs and kills that row", () => {
    const tool = process("running");
    const actions = renderStep(tool, null);
    screen.getByRole("button", { name: "Stop cargo test" }).click();
    expect(actions.kill).toHaveBeenCalledWith(tool);
    expect(actions.interrupt).not.toBeCalled();
  });

  it("offers no Stop once it has settled", () => {
    renderStep(process("success"), null);
    expect(screen.queryByRole("button", { name: /^Stop / })).toBeNull();
  });
});

describe("subagent list", () => {
  it("lists each started session with its state and a way in", () => {
    render(
      <SubagentList workers={workersWith(reached(caused("child-req", "completed")))} />,
    );
    expect(screen.getByText("Subagents")).toBeInTheDocument();
    expect(screen.getByRole("link", { name: /Reviewer/ })).toBeInTheDocument();
  });

  it("renders nothing for a session that started no other session", () => {
    const view = render(<SubagentList workers={NO_WORKERS} />);
    expect(view.container).toBeEmptyDOMElement();
  });
});
