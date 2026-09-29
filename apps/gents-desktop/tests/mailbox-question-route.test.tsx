import { act, renderHook } from "@testing-library/react";
import { useState } from "react";
import { describe, expect, it, vi } from "vitest";

import type {
  DesktopApiAdapter,
  DesktopSessionSnapshot,
  MailboxItemView,
} from "@source-inc/gents-desktop-client";
import { useDesktopMailboxRoute } from "../src/hooks/useDesktopMailboxRoute";

const item = {
  itemId: "item-1",
  action: "start_request",
  kind: "ask",
  targetAgentDid: "did:agent",
  targetBehaviorId: "engineer",
  sessionId: "session-1",
} as MailboxItemView;

function useRoute(api: DesktopApiAdapter) {
  const [agent, setAgent] = useState<string | null>(null);
  const [behavior, setBehavior] = useState<string | null>(null);
  const [session, setSessionId] = useState<string | null>(null);
  const [, setSession] = useState<DesktopSessionSnapshot | null>(null);
  return useDesktopMailboxRoute({
    api,
    refreshSnapshot: async () => {},
    selectedAgentDid: agent,
    selectedBehaviorId: behavior,
    selectedSessionId: session,
    setError: () => {},
    setSelectedAgentDid: setAgent,
    setSelectedBehaviorId: setBehavior,
    setSelectedSessionId: setSessionId,
    setSession,
  });
}

describe("answering a mailbox question", () => {
  it("sends the answer as the item's reply and retires a compose route on it", async () => {
    const sendChatMessage = vi.fn().mockResolvedValue({});
    const api = {
      startMailboxRequest: vi.fn().mockResolvedValue(item),
      sendChatMessage,
    } as unknown as DesktopApiAdapter;
    const { result } = renderHook(() => useRoute(api));
    await act(async () => {
      await result.current.onOpenMailboxItem(item.itemId);
    });
    expect(result.current.pendingMailboxCauseId).toBe("item-1");
    const answer = { option_ids: ["yes"], free_text: null };
    await act(async () => {
      await result.current.onAnswerMailboxQuestion(item, answer);
    });
    expect(sendChatMessage).toHaveBeenCalledWith({
      agentDid: "did:agent",
      behaviorId: "engineer",
      sessionId: "session-1",
      content: "",
      causedBySourceDocId: "item-1",
      answer,
    });
    expect(result.current.pendingMailboxCauseId).toBeNull();
  });
});
