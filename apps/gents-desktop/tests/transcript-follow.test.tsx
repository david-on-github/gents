import { act, fireEvent, renderHook } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import type { RefObject } from "react";

import { useFollowTail, useOlderPages } from "../src/ui/lib/scroll";

function transcriptFixture() {
  const owner = document.createElement("div");
  const viewport = document.createElement("div");
  viewport.dataset.slot = "scroll-area-viewport";
  owner.append(viewport);

  let scrollHeight = 500;
  Object.defineProperties(viewport, {
    clientHeight: { configurable: true, get: () => 200 },
    scrollHeight: { configurable: true, get: () => scrollHeight },
  });

  return {
    ownerRef: { current: owner } as RefObject<HTMLDivElement | null>,
    viewport,
    growTo(height: number) {
      scrollHeight = height;
    },
  };
}

describe("transcript streaming follow", () => {
  it("stays pinned across growth, releases on scroll up, and relocks at the tip", () => {
    const fixture = transcriptFixture();
    const { result, rerender } = renderHook(
      ({ signal }) => useFollowTail(fixture.ownerRef, "session-1", signal),
      { initialProps: { signal: "assistant:10" } },
    );

    expect(fixture.viewport.scrollTop).toBe(500);
    expect(result.current.atBottom).toBe(true);

    // The model appends a chunk larger than the proximity threshold. Follow is
    // based on the reader's prior intent, not the newly increased height.
    fixture.growTo(900);
    rerender({ signal: "assistant:410" });
    expect(fixture.viewport.scrollTop).toBe(900);

    act(() => {
      fixture.viewport.scrollTop = 100;
      fixture.viewport.dispatchEvent(new Event("scroll"));
    });
    expect(result.current.atBottom).toBe(false);

    fixture.growTo(1_200);
    rerender({ signal: "assistant:710" });
    expect(fixture.viewport.scrollTop).toBe(100);

    act(() => {
      fixture.viewport.scrollTop = 1_000;
      fixture.viewport.dispatchEvent(new Event("scroll"));
    });
    expect(result.current.atBottom).toBe(true);

    fixture.growTo(1_500);
    rerender({ signal: "assistant:1010" });
    expect(fixture.viewport.scrollTop).toBe(1_500);
  });
});

describe("older transcript pages", () => {
  it("loads on upward navigation only, deduplicates requests and anchors prepends", async () => {
    const fixture = transcriptFixture();
    fixture.viewport.scrollTop = 300;
    let finish!: (value: boolean) => void;
    const load = vi.fn(
      () =>
        new Promise<boolean>((resolve) => {
          finish = resolve;
        }),
    );
    const frames: FrameRequestCallback[] = [];
    vi.stubGlobal("requestAnimationFrame", (callback: FrameRequestCallback) => {
      frames.push(callback);
      return frames.length;
    });
    const { result, unmount } = renderHook(() =>
      useOlderPages(fixture.ownerRef, "session-1", true, load),
    );
    expect(load).not.toHaveBeenCalled();
    act(() => {
      fixture.viewport.scrollTop = 100;
      fixture.viewport.dispatchEvent(new Event("scroll"));
      fixture.viewport.dispatchEvent(new WheelEvent("wheel", { deltaY: -20 }));
    });
    expect(load).toHaveBeenCalledTimes(1);
    expect(result.current).toBe(true);
    await act(async () => {
      fixture.growTo(900);
      finish(true);
      await Promise.resolve();
    });
    act(() => frames.splice(0).forEach((frame) => frame(0)));
    expect(fixture.viewport.scrollTop).toBe(500);
    expect(result.current).toBe(false);
    unmount();
    vi.unstubAllGlobals();
  });

  it("ignores late scroll correction after switching sessions and stops at history's start", async () => {
    const fixture = transcriptFixture();
    fixture.viewport.scrollTop = 0;
    let finish!: (value: boolean) => void;
    const load = vi.fn(
      () =>
        new Promise<boolean>((resolve) => {
          finish = resolve;
        }),
    );
    const { rerender, unmount } = renderHook(
      ({ subject, older }) => useOlderPages(fixture.ownerRef, subject, older, load),
      { initialProps: { subject: "a", older: true } },
    );
    act(() => fixture.viewport.dispatchEvent(new WheelEvent("wheel", { deltaY: -20 })));
    expect(load).toHaveBeenCalledTimes(1);
    rerender({ subject: "b", older: false });
    await act(async () => {
      fixture.growTo(900);
      finish(true);
      await Promise.resolve();
    });
    expect(fixture.viewport.scrollTop).toBe(0);
    act(() => fixture.viewport.dispatchEvent(new WheelEvent("wheel", { deltaY: -20 })));
    expect(load).toHaveBeenCalledTimes(1);
    unmount();
  });
});

describe("short transcript upward intent", () => {
  it.each(["touch", "keyboard"])(
    "loads older content through %s at the top",
    async (input) => {
      const fixture = transcriptFixture();
      const load = vi.fn(async () => false);
      const { unmount } = renderHook(() =>
        useOlderPages(fixture.ownerRef, "a", true, load),
      );
      expect(load).not.toHaveBeenCalled();
      await act(async () => {
        if (input === "touch") {
          fireEvent.touchStart(fixture.viewport, { touches: [{ clientY: 50 }] });
          fireEvent.touchMove(fixture.viewport, { touches: [{ clientY: 80 }] });
        } else {
          fireEvent.keyDown(fixture.viewport, { key: "PageUp" });
        }
      });
      expect(load).toHaveBeenCalledTimes(1);
      unmount();
    },
  );
});
