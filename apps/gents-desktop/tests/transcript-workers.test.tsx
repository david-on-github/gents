import { renderHook, waitFor } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import type {
  CausedRequestView,
  DesktopSessionProvenanceRequest,
  RenderedTimelineItem,
  RenderedToolCallView,
  SessionProvenanceView,
} from "@source-inc/gents-desktop-client";
import type { Shell } from "@/hooks/useShell";

import {
  LINEAGE_REFRESH_MS,
  subagentsOf,
  useSessionProvenance,
  useWorkers,
} from "../src/ui/screens/workers";
import { workerNow } from "../src/ui/screens/WorkerStep";

const AGENT = "did:key:parent";
const PERSON = "did:key:person";

const call = (
  requestId: string,
  toolCallId: string,
  statusKind = "success",
  action: "start" | "message" = "start",
): RenderedToolCallView =>
  ({
    itemKey: `item-${toolCallId}`,
    toolName: action === "start" ? "create_session" : "send_message",
    toolCallId,
    statusKind,
    requestId,
    awaitMode: "background",
    presentation: {
      kind: "subagent",
      action,
      name: "crew-explorer",
      sessionId: null,
      description: "explore",
      output: null,
    },
  }) as unknown as RenderedToolCallView;

const group = (...tools: RenderedToolCallView[]): RenderedTimelineItem =>
  ({
    kind: "toolGroup",
    itemKey: `group-${tools[0]?.itemKey}`,
    messageSequence: null,
    tools,
  }) as RenderedTimelineItem;

const caused = (
  requestId: string,
  sessionId: string,
  lifecycleState: string,
  byRequest: string,
  byToolCall: string,
  createdAt: string,
): CausedRequestView => ({
  requestId,
  requestDocId: `doc-${requestId}`,
  sessionId,
  agentDid: AGENT,
  requesterDid: null,
  behaviorId: "crew-explorer",
  lifecycleState,
  interruptRequestedAt: null,
  createdAt,
  hop: 1,
  causedByRequestId: byRequest,
  causedByRequestDocId: `doc-${byRequest}`,
  causedByToolCallId: byToolCall,
  causedByToolCallDocId: `doc-${byToolCall}`,
  causedBySessionId: "parent-session",
});

/* `started` is what the bridge's origin owner returns: only the requests
   that began their session */
const view = (
  sent: CausedRequestView[],
  started: CausedRequestView[] = sent,
): SessionProvenanceView => ({
  sessionId: "parent-session",
  startedBy: null,
  received: [],
  started,
  sent,
});

function shellFor(
  api: Shell["api"],
  timelineItems: RenderedTimelineItem[],
  {
    sessionId = "parent-session",
    sessions = null as unknown[] | null,
  }: { sessionId?: string; sessions?: unknown[] | null } = {},
): Shell {
  return {
    api,
    selectedSession: { sessionId, timelineItems },
    selectedDeployment: {
      agentDid: AGENT,
      sessions: sessions ?? [{ agentDid: AGENT, sessionId, requesterDid: PERSON }],
    },
  } as unknown as Shell;
}

function apiWith(
  provenance: (
    request: DesktopSessionProvenanceRequest,
  ) => Promise<SessionProvenanceView>,
) {
  return {
    sessionProvenance: vi.fn(provenance),
    fetchOperationsSnapshot: vi.fn().mockResolvedValue({ backgroundedTools: [] }),
  } as unknown as Shell["api"] & {
    sessionProvenance: ReturnType<typeof vi.fn>;
    fetchOperationsSnapshot: ReturnType<typeof vi.fn>;
  };
}

function useBoth(shell: Shell) {
  return useWorkers(shell, useSessionProvenance(shell));
}

