import { useEffect, useRef, type MutableRefObject } from "react";

import type {
  DesktopClientUpdatedListenerFactory,
  DesktopSessionSnapshot,
} from "@source-inc/gents-desktop-client";
import { listenToDesktopClientUpdates } from "@source-inc/gents-desktop-client";

import { createDesktopProjectionController } from "./desktopProjectionController";
import {
  desktopUpdateRefreshScope,
  logShellEvent,
  timingConfig,
} from "./desktopShellRuntime";

type DesktopProjectionEffectsArgs = {
  clientAvailable: boolean;
  listenToUpdates: DesktopClientUpdatedListenerFactory;
  refreshSession: (sessionId: string | null) => Promise<DesktopSessionSnapshot | null>;
  refreshSessionLiveDelta: () => Promise<boolean>;
  refreshSnapshot: () => Promise<void>;
  selectedAgentDid: string | null;
  selectedSessionId: string | null;
  selectedSessionIdRef: MutableRefObject<string | null>;
  selectedTrackedRequestId: string | null;
  selectedTrackedRequestIdRef: MutableRefObject<string | null>;
  /** Store version the projected session snapshot was built from, if any. */
  projectedStoreVersionRef?: MutableRefObject<number | null>;
  setError: (error: string | null) => void;
};

/**
 * Own all event, polling, selection, and foreground reads for the bounded
 * desktop projection. Keeping this lifecycle beside the controller prevents
 * new shell effects from accidentally creating a second refresh owner.
 */
export function useDesktopProjectionEffects({
  clientAvailable,
  listenToUpdates,
  refreshSession,
  refreshSessionLiveDelta,
  refreshSnapshot,
  selectedAgentDid,
  selectedSessionId,
  selectedSessionIdRef,
  selectedTrackedRequestId,
  selectedTrackedRequestIdRef,
  projectedStoreVersionRef,
  setError,
}: DesktopProjectionEffectsArgs) {
  // The selection whose bounded session projection this owner has already
  // read. The first read changes the tracked request and agent DID that this
  // effect depends on, which recreates the controller; only a new selection
  // (or a restarted client) justifies reading the same session again.
  const readSelectionRef = useRef<string | null>(null);
  useEffect(() => {
    // There is no bounded desktop projection to observe until the client is
    // running. Starting this owner during configuration bootstrap races the
    // lifecycle's authoritative snapshot read and can clear its failure while
    // leaving startupPhase at configuration-error.
    if (!clientAvailable) {
      readSelectionRef.current = null;
      return;
    }

    let disposed = false;
    let unlisten: (() => void) | undefined;
    let pollTimer: ReturnType<typeof setTimeout> | undefined;

    const reportListenerError = (listenerError: unknown) => {
      if (disposed) return;
      const message =
        listenerError instanceof Error ? listenerError.message : String(listenerError);
      logShellEvent(`desktop update listener failed: ${message}`);
      setError(message);
    };
    const controller = createDesktopProjectionController({
      currentSessionId: () => selectedSessionIdRef.current,
      refreshSnapshot,
      refreshSession,
      refreshSessionLiveDelta,
      onError: reportListenerError,
    });

    void listenToDesktopClientUpdates(
      async (event) => {
        if (disposed) return;
        const scope = desktopUpdateRefreshScope(
          event.reason,
          selectedSessionIdRef.current,
          selectedTrackedRequestIdRef.current,
        );
        // A bounded session read merges the selected request into the store
        // and so emits its own store notice. When the projected session was
        // built from that revision or a later one, the session part of the
        // refresh would only reread the same rows; the fleet index still
        // observes the change.
        const projected = projectedStoreVersionRef?.current ?? null;
        if (
          scope === "full" &&
          event.reason === "store" &&
          typeof event.storeVersion === "number" &&
          projected !== null &&
          event.storeVersion <= projected
        ) {
          await controller.request("snapshot");
          return;
        }
        await controller.request(
          scope,
          event.reason === "store" && typeof event.storeVersion === "number"
            ? event.storeVersion
            : null,
        );
      },
      reportListenerError,
      listenToUpdates,
    )
      .then((cleanup) => {
        if (disposed) {
          cleanup();
          return;
        }
        unlisten = cleanup;
      })
      .catch(reportListenerError);

    const selection = selectedSessionId ?? "";
    if (readSelectionRef.current !== selection) {
      void controller.request("session").then(() => {
        readSelectionRef.current = selection;
      });
    }

    const pollMs = timingConfig().activeSessionPollMs;
    if (
      clientAvailable &&
      selectedSessionId &&
      selectedTrackedRequestId &&
      pollMs !== null
    ) {
      const poll = async () => {
        // Healthy polling follows the live cursor; continuity failure promotes
        // itself to one full bounded database projection.
        await controller.request("sessionDelta");
        if (!disposed) pollTimer = setTimeout(poll, pollMs);
      };
      pollTimer = setTimeout(poll, pollMs);
    }

    const refreshForegroundState = () => {
      if (document.visibilityState !== "hidden") void controller.request("full");
    };
    const onVisibilityChange = () => {
      if (document.visibilityState === "visible") refreshForegroundState();
    };
    if (clientAvailable) {
      document.addEventListener("visibilitychange", onVisibilityChange);
      window.addEventListener("focus", refreshForegroundState);
    }

    return () => {
      disposed = true;
      controller.dispose();
      if (pollTimer) clearTimeout(pollTimer);
      document.removeEventListener("visibilitychange", onVisibilityChange);
      window.removeEventListener("focus", refreshForegroundState);
      unlisten?.();
    };
  }, [
    clientAvailable,
    listenToUpdates,
    selectedAgentDid,
    selectedSessionId,
    selectedTrackedRequestId,
    selectedTrackedRequestIdRef,
    setError,
  ]);
}
