import { fireEvent, render, screen, within } from "@testing-library/react";
import type { ReactElement } from "react";
import { describe, expect, it, vi } from "vitest";

import type { MailboxItemView } from "@source-inc/gents-desktop-client";
import type { Shell } from "@/hooks/useShell";
import { MailboxScreen } from "../src/ui/screens/MailboxScreen";

/* the sender's hover card reads the whole deployment; the card is not
   what these cases are about */
vi.mock("../src/ui/screens/HoverCards", () => ({
  BehaviorHoverCard: ({ children }: { children: ReactElement }) => children,
}));

const item = (over: Partial<MailboxItemView> = {}): MailboxItemView => ({
  itemId: "item-1",
  itemKey: "key-1",
  requesterDid: "did:key:person",
  agentDid: "did:key:agent",
  status: "open",
  kind: "finished",
  action: "ack",
  title: "Refactor landed",
  summary: null,
  payload: null,
  sourceKind: "session",
  sourceId: "session-1",
  sessionId: "session-1",
  requestId: null,
  graphRunId: null,
  causeDocId: null,
  targetAgentDid: "did:key:agent",
  targetBehaviorId: "engineer",
  expectedCollection: null,
  parentItemId: null,
  deadlineAt: null,
  createdAt: new Date(Date.now() - 5 * 60_000).toISOString(),
  ...over,
});

const shellWith = (items: MailboxItemView[]) =>
  ({
    selectedDeployment: {
      mailboxItems: items,
      behaviors: [{ behaviorId: "engineer", displayName: "Engineer" }],
      sessions: [{ sessionId: "session-1", title: "Mailbox cleanup" }],
    },
  }) as unknown as Shell;

describe("mailbox item", () => {
  it("shows the title, sender, session, time, kind and status", () => {
    render(<MailboxScreen shell={shellWith([item()])} />);
    expect(screen.getByRole("heading", { name: "Refactor landed" })).toBeVisible();
    const meta = screen.getByTestId("mailbox-item-meta");
    expect(within(meta).getByText("Finished")).toBeVisible();
    expect(within(meta).getByText("Open")).toBeVisible();
    expect(within(meta).getByText("Engineer")).toBeVisible();
    const link = within(meta).getByText("in Mailbox cleanup");
    expect(link.closest("a")).toHaveAttribute("href");
    const times = screen.getAllByText("5m ago");
    expect(times[0]!.tagName).toBe("TIME");
    expect(times[0]).toHaveAttribute("title");
  });

  it("names the source by agent, session and time, never its raw identity", () => {
    const sourceId =
      '["event","did:key:agent","did:key:person","engineer","request-1"]';
    render(
      <MailboxScreen shell={shellWith([item({ sourceKind: "agent", sourceId })])} />,
    );
    expect(screen.queryByText(sourceId, { exact: false })).toBeNull();
    expect(screen.queryByText(/did:key:/)).toBeNull();
  });

  it("renders the summary as markdown and keeps its line breaks", () => {
    const summary = "First line\nsecond line\n\n- one\n- two\n\n**bold**";
    render(<MailboxScreen shell={shellWith([item({ summary })])} />);
    const body = screen.getByTestId("mailbox-item-body");
    expect(body.querySelectorAll("li")).toHaveLength(2);
    expect(body.querySelector("strong")).toHaveTextContent("bold");
    const first = body.querySelector("p")!;
    expect(first.querySelector("br")).not.toBeNull();
    expect(first).toHaveTextContent("First line");
    expect(first).toHaveTextContent("second line");
  });

  it("renders a JSON payload as a code block and a text payload as markdown", () => {
    render(
      <MailboxScreen
        shell={shellWith([
          item({ itemId: "a", title: "json", payload: '{"pr":42}' }),
          item({ itemId: "b", title: "text", payload: "## Next\n1. review" }),
        ])}
      />,
    );
    const [json, text] = screen.getAllByTestId("mailbox-item-body");
    expect(json!.querySelector("pre")).toHaveTextContent('"pr": 42');
    expect(text!.querySelector("h2")).toHaveTextContent("Next");
    expect(text!.querySelector("ol li")).toHaveTextContent("review");
  });

  it("folds a long body behind show more and unfolds it", () => {
    const summary = Array.from({ length: 20 }, (_, i) => `line ${i}`).join("\n");
    render(<MailboxScreen shell={shellWith([item({ summary })])} />);
    const body = screen.getByTestId("mailbox-item-body");
    expect(body).toHaveClass("max-h-48");
    const more = screen.getByRole("button", { name: "Show more" });
    expect(more).toHaveAttribute("aria-expanded", "false");
    fireEvent.click(more);
    expect(body).not.toHaveClass("max-h-48");
    const less = screen.getByRole("button", { name: "Show less" });
    expect(less).toHaveAttribute("aria-expanded", "true");
    fireEvent.click(less);
    expect(body).toHaveClass("max-h-48");
  });

  it("offers no fold for a short body", () => {
    render(<MailboxScreen shell={shellWith([item({ summary: "short" })])} />);
    expect(screen.queryByRole("button", { name: "Show more" })).toBeNull();
    expect(screen.getByTestId("mailbox-item-body")).not.toHaveClass("max-h-48");
  });
});
