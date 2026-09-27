/* The sessions this one's calls reached. A session whose first request one
   of its calls caused is a "subagent" to the person; a session it only sent
   a message to is not. Both are ordinary sessions to the runtime. Read from
   the bridge's session provenance, which applies the runtime's
   session-origin rule, joined to the session list for who each one is and
   to this transcript's rows by the call that reached it. Every fact here
   has a field in the bridge contract; where the contract is silent the
   state says so instead of guessing. */
import { useEffect, useMemo, useRef, useState } from "react";
import type {
  BackgroundedToolView,
  CausedRequestView,
  DesktopOperationsSnapshot,
  RenderedToolCallView,
  SessionProvenanceView,
  SessionSummary,
} from "@source-inc/gents-desktop-client";
import type { Shell } from "@/hooks/useShell";
import { isLive } from "@/lib/live";

export type Subagent = {
  sessionId: string;
  agentDid: string | null;
  summary: SessionSummary | null;
  /* the request that began it, which this session caused */
  origin: CausedRequestView;
  /* every request this session's calls caused there, oldest first */
  requests: CausedRequestView[];
};

/* what one create_session/send_message row reached */
export type Reached = {
  /* the request this row's call caused */
  request: CausedRequestView;
  summary: SessionSummary | null;
  /* the subagent, when the call reached a session this one started */
  subagent: Subagent | null;
};

export type Workers = {
  /* the sessions this one started, in the order each began */
  all: Subagent[];
  byToolCall: (tool: RenderedToolCallView) => Reached | null;
  /* the operations facts for a background process row */
  background: (tool: RenderedToolCallView) => BackgroundedToolView | null;
  loaded: boolean;
};

export const NO_WORKERS: Workers = {
  all: [],
  byToolCall: () => null,
  background: () => null,
  loaded: false,
};

const byCreation = (a: CausedRequestView, b: CausedRequestView) =>
  (a.createdAt ?? "").localeCompare(b.createdAt ?? "") ||
  a.requestId.localeCompare(b.requestId);

/* A session is identified by its whole scope, not its label alone. */
const scopeKey = (r: CausedRequestView) =>
  `${r.agentDid ?? ""}\u0000${r.sessionId ?? ""}\u0000${r.requesterDid ?? ""}`;

/* The subagents in a provenance view: one per started session, with every
   request this session caused in it. */
export function subagentsOf(
  provenance: Pick<SessionProvenanceView, "started" | "sent">,
  sessions: readonly SessionSummary[] | undefined,
): Subagent[] {
  const summaries = new Map((sessions ?? []).map((s) => [s.sessionId, s]));
  return [...provenance.started]
    .filter((origin) => origin.sessionId)
    .sort(byCreation)
    .map((origin) => ({
      sessionId: origin.sessionId!,
      agentDid: origin.agentDid,
      summary: summaries.get(origin.sessionId!) ?? null,
      origin,
      requests: provenance.sent
        .filter((r) => scopeKey(r) === scopeKey(origin))
        .sort(byCreation),
    }));
}

const isBackgroundProcess = (t: RenderedToolCallView) =>
  t.presentation.kind === "process" && t.awaitMode === "background";

/* While any request this session caused is still running the provenance is
   asked again at most this often, besides on transcript and session-list
   changes: a session on another agent moves no local cue. */
export const LINEAGE_REFRESH_MS = 10_000;

type Held<T> = { scope: string; value: T };

/* The selected session's provenance. Its scope is exact: the session's
   agent, label and requester, as the session list reports them. */