describe("subagents of a session", () => {
  it("are the sessions this one started, with every request it caused there", () => {
    const origin = caused("r-a1", "session-a", "completed", "req-1", "call-a1", "1");
    const all = subagentsOf(
      view(
        [
          caused("r-a2", "session-a", "processing", "req-2", "call-a2", "3"),
          caused("r-m", "session-messaged", "processing", "req-1", "call-m", "2"),
          origin,
        ],
        [origin],
      ),
      [
        {
          agentDid: AGENT,
          sessionId: "session-a",
          requesterDid: null,
          title: "Explorer",
        },
        /* the same label under another requester is another session */
        {
          agentDid: AGENT,
          sessionId: "session-a",
          requesterDid: PERSON,
          title: "Other",
        },
      ] as never,
    );
    expect(all.map((s) => s.sessionId)).toEqual(["session-a"]);
    expect(all[0]!.requests.map((r) => r.requestId)).toEqual(["r-a1", "r-a2"]);
    expect(all[0]!.origin.requestId).toBe("r-a1");
    expect(all[0]!.summary?.title).toBe("Explorer");
  });

  it("joins each call to the request it caused, subagent or not", async () => {
    const started = caused(
      "r-done",
      "session-done",
      "completed",
      "req-1",
      "call-done",
      "1",
    );
    const messaged = caused("r-m", "session-old", "processing", "req-1", "call-m", "2");
    const api = apiWith(async () => view([started, messaged], [started]));
    const start = call("req-1", "call-done");
    const message = call("req-1", "call-m", "running", "message");
    const { result } = renderHook(() =>
      useBoth(shellFor(api, [group(start, message)])),
    );

    await waitFor(() => expect(result.current.byToolCall(start)).toBeTruthy());
    const reachedStart = result.current.byToolCall(start)!;
    expect(reachedStart.request.requestId).toBe("r-done");
    expect(reachedStart.subagent?.sessionId).toBe("session-done");
    expect(workerNow(start, reachedStart)).toEqual({ tone: "done", text: "finished" });

    const reachedMessage = result.current.byToolCall(message)!;
    expect(reachedMessage.request.sessionId).toBe("session-old");
    expect(reachedMessage.subagent, "a messaged session is not a subagent").toBeNull();
    expect(result.current.all.map((s) => s.sessionId)).toEqual(["session-done"]);
  });

  it("asks for the session's exact scope, requester included", async () => {
    const api = apiWith(async () => view([]));
    renderHook(() => useBoth(shellFor(api, [group(call("req-1", "call-1"))])));
    await waitFor(() =>
      expect(api.sessionProvenance).toHaveBeenCalledWith({
        sessionId: "parent-session",
        agentDid: AGENT,
        requesterDid: PERSON,
      }),
    );
  });

  it("does not ask while the session's scope is unknown", async () => {
    const api = apiWith(async () => view([]));
    renderHook(() =>
      useBoth(shellFor(api, [group(call("req-1", "call-1"))], { sessions: [] })),
    );
    await new Promise((resolve) => setTimeout(resolve, 20));
    expect(api.sessionProvenance).not.toHaveBeenCalled();
  });

  it("does not ask while two listed scopes share the session's label", async () => {
    const api = apiWith(async () => view([]));
    renderHook(() =>
      useBoth(
        shellFor(api, [group(call("req-1", "call-1"))], {
          sessions: [
            { agentDid: AGENT, sessionId: "parent-session", requesterDid: PERSON },
            { agentDid: AGENT, sessionId: "parent-session", requesterDid: null },
          ],
        }),
      ),
    );
    await new Promise((resolve) => setTimeout(resolve, 20));
    expect(api.sessionProvenance).not.toHaveBeenCalled();
  });

  it("joins a call only through the lineage, never through a summary's latest request", async () => {
    const api = apiWith(async () => view([]));
    const unknown = call("req-1", "call-unknown", "running");
    const shell = shellFor(api, [group(unknown)], {
      sessions: [
        { agentDid: AGENT, sessionId: "parent-session", requesterDid: PERSON },
        {
          agentDid: AGENT,
          sessionId: "unrelated-session",
          latestRequestId: "call-unknown",
        },
      ],
    });
    const { result } = renderHook(() => useBoth(shell));
    await waitFor(() => expect(result.current.loaded).toBe(true));
    expect(result.current.byToolCall(unknown)).toBeNull();
  });

  it("does not join a call from another request with the same call id", async () => {
    const api = apiWith(async () =>
      view([caused("r-1", "session-1", "completed", "req-1", "call-1", "1")]),
    );
    const other = call("req-2", "call-1");
    const { result } = renderHook(() => useBoth(shellFor(api, [group(other)])));
    await waitFor(() => expect(result.current.loaded).toBe(true));
    expect(result.current.byToolCall(other)).toBeNull();
  });

  it("does not keep another session's provenance for the render that switches", async () => {
    const api = apiWith(async () =>
      view([caused("r-a", "session-a", "completed", "req-1", "call-a", "1")]),
    );
    const items = [group(call("req-1", "call-a"))];
    const { result, rerender } = renderHook(
      ({ sessionId }: { sessionId: string }) =>
        useBoth(shellFor(api, items, { sessionId })),
      { initialProps: { sessionId: "session-x" } },
    );
    await waitFor(() => expect(result.current.all).toHaveLength(1));
    api.sessionProvenance.mockImplementationOnce(() => new Promise(() => {}));
    rerender({ sessionId: "session-y" });
    expect(result.current.all).toHaveLength(0);
  });
});

