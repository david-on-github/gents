import { act, renderHook } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

import { useDesktopProjectionEffects } from "../src/hooks/useDesktopProjectionEffects";

function harness() {
  const refreshSession = vi.fn(async () => null);
  const refreshSnapshot = vi.fn(async () => {});
  const refreshSessionLiveDelta = vi.fn(async () => true);
  const selectedSessionIdRef = { current: "session-1" as string | null };
  const selectedTrackedRequestIdRef = { current: null as string | null };
  const hook = renderHook(
    (props: {
      selectedTrackedRequestId: string | null;
      selectedSessionId: string | null;
    }) =>
      useDesktopProjectionEffects({
        clientAvailable: true,
        listenToUpdates: () => Promise.resolve(() => {}),
        refreshSession,
        refreshSessionLiveDelta,
        refreshSnapshot,
        selectedAgentDid: "did:key:agent",
        selectedSessionId: props.selectedSessionId,
        selectedSessionIdRef,
        selectedTrackedRequestId: props.selectedTrackedRequestId,
        selectedTrackedRequestIdRef,
        setError: () => {},
      }),
    {
      initialProps: { selectedTrackedRequestId: null, selectedSessionId: "session-1" },
    },
  );
  return { hook, refreshSession, selectedSessionIdRef };
}

describe("useDesktopProjectionEffects", () => {
  it("reads a selected session once even when its first read changes the tracked request", async () => {
    const { hook, refreshSession } = harness();
    await act(async () => {});
    expect(refreshSession).toHaveBeenCalledTimes(1);

    // The first bounded read settles the turn state; the derived tracked
    // request changes and recreates the controller.
    await act(async () => {
      hook.rerender({
        selectedTrackedRequestId: "request-1",
        selectedSessionId: "session-1",
      });
    });
    await act(async () => {
      hook.rerender({ selectedTrackedRequestId: null, selectedSessionId: "session-1" });
    });
    expect(refreshSession).toHaveBeenCalledTimes(1);
  });

  it("reads again when the selection changes", async () => {
    const { hook, refreshSession, selectedSessionIdRef } = harness();
    await act(async () => {});
    selectedSessionIdRef.current = "session-2";
    await act(async () => {
      hook.rerender({ selectedTrackedRequestId: null, selectedSessionId: "session-2" });
    });
    expect(refreshSession).toHaveBeenCalledTimes(2);
    expect(refreshSession).toHaveBeenLastCalledWith("session-2");
  });
});