export function useSessionProvenance(shell: Shell): SessionProvenanceView | null {
  const session = shell.selectedSession;
  const sessionId = session?.sessionId ?? null;
  const agentDid = shell.selectedDeployment?.agentDid ?? null;
  const sessions = shell.selectedDeployment?.sessions;
  const summary = sessions?.find((s) => s.sessionId === sessionId) ?? null;
  const listed = summary !== null;
  const requesterDid = summary?.requesterDid ?? null;
  const scope = `${agentDid ?? ""}\u0000${sessionId ?? ""}\u0000${requesterDid ?? ""}`;
  const [held, setHeld] = useState<Held<SessionProvenanceView> | null>(null);
  const provenance = held?.scope === scope ? held.value : null;
  /* a new turn, a tool state and a session-list move are the local cues;
     their values, not their identities, are what is compared */
  const cue = (session?.timelineItems ?? [])
    .map((i) =>
      i.kind === "toolGroup"
        ? i.tools.map((t) => `${t.itemKey}:${t.statusKind}`).join()
        : i.itemKey,
    )
    .join();
  const sessionsCue = (sessions ?? [])
    .map((s) => `${s.sessionId}:${s.turnState ?? ""}:${s.updatedAt ?? ""}`)
    .join();
  const unsettled = provenance?.sent.some((r) => isLive(r.lifecycleState)) ?? false;
  const [tick, setTick] = useState(0);
  useEffect(() => {
    if (!unsettled) return;
    const timer = window.setInterval(() => setTick((t) => t + 1), LINEAGE_REFRESH_MS);
    return () => window.clearInterval(timer);
  }, [unsettled]);
  const generation = useRef(0);
  useEffect(() => {
    /* without the session's summary its exact scope is unknown */
    if (!agentDid || !sessionId || !listed) return;
    const ask = ++generation.current;
    void shell.api.sessionProvenance({ sessionId, agentDid, requesterDid }).then(
      (value) => {
        if (generation.current === ask) setHeld({ scope, value });
      },
      () => {
        /* the last known lineage stays; the next cue asks again */
      },
    );
  }, [
    shell.api,
    agentDid,
    sessionId,
    requesterDid,
    listed,
    scope,
    cue,
    sessionsCue,
    tick,
  ]);
  return provenance;
}

export function useWorkers(
  shell: Shell,
  provenance: SessionProvenanceView | null,
): Workers {
  const session = shell.selectedSession;
  const agentDid = shell.selectedDeployment?.agentDid ?? null;
  const sessions = shell.selectedDeployment?.sessions;
  const [heldOps, setHeldOps] = useState<Held<DesktopOperationsSnapshot> | null>(null);
  const ops = heldOps?.scope === agentDid ? heldOps.value : null;
  const tools = useMemo(
    () =>
      session?.timelineItems.flatMap((i) => (i.kind === "toolGroup" ? i.tools : [])) ??
      [],
    [session?.timelineItems],
  );
  const hasProcesses = tools.some(isBackgroundProcess);
  const cue = tools.map((t) => `${t.itemKey}:${t.statusKind}`).join();
  useEffect(() => {
    if (!hasProcesses || !agentDid) {
      setHeldOps(null);
      return;
    }
    let live = true;
    void shell.api.fetchOperationsSnapshot({ agentDid }).then(
      (o) => live && setHeldOps({ scope: agentDid, value: o }),
      () => live && setHeldOps(null),
    );
    return () => {
      live = false;
    };
  }, [shell.api, hasProcesses, agentDid, cue]);
  return useMemo(() => {
    if (!provenance && !ops) return NO_WORKERS;
    const all = provenance ? subagentsOf(provenance, sessions) : [];
    const byScope = new Map(all.map((s) => [scopeKey(s.origin), s]));
    const summaries = new Map((sessions ?? []).map((s) => [s.sessionId, s]));
    const backgrounded = ops?.backgroundedTools ?? [];
    return {
      loaded: true,
      all,
      byToolCall: (tool) => {
        if (!tool.toolCallId || !tool.requestId) return null;
        const request = provenance?.sent.find(
          (r) =>
            r.causedByToolCallId === tool.toolCallId &&
            r.causedByRequestId === tool.requestId,
        );
        if (!request) return null;
        return {
          request,
          summary: request.sessionId
            ? (summaries.get(request.sessionId) ?? null)
            : null,
          subagent: byScope.get(scopeKey(request)) ?? null,
        };
      },
      background: (tool) =>
        backgrounded.find(
          (b) => b.toolCallId === tool.toolCallId && b.requestId === tool.requestId,
        ) ?? null,
    };
  }, [provenance, ops, sessions]);
}