describe("subagent lineage freshness", () => {
  it("asks again on a session-list change and keeps the last view on a failed ask", async () => {
    let state = "processing";
    const api = apiWith(async () =>
      view([caused("r-1", "session-1", state, "req-1", "call-1", "1")]),
    );
    const tool = call("req-1", "call-1", "success");
    const items = [group(tool)];
    const own = { agentDid: AGENT, sessionId: "parent-session", requesterDid: PERSON };
    const { result, rerender } = renderHook(
      ({ sessions }: { sessions: unknown[] }) =>
        useBoth(shellFor(api, items, { sessions })),
      { initialProps: { sessions: [own] as unknown[] } },
    );
    await waitFor(() =>
      expect(result.current.byToolCall(tool)?.request.lifecycleState).toBe(
        "processing",
      ),
    );

    api.sessionProvenance.mockRejectedValueOnce(new Error("bridge busy"));
    rerender({ sessions: [own, { sessionId: "other", turnState: "running" }] });
    await waitFor(() => expect(api.sessionProvenance).toHaveBeenCalledTimes(2));
    expect(result.current.byToolCall(tool)?.request.lifecycleState).toBe("processing");

    state = "completed";
    rerender({ sessions: [own, { sessionId: "other", turnState: "completed" }] });
    await waitFor(() =>
      expect(result.current.byToolCall(tool)?.request.lifecycleState).toBe("completed"),
    );
  });

  it("polls while a caused request runs with no local cue, and stops once it settles", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    try {
      let state = "processing";
      const api = apiWith(async () =>
        view([caused("r-remote", "session-remote", state, "req-1", "call-1", "1")]),
      );
      const tool = call("req-1", "call-1", "success");
      const shell = shellFor(api, [group(tool)]);
      const { result } = renderHook(() => useBoth(shell));
      await waitFor(() => expect(api.sessionProvenance).toHaveBeenCalledTimes(1));

      state = "completed";
      await vi.advanceTimersByTimeAsync(LINEAGE_REFRESH_MS + 1_000);
      await waitFor(() =>
        expect(result.current.byToolCall(tool)?.request.lifecycleState).toBe(
          "completed",
        ),
      );
      const asked = api.sessionProvenance.mock.calls.length;
      await vi.advanceTimersByTimeAsync(LINEAGE_REFRESH_MS * 3);
      expect(api.sessionProvenance).toHaveBeenCalledTimes(asked);
    } finally {
      vi.useRealTimers();
    }
  });

  it("asks for operations facts only when the transcript has a background process", async () => {
    const api = apiWith(async () => view([]));
    renderHook(() => useBoth(shellFor(api, [group(call("req-1", "call-1"))])));
    await waitFor(() => expect(api.sessionProvenance).toHaveBeenCalled());
    expect(api.fetchOperationsSnapshot).not.toHaveBeenCalled();

    const process = {
      ...call("req-1", "call-p"),
      toolName: "spawn_process",
      presentation: { kind: "process", action: "spawn", target: "bash" },
    } as unknown as RenderedToolCallView;
    renderHook(() => useBoth(shellFor(api, [group(process)])));
    await waitFor(() => expect(api.fetchOperationsSnapshot).toHaveBeenCalled());
  });
});
